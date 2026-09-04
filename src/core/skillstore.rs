//! Skill auto-discovery: directory scanning, front-matter parsing, hot reload.
//!
//! Scan order (first match wins):
//!   1. project:  `<cwd>/.caby/skills/`
//!   2. global:   `~/.config/caby/skills/`
//!
//! A filesystem watcher (notify) watches both trees and rebuilds the in-memory
//! index (with a short debounce) on any create/modify/remove — the PRD target
//! is index reload within 100 ms of a `.md` change, no restarts required.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use crate::core::matcher::{Doc, Matcher};
use crate::core::yaml_fm::{split_front_matter, SkillMeta};
use crate::util::{global_skills_dir, log_debug, log_info, log_warn, project_skills_dir};

/// Debounce for fs-event bursts. Small enough that index reload stays
/// well under the 100 ms PRD budget, large enough to coalesce the 2-5 events
/// a single editor write typically produces.
const DEBOUNCE: Duration = Duration::from_millis(12);

#[derive(Debug, Clone)]
pub struct Skill {
    pub meta: SkillMeta,
    /// Markdown body = the SOP text handed to the model.
    pub body: String,
    pub path: PathBuf,
    /// 0 = project, 1 = global.
    pub priority: u8,
}

impl Skill {
    pub fn name(&self) -> &str {
        &self.meta.name
    }
    pub fn is_fallback(&self) -> bool {
        self.meta.fallback || self.meta.allowed_tools.is_empty()
    }
}

pub struct SkillStore {
    skills: Vec<Skill>,
    by_name: HashMap<String, usize>,
    matcher: Matcher,
    revision: AtomicU64,
    _watcher: Option<Box<dyn Watcher + Send + Sync>>,
}

impl SkillStore {
    pub fn new() -> Self {
        SkillStore {
            skills: Vec::new(),
            by_name: HashMap::new(),
            matcher: Matcher::new(),
            revision: AtomicU64::new(0),
            _watcher: None,
        }
    }

    pub fn scan_paths(&mut self) {
        self.scan_from(project_skills_dir(), global_skills_dir());
    }

