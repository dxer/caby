//! Schema Minifier.
//!
//! Downstream MCP servers return verbose JSON Schemas (often carrying
//! `$schema`, `title`, `pattern`, `minLength`, `default`, `examples`, internal
//! validation regexes, ...) that cost real tokens every time they are handed
//! to a model. The minifier recursively prunes those fields and keeps only
//! the call essentials: `type`, `properties`, `required`, `description`,
//! `items`, small `enum`s and type-alternatives. Typical savings: 30-50%.

use serde_json::{Map, Value};

/// Fields dropped from every schema node.
const STRIPPED_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "$ref",
    "title",
    "definitions",
    "$defs",
    "examples",
    "example",
    "default",
    "const",
    "pattern",
    "format",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "deprecated",
    "readOnly",
    "writeOnly",
    "additionalProperties",
    "additionalItems",
    "discriminator",
    "xml",
    "externalDocs",
    "operationId",
    "nullable",
];

/// Fields that must survive (type information).
const KEPT_KEYS: &[&str] = &[
    "type",
    "properties",
    "required",
    "description",
    "items",
    "anyOf",
    "oneOf",
    "allOf",
    "enum",
    "prefixItems",
];

/// Maximum description length retained before truncation.
const MAX_DESCRIPTION_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, Default)]
pub struct MinifyStats {
    pub original_chars: usize,
    pub minified_chars: usize,
    pub fields_removed: usize,
}

impl MinifyStats {
    #[allow(dead_code)]
    pub fn reduction_pct(&self) -> f64 {
        if self.original_chars == 0 {
            return 0.0;
        }
        100.0 * (1.0 - self.minified_chars as f64 / self.original_chars as f64)
    }
}

/// Minify a single input schema, returning (minified_json, stats).
pub fn minify_schema(input: &Value) -> (Value, MinifyStats) {
    let original_chars = input.to_string().len();
    let mut stats = MinifyStats {
        original_chars,
        ..Default::default()
    };
    let out = prune(input, &mut stats);
    stats.minified_chars = out.to_string().len();
    (out, stats)
}

fn prune(node: &Value, stats: &mut MinifyStats) -> Value {
    prune_inner(node, stats, false)
}

/// `in_properties`: keys inside a `properties` object are user-defined field
/// names and MUST be preserved (their values are pruned as schemas).
fn prune_inner(node: &Value, stats: &mut MinifyStats, in_properties: bool) -> Value {
    match node {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, val) in map {
                if in_properties {
                    out.insert(key.clone(), prune_inner(val, stats, false));
                    continue;
                }
                // keep a trimmed description
                if key == "description" {
                    if let Value::String(s) = val {
                        let trimmed = truncate(s);
                        out.insert(key.clone(), Value::String(trimmed));
                        continue;
                    }
                }
                if is_strippable(key) {
                    stats.fields_removed += 1;
                    continue;
                }
                if !is_keepable(key) {
                    // unknown structural field: drop non-type metadata
                    stats.fields_removed += 1;
                    continue;
                }
                // `enum` kept only when small — it is type information.
                if key == "enum" {
                    if let Value::Array(items) = val {
                        if items.len() > 8 {
                            stats.fields_removed += 1;
                            continue;
                        }
                    }
                }
                let recurse_props = key == "properties";
                out.insert(key.clone(), prune_inner(val, stats, recurse_props));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| prune_inner(v, stats, false)).collect())
        }
        other => other.clone(),
    }
}

fn is_strippable(key: &str) -> bool {
    STRIPPED_KEYS.contains(&key)
}

fn is_keepable(key: &str) -> bool {
    KEPT_KEYS.contains(&key)
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_DESCRIPTION_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_DESCRIPTION_CHARS).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const VERBOSE: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Get Pull Request",
  "type": "object",
  "additionalProperties": false,
  "required": ["pull_number", "repo"],
  "properties": {
    "pull_number": {
      "type": "integer",
      "title": "Pull Number",
      "minimum": 1,
      "examples": [42],
      "description": "The pull request number to fetch details for"
    },
    "repo": {
      "type": "string",
      "pattern": "^[a-zA-Z0-9-]+/[a-zA-Z0-9-]+$",
      "minLength": 3,
      "default": "owner/repo",
      "description": "Owner/repository slug"
    },
    "secret": {
      "type": "string",
      "format": "password",
      "const": "SENSITIVE",
      "description": "Never exposed"
    }
  }
}"##;

    #[test]
    fn strips_noise_keeps_essentials() {
        let v: Value = serde_json::from_str(VERBOSE).unwrap();
        let (min, stats) = minify_schema(&v);

        let obj = min.as_object().unwrap();
        assert!(!obj.contains_key("$schema"));
        assert!(!obj.contains_key("title"));
        assert!(!obj.contains_key("additionalProperties"));
        assert_eq!(obj["type"], Value::from("object"));

        let props = obj["properties"].as_object().unwrap();
        let pr = &props["pull_number"];
        assert!(!pr.as_object().unwrap().contains_key("minimum"));
        assert!(!pr.as_object().unwrap().contains_key("examples"));
        assert_eq!(pr["type"], Value::from("integer"));
        assert!(pr["description"].as_str().unwrap().contains("pull request number"));

        let repo = &props["repo"];
        assert!(!repo.as_object().unwrap().contains_key("pattern"));
        assert!(!repo.as_object().unwrap().contains_key("default"));
        assert!(!repo.as_object().unwrap().contains_key("minLength"));

        assert!(stats.reduction_pct() > 30.0, "reduction {}%", stats.reduction_pct());
        assert!(stats.fields_removed >= 10);
    }

    #[test]
    fn keeps_small_enum_and_items() {
        let v = json!({
            "type": "object",
            "properties": {
                "state": {"type": "string", "enum": ["open", "closed", "merged"]},
                "labels": {"type": "array", "items": {"type": "string"}}
            }
        });
        let (min, _) = minify_schema(&v);
        let props = min["properties"].as_object().unwrap();
        assert_eq!(props["state"]["enum"].as_array().unwrap().len(), 3);
        assert_eq!(props["labels"]["items"]["type"], Value::from("string"));
    }

    #[test]
    fn drops_huge_enum() {
        let items: Vec<Value> = (0..20).map(Value::from).collect();
        let v = json!({"type": "string", "enum": items});
        let (min, _) = minify_schema(&v);
        assert!(!min.as_object().unwrap().contains_key("enum"));
    }

    #[test]
    fn truncates_long_descriptions() {
        let long = "x".repeat(500);
        let v = json!({"type": "string", "description": long});
        let (min, _) = minify_schema(&v);
        assert!(min["description"].as_str().unwrap().chars().count() <= 201);
    }
}