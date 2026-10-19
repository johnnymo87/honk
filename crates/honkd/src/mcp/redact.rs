//! Redaction of MCP secrets in relayed errors.
//!
//! When enabling an MCP extension fails upstream, goose's error string may contain
//! sensitive tokens passed via environment variables. Honkd inspects the catalog
//! entry's `{file:}` secret files and redacts any occurrences of those secrets before
//! returning the error to viewers.

use crate::mcp::catalog::{EnvVarSpec, GooseExtensionSpec};
use std::collections::BTreeSet;

/// Redacts any `{file:}` secret values found in `err`, and caps the result to
/// its last 2000 characters (prefixed with '…' if truncated).
///
/// Secret values shorter than 4 characters are skipped to avoid over-redacting
/// common short substrings or whitespace.
pub fn redact_mcp_error(err: &str, spec: &GooseExtensionSpec) -> String {
    let mut needles = BTreeSet::new();

    for var in spec.env.values() {
        if let EnvVarSpec::File(path) = var
            && let Ok(content) = std::fs::read_to_string(path)
        {
            let trimmed = content.trim();
            if trimmed.chars().count() >= 4 {
                needles.insert(trimmed.to_string());
            }
            if let Some(first_line) = content.lines().next() {
                let first_trimmed = first_line.trim();
                if first_trimmed.chars().count() >= 4 {
                    needles.insert(first_trimmed.to_string());
                }
            }
        }
    }

    // Sort needles by length descending so longer matches take precedence
    let mut needle_list: Vec<String> = needles.into_iter().collect();
    needle_list.sort_by_key(|a| std::cmp::Reverse(a.len()));

    let mut redacted = err.to_string();
    for needle in needle_list {
        redacted = redacted.replace(&needle, "[redacted]");
    }

    // Cap error to last ~2000 chars (prefix "…" when truncated)
    let char_count = redacted.chars().count();
    if char_count > 2000 {
        let suffix: String = redacted.chars().skip(char_count - 2000).collect();
        format!("…{suffix}")
    } else {
        redacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn make_spec_with_files(files: Vec<(&str, PathBuf)>) -> GooseExtensionSpec {
        let mut env = BTreeMap::new();
        for (k, path) in files {
            env.insert(k.to_string(), EnvVarSpec::File(path));
        }
        GooseExtensionSpec {
            name: "test-server".to_string(),
            command: vec!["test-cmd".to_string()],
            env,
        }
    }

    #[test]
    fn test_secret_in_stderr_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("slack_token");
        std::fs::write(&secret_file, "xoxb-12345-67890-abcdef\n").unwrap();

        let spec = make_spec_with_files(vec![("SLACK_TOKEN", secret_file)]);
        let err = "Failed to connect: token xoxb-12345-67890-abcdef was rejected";
        let redacted = redact_mcp_error(err, &spec);
        assert_eq!(redacted, "Failed to connect: token [redacted] was rejected");
    }

    #[test]
    fn test_missing_file_does_not_panic() {
        let missing = PathBuf::from("/nonexistent/path/to/secret");
        let spec = make_spec_with_files(vec![("TOKEN", missing)]);
        let err = "Some error message without secrets";
        let redacted = redact_mcp_error(err, &spec);
        assert_eq!(redacted, err);
    }

    #[test]
    fn test_empty_file_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("empty_token");
        std::fs::write(&secret_file, "   \n\t  \n").unwrap();

        let spec = make_spec_with_files(vec![("TOKEN", secret_file)]);
        let err = "Normal error message";
        let redacted = redact_mcp_error(err, &spec);
        assert_eq!(redacted, err);
    }

    #[test]
    fn test_short_values_skipped_under_four_chars() {
        let dir = tempfile::tempdir().unwrap();
        let short_file = dir.path().join("short_token");
        std::fs::write(&short_file, "abc\n").unwrap(); // length 3 < 4

        let spec = make_spec_with_files(vec![("SHORT", short_file)]);
        let err = "Error with abc in message";
        let redacted = redact_mcp_error(err, &spec);
        assert_eq!(redacted, "Error with abc in message");

        // Length 4 should be redacted
        let len4_file = dir.path().join("len4_token");
        std::fs::write(&len4_file, "abcd\n").unwrap(); // length 4 >= 4
        let spec4 = make_spec_with_files(vec![("LEN4", len4_file)]);
        let err4 = "Error with abcd in message";
        let redacted4 = redact_mcp_error(err4, &spec4);
        assert_eq!(redacted4, "Error with [redacted] in message");
    }

    #[test]
    fn test_multiline_secret_redacts_both_trimmed_and_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let multi_file = dir.path().join("multi_token");
        std::fs::write(
            &multi_file,
            "first_secret_line_123\nsecond_secret_line_456\n",
        )
        .unwrap();

        let spec = make_spec_with_files(vec![("MULTI", multi_file)]);
        let err = "Error: first_secret_line_123 failed authentication";
        let redacted = redact_mcp_error(err, &spec);
        assert_eq!(redacted, "Error: [redacted] failed authentication");
    }

    #[test]
    fn test_capping_to_last_2000_chars_with_ellipsis() {
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("token");
        std::fs::write(&secret_file, "secret12345").unwrap();
        let spec = make_spec_with_files(vec![("TOKEN", secret_file)]);

        // 3000 chars of 'x' followed by secret12345 and trailing text
        let long_err = format!("{} secret12345 end", "a".repeat(3000));
        let redacted = redact_mcp_error(&long_err, &spec);
        assert!(redacted.starts_with('…'));
        assert_eq!(redacted.chars().count(), 2001); // '…' + 2000 chars
        assert!(redacted.contains("[redacted] end"));
        assert!(!redacted.contains("secret12345"));
    }
}
