//! MCP catalog loader and translator.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpKind {
    Local,
    Remote,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvVarSpec {
    Literal(String),
    File(PathBuf),
    Env(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GooseExtensionSpec {
    pub name: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, EnvVarSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub name: String,
    pub supported: Result<GooseExtensionSpec, String>,
    pub kind: McpKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
}

/// Description marker prefix identifying MCP extensions managed by honk.
///
/// Goose persists added extensions in the session database. Honk stamps every extension
/// it adds with `honk-mcp:<name>` in its `description`. This marker allows honkd to:
/// 1. Recognize honk-managed extensions vs other extensions added by the user or goose
///    (e.g., built-in developer tools, platform extensions, or coordinator tools).
/// 2. Protect third-party extensions from being modified or removed by honk's `/mcp` commands.
pub const MARKER_PREFIX: &str = "honk-mcp:";

/// Returns the marker description string for an MCP server name.
pub fn marker(name: &str) -> String {
    format!("{MARKER_PREFIX}{name}")
}

// Disallowed environment keys copied from goose:
// crates/goose/src/agents/extension.rs around lines 89-141.
// These variables can hijack binary paths, dynamic linkers, language runtimes,
// or session data. Goose silently drops these keys.
const DISALLOWED_ENV_KEYS: [&str; 31] = [
    // Binary path manipulation
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "windir",
    // Dynamic linker hijacking (Linux/macOS)
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_DEBUG",
    "LD_BIND_NOW",
    "LD_ASSUME_KERNEL",
    // macOS dynamic linker variables
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_FRAMEWORK_PATH",
    // Python / Node / Ruby / Java / Golang hijacking
    "PYTHONPATH",
    "PYTHONHOME",
    "NODE_OPTIONS",
    "RUBYOPT",
    "GEM_PATH",
    "GEM_HOME",
    "CLASSPATH",
    "GO111MODULE",
    "GOROOT",
    // Windows-specific process & DLL hijacking
    "APPINIT_DLLS",
    "SESSIONNAME",
    "ComSpec",
    "TEMP",
    "TMP",
    "LOCALAPPDATA",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
];

pub fn name_to_key(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => result.push(c),
            c if c.is_whitespace() => continue,
            _ => result.push('_'),
        }
    }
    result.to_lowercase()
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_disallowed_env_key(key: &str) -> bool {
    DISALLOWED_ENV_KEYS
        .iter()
        .any(|disallowed| disallowed.eq_ignore_ascii_case(key))
}

fn key_looks_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("API_KEY")
        || upper.contains("AUTH")
        || upper.ends_with("_KEY")
}

impl GooseExtensionSpec {
    /// Translates this MCP extension spec into the JSON value for goose's
    /// `_goose/unstable/session/extensions/add` RPC (`extension` parameter).
    ///
    /// If there is no environment defined, the server command and args are passed directly,
    /// preserving goose's npx/uvx malware checks.
    ///
    /// If an environment is defined, the process is wrapped with `/bin/sh -c WRAPPER_SCRIPT`.
    /// Secrets are referenced only by file path (never by value) in argv, ensuring secret
    /// contents never appear in process listings, database logs, or crash dumps.
    pub fn to_add_extension_json(&self) -> serde_json::Value {
        if self.env.is_empty() {
            let cmd0 = self.command.first().cloned().unwrap_or_default();
            let args = self.command.iter().skip(1).cloned().collect::<Vec<_>>();
            serde_json::json!({
                "type": "mcp",
                "description": marker(&self.name),
                "server": {
                    "name": self.name,
                    "command": cmd0,
                    "args": args,
                    "env": []
                }
            })
        } else {
            let mut args = Vec::new();
            args.push("-c".to_string());
            args.push(crate::mcp::wrapper::WRAPPER_SCRIPT.to_string());
            args.push(format!("honk-mcp-{}", self.name));

            // BTreeMap iteration is sorted by key
            for (key, val_spec) in &self.env {
                args.push(key.clone());
                match val_spec {
                    EnvVarSpec::Literal(val) => {
                        args.push("lit".to_string());
                        args.push(val.clone());
                    }
                    EnvVarSpec::File(path) => {
                        args.push("file".to_string());
                        args.push(path.to_string_lossy().to_string());
                    }
                    EnvVarSpec::Env(parent_var) => {
                        args.push("env".to_string());
                        args.push(parent_var.clone());
                    }
                }
            }

            args.push("--".to_string());
            for c in &self.command {
                args.push(c.clone());
            }

            serde_json::json!({
                "type": "mcp",
                "description": marker(&self.name),
                "server": {
                    "name": self.name,
                    "command": "/bin/sh",
                    "args": args,
                    "env": []
                }
            })
        }
    }
}