    /// Scan explicit directories (project dir wins on duplicate names).
    /// Exists so tests don't need to mutate cwd / XDG env vars.
    pub fn scan_from(&mut self, project_dir: PathBuf, global_dir: PathBuf) {
        let mut collected: Vec<Skill> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (priority, dir) in [(0u8, project_dir), (1u8, global_dir)] {
            if !dir.is_dir() {
                continue;
            }
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) => {
                    log_warn!("cannot read skills dir {}: {e}", dir.display());
                    continue;
                }
            };
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
                .collect();
            files.sort();
            for file in files {
                match load_skill_file(&file, priority) {
                    Some(skill) => {
                        // project shadows global on duplicate names
                        if seen.contains(&skill.name().to_lowercase()) {
                            if priority == 0 {
                                collected.retain(|s| !s.name().eq_ignore_ascii_case(skill.name()));
                            } else {
                                continue; // global dup hidden by project
                            }
                        }
                        seen.insert(skill.name().to_lowercase());
                        collected.push(skill);
                    }
                    None => log_debug!("skip {}", file.display()),
                }
            }
        }
        self.rebuild(collected);
    }

    fn rebuild(&mut self, skills: Vec<Skill>) {
        self.skills = skills;
        self.by_name = self
            .skills
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name().to_lowercase(), i))
            .collect();
        // matcher index over all non-fallback skills
        let docs: Vec<Doc> = self
            .skills
            .iter()
            .filter(|s| !s.is_fallback())
            .map(|s| Doc::build(s.name().to_string(), &s.meta.searchable_text()))
            .collect();
        self.matcher.rebuild(docs);
        self.revision.fetch_add(1, Ordering::SeqCst);
        log_info!("skill index reloaded: {} skills active", self.skills.len());
    }

    #[allow(dead_code)]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    pub fn all_skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Case-insensitive lookup, project skills shadow global ones.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name
            .get(&name.to_lowercase())
            .and_then(|&i| self.skills.get(i))
    }

    pub fn rank_regular(&self, query: &str, top_k: usize, min_score: f64) -> Vec<(&Skill, f64)> {
        self.matcher
            .rank(query, top_k)
            .into_iter()
            .filter(|(_, s)| *s >= min_score)
            .filter_map(|(id, score)| {
                // id is an index name (doc id = skill name); match case-insensitively
                self.skills
                    .iter()
                    .find(|s| s.name().to_lowercase() == id.to_lowercase())
                    .map(|s| (s, score))
            })
            .collect()
    }

    /// Fallback skills: shown when nothing matched.
    pub fn fallback_skills(&self) -> Vec<&Skill> {
        self.skills.iter().filter(|s| s.is_fallback()).collect()
    }

    /// Start hot-reload watchers on the skill directories (if any exist).
    /// The callback only flags dirtiness; the async rescan loop performs the
    /// debounced rebuild (see `run_debounced_rescan`).
    pub fn start_watchers(
        &mut self,
        dirty: Arc<std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<()> {
        self.start_watchers_on(dirty, project_skills_dir(), global_skills_dir())
    }

    /// Build the platform skill watcher.
    ///
    /// macOS's FSEvents delivers events with scheduler-dependent latency
    /// (flaky on CI: a skill write can take hundreds of ms to surface), so on
    /// macOS we poll the directories every 30 ms instead — deterministic
    /// ≤30 ms detection, one stat per dir per tick. Other platforms keep
    /// event-driven watchers (inotify / kqueue / ReadDirectoryChangesW).
    /// Both branches are type-checked on every platform (`cfg!` is a runtime
    /// bool), so a normal `cargo check` catches both.
    // ponytail: macOS polls at a fixed 20 ms cadence; revisit only if the
    // per-tick stat cost ever shows up (it won't: ≤2 flat dirs).
    fn start_watchers_on(
        &mut self,
        dirty: Arc<std::sync::atomic::AtomicBool>,
        project_dir: PathBuf,
        global_dir: PathBuf,
    ) -> anyhow::Result<()> {
        let mut watcher = make_watcher(dirty)?;
        for dir in [project_dir, global_dir] {
            if dir.is_dir() || dir.parent().is_some_and(|p| p.is_dir()) {
                let target = if dir.is_dir() {
                    dir.clone()
                } else {
                    dir.parent().unwrap().to_path_buf()
                };
                match watcher.watch(&target, RecursiveMode::Recursive) {
                    Ok(()) => log_info!("watching skills: {}", target.display()),
                    Err(e) => log_warn!("cannot watch {}: {e}", target.display()),
                }
            }
        }

        self._watcher = Some(watcher);
        Ok(())
    }
}

/// Build the platform skill watcher.
///
/// macOS's FSEvents delivers events with scheduler-dependent latency (flaky
/// on CI: a skill write can take hundreds of ms to surface), so on macOS we
/// poll the directories every 30 ms instead — deterministic ≤30 ms detection
/// at the cost of one stat per dir per tick. Other platforms keep
/// event-driven watchers (inotify / kqueue / ReadDirectoryChangesW).
/// `cfg!` is a runtime bool, so both branches are type-checked on every
/// platform — a regular `cargo check` covers both.
// ponytail: macOS polls at a fixed 20 ms cadence; revisit only if the
// per-tick stat cost ever shows up (it won't: ≤2 flat dirs).
fn make_watcher(
    dirty: Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<Box<dyn Watcher + Send + Sync>> {
    let handler = move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            let relevant = matches!(
                ev.kind,
                notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_)
            );
            if relevant {
                dirty.store(true, Ordering::Release);
            }
        }
        Err(e) => log_warn!("skills watcher event error: {e}"),
    };
    if cfg!(target_os = "macos") {
        // Compare file contents: PollWatcher only tracks second-resolution
        // mtime, so a modify in the same second as a create would otherwise
        // be missed entirely.
        let config = notify::Config::default()
            .with_poll_interval(std::time::Duration::from_millis(20))
            .with_compare_contents(true);
        Ok(Box::new(notify::PollWatcher::new(handler, config)?))
    } else {
        Ok(Box::new(notify::recommended_watcher(handler)?))
    }
}

