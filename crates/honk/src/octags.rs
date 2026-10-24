//! Tagging a new goose session in oc-tags.
//!
//! oc-tags keeps a sidecar database of session -> tag. Goose sessions are
//! stored there as `goose:<id>`. `honk chat --new` tags the new session,
//! either explicitly with `--tag <tag>` or by inheriting the launching
//! session's tag when unset.
//!
//! Tagging is bookkeeping, so it is best-effort: it runs in the background, it
//! is time-bounded, and a failure is a warning that says how to apply the tag
//! by hand. The chat never waits for it.

use std::time::Duration;

/// Environment variable that overrides the oc-tags executable.
pub const BIN_ENV: &str = "HONK_OC_TAGS_BIN";
/// The executable used when [`BIN_ENV`] is unset.
pub const DEFAULT_BIN: &str = "oc-tags";
/// How long `oc-tags set` may take before it is killed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `oc-tags which` may take before it is killed.
pub const DEFAULT_WHICH_TIMEOUT: Duration = Duration::from_secs(4);
/// Environment variable indicating an opencode launcher session.
pub const OPENCODE_SESSION_ID_ENV: &str = "OPENCODE_SESSION_ID";
/// Environment variable indicating an agent launcher session (prefixed with goose:).
pub const AGENT_SESSION_ID_ENV: &str = "AGENT_SESSION_ID";

/// Resolves the launching session id from environment lookup.
///
/// Precedence:
/// 1. `$OPENCODE_SESSION_ID` if set and non-empty: used as-is.
/// 2. `$AGENT_SESSION_ID` if set and non-empty: used as `goose:<id>`.
/// 3. Otherwise `None`.
pub fn resolve_launcher_from(env_get: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(id) = env_get(OPENCODE_SESSION_ID_ENV).filter(|s| !s.is_empty()) {
        return Some(id);
    }
    let agent = env_get(AGENT_SESSION_ID_ENV).filter(|s| !s.is_empty());
    agent.map(|id| format!("goose:{id}"))
}

/// Resolves the launcher session from the process environment.
pub fn launcher_from_env() -> Option<String> {
    resolve_launcher_from(|k| std::env::var(k).ok())
}

/// What to tag a new session with, and which oc-tags to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagSettings {
    pub tag: String,
    pub bin: String,
    pub timeout: Duration,
}

impl TagSettings {
    /// Uses `$HONK_OC_TAGS_BIN` when it is set and non-empty, else `oc-tags`.
    pub fn new(tag: impl Into<String>) -> Self {
        let bin = std::env::var(BIN_ENV)
            .ok()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| DEFAULT_BIN.to_string());
        Self {
            tag: tag.into(),
            bin,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// The oc-tags id of a goose session: `goose:<id>`.
pub fn tagged_id(goose_session_id: &str) -> String {
    format!("goose:{goose_session_id}")
}

/// Validates a `--tag` value before any session exists.
///
/// The same rule as `opencode-launch --tag` (validate_tag in
/// workstation's pkgs/opencode-launch): a letter or digit, then up to 63 of
/// letters, digits and `._:/-`. `auto:` is oc-tags' namespace for tags it
/// derives itself, and a bare `auto` means "no tag" to opencode-launch, so both
/// are refused rather than given a second meaning here.
pub fn parse_tag(s: &str) -> Result<String, String> {
    let mut chars = s.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c));
    if !first_ok || !rest_ok || s.len() > 64 {
        return Err(
            "must be a letter or digit followed by up to 63 letters, digits or ._:/-".to_string(),
        );
    }
    let lower = s.to_ascii_lowercase();
    if lower == "auto" || lower.starts_with("auto:") {
        return Err("'auto' and 'auto:' tags are reserved for oc-tags".to_string());
    }
    Ok(s.to_string())
}

/// Parsed value of a `--tag` CLI argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagArg {
    /// An explicit tag to assign to the new session.
    Explicit(String),
    /// An opt-out from tagging and inheritance (`--tag auto`).
    NoInherit,
}

/// Parses the `--tag` CLI argument.
///
/// A bare `auto` (case-insensitive) opts out of inheritance and tagging.
/// All other values must pass [`parse_tag`].
pub fn parse_tag_arg(s: &str) -> Result<TagArg, String> {
    if s.eq_ignore_ascii_case("auto") {
        Ok(TagArg::NoInherit)
    } else {
        parse_tag(s).map(TagArg::Explicit)
    }
}

