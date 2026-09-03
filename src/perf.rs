//! Performance gate for the PRD's "internal routing + schema minification +
//! intent matching ≤ 5 ms" requirement.
//!
//! These are pure CPU workloads measured best-of-N. Debug builds run much
//! slower, so the strict 5 ms bound is enforced for release builds
//! (`cargo test --release`) and a relaxed sanity bound in debug.

#[cfg(test)]
mod tests {
    use crate::core::matcher::{Doc, Matcher};
    use crate::core::minifier::minify_schema;
    use serde_json::{json, Value};
    use std::time::Instant;

    fn verbose_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Search Code",
            "type": "object",
            "additionalProperties": false,
            "required": ["query", "repo"],
            "properties": {
                "query": {
                    "type": "string",
                    "pattern": "^[a-zA-Z0-9 _.\\-]+$",
                    "minLength": 1,
                    "maxLength": 256,
                    "description": "Search query string used to match code snippets"
                },
                "repo": {
                    "type": "string",
                    "pattern": "^[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+$",
                    "minLength": 3,
                    "default": "owner/repo",
                    "description": "Owner/repository slug to scope the search"
                },
                "path": {
                    "type": "string",
                    "examples": ["src/main.rs"],
                    "description": "Optional path filter"
                },
                "per_page": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 10,
                    "description": "Number of results per page"
                }
            }
        })
    }

    fn skill_corpus(n: usize) -> Vec<Doc> {
        (0..n)
            .map(|i| {
                Doc::build(
                    format!("skill_{i}"),
                    &format!(
                        "Skill {i} focused on domain {i}: database query analysis, index health,
                         postgres performance, code review of pull requests, github issue
                         management, docker deployment, kubernetes operations, ci pipeline
                         debugging, frontend accessibility audit, api rate limit tuning"
                    ),
                )
            })
            .collect()
    }

    fn best_of<F: FnMut()>(mut f: F, iters: usize) -> f64 {
        // warmup
        f();
        let mut best = f64::MAX;
        for _ in 0..iters {
            let t = Instant::now();
            f();
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        best
    }

    #[test]
    fn intent_matching_over_100_skills_under_5ms() {
        let mut matcher = Matcher::new();
        matcher.rebuild(skill_corpus(100));
        let per = best_of(
            || {
                matcher.rank("postgres slow query index health", 3);
            },
            200,
        );
        let strict = !cfg!(debug_assertions);
        eprintln!(
            "matcher.rank over 100 skills: best {per:.3} ms ({})",
            if strict { "strict" } else { "debug" }
        );
        if strict {
            assert!(per <= 5.0, "intent matching took {per:.3} ms (>5 ms)");
        } else {
            assert!(
                per <= 50.0,
                "debug intent matching pathologically slow: {per:.3} ms"
            );
        }
    }

    #[test]
    fn schema_minification_under_5ms() {
        let schema = verbose_schema();
        let per = best_of(
            || {
                let _ = minify_schema(&schema);
            },
            300,
        );
        let strict = !cfg!(debug_assertions);
        eprintln!(
            "minify_schema: best {per:.3} ms ({})",
            if strict { "strict" } else { "debug" }
        );
        if strict {
            assert!(per <= 5.0, "schema minification took {per:.3} ms (>5 ms)");
        } else {
            assert!(
                per <= 50.0,
                "debug minification pathologically slow: {per:.3} ms"
            );
        }
    }

    #[test]
    fn full_discovery_pipeline_under_5ms() {
        // discovery = match + payload assembly over registered tool schemas
        let mut matcher = Matcher::new();
        matcher.rebuild(skill_corpus(50));
        let schemas: Vec<Value> = (0..20).map(|_| verbose_schema()).collect();
        let per = best_of(
            || {
                let ranked = matcher.rank("github pull request code review", 3);
                for (id, _) in ranked {
                    // simulate payload assembly: minify each whitelisted schema
                    let _n = id.len();
                }
                for s in &schemas {
                    let _ = minify_schema(s);
                }
            },
            100,
        );
        let strict = !cfg!(debug_assertions);
        eprintln!(
            "discovery pipeline (match + 20 minified schemas): best {per:.3} ms ({})",
            if strict { "strict" } else { "debug" }
        );
        if strict {
            assert!(per <= 5.0, "discovery pipeline took {per:.3} ms (>5 ms)");
        } else {
            assert!(
                per <= 50.0,
                "debug discovery pipeline pathologically slow: {per:.3} ms"
            );
        }
    }
}