impl Catalog {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::load_with_home(path, home.as_deref())
    }

    pub fn load_with_home(path: impl AsRef<Path>, home: Option<&Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        Self::load_from_str(&content, home)
    }

    pub fn load_from_str(json_str: &str, home: Option<&Path>) -> anyhow::Result<Self> {
        let root: serde_json::Value = serde_json::from_str(json_str)?;
        let mcp_obj = match root.get("mcp").and_then(|v| v.as_object()) {
            Some(obj) => obj,
            None => {
                return Ok(Self {
                    entries: Vec::new(),
                });
            }
        };

        let mut entries = Vec::new();
        for (name, val) in mcp_obj {
            let entry = parse_entry(name, val, home);
            entries.push(entry);
        }

        // Keep entries sorted by name
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { entries })
    }

    pub fn get(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

fn parse_entry(name: &str, val: &serde_json::Value, home: Option<&Path>) -> CatalogEntry {
    let type_str = val.get("type").and_then(|v| v.as_str());
    let kind = match type_str {
        Some("local") => McpKind::Local,
        Some("remote") => McpKind::Remote,
        _ => McpKind::Unknown,
    };

    // Validate server name
    if name.is_empty() || name != name_to_key(name) {
        return CatalogEntry {
            name: name.to_string(),
            supported: Err(format!(
                "invalid server name '{name}'; must be a non-empty goose extension key matching [a-z0-9_-]"
            )),
            kind,
        };
    }

    // Validate type
    match type_str {
        Some("local") => {}
        Some("remote") => {
            return CatalogEntry {
                name: name.to_string(),
                supported: Err("remote MCP servers are not supported yet".to_string()),
                kind: McpKind::Remote,
            };
        }
        Some(other) => {
            return CatalogEntry {
                name: name.to_string(),
                supported: Err(format!("unsupported MCP server type '{other}'")),
                kind: McpKind::Unknown,
            };
        }
        None => {
            return CatalogEntry {
                name: name.to_string(),
                supported: Err("missing MCP server type".to_string()),
                kind: McpKind::Unknown,
            };
        }
    }

    // Validate command
    let cmd_val = match val.get("command") {
        Some(c) => c,
        None => {
            return CatalogEntry {
                name: name.to_string(),
                supported: Err("missing command".to_string()),
                kind,
            };
        }
    };

    let cmd_arr = match cmd_val.as_array() {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            return CatalogEntry {
                name: name.to_string(),
                supported: Err("command must be a non-empty array of strings".to_string()),
                kind,
            };
        }
    };

    let mut command = Vec::new();
    for item in cmd_arr {
        let s = match item.as_str() {
            Some(s) if !s.is_empty() => s,
            _ => {
                return CatalogEntry {
                    name: name.to_string(),
                    supported: Err("command items must be non-empty strings".to_string()),
                    kind,
                };
            }
        };

        if s.contains("{file:") || s.contains("{env:") {
            return CatalogEntry {
                name: name.to_string(),
                supported: Err("'{file:' or '{env:' cannot appear in command items".to_string()),
                kind,
            };
        }

        command.push(s.to_string());
    }

    // If command[0] is an absolute path, verify existence
    let cmd0 = &command[0];
    let cmd0_path = Path::new(cmd0);
    if cmd0_path.is_absolute() && !cmd0_path.exists() {
        return CatalogEntry {
            name: name.to_string(),
            supported: Err(format!("command path '{cmd0}' does not exist")),
            kind,
        };
    }

    // Validate environment
    let mut env = BTreeMap::new();
    if let Some(env_val) = val.get("environment") {
        let env_obj = match env_val.as_object() {
            Some(obj) => obj,
            None => {
                return CatalogEntry {
                    name: name.to_string(),
                    supported: Err("environment must be an object".to_string()),
                    kind,
                };
            }
        };

        for (env_key, env_v) in env_obj {
            if !is_valid_env_key(env_key) {
                return CatalogEntry {
                    name: name.to_string(),
                    supported: Err(format!("invalid environment key '{env_key}'")),
                    kind,
                };
            }

            if is_disallowed_env_key(env_key) {
                return CatalogEntry {
                    name: name.to_string(),
                    supported: Err(format!("disallowed environment variable '{env_key}'")),
                    kind,
                };
            }

            let v_str = match env_v.as_str() {
                Some(s) => s,
                None => {
                    return CatalogEntry {
                        name: name.to_string(),
                        supported: Err(format!(
                            "environment value for '{env_key}' must be a string"
                        )),
                        kind,
                    };
                }
            };

            let has_file = v_str.contains("{file:");
            let has_env = v_str.contains("{env:");

            if has_file || has_env {
                // Must be EXACTLY {file:PATH} or {env:NAME}
                if has_file && has_env {
                    return CatalogEntry {
                        name: name.to_string(),
                        supported: Err(format!(
                            "environment value for '{env_key}' cannot contain multiple references"
                        )),
                        kind,
                    };
                }

                if has_file {
                    if !v_str.starts_with("{file:") || !v_str.ends_with('}') {
                        return CatalogEntry {
                            name: name.to_string(),
                            supported: Err(format!(
                                "environment value for '{env_key}' containing {{file: must be exactly '{{file:PATH}}'"
                            )),
                            kind,
                        };
                    }
                    let inner = &v_str["{file:".len()..v_str.len() - 1];
                    if inner.is_empty()
                        || inner.contains("{file:")
                        || inner.contains("{env:")
                        || inner.contains('}')
                    {
                        return CatalogEntry {
                            name: name.to_string(),
                            supported: Err(format!(
                                "invalid file path in '{{file:...}}' for '{env_key}'"
                            )),
                            kind,
                        };
                    }

                    let resolved_path = if let Some(stripped) = inner.strip_prefix("~/") {
                        match home {
                            Some(h) => h.join(stripped),
                            None => {
                                return CatalogEntry {
                                    name: name.to_string(),
                                    supported: Err(format!(
                                        "HOME not set to expand '~/' in '{inner}' for '{env_key}'"
                                    )),
                                    kind,
                                };
                            }
                        }
                    } else {
                        PathBuf::from(inner)
                    };

                    if !resolved_path.is_absolute() {
                        return CatalogEntry {
                            name: name.to_string(),
                            supported: Err(format!(
                                "file path must be absolute in '{{file:{inner}}}' for '{env_key}'"
                            )),
                            kind,
                        };
                    }

                    env.insert(env_key.clone(), EnvVarSpec::File(resolved_path));
                } else {
                    // has_env
                    if !v_str.starts_with("{env:") || !v_str.ends_with('}') {
                        return CatalogEntry {
                            name: name.to_string(),
                            supported: Err(format!(
                                "environment value for '{env_key}' containing {{env: must be exactly '{{env:NAME}}'"
                            )),
                            kind,
                        };
                    }
                    let inner = &v_str["{env:".len()..v_str.len() - 1];
                    if inner.is_empty()
                        || inner.contains("{file:")
                        || inner.contains("{env:")
                        || inner.contains('}')
                        || !is_valid_env_key(inner)
                    {
                        return CatalogEntry {
                            name: name.to_string(),
                            supported: Err(format!(
                                "invalid environment variable name '{inner}' in '{{env:...}}' for '{env_key}'"
                            )),
                            kind,
                        };
                    }

                    env.insert(env_key.clone(), EnvVarSpec::Env(inner.to_string()));
                }
            } else {
                // Literal value
                if key_looks_secret(env_key) {
                    return CatalogEntry {
                        name: name.to_string(),
                        supported: Err(format!(
                            "literal value for secret-like key '{env_key}' is not allowed; use {{file:...}} or {{env:...}}"
                        )),
                        kind,
                    };
                }
                env.insert(env_key.clone(), EnvVarSpec::Literal(v_str.to_string()));
            }
        }
    }

    CatalogEntry {
        name: name.to_string(),
        supported: Ok(GooseExtensionSpec {
            name: name.to_string(),
            command,
            env,
        }),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_name_to_key_rules() {
        assert_eq!(name_to_key("slack"), "slack");
        assert_eq!(name_to_key("slack-ro"), "slack-ro");
        assert_eq!(name_to_key("server_1"), "server_1");
        assert_eq!(name_to_key("Slack"), "slack");
        assert_eq!(name_to_key("slack app"), "slackapp");
        assert_eq!(name_to_key("slack.app"), "slack_app");
    }

    #[test]
    fn test_empty_catalog_when_mcp_missing() {
        let json = r#"{ "other_field": 123 }"#;
        let cat = Catalog::load_from_str(json, None).unwrap();
        assert!(cat.entries.is_empty());
    }

    #[test]
    fn test_remote_server_unsupported() {
        let json = r#"{
            "mcp": {
                "datadog": {
                    "type": "remote",
                    "url": "https://example.com/mcp"
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json, None).unwrap();
        assert_eq!(cat.entries.len(), 1);
        let entry = &cat.entries[0];
        assert_eq!(entry.name, "datadog");
        assert_eq!(entry.kind, McpKind::Remote);
        assert!(entry.supported.is_err());
        assert_eq!(
            entry.supported.as_ref().unwrap_err(),
            "remote MCP servers are not supported yet"
        );
    }

    #[test]
    fn test_invalid_server_name_unsupported() {
        // Uppercase or space or symbol in name fails name == name_to_key(name)
        let json = r#"{
            "mcp": {
                "Slack": {
                    "type": "local",
                    "command": ["slack-mcp"]
                },
                "bad space": {
                    "type": "local",
                    "command": ["slack-mcp"]
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json, None).unwrap();
        for entry in &cat.entries {
            assert!(entry.supported.is_err());
            assert!(entry.supported.as_ref().unwrap_err().contains("name"));
        }
    }

    #[test]
    fn test_command_validation() {
        // Empty command
        let json_empty = r#"{
            "mcp": {
                "srv1": { "type": "local", "command": [] }
            }
        }"#;
        let cat = Catalog::load_from_str(json_empty, None).unwrap();
        assert!(cat.entries[0].supported.is_err());

        // File ref in command
        let json_file_in_cmd = r#"{
            "mcp": {
                "srv2": { "type": "local", "command": ["run", "{file:/secret}"] }
            }
        }"#;
        let cat = Catalog::load_from_str(json_file_in_cmd, None).unwrap();
        assert!(
            cat.entries[0]
                .supported
                .as_ref()
                .unwrap_err()
                .contains("{file:")
        );

        // Env ref in command
        let json_env_in_cmd = r#"{
            "mcp": {
                "srv3": { "type": "local", "command": ["run", "{env:FOO}"] }
            }
        }"#;
        let cat = Catalog::load_from_str(json_env_in_cmd, None).unwrap();
        assert!(
            cat.entries[0]
                .supported
                .as_ref()
                .unwrap_err()
                .contains("{env:")
        );

        // Non-existent absolute path
        let json_abs_missing = r#"{
            "mcp": {
                "srv4": { "type": "local", "command": ["/tmp/nonexistent-abs-bin-honk-test"] }
            }
        }"#;
        let cat = Catalog::load_from_str(json_abs_missing, None).unwrap();
        assert!(
            cat.entries[0]
                .supported
                .as_ref()
                .unwrap_err()
                .contains("does not exist")
        );

        // Relative command passes through
        let json_rel = r#"{
            "mcp": {
                "srv5": { "type": "local", "command": ["npx", "-y", "pkg"] }
            }
        }"#;
        let cat = Catalog::load_from_str(json_rel, None).unwrap();
        assert!(cat.entries[0].supported.is_ok());
    }

    #[test]
    fn test_disallowed_env_keys() {
        for disallowed in &["PATH", "path", "LD_PRELOAD", "NODE_OPTIONS", "PYTHONPATH"] {
            let json = format!(
                r#"{{
                "mcp": {{
                    "srv": {{
                        "type": "local",
                        "command": ["npx"],
                        "environment": {{
                            "{disallowed}": "/usr/bin"
                        }}
                    }}
                }}
            }}"#
            );
            let cat = Catalog::load_from_str(&json, None).unwrap();
            let err = cat.entries[0].supported.as_ref().unwrap_err();
            assert!(
                err.contains("disallowed") || err.contains("Disallowed"),
                "expected disallowed error for {disallowed}, got: {err}"
            );
        }
    }

    #[test]
    fn test_invalid_env_key_syntax() {
        let json = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "123BAD": "val",
                        "BAD-KEY": "val"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json, None).unwrap();
        assert!(cat.entries[0].supported.is_err());
    }

    #[test]
    fn test_secret_literal_check() {
        let secret_keys = [
            "SLACK_BOT_TOKEN",
            "USER_SECRET",
            "DB_PASSWORD",
            "ROOT_PASSWD",
            "CLIENT_API_KEY",
            "AUTH_HEADER",
            "MY_KEY",
        ];
        for key in secret_keys {
            let json = format!(
                r#"{{
                "mcp": {{
                    "srv": {{
                        "type": "local",
                        "command": ["npx"],
                        "environment": {{
                            "{key}": "literal-secret-value"
                        }}
                    }}
                }}
            }}"#
            );
            let cat = Catalog::load_from_str(&json, None).unwrap();
            let err = cat.entries[0].supported.as_ref().unwrap_err();
            assert!(
                err.contains("secret") || err.contains("literal"),
                "expected secret literal rejection for {key}, got: {err}"
            );
        }

        // But {file:...} for a secret key is permitted!
        let json_file_secret = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "SLACK_BOT_TOKEN": "{file:/run/secrets/token}"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json_file_secret, None).unwrap();
        assert!(cat.entries[0].supported.is_ok());

        // And non-secret literal is permitted!
        let json_non_secret = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "LOG_LEVEL": "debug"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json_non_secret, None).unwrap();
        assert!(cat.entries[0].supported.is_ok());
    }

    #[test]
    fn test_env_ref_validation() {
        // Valid env ref
        let json_valid = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "MY_VAR": "{env:PARENT_ENV_VAR}"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json_valid, None).unwrap();
        assert!(cat.entries[0].supported.is_ok());

        // Malformed env refs
        for bad in &[
            "{env:}",
            "{env:123BAD}",
            "{env:BAD-VAR}",
            "prefix {env:FOO}",
            "{env:FOO} suffix",
        ] {
            let json = format!(
                r#"{{
                "mcp": {{
                    "srv": {{
                        "type": "local",
                        "command": ["npx"],
                        "environment": {{
                            "MY_VAR": "{bad}"
                        }}
                    }}
                }}
            }}"#
            );
            let cat = Catalog::load_from_str(&json, None).unwrap();
            assert!(
                cat.entries[0].supported.is_err(),
                "expected rejection for bad env ref: {bad}"
            );
        }
    }

    #[test]
    fn test_file_ref_validation_and_home_expansion() {
        let fake_home = Path::new("/custom/home/user");

        // ~/ expansion
        let json_home = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "TOKEN": "{file:~/.config/token}"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json_home, Some(fake_home)).unwrap();
        let spec = cat.entries[0].supported.as_ref().unwrap();
        let env_spec = spec.env.get("TOKEN").unwrap();
        assert_eq!(
            env_spec,
            &EnvVarSpec::File(PathBuf::from("/custom/home/user/.config/token"))
        );

        // Relative path rejected
        let json_rel = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "TOKEN": "{file:relative/path/to/token}"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json_rel, Some(fake_home)).unwrap();
        assert!(
            cat.entries[0]
                .supported
                .as_ref()
                .unwrap_err()
                .contains("absolute")
        );

        // Malformed file ref containing {file:
        let json_malformed = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "TOKEN": "prefix {file:/run/secrets/token}"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json_malformed, Some(fake_home)).unwrap();
        assert!(cat.entries[0].supported.is_err());
    }

    #[test]
    fn test_file_ref_with_inner_closing_brace_unsupported() {
        let fake_home = Path::new("/home/dev");
        let json = r#"{
            "mcp": {
                "srv": {
                    "type": "local",
                    "command": ["npx"],
                    "environment": {
                        "TOKEN": "{file:/run/secrets/foo}bar}"
                    }
                }
            }
        }"#;
        let cat = Catalog::load_from_str(json, Some(fake_home)).unwrap();
        assert!(cat.entries[0].supported.is_err());
        assert!(
            cat.entries[0]
                .supported
                .as_ref()
                .unwrap_err()
                .contains("invalid file path")
        );
    }

    #[test]
    fn test_translation_without_environment() {
        let spec = GooseExtensionSpec {
            name: "chrome-devtools".to_string(),
            command: vec![
                "npx".to_string(),
                "-y".to_string(),
                "chrome-devtools-mcp".to_string(),
            ],
            env: BTreeMap::new(),
        };

        let json = spec.to_add_extension_json();
        assert_eq!(json["type"], "mcp");
        assert_eq!(json["description"], "honk-mcp:chrome-devtools");
        assert_eq!(json["server"]["name"], "chrome-devtools");
        assert_eq!(json["server"]["command"], "npx");
        assert_eq!(
            json["server"]["args"],
            serde_json::json!(["-y", "chrome-devtools-mcp"])
        );
        assert_eq!(json["server"]["env"], serde_json::json!([]));
    }

    #[test]
    fn test_translation_with_environment_and_sorting() {
        let mut env = BTreeMap::new();
        env.insert(
            "SLACK_BOT_TOKEN".to_string(),
            EnvVarSpec::File(PathBuf::from("/run/secrets/slack-token")),
        );
        env.insert(
            "APP_ENV".to_string(),
            EnvVarSpec::Literal("production".to_string()),
        );
        env.insert(
            "PORT".to_string(),
            EnvVarSpec::Env("PARENT_PORT".to_string()),
        );

        let spec = GooseExtensionSpec {
            name: "slack".to_string(),
            command: vec!["slack-mcp".to_string(), "--verbose".to_string()],
            env,
        };

        let json = spec.to_add_extension_json();
        assert_eq!(json["type"], "mcp");
        assert_eq!(json["description"], "honk-mcp:slack");
        assert_eq!(json["server"]["name"], "slack");
        assert_eq!(json["server"]["command"], "/bin/sh");
        assert_eq!(json["server"]["env"], serde_json::json!([]));

        let args = json["server"]["args"].as_array().unwrap();
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], crate::mcp::wrapper::WRAPPER_SCRIPT);
        assert_eq!(args[2], "honk-mcp-slack");

        // Args must be sorted by env key: APP_ENV, PORT, SLACK_BOT_TOKEN
        // APP_ENV lit production
        assert_eq!(args[3], "APP_ENV");
        assert_eq!(args[4], "lit");
        assert_eq!(args[5], "production");
        // PORT env PARENT_PORT
        assert_eq!(args[6], "PORT");
        assert_eq!(args[7], "env");
        assert_eq!(args[8], "PARENT_PORT");
        // SLACK_BOT_TOKEN file /run/secrets/slack-token
        assert_eq!(args[9], "SLACK_BOT_TOKEN");
        assert_eq!(args[10], "file");
        assert_eq!(args[11], "/run/secrets/slack-token");

        // Followed by "--" and the command
        assert_eq!(args[12], "--");
        assert_eq!(args[13], "slack-mcp");
        assert_eq!(args[14], "--verbose");

        // Check secret path only, never secret contents
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(serialized.contains("/run/secrets/slack-token"));
    }

    #[test]
    fn test_full_realistic_catalog_fixture() {
        let dir = tempdir().unwrap();
        let cat_file = dir.path().join("opencode.json");

        let json = r#"{
            "mcp": {
                "slack": {
                    "type": "local",
                    "command": ["slack-mcp"],
                    "environment": {
                        "SLACK_BOT_TOKEN": "{file:/run/secrets/slack-token}",
                        "LOG_LEVEL": "info",
                        "MODE": "full",
                        "TIMEOUT": "30",
                        "CHANNEL_PREFIX": "proj-",
                        "MAX_MESSAGES": "100"
                    },
                    "enabled": false
                },
                "slack-ro": {
                    "type": "local",
                    "command": ["slack-mcp"],
                    "environment": {
                        "SLACK_BOT_TOKEN": "{file:/run/secrets/slack-token}",
                        "READ_ONLY": "true"
                    },
                    "enabled": false
                },
                "figma": {
                    "type": "local",
                    "command": ["figma-mcp"],
                    "environment": {
                        "FIGMA_ACCESS_TOKEN": "{file:/run/secrets/figma-token}"
                    },
                    "enabled": false
                },
                "chrome-devtools": {
                    "type": "local",
                    "command": ["npx", "-y", "chrome-devtools-mcp"],
                    "enabled": false
                },
                "datadog": {
                    "type": "remote",
                    "url": "https://mcp.datadog.example/sse"
                },
                "notion": {
                    "type": "remote",
                    "url": "https://mcp.notion.example/sse"
                },
                "macos-tool": {
                    "type": "local",
                    "command": ["macos-tool"],
                    "environment": {
                        "AUTH_TOKEN": "{file:~/.secrets/token.txt}"
                    }
                }
            }
        }"#;
        fs::write(&cat_file, json).unwrap();

        let home = Path::new("/var/mockhome");
        let catalog = Catalog::load_with_home(&cat_file, Some(home)).unwrap();

        // Must be sorted alphabetically
        let names: Vec<_> = catalog.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "chrome-devtools",
                "datadog",
                "figma",
                "macos-tool",
                "notion",
                "slack",
                "slack-ro"
            ]
        );

        // Check local supported
        assert!(catalog.get("slack").unwrap().supported.is_ok());
        assert!(catalog.get("slack-ro").unwrap().supported.is_ok());
        assert!(catalog.get("figma").unwrap().supported.is_ok());
        assert!(catalog.get("chrome-devtools").unwrap().supported.is_ok());
        assert!(catalog.get("macos-tool").unwrap().supported.is_ok());

        // Check remotes unsupported
        assert_eq!(catalog.get("datadog").unwrap().kind, McpKind::Remote);
        assert!(catalog.get("datadog").unwrap().supported.is_err());
        assert_eq!(catalog.get("notion").unwrap().kind, McpKind::Remote);
        assert!(catalog.get("notion").unwrap().supported.is_err());

        // Check macos-tool expanded home
        let macos_spec = catalog
            .get("macos-tool")
            .unwrap()
            .supported
            .as_ref()
            .unwrap();
        assert_eq!(
            macos_spec.env.get("AUTH_TOKEN").unwrap(),
            &EnvVarSpec::File(PathBuf::from("/var/mockhome/.secrets/token.txt"))
        );
    }
}
