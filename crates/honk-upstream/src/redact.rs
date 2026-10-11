use std::path::{Path, PathBuf};

use serde_json::Value;

/// Resolves the file path for custom redaction needles:
/// 1. `$HONK_REDACT_FILE`
/// 2. `$XDG_CONFIG_HOME/honk/redact.txt`
/// 3. `$HOME/.config/honk/redact.txt`
pub fn resolve_redact_file() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("HONK_REDACT_FILE") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return Some(Path::new(trimmed).join("honk/redact.txt"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return Some(Path::new(trimmed).join(".config/honk/redact.txt"));
        }
    }
    None
}

/// Parses redaction needles from a file: one needle per line, ignoring
/// blank lines and `#` comments. Missing file returns empty list.
pub fn parse_redact_file(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn replace_ascii_case_insensitive(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    let from_bytes = from.as_bytes();
    let from_len = from_bytes.len();
    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;
    let mut i = 0;
    while i + from_len <= text.len() {
        if text.is_char_boundary(i)
            && text.is_char_boundary(i + from_len)
            && text.as_bytes()[i..i + from_len].eq_ignore_ascii_case(from_bytes)
        {
            result.push_str(&text[last_end..i]);
            result.push_str(to);
            i += from_len;
            last_end = i;
            continue;
        }
        i += 1;
    }
    result.push_str(&text[last_end..]);
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactRule {
    pub from: String,
    pub to: String,
    pub case_insensitive: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    rules: Vec<RedactRule>,
}

impl Redactor {
    /// Creates a new `Redactor` with a list of `(needle, replacement)` pairs.
    ///
    /// Empty needles are discarded. Replacements are ordered from longest to shortest
    /// needles to prevent prefix clobbering. Needles with replacement `<ORG>` are treated
    /// as org needles and matched ASCII case-insensitively.
    pub fn new(replacements: Vec<(String, String)>) -> Self {
        let mut rules: Vec<RedactRule> = replacements
            .into_iter()
            .filter(|(from, _)| !from.is_empty())
            .map(|(from, to)| {
                let case_insensitive = to == "<ORG>";
                RedactRule {
                    from,
                    to,
                    case_insensitive,
                }
            })
            .collect();
        rules.sort_by_key(|r| std::cmp::Reverse(r.from.len()));
        Self { rules }
    }

    /// Builds a `Redactor` from environment context:
    /// - `HONK_GOOSE_SECRET` or `$HOME/.local/state/honk-sandbox/secret`
    /// - contents of `/run/secrets/google_cloud_project` or GCP env vars
    /// - `$HOME`
    /// - extra needles from `$HONK_REDACT_FILE` or default config file
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

        // 4. Extra needles from config file (modeled as <ORG>)
        if let Some(path) = resolve_redact_file() {
            for needle in parse_redact_file(&path) {
                replacements.push((needle, "<ORG>".to_string()));
            }
        }

        Self::new(replacements)
    }

    /// Appends an extra replacement pair.
    pub fn add(&mut self, from: impl Into<String>, to: impl Into<String>) {
        let from = from.into();
        let to = to.into();
        if !from.is_empty() {
            let case_insensitive = to == "<ORG>";
            self.rules.push(RedactRule {
                from,
                to,
                case_insensitive,
            });
            self.rules.sort_by_key(|r| std::cmp::Reverse(r.from.len()));
        }
    }

    /// Appends an org needle with replacement `<ORG>` and ASCII case-insensitive matching.
    pub fn add_org_needle(&mut self, needle: impl Into<String>) {
        self.add(needle, "<ORG>");
    }

    /// Returns true if this redactor contains at least one org needle.
    pub fn has_org_needles(&self) -> bool {
        self.rules.iter().any(|r| r.to == "<ORG>")
    }

    /// Applies redaction to a single string.
    pub fn redact_string(&self, s: &str) -> String {
        let mut out = s.to_string();
        for rule in &self.rules {
            if rule.case_insensitive {
                out = replace_ascii_case_insensitive(&out, &rule.from, &rule.to);
            } else {
                out = out.replace(&rule.from, &rule.to);
            }
        }
        out
    }

    /// Recursively applies string replacements to all string values and object keys in `value`.
    pub fn apply(&self, value: Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.redact_string(&s)),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| self.apply(item)).collect())
            }
            Value::Object(map) => {
                let mut new_map = serde_json::Map::with_capacity(map.len());
                for (k, v) in map {
                    let base_key = self.redact_string(&k);
                    let mut candidate_key = base_key.clone();
                    let mut counter = 2;
                    while new_map.contains_key(&candidate_key) {
                        candidate_key = format!("{base_key}#{counter}");
                        counter += 1;
                    }
                    new_map.insert(candidate_key, self.apply(v));
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
    fn redacts_substrings_in_values_and_object_keys() {
        let r = Redactor::new(vec![
            ("s3cret".into(), "<SECRET>".into()),
            ("/home/dev".into(), "$HOME".into()),
        ]);
        assert_eq!(
            r.apply(json!({"a":"s3cret","cwd":"/home/dev/x","s3cret_key":1})),
            json!({"a":"<SECRET>","cwd":"$HOME/x","<SECRET>_key":1})
        );
    }

    #[test]
    fn redacts_nested_object_keys() {
        let r = Redactor::new(vec![("s3cret".into(), "<SECRET>".into())]);
        assert_eq!(
            r.apply(json!({"s3cret": "value", "nested": {"s3cret": 42}})),
            json!({"<SECRET>": "value", "nested": {"<SECRET>": 42}})
        );
    }

    #[test]
    fn org_needles_match_case_insensitively_for_ascii() {
        let r = Redactor::new(vec![("acme".into(), "<ORG>".into())]);
        assert_eq!(
            r.apply(json!({
                "AcMe_key": "Welcome to ACME, acme, and AcMe!",
                "nested": {"ACME_CORP": "acme"}
            })),
            json!({
                "<ORG>_key": "Welcome to <ORG>, <ORG>, and <ORG>!",
                "nested": {"<ORG>_CORP": "<ORG>"}
            })
        );
    }

    #[test]
    fn non_org_needles_match_case_sensitively() {
        let r = Redactor::new(vec![("s3cret".into(), "<SECRET>".into())]);
        assert_eq!(
            r.apply(json!({"s3cret": "s3cret", "S3CRET": "S3CRET"})),
            json!({"<SECRET>": "<SECRET>", "S3CRET": "S3CRET"})
        );
    }

    #[test]
    fn colliding_redacted_keys_are_disambiguated() {
        let mut r = Redactor::default();
        r.add_org_needle("alice");
        r.add_org_needle("bob");
        r.add_org_needle("charlie");
        let val = json!({
            "alice": 1,
            "bob": 2,
            "charlie": 3,
        });
        let redacted = r.apply(val);
        assert_eq!(
            redacted,
            json!({
                "<ORG>": 1,
                "<ORG>#2": 2,
                "<ORG>#3": 3,
            })
        );
    }

    #[test]
    fn org_needles_with_utf8_text_and_substring_overlap() {
        let mut r = Redactor::default();
        r.add_org_needle("acme corp");
        r.add_org_needle("acme");
        assert!(r.has_org_needles());

        // Longest needle "acme corp" should be replaced first, and UTF-8 chars around it preserved
        assert_eq!(
            r.apply(json!({
                "föö_ACME CORP_bär": "Hello 🦀 ACME CORP and aCmE! 🚀",
                "nested": ["ACME", "other"]
            })),
            json!({
                "föö_<ORG>_bär": "Hello 🦀 <ORG> and <ORG>! 🚀",
                "nested": ["<ORG>", "other"]
            })
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

    #[test]
    fn parses_redact_file_with_comments_and_blank_lines() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "# Comment line").unwrap();
        writeln!(tmp).unwrap();
        writeln!(tmp, "  acme  ").unwrap();
        writeln!(tmp, "# Another comment").unwrap();
        writeln!(tmp, "ACME_CORP").unwrap();
        writeln!(tmp).unwrap();

        let needles = parse_redact_file(tmp.path());
        assert_eq!(needles, vec!["acme", "ACME_CORP"]);

        let r = Redactor::new(
            needles
                .into_iter()
                .map(|n| (n, "<ORG>".to_string()))
                .collect(),
        );
        assert_eq!(
            r.apply(json!({"msg": "Welcome to acme and ACME_CORP!"})),
            json!({"msg": "Welcome to <ORG> and <ORG>!"})
        );
    }

    #[test]
    fn missing_redact_file_returns_empty() {
        let p = Path::new("/path/that/does/not/exist/honk_test_missing.txt");
        let needles = parse_redact_file(p);
        assert!(needles.is_empty());
    }
}
