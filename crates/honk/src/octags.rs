//! Tagging a new goose session in oc-tags, so it shows up under its program.
//!
//! oc-tags (a workstation tool) keeps a sidecar database of session -> tag. The
//! nvim stall-watch picker (`,fp`) lists a program's sessions with
//! `oc-tags sessions <tag>`, and spend reports group by the same tags. Goose
//! sessions are stored there as `goose:<id>`, the same id the pickers use for
//! their rows. `honk chat --new --tag <tag>` writes that tag when it creates the
//! session, as `opencode-launch --tag` does for opencode sessions.
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

/// The chat notice for a tagging outcome.
pub fn notice(
    settings: &TagSettings,
    goose_session_id: &str,
    outcome: &anyhow::Result<String>,
) -> crate::chat::Notice {
    match outcome {
        Ok(line) => crate::chat::Notice::info(format!("Tag: {line}")),
        Err(e) => {
            let id = tagged_id(goose_session_id);
            crate::chat::Notice::warning(format!(
                "Tag: '{tag}' not applied to {id}: {e:#}; apply it with: oc-tags set {tag} {id}",
                tag = settings.tag
            ))
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
}
