//! Front-matter parsing for skill `.md` files.
//!
//! Skills are single Markdown files whose header carries a small YAML subset:
//!
//! ```markdown
//! ---
//! name: PR 代码审查与质量检查
//! description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
//! keywords:
//!   - code review
//!   - pull request
//! allowed_tools:
//!   - github:get_pull_request
//!   - github:create_review_comment
//! fallback: false
//! ---
//! # 执行准则与安全规范
//! ...
//! ```
//!
//! The parser supports scalars (plain / single / double quoted), booleans,
//! integers, `|`/`>` multi-line scalars, `#` comments and `- ` list items.
//! It intentionally does not try to be a full YAML engine — skills that need
//! exotic YAML features are out of scope by design.

use crate::util::log_debug;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// Human-readable skill name (may be non-ASCII).
    pub name: String,
    /// When to use this skill — used by the intent matcher.
    pub description: String,
    /// Extra search keywords, optional.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Whitelist of `server:tool` actions this skill may invoke.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Fallback skills are listed when no query matches; never authorized.
    #[serde(default)]
    pub fallback: bool,
    /// Optional internal version tag.
    #[serde(default)]
    pub version: Option<String>,
}

impl SkillMeta {
    /// Token stream used by the matcher: name + description + keywords.
    pub fn searchable_text(&self) -> String {
        let mut s = self.name.clone();
        s.push('\n');
        s.push_str(&self.description);
        for k in &self.keywords {
            s.push('\n');
            s.push_str(k);
        }
        s
    }
}

/// Split a markdown file into `front-matter` + `body`.
pub fn split_front_matter(content: &str) -> Option<(SkillMeta, String)> {
    let trimmed_start = content.trim_start();
    if !trimmed_start.starts_with("---") {
        return None;
    }
    let after = &trimmed_start[3..];
    // allow `---` alone on first line or `---\n`
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("\n---")?;
    let yaml_text = &after[..end];
    let body_start = end + 4;
    let body = after[body_start..].trim_start().to_string();
    let meta = parse_front_matter(yaml_text)?;
    Some((meta, body))
}

pub fn parse_front_matter(yaml_text: &str) -> Option<SkillMeta> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut keywords: Vec<String> = Vec::new();
    let mut allowed_tools: Vec<String> = Vec::new();
    let mut fallback = false;
    let mut version: Option<String> = None;

    let mut lines: Vec<&str> = yaml_text.lines().collect();
    // Strip comment-only leading lines.
    while let Some(first) = lines.first() {
        if first.trim_start().starts_with('#') || first.trim().is_empty() {
            lines.remove(0);
        } else {
            break;
        }
    }

    let mut idx = 0;
    while idx < lines.len() {
        let raw = lines[idx];
        let line = strip_comment(raw).trim_end();
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }
        if !is_top_level_key(line) {
            log_debug!("front-matter: ignoring unsupported line: {}", line.trim());
            idx += 1;
            continue;
        }
        let (key, value) = split_key_value(line);
        match key.trim() {
            "name" => {
                name = Some(parse_scalar(value.trim()).unwrap_or_default());
            }
            "description" => {
                let value = value.trim();
                let mut out =
                    if value.is_empty() || value.starts_with('|') || value.starts_with('>') {
                        String::new()
                    } else {
                        parse_scalar(value).unwrap_or_default()
                    };
                // multi-line continuation: indented lines that follow
                while idx + 1 < lines.len() {
                    let next = lines[idx + 1];
                    if next.starts_with(' ') || next.starts_with('\t') {
                        let cont = strip_comment(next).trim();
                        if !cont.is_empty() {
                            if out.is_empty() {
                                out.push_str(cont);
                            } else {
                                out.push(' ');
                                out.push_str(cont);
                            }
                        }
                        idx += 1;
                    } else if next.trim().is_empty() {
                        idx += 1;
                    } else {
                        break;
                    }
                }
                description = Some(out);
            }
            "keywords" => {
                keywords = parse_list_block(&lines[idx + 1..])
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                idx += count_list_block(&lines[idx + 1..]);
            }
            "allowed_tools" | "allowedTools" => {
                allowed_tools = parse_list_block(&lines[idx + 1..])
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                idx += count_list_block(&lines[idx + 1..]);
            }
            "fallback" => {
                fallback = parse_bool(parse_scalar(value.trim()).as_deref().unwrap_or(""));
            }
            "version" => {
                version = parse_scalar(value.trim());
            }
            _ => {}
        }
        idx += 1;
    }

    Some(SkillMeta {
        name: name?,
        description: description.unwrap_or_default(),
        keywords,
        allowed_tools,
        fallback,
        version,
    })
}

