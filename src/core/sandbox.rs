//! Security sandbox: whitelist enforcement for `call_action`.
//!
//! A tool call is ONLY routed downstream when `action` is strictly present in
//! the activated skill's `allowed_tools`. Anything else is blocked with a
//! standard error and never reaches a downstream process — the PRD requires
//! 100% interception of hallucinated / out-of-whitelist calls.

use crate::core::registry::split_action;
use crate::core::skillstore::Skill;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Action authorized; `server:tool` parsed.
    Allow { server: String, tool: String },
    /// Action blocked; reason explains why.
    Deny { reason: String },
}

/// Authorize `action` against the activated skill's whitelist.
///
/// Allowed entries may be `server:tool` or a bare `tool` name (matched
/// against the tool part of `action`). Skills with an empty whitelist deny
/// everything.
pub fn authorize(skill: &Skill, action: &str) -> Verdict {
    let (server, tool) = match split_action(action) {
        Some(pair) => pair,
        None => {
            return Verdict::Deny {
                reason: format!(
                    "action '{action}' is malformed (expected 'server_name:tool_name')"
                ),
            }
        }
    };

    if skill.meta.allowed_tools.is_empty() {
        return Verdict::Deny {
            reason: format!(
                "skill '{}' authorizes no tools — it must be discovered and cannot be used to call actions",
                skill.name()
            ),
        };
    }

    let whitelist = &skill.meta.allowed_tools;
    let allowed = whitelist
        .iter()
        .any(|entry| entry == action || entry == &tool || entry == &format!("{server}:{tool}"));

    if allowed {
        Verdict::Allow { server, tool }
    } else {
        Verdict::Deny {
            reason: format!(
                "BLOCKED: action '{action}' is not in the allowed_tools whitelist of skill '{}' \
                 (whitelist: {})",
                skill.name(),
                whitelist.join(", ")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::yaml_fm::SkillMeta;

    fn skill_with(tools: Vec<&str>) -> Skill {
        Skill {
            meta: SkillMeta {
                name: "review".into(),
                description: String::new(),
                keywords: vec![],
                allowed_tools: tools.iter().map(|s| s.to_string()).collect(),
                fallback: false,
                version: None,
            },
            body: String::new(),
            path: std::path::PathBuf::new(),
            priority: 0,
        }
    }

    #[test]
    fn exact_whitelist_match_allows() {
        let s = skill_with(vec!["github:get_pull_request"]);
        assert_eq!(
            authorize(&s, "github:get_pull_request"),
            Verdict::Allow {
                server: "github".into(),
                tool: "get_pull_request".into()
            }
        );
    }

    #[test]
    fn out_of_whitelist_is_blocked() {
        let s = skill_with(vec!["github:get_pull_request"]);
        let verdict = authorize(&s, "github:create_review_comment");
        assert!(matches!(verdict, Verdict::Deny { .. }));
        if let Verdict::Deny { reason } = verdict {
            assert!(reason.starts_with("BLOCKED:"));
        }
    }

    #[test]
    fn blocked_never_allowed_even_for_other_server() {
        let s = skill_with(vec!["github:get_pull_request"]);
        assert!(matches!(
            authorize(&s, "postgres:query"),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn bare_tool_name_matches() {
        let s = skill_with(vec!["get_pull_request"]);
        assert!(matches!(
            authorize(&s, "github:get_pull_request"),
            Verdict::Allow { .. }
        ));
    }

    #[test]
    fn empty_whitelist_denies_everything() {
        let s = skill_with(vec![]);
        assert!(matches!(authorize(&s, "github:x"), Verdict::Deny { .. }));
    }

    #[test]
    fn malformed_action_denied() {
        let s = skill_with(vec!["github:get_pull_request"]);
        assert!(matches!(
            authorize(&s, "no-colon-here"),
            Verdict::Deny { .. }
        ));
        assert!(matches!(
            authorize(&s, ":leading-colon"),
            Verdict::Deny { .. }
        ));
    }
}