/// Runs `<bin> set -- <tag> goose:<id>` and returns oc-tags' own success line.
///
/// The line is returned rather than rebuilt from the tag because oc-tags
/// normalises it (lowercases), and the stored form is what the picker and the
/// reports show. The error is a one-line reason, never a dump: a timeout prints
/// nothing, and a locked tags.db prints a traceback whose last line is the
/// useful one.
pub async fn tag_session(settings: &TagSettings, goose_session_id: &str) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt;
    // NOT kill_on_drop: if honk exits while oc-tags is still writing, the write
    // should finish (it is idempotent and bounded by oc-tags' own busy
    // timeout). Only this function's own timeout kills it.
    let child = tokio::process::Command::new(&settings.bin)
        .args(["set", "--", &settings.tag, &tagged_id(goose_session_id)])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("'{}' not found", settings.bin)
        }
        Err(e) => anyhow::bail!("could not run '{}': {e}", settings.bin),
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let read_out = async {
            match stdout.as_mut() {
                Some(s) => s.read_to_end(&mut out).await.map(|_| ()),
                None => Ok(()),
            }
        };
        let read_err = async {
            match stderr.as_mut() {
                Some(s) => s.read_to_end(&mut err).await.map(|_| ()),
                None => Ok(()),
            }
        };
        tokio::try_join!(read_out, read_err)?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };
    let res = tokio::time::timeout(settings.timeout, run).await;
    let (status, out, err) = match res {
        Ok(r) => r.map_err(|e| anyhow::anyhow!("could not run '{}': {e}", settings.bin))?,
        Err(_) => {
            let _ = child.start_kill();
            anyhow::bail!("timed out after {:?}", settings.timeout)
        }
    };
    let last_line = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(str::to_string)
    };
    if status.success() {
        return Ok(last_line(&out).unwrap_or_else(|| {
            format!("tagged {} as {}", tagged_id(goose_session_id), settings.tag)
        }));
    }
    let reason = last_line(&err)
        .or_else(|| last_line(&out))
        .unwrap_or_else(|| match status.code() {
            Some(c) => format!("exit {c}, no output"),
            None => "killed by a signal, no output".to_string(),
        });
    anyhow::bail!("{reason}")
}

/// The warning for a tag that had not been confirmed when honk exited. The
/// write is not cancelled; this says how to check it.
pub fn unconfirmed_text(settings: &TagSettings, goose_session_id: &str) -> String {
    let id = tagged_id(goose_session_id);
    format!(
        "Tag: '{tag}' for {id} was not confirmed before exit; check with: oc-tags which {id}",
        tag = settings.tag
    )
}

/// The chat notice for a tagging outcome, with an optional suffix.
pub fn notice_with_suffix(
    settings: &TagSettings,
    goose_session_id: &str,
    outcome: &anyhow::Result<String>,
    suffix: Option<&str>,
) -> crate::chat::Notice {
    match outcome {
        Ok(line) => {
            let s = suffix.unwrap_or("");
            crate::chat::Notice::info(format!("Tag: {line}{s}"))
        }
        Err(e) => {
            let id = tagged_id(goose_session_id);
            crate::chat::Notice::warning(format!(
                "Tag: '{tag}' not applied to {id}: {e:#}; apply it with: oc-tags set {tag} {id}",
                tag = settings.tag
            ))
        }
    }
}

/// The chat notice for a tagging outcome.
pub fn notice(
    settings: &TagSettings,
    goose_session_id: &str,
    outcome: &anyhow::Result<String>,
) -> crate::chat::Notice {
    notice_with_suffix(settings, goose_session_id, outcome, None)
}

/// The outcome of querying the launcher session via `oc-tags which`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WhichOutcome {
    /// Eligible session tag to inherit, with the root session id for attribution.
    Inherit { tag: String, root: String },
    /// No tag to inherit (`dir` or `auto` kind); silent.
    Skip,
    /// Executable binary was not found.
    NotFound(String),
    /// Query failed or tag was invalid.
    Failure(String),
}

