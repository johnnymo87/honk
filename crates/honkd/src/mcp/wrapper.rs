//! POSIX wrapper script for injecting environment variables and secrets into MCP servers.

/// POSIX wrapper script for launching MCP servers with environment variables.
///
/// Secrets and environment variables are supplied via arguments to this wrapper:
/// `/bin/sh -c WRAPPER_SCRIPT honk-mcp-<name> K1 kind1 v1 ... -- cmd0 rest...`
///
/// Kind is one of:
/// - `lit`: literal string value in `$3`
/// - `file`: file path in `$3`, trimmed of leading/trailing blanks
/// - `env`: parent environment variable name in `$3`
///
/// This ensures secret contents are never present in process argv, command lines,
/// or persistent storage.
pub const WRAPPER_SCRIPT: &str = r#"while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
  [ "$#" -lt 3 ] && { echo "honk-mcp: malformed args" >&2; exit 64; }
  case $2 in
    lit) v=$3 ;;
    file)
      v=
      { IFS= read -r v || [ -n "$v" ]; } <"$3" || { echo "honk-mcp: cannot read $3" >&2; exit 78; }
      case "$v" in
        *[![:blank:]]*)
          v="${v#"${v%%[![:blank:]]*}"}"
          v="${v%"${v##*[![:blank:]]}"}"
          ;;
        *) v="" ;;
      esac
      ;;
    env) eval "v=\${$3-}" ;;
    *) echo "honk-mcp: bad kind $2" >&2; exit 64 ;;
  esac
  export "$1=$v"; shift 3
done
[ "$1" = -- ] || { echo "honk-mcp: missing --" >&2; exit 64; }
shift; exec "$@"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn test_wrapper_lit_file_env_export() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "file_secret_content").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_LIT")
            .arg("lit")
            .arg("lit_value")
            .arg("VAR_FILE")
            .arg("file")
            .arg(tmp.path())
            .arg("VAR_ENV")
            .arg("env")
            .arg("PARENT_VAR")
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo LIT=$VAR_LIT FILE=$VAR_FILE ENV=$VAR_ENV")
            .env("PARENT_VAR", "env_value")
            .output()
            .expect("execute sh");

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "LIT=lit_value FILE=file_secret_content ENV=env_value"
        );
    }

    #[test]
    fn test_wrapper_file_whitespace_trimmed() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "  \t trimmed_secret \t ").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_FILE")
            .arg("file")
            .arg(tmp.path())
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo \"[$VAR_FILE]\"")
            .output()
            .expect("execute sh");

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "[trimmed_secret]");
    }

    #[test]
    fn test_wrapper_file_without_trailing_newline() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "no_newline_secret").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_FILE")
            .arg("file")
            .arg(tmp.path())
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo \"[$VAR_FILE]\"")
            .output()
            .expect("execute sh");

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "[no_newline_secret]");
    }

    #[test]
    fn test_wrapper_missing_file_exits_78() {
        let missing_path = "/tmp/nonexistent-honk-test-token-file-12345.secret";

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_FILE")
            .arg("file")
            .arg(missing_path)
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo executed")
            .output()
            .expect("execute sh");

        assert_eq!(output.status.code(), Some(78));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(missing_path),
            "stderr should mention path: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("executed"));
    }

    #[test]
    fn test_wrapper_malformed_args_exits_64() {
        // Missing -- delimiter
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_LIT")
            .arg("lit")
            .arg("value")
            .output()
            .expect("execute sh");
        assert_eq!(output.status.code(), Some(64));

        // Bad kind
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_BAD")
            .arg("unknown_kind")
            .arg("value")
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo executed")
            .output()
            .expect("execute sh");
        assert_eq!(output.status.code(), Some(64));

        // Fewer than 3 args before --
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(WRAPPER_SCRIPT)
            .arg("honk-mcp-test")
            .arg("VAR_ONLY")
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo executed")
            .output()
            .expect("execute sh");
        assert_eq!(output.status.code(), Some(64));
    }
}