fn strip_comment(line: &str) -> &str {
    // naive: cut a `#` only when not inside quotes
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q && prev != '\\' => quote = None,
            (None, '#') if i == 0 || line.as_bytes()[i - 1..i].first() == Some(&b' ') => {
                return &line[..i];
            }
            _ => {}
        }
        prev = c;
    }
    line
}

fn is_top_level_key(line: &str) -> bool {
    let trimmed = line.trim_start();
    // key must be `xxx:` and not a list item
    if trimmed.starts_with("- ") || trimmed.starts_with("  ") {
        return false;
    }
    trimmed
        .split_once(':')
        .map(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(false)
}

fn split_key_value(line: &str) -> (String, String) {
    let trimmed = line.trim_start();
    match trimmed.split_once(':') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (trimmed.to_string(), String::new()),
    }
}

fn parse_scalar(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return Some(String::new());
    }
    if s.starts_with('"') {
        let inner = s.trim_start_matches('"');
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        '"' => '"',
                        '\\' => '\\',
                        other => other,
                    });
                }
            } else if c == '"' {
                break;
            } else {
                out.push(c);
            }
        }
        return Some(out);
    }
    if s.starts_with('\'') {
        return Some(
            s.trim_start_matches('\'')
                .trim_end_matches('\'')
                .to_string(),
        );
    }
    Some(s.to_string())
}

fn parse_bool(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

/// Parse a `- item` list from a block of subsequent lines. Returns items.
fn parse_list_block(lines: &[&str]) -> Vec<String> {
    let mut items = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ") {
            items.push(parse_scalar(rest.trim()).unwrap_or_default());
        } else {
            break; // the list ended
        }
    }
    items
}

/// How many lines the list block consumed.
fn count_list_block(lines: &[&str]) -> usize {
    let mut n = 0;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            n += 1;
            continue;
        }
        if trimmed.starts_with("- ") {
            n += 1;
        } else {
            break;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prd_style_skill() {
        let src = r#"---
name: PR 代码审查与质量检查
description: 当需要查看 GitHub PR 变更、审查代码 diff、分析潜在 bug、发表 review 评论时使用
allowed_tools:
  - github:get_pull_request
  - github:create_review_comment
---

# 执行准则与安全规范
1. 必须先通过 `get_pull_request` 拉取完整 diff。
"#;
        let (meta, body) = split_front_matter(src).expect("front matter");
        assert_eq!(meta.name, "PR 代码审查与质量检查");
        assert!(meta.description.contains("PR"));
        assert_eq!(
            meta.allowed_tools,
            vec!["github:get_pull_request", "github:create_review_comment"]
        );
        assert!(!meta.fallback);
        assert!(body.contains("执行准则"));
    }

    #[test]
    fn quoted_and_bool_and_multiline() {
        let src = r#"---
name: "DB Analytics"
description: >
  Analyze postgres slow queries
  and index health
fallback: true
version: "1.2"
---"#;
        let (meta, body) = split_front_matter(src).expect("front matter");
        assert_eq!(meta.name, "DB Analytics");
        let desc = meta.description.replace('\n', " ");
        assert!(
            desc.contains("slow queries") && desc.contains("index health"),
            "{}",
            desc
        );
        assert!(meta.fallback);
        assert_eq!(meta.version.as_deref(), Some("1.2"));
        assert!(body.is_empty());
    }

    #[test]
    fn missing_front_matter_is_none() {
        assert!(split_front_matter("# Just a doc\nno front matter").is_none());
    }

    #[test]
    fn comments_ignored() {
        let src = r#"---
# this is a comment
name: Clean Name # trailing comment
description: some desc
---"#;
        let (meta, _) = split_front_matter(src).unwrap();
        assert_eq!(meta.name, "Clean Name");
    }

    #[test]
    fn empty_allowed_tools_defaults() {
        let src = "---\nname: helper\ndescription: generic helper\n---";
        let (meta, _) = split_front_matter(src).unwrap();
        assert!(meta.allowed_tools.is_empty());
    }
}