/// Queries `<bin> which -- <launcher>` to discover the launching session's tag.
pub async fn query_launcher_tag(bin: &str, launcher: &str, timeout: Duration) -> WhichOutcome {
    use tokio::io::AsyncReadExt;
    let child = tokio::process::Command::new(bin)
        .args(["which", "--", launcher])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return WhichOutcome::NotFound(bin.to_string());
        }
        Err(e) => return WhichOutcome::Failure(format!("could not run '{bin}': {e}")),
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let run = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let read_out = async {
            match stdout.as_mut() {
                Some(s) => s.read_to_end(&mut out).await.map(|_| ()),
                None => Ok(()),
            }
        };
        let read_err = async {
            match stderr.as_mut() {
                Some(s) => s.read_to_end(&mut err).await.map(|_| ()),
                None => Ok(()),
            }
        };
        tokio::try_join!(read_out, read_err)?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };
    let res = tokio::time::timeout(timeout, run).await;
    let (status, out, _err) = match res {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return WhichOutcome::Failure(format!("could not run '{bin}': {e}")),
        Err(_) => {
            let _ = child.start_kill();
            return WhichOutcome::Failure(format!(
                "'oc-tags which' failed (timed out after {timeout:?})"
            ));
        }
    };
    if !status.success() {
        let reason = match status.code() {
            Some(c) => format!("exit {c}"),
            None => "killed by signal".to_string(),
        };
        return WhichOutcome::Failure(format!("'oc-tags which' failed ({reason})"));
    }
    let line = String::from_utf8_lossy(&out)
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches(['\r', '\n'])
        .to_string();
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 4 || parts[3].is_empty() {
        return WhichOutcome::Failure("'oc-tags which' printed no kind column".to_string());
    }
    let w_tag = parts[0];
    let w_root = parts[2];
    let w_kind = parts[3];
    match w_kind {
        "session" => match parse_tag(w_tag) {
            Ok(valid_tag) => {
                let root = if w_root.is_empty() {
                    launcher.to_string()
                } else {
                    w_root.to_string()
                };
                WhichOutcome::Inherit {
                    tag: valid_tag,
                    root,
                }
            }
            Err(_) => WhichOutcome::Failure(format!("its tag '{w_tag}' is not a valid launch tag")),
        },
        "dir" | "auto" => WhichOutcome::Skip,
        unknown => WhichOutcome::Failure(format!("unknown tag kind '{unknown}'")),
    }
}

/// Settings for querying and inheriting the launching session's tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InheritSettings {
    pub launcher: String,
    pub bin: String,
    pub custom_bin: bool,
    pub which_timeout: Duration,
    pub set_timeout: Duration,
}

