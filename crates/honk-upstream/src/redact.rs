use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    replacements: Vec<(String, String)>,
}

impl Redactor {
    /// Creates a new `Redactor` with a list of `(needle, replacement)` pairs.
    ///
    /// Empty needles are discarded. Replacements are ordered from longest to shortest
    /// needles to prevent prefix clobbering.
    pub fn new(mut replacements: Vec<(String, String)>) -> Self {
        replacements.retain(|(from, _)| !from.is_empty());
        replacements.sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));
        Self { replacements }
    }

    /// Builds a `Redactor` from environment context:
    /// - `HONK_GOOSE_SECRET` or `$HOME/.local/state/honk-sandbox/secret`
    /// - contents of `/run/secrets/google_cloud_project` or GCP env vars
    /// - `$HOME`
    pub fn from_env() -> Self {
        let mut replacements = Vec::new();

        // 1. Sandbox secret
        let secret = std::env::var("HONK_GOOSE_SECRET")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if let Ok(home) = std::env::var("HOME") {
                    let path = Path::new(&home).join(".local/state/honk-sandbox/secret");
                    std::fs::read_to_string(path)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });
        if let Some(s) = secret {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty() {
                replacements.push((trimmed, "<SANDBOX_SECRET>".to_string()));
            }
        }

        // 2. /run/secrets/google_cloud_project
        if let Ok(gcp_bytes) = std::fs::read_to_string("/run/secrets/google_cloud_project") {
            let gcp = gcp_bytes.trim().to_string();
            if !gcp.is_empty() {
                replacements.push((gcp, "<GCP_PROJECT_ID>".to_string()));
            }
        }
        for var in ["GCP_PROJECT_ID", "GOOGLE_CLOUD_PROJECT"] {
            if let Ok(val) = std::env::var(var) {
                let val = val.trim().to_string();
                if !val.is_empty() {
                    replacements.push((val, "<GCP_PROJECT_ID>".to_string()));
                }
            }
        }

        // 3. $HOME
        if let Ok(home) = std::env::var("HOME") {
            let home = home.trim().to_string();
            if !home.is_empty() {
                replacements.push((home, "$HOME".to_string()));
            }
        }

        // 4. Org name
        for org in ["wonder", "Wonder", "WONDER"] {
            replacements.push((org.to_string(), "<ORG>".to_string()));
        }

        Self::new(replacements)
    }

    /// Appends an extra replacement pair.
    pub fn add(&mut self, from: impl Into<String>, to: impl Into<String>) {
        let from = from.into();
        if !from.is_empty() {
            self.replacements.push((from, to.into()));
            self.replacements
                .sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));
        }
    }

    /// Recursively applies string replacements to all string values in `value`.
    /// Object keys are never modified.
    pub fn apply(&self, value: Value) -> Value {
        match value {
            Value::String(s) => {
                let mut out = s;
                for (from, to) in &self.replacements {
                    out = out.replace(from, to);
                }
                Value::String(out)
            }
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| self.apply(item)).collect())
            }
            Value::Object(map) => {
                let mut new_map = serde_json::Map::with_capacity(map.len());
                for (k, v) in map {
                    new_map.insert(k, self.apply(v));
                }
                Value::Object(new_map)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replaces_substrings_in_values_but_leaves_keys_untouched() {
        let r = Redactor::new(vec![
            ("s3cret".into(), "<SECRET>".into()),
            ("/home/dev".into(), "$HOME".into()),
        ]);
        assert_eq!(
            r.apply(json!({"a":"s3cret","cwd":"/home/dev/x","n":1})),
            json!({"a":"<SECRET>","cwd":"$HOME/x","n":1})
        );
    }

    #[test]
    fn leaves_object_keys_untouched() {
        let r = Redactor::new(vec![("s3cret".into(), "<SECRET>".into())]);
        assert_eq!(
            r.apply(json!({"s3cret": "value", "nested": {"s3cret": 42}})),
            json!({"s3cret": "value", "nested": {"s3cret": 42}})
        );
    }

    #[test]
    fn handles_arrays_and_nested_structures() {
        let r = Redactor::new(vec![("token123".into(), "<TOKEN>".into())]);
        assert_eq!(
            r.apply(json!([
                "token123",
                {"info": ["nested token123", true, null]},
                100
            ])),
            json!([
                "<TOKEN>",
                {"info": ["nested <TOKEN>", true, null]},
                100
            ])
        );
    }

    #[test]
    fn longest_pattern_replaced_first() {
        let r = Redactor::new(vec![
            ("/home/dev".into(), "$HOME".into()),
            ("/home/dev/special".into(), "$SPECIAL".into()),
        ]);
        assert_eq!(
            r.apply(json!({"path": "/home/dev/special/file"})),
            json!({"path": "$SPECIAL/file"})
        );
    }
}