fn load_skill_file(path: &Path, priority: u8) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (meta, body) = split_front_matter(&raw)?;
    Some(Skill {
        meta,
        body,
        path: path.to_path_buf(),
        priority,
    })
}

/// Debounced rescan task: waits for a shared "dirty" flag set by the watcher
/// callback, then rescans the store. Keeps index reloads well under 100 ms.
pub async fn run_debounced_rescan(
    store: Arc<std::sync::Mutex<SkillStore>>,
    dirty: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.notified() => return Ok(()),
            _ = async {
                // busy-wait for dirtiness (cheap; wakeups are debounced)
                loop {
                    if dirty.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } => {
                // debounce: absorb a burst of fs events
                tokio::time::sleep(DEBOUNCE).await;
                dirty.store(false, Ordering::Release);
                if let Ok(mut guard) = store.lock() {
                    guard.scan_paths();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skills(project: &std::path::Path, files: &[(&str, &str)]) {
        for (name, content) in files {
            std::fs::write(project.join(name), content).unwrap();
        }
    }

    #[test]
    fn scans_project_and_global_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".caby").join("skills");
        std::fs::create_dir_all(&project).unwrap();
        write_skills(
            &project,
            &[(
                "git_review.md",
                "---\nname: PR 代码审查与质量检查\ndescription: review PR diffs\nallowed_tools:\n  - github:get_pull_request\n---\n# SOP\n",
            )],
        );
        let global = tmp.path().join("glob").join("skills");
        std::fs::create_dir_all(&global).unwrap();
        write_skills(
            &global,
            &[(
                "general_helper.md",
                "---\nname: General Helper\ndescription: generic assistance\n---\n# SOP\n",
            )],
        );

        let mut store = SkillStore::new();
        store.scan_from(project, global);
        assert_eq!(store.all_skills().len(), 2);
        assert!(store.get("PR 代码审查与质量检查").is_some());
        assert!(store.get("General Helper").is_some());
    }

    #[test]
    fn hot_reload_detects_new_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".caby").join("skills");
        std::fs::create_dir_all(&project).unwrap();
        let global = tmp.path().join("glob-missing").join("skills");

        let mut store = SkillStore::new();
        store.scan_from(project.clone(), global.clone());
        assert!(store.all_skills().is_empty());
        let rev0 = store.revision();

        write_skills(
            &project,
            &[(
                "new_skill.md",
                "---\nname: New Skill\ndescription: something new\n---\nbody",
            )],
        );
        store.scan_from(project.clone(), global.clone()); // manual rescan (sync path)
        assert_eq!(store.all_skills().len(), 1);
        assert!(store.revision() > rev0);

        std::fs::remove_file(project.join("new_skill.md")).unwrap();
        store.scan_from(project, global);
        assert!(store.all_skills().is_empty());
    }

    #[test]
    fn fallback_skills_excluded_from_ranking() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join(".caby").join("skills");
        std::fs::create_dir_all(&project).unwrap();
        let global = tmp.path().join("glob-missing").join("skills");

        write_skills(
            &project,
            &[
                (
                    "fallback.md",
                    "---\nname: Everything Helper\ndescription: catch all fallback\nfallback: true\n---\nSOP",
                ),
                (
                    "real.md",
                    "---\nname: Postgres Tuning\ndescription: database slow query analysis\nallowed_tools:\n  - postgres:query\n---\nSOP",
                ),
            ],
        );

        let mut store = SkillStore::new();
        store.scan_from(project, global);
        let ranked = store.rank_regular("slow postgres query", 5, 0.0);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0.name(), "Postgres Tuning");
        assert_eq!(store.fallback_skills().len(), 1);
    }
}