impl InheritSettings {
    /// Uses `$HONK_OC_TAGS_BIN` when set and non-empty, else `oc-tags`.
    pub fn new(launcher: impl Into<String>) -> Self {
        let custom_bin = std::env::var(BIN_ENV)
            .ok()
            .filter(|b| !b.is_empty())
            .is_some();
        let bin = std::env::var(BIN_ENV)
            .ok()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| DEFAULT_BIN.to_string());
        Self {
            launcher: launcher.into(),
            bin,
            custom_bin,
            which_timeout: DEFAULT_WHICH_TIMEOUT,
            set_timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// What tagging action to take when a new session is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TagAction {
    /// Explicit tag passed via `--tag <tag>`.
    Explicit(TagSettings),
    /// Inherit tag from the launching session if set.
    Inherit(InheritSettings),
}

impl From<TagSettings> for TagAction {
    fn from(settings: TagSettings) -> Self {
        TagAction::Explicit(settings)
    }
}

impl From<InheritSettings> for TagAction {
    fn from(settings: InheritSettings) -> Self {
        TagAction::Inherit(settings)
    }
}

/// The warning when tag inheritance from a launcher did not finish before exit.
pub fn unconfirmed_inherit_text(launcher: &str, goose_session_id: &str) -> String {
    let id = tagged_id(goose_session_id);
    format!(
        "Tag: tag inheritance from {launcher} did not finish before exit; to apply it: oc-tags which {launcher}, then oc-tags set -- <tag> {id}"
    )
}

/// The warning for an unconfirmed tag action when honk exited.
pub fn unconfirmed_action_text(action: &TagAction, goose_session_id: &str) -> String {
    match action {
        TagAction::Explicit(settings) => unconfirmed_text(settings, goose_session_id),
        TagAction::Inherit(settings) => {
            unconfirmed_inherit_text(&settings.launcher, goose_session_id)
        }
    }
}

/// Executes the specified tag action for a newly created session.
///
/// Returns `Some(Notice)` if a notice should be displayed in chat, or `None` if
/// no tagging was performed (e.g. the launcher had kind `dir` or `auto`).
pub async fn run_tag_action(
    action: &TagAction,
    goose_session_id: &str,
) -> Option<crate::chat::Notice> {
    run_tag_action_with_unconfirmed(action, goose_session_id, None).await
}

/// Executes the specified tag action, updating the pending unconfirmed exit notice
/// as the inheritance and tagging steps progress.
pub async fn run_tag_action_with_unconfirmed(
    action: &TagAction,
    goose_session_id: &str,
    unconfirmed: Option<&std::sync::Mutex<Option<String>>>,
) -> Option<crate::chat::Notice> {
    match action {
        TagAction::Explicit(settings) => {
            let outcome = tag_session(settings, goose_session_id).await;
            if let Some(u) = unconfirmed {
                *u.lock().unwrap() = None;
            }
            Some(notice(settings, goose_session_id, &outcome))
        }
        TagAction::Inherit(settings) => {
            match query_launcher_tag(&settings.bin, &settings.launcher, settings.which_timeout)
                .await
            {
                WhichOutcome::Skip => {
                    if let Some(u) = unconfirmed {
                        *u.lock().unwrap() = None;
                    }
                    None
                }
                WhichOutcome::NotFound(bin) => {
                    if let Some(u) = unconfirmed {
                        *u.lock().unwrap() = None;
                    }
                    if !settings.custom_bin {
                        None
                    } else {
                        Some(crate::chat::Notice::warning(format!(
                            "Tag: tag not inherited from {launcher}: '{bin}' not found",
                            launcher = settings.launcher
                        )))
                    }
                }
                WhichOutcome::Failure(reason) => {
                    if let Some(u) = unconfirmed {
                        *u.lock().unwrap() = None;
                    }
                    Some(crate::chat::Notice::warning(format!(
                        "Tag: tag not inherited from {launcher}: {reason}",
                        launcher = settings.launcher
                    )))
                }
                WhichOutcome::Inherit { tag, root } => {
                    let set_settings = TagSettings {
                        tag,
                        bin: settings.bin.clone(),
                        timeout: settings.set_timeout,
                    };
                    if let Some(u) = unconfirmed {
                        *u.lock().unwrap() =
                            Some(unconfirmed_text(&set_settings, goose_session_id));
                    }
                    let outcome = tag_session(&set_settings, goose_session_id).await;
                    if let Some(u) = unconfirmed {
                        *u.lock().unwrap() = None;
                    }
                    Some(notice_with_suffix(
                        &set_settings,
                        goose_session_id,
                        &outcome,
                        Some(&format!(" (inherited from {root})")),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::NoticeKind;

    /// Writes an executable script FROM A CHILD PROCESS. Written by this
    /// process, its write fd could be inherited by another test thread's
    /// fork for the moment before that child execs, and exec'ing the script
    /// then fails with ETXTBSY ("Text file busy").
    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::io::Write;
        let p = dir.join(name);
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(&p)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .unwrap();
        drop(stdin);
        assert!(child.wait().unwrap().success());
        p.to_string_lossy().into_owned()
    }

    fn settings(bin: String) -> TagSettings {
        TagSettings {
            tag: "Honk".to_string(),
            bin,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn parse_tag_accepts_program_tags() {
        for ok in [
            "honk",
            "Billing-Job",
            "a",
            "eng/agents",
            "x.y_z:1",
            "9lives",
        ] {
            assert_eq!(parse_tag(ok), Ok(ok.to_string()), "{ok}");
        }
        let max = format!("a{}", "b".repeat(63));
        assert_eq!(parse_tag(&max), Ok(max.clone()));
    }

    #[test]
    fn parse_tag_refuses_what_opencode_launch_refuses() {
        let too_long = format!("a{}", "b".repeat(64));
        for bad in [
            "",
            "-x",
            "_x",
            "has space",
            "tab\tx",
            "new\nline",
            "semi;colon",
            "dollar$",
            "quote'",
            "é",
            too_long.as_str(),
            "auto:honk",
            "AUTO:honk",
            "auto",
            "Auto",
        ] {
            assert!(parse_tag(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn parse_tag_arg_recognizes_bare_auto_as_opt_out() {
        for auto in ["auto", "AUTO", "Auto", "aUtO"] {
            assert_eq!(parse_tag_arg(auto), Ok(TagArg::NoInherit), "{auto}");
        }
        for bad in ["auto:honk", "AUTO:honk", "auto:x", "-x", ""] {
            assert!(parse_tag_arg(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(
            parse_tag_arg("honk"),
            Ok(TagArg::Explicit("honk".to_string()))
        );
    }

    #[test]
    fn resolve_launcher_precedence_and_fallback() {
        // OPENCODE_SESSION_ID takes precedence and is used as-is
        let env = |k: &str| match k {
            OPENCODE_SESSION_ID_ENV => Some("ses_opencode_123".to_string()),
            AGENT_SESSION_ID_ENV => Some("agent_456".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_launcher_from(env).as_deref(),
            Some("ses_opencode_123")
        );

        // AGENT_SESSION_ID fallback uses goose: prefix
        let env = |k: &str| match k {
            AGENT_SESSION_ID_ENV => Some("20261024_1".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_launcher_from(env).as_deref(),
            Some("goose:20261024_1")
        );

        // Empty OPENCODE_SESSION_ID falls back to AGENT_SESSION_ID
        let env = |k: &str| match k {
            OPENCODE_SESSION_ID_ENV => Some("".to_string()),
            AGENT_SESSION_ID_ENV => Some("20261024_2".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_launcher_from(env).as_deref(),
            Some("goose:20261024_2")
        );

        // Neither set
        assert_eq!(resolve_launcher_from(|_| None), None);

        // Both empty
        let env = |k: &str| match k {
            OPENCODE_SESSION_ID_ENV => Some("".to_string()),
            AGENT_SESSION_ID_ENV => Some("".to_string()),
            _ => None,
        };
        assert_eq!(resolve_launcher_from(env), None);
    }

    #[test]
    fn unconfirmed_inherit_text_format() {
        assert_eq!(
            unconfirmed_inherit_text("ses_launcher", "20261024_1"),
            "Tag: tag inheritance from ses_launcher did not finish before exit; to apply it: oc-tags which ses_launcher, then oc-tags set -- <tag> goose:20261024_1"
        );
    }

    #[test]
    fn tagged_id_uses_the_picker_namespace() {
        assert_eq!(tagged_id("20261020_3"), "goose:20261020_3");
    }

    #[tokio::test]
    async fn tag_session_passes_tag_first_then_goose_id() {
        let tmp = tempfile::tempdir().unwrap();
        let argv = tmp.path().join("argv");
        let bin = script(
            tmp.path(),
            "oc-tags",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\necho \"Tagged session 'goose:20261020_3' as 'honk'\"",
                argv.display()
            ),
        );
        let out = tag_session(&settings(bin), "20261020_3").await.unwrap();
        assert_eq!(out, "Tagged session 'goose:20261020_3' as 'honk'");
        let seen = std::fs::read_to_string(argv).unwrap();
        assert_eq!(seen, "set\n--\nHonk\ngoose:20261020_3\n");
    }

    #[tokio::test]
    async fn tag_session_failure_reports_the_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "oc-tags",
            "echo 'Traceback (most recent call last):' >&2\necho '  File x' >&2\necho 'sqlite3.OperationalError: database is locked' >&2\nexit 1",
        );
        let err = tag_session(&settings(bin), "20261020_3")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "sqlite3.OperationalError: database is locked");
    }

    #[tokio::test]
    async fn tag_session_silent_failure_reports_the_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "oc-tags", "exit 3");
        let err = tag_session(&settings(bin), "1_1")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "exit 3, no output");
    }

    #[tokio::test]
    async fn tag_session_missing_binary_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("nope").to_string_lossy().into_owned();
        let err = tag_session(&settings(bin.clone()), "1_1")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, format!("'{bin}' not found"));
    }

    #[tokio::test]
    async fn tag_session_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "oc-tags", "sleep 30");
        let mut s = settings(bin);
        s.timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let err = tag_session(&s, "1_1").await.unwrap_err().to_string();
        assert_eq!(err, "timed out after 300ms");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn notice_success_is_info_with_oc_tags_own_line() {
        let s = settings("oc-tags".into());
        let n = notice(
            &s,
            "1_1",
            &Ok("Tagged session 'goose:1_1' as 'honk'".into()),
        );
        assert_eq!(n.kind, NoticeKind::Info);
        assert_eq!(n.text, "Tag: Tagged session 'goose:1_1' as 'honk'");
    }

    #[test]
    fn notice_failure_is_a_warning_with_the_hand_fix() {
        let s = settings("oc-tags".into());
        let n = notice(&s, "1_1", &Err(anyhow::anyhow!("timed out after 10s")));
        assert_eq!(n.kind, NoticeKind::Warning);
        assert_eq!(
            n.text,
            "Tag: 'Honk' not applied to goose:1_1: timed out after 10s; apply it with: oc-tags set Honk goose:1_1"
        );
    }

    #[tokio::test]
    async fn query_launcher_tag_session_inherits() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "oc-tags",
            "printf 'Billing-Job\\tsession\\tses_root\\tsession\\n'",
        );
        let outcome = query_launcher_tag(&bin, "ses_parent", Duration::from_secs(5)).await;
        assert_eq!(
            outcome,
            WhichOutcome::Inherit {
                tag: "Billing-Job".to_string(),
                root: "ses_root".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn query_launcher_tag_empty_root_uses_launcher() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "oc-tags",
            "printf 'Billing-Job\\tsession\\t\\tsession\\n'",
        );
        let outcome = query_launcher_tag(&bin, "ses_parent", Duration::from_secs(5)).await;
        assert_eq!(
            outcome,
            WhichOutcome::Inherit {
                tag: "Billing-Job".to_string(),
                root: "ses_parent".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn query_launcher_tag_auto_and_dir_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "oc-tags",
            "printf 'auto:honk\\tdir\\t\\tauto\\n'",
        );
        let outcome = query_launcher_tag(&bin, "ses_parent", Duration::from_secs(5)).await;
        assert_eq!(outcome, WhichOutcome::Skip);

        let bin2 = script(tmp.path(), "oc-tags2", "printf 'honk\\tdir\\t\\tdir\\n'");
        let outcome2 = query_launcher_tag(&bin2, "ses_parent", Duration::from_secs(5)).await;
        assert_eq!(outcome2, WhichOutcome::Skip);
    }

    #[tokio::test]
    async fn query_launcher_tag_missing_kind_column() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "oc-tags",
            "printf 'Billing-Job\\tsession\\tses_root\\n'",
        );
        let outcome = query_launcher_tag(&bin, "ses_parent", Duration::from_secs(5)).await;
        assert_eq!(
            outcome,
            WhichOutcome::Failure("'oc-tags which' printed no kind column".to_string())
        );
    }

    #[tokio::test]
    async fn query_launcher_tag_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(tmp.path(), "oc-tags", "sleep 30");
        let outcome = query_launcher_tag(&bin, "ses_parent", Duration::from_millis(100)).await;
        assert_eq!(
            outcome,
            WhichOutcome::Failure("'oc-tags which' failed (timed out after 100ms)".to_string())
        );
    }

    #[tokio::test]
    async fn query_launcher_tag_invalid_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = script(
            tmp.path(),
            "oc-tags",
            "printf 'auto:honk\\tsession\\tses_root\\tsession\\n'",
        );
        let outcome = query_launcher_tag(&bin, "ses_parent", Duration::from_secs(5)).await;
        assert_eq!(
            outcome,
            WhichOutcome::Failure("its tag 'auto:honk' is not a valid launch tag".to_string())
        );
    }

    #[tokio::test]
    async fn run_tag_action_explicit_skips_which() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let bin = script(
            tmp.path(),
            "oc-tags",
            &format!(
                "echo \"$1\" >> '{}'\nif [ \"$1\" = 'set' ]; then echo \"Tagged session '$4' as '$3'\"; fi",
                log.display()
            ),
        );
        let action = TagAction::Explicit(TagSettings {
            tag: "billing".to_string(),
            bin,
            timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await.unwrap();
        assert_eq!(notice.kind, NoticeKind::Info);
        assert_eq!(
            notice.text,
            "Tag: Tagged session 'goose:20261024_1' as 'billing'"
        );
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.trim(), "set");
    }

    #[tokio::test]
    async fn run_tag_action_inherit_session_calls_which_then_set() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let bin = script(
            tmp.path(),
            "oc-tags",
            &format!(
                r#"echo "$1" >> '{}'
if [ "$1" = 'which' ]; then
  printf 'Billing-Job\tsession\tses_root\tsession\n'
elif [ "$1" = 'set' ]; then
  echo "Tagged session '$4' as '$3'"
fi"#,
                log.display()
            ),
        );
        let action = TagAction::Inherit(InheritSettings {
            launcher: "ses_parent".to_string(),
            bin,
            custom_bin: true,
            which_timeout: Duration::from_secs(5),
            set_timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await.unwrap();
        assert_eq!(notice.kind, NoticeKind::Info);
        assert_eq!(
            notice.text,
            "Tag: Tagged session 'goose:20261024_1' as 'Billing-Job' (inherited from ses_root)"
        );
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls, "which\nset\n");
    }

    #[tokio::test]
    async fn run_tag_action_inherit_auto_skips_set_and_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let bin = script(
            tmp.path(),
            "oc-tags",
            &format!(
                r#"echo "$1" >> '{}'
if [ "$1" = 'which' ]; then
  printf 'auto:honk\tdir\t\tauto\n'
fi"#,
                log.display()
            ),
        );
        let action = TagAction::Inherit(InheritSettings {
            launcher: "ses_parent".to_string(),
            bin,
            custom_bin: true,
            which_timeout: Duration::from_secs(5),
            set_timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await;
        assert!(notice.is_none());
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.trim(), "which");
    }

    #[tokio::test]
    async fn run_tag_action_inherit_failure_returns_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let bin = script(
            tmp.path(),
            "oc-tags",
            &format!(
                r#"echo "$1" >> '{}'
if [ "$1" = 'which' ]; then
  exit 1
fi"#,
                log.display()
            ),
        );
        let action = TagAction::Inherit(InheritSettings {
            launcher: "ses_parent".to_string(),
            bin,
            custom_bin: true,
            which_timeout: Duration::from_secs(5),
            set_timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await.unwrap();
        assert_eq!(notice.kind, NoticeKind::Warning);
        assert_eq!(
            notice.text,
            "Tag: tag not inherited from ses_parent: 'oc-tags which' failed (exit 1)"
        );
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.trim(), "which");
    }

    #[tokio::test]
    async fn run_tag_action_inherit_default_bin_missing_skips_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp
            .path()
            .join("nonexistent-oc-tags")
            .to_string_lossy()
            .into_owned();
        let action = TagAction::Inherit(InheritSettings {
            launcher: "ses_parent".to_string(),
            bin,
            custom_bin: false,
            which_timeout: Duration::from_secs(5),
            set_timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await;
        assert!(notice.is_none());
    }

    #[tokio::test]
    async fn run_tag_action_inherit_custom_bin_missing_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp
            .path()
            .join("nonexistent-oc-tags")
            .to_string_lossy()
            .into_owned();
        let action = TagAction::Inherit(InheritSettings {
            launcher: "ses_parent".to_string(),
            bin: bin.clone(),
            custom_bin: true,
            which_timeout: Duration::from_secs(5),
            set_timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await.unwrap();
        assert_eq!(notice.kind, NoticeKind::Warning);
        assert_eq!(
            notice.text,
            format!("Tag: tag not inherited from ses_parent: '{bin}' not found")
        );
    }

    #[tokio::test]
    async fn run_tag_action_explicit_missing_bin_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp
            .path()
            .join("nonexistent-oc-tags")
            .to_string_lossy()
            .into_owned();
        let action = TagAction::Explicit(TagSettings {
            tag: "billing".to_string(),
            bin: bin.clone(),
            timeout: Duration::from_secs(5),
        });
        let notice = run_tag_action(&action, "20261024_1").await.unwrap();
        assert_eq!(notice.kind, NoticeKind::Warning);
        assert_eq!(
            notice.text,
            format!(
                "Tag: 'billing' not applied to goose:20261024_1: '{bin}' not found; apply it with: oc-tags set billing goose:20261024_1"
            )
        );
    }
}
