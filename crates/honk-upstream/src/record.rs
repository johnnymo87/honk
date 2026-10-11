use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::redact::Redactor;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    In,
    Out,
    Marker,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordEntry {
    pub t_ms: u64,
    pub dir: Direction,
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

struct Inner {
    start: Instant,
    writer: Mutex<BufWriter<File>>,
    redactor: Redactor,
}

#[derive(Clone, Default)]
pub struct Recorder {
    inner: Option<Arc<Inner>>,
}

fn path_has_fixtures_component(path: &Path) -> bool {
    if path.components().any(|c| c.as_os_str() == "fixtures") {
        return true;
    }
    if let Ok(abs) = std::path::absolute(path)
        && abs.components().any(|c| c.as_os_str() == "fixtures")
    {
        return true;
    }
    if let Ok(canon) = std::fs::canonicalize(path)
        && canon.components().any(|c| c.as_os_str() == "fixtures")
    {
        return true;
    }
    false
}

impl Recorder {
    /// Creates a disabled recorder that discards all recorded events.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Creates a recorder that appends JSONL lines to `path`.
    ///
    /// When writing to any path with a `fixtures` component, refuses unless the redactor has
    /// org needles configured or `HONK_ALLOW_NO_REDACT=1` is set.
    pub fn file(path: impl AsRef<Path>, redactor: Redactor) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if path_has_fixtures_component(path)
            && std::env::var("HONK_ALLOW_NO_REDACT").as_deref() != Ok("1")
            && !redactor.has_org_needles()
        {
            panic!(
                "probe recorder writing under fixtures/ requires a non-empty needle file at ~/.config/honk/redact.txt \
                 (or $HONK_REDACT_FILE). Set HONK_ALLOW_NO_REDACT=1 to override."
            );
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir '{}'", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open record file '{}'", path.display()))?;
        Ok(Self {
            inner: Some(Arc::new(Inner {
                start: Instant::now(),
                writer: Mutex::new(BufWriter::new(file)),
                redactor,
            })),
        })
    }

    /// Whether this recorder is writing to a destination.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Records an event to the underlying log if enabled.
    pub fn record(
        &self,
        dir: Direction,
        stream: impl Into<String>,
        http_status: Option<u16>,
        frame: Option<Value>,
        note: Option<String>,
    ) {
        let Some(inner) = &self.inner else {
            return;
        };

        let t_ms = inner.start.elapsed().as_millis() as u64;
        let frame = frame.map(|f| inner.redactor.apply(f));
        let note = note.map(|n| inner.redactor.redact_string(&n));
        let entry = RecordEntry {
            t_ms,
            dir,
            stream: stream.into(),
            http_status,
            frame,
            note,
        };

        if let Ok(line) = serde_json::to_string(&entry)
            && let Ok(mut writer) = inner.writer.lock()
        {
            let _ = writeln!(writer, "{line}");
            let _ = writer.flush();
        }
    }

    /// Convenience for recording an incoming frame.
    pub fn record_in(&self, stream: impl Into<String>, frame: Value) {
        self.record(Direction::In, stream, None, Some(frame), None);
    }

    /// Convenience for recording an outbound POST or response.
    pub fn record_out(
        &self,
        stream: impl Into<String>,
        http_status: Option<u16>,
        frame: Option<Value>,
    ) {
        self.record(Direction::Out, stream, http_status, frame, None);
    }

    /// Convenience for recording a marker.
    pub fn record_marker(&self, stream: impl Into<String>, note: impl Into<String>) {
        self.record(Direction::Marker, stream, None, None, Some(note.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead;

    #[test]
    fn round_trip_test_with_redaction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file_path = dir.path().join("test.jsonl");

        let redactor = Redactor::new(vec![("s3cret".into(), "<SECRET>".into())]);
        let rec = Recorder::file(&file_path, redactor)?;

        rec.record(
            Direction::Out,
            "post",
            Some(202),
            Some(json!({"auth": "s3cret", "cmd": "init"})),
            None,
        );
        rec.record(
            Direction::Marker,
            "conn",
            None,
            None,
            Some("reconnecting".into()),
        );
        rec.record(
            Direction::In,
            "session:s1",
            None,
            Some(json!({"result": "ok"})),
            None,
        );

        let file = std::fs::File::open(&file_path)?;
        let reader = std::io::BufReader::new(file);
        let entries: Vec<RecordEntry> = reader
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect();

        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].dir, Direction::Out);
        assert_eq!(entries[0].stream, "post");
        assert_eq!(entries[0].http_status, Some(202));
        assert_eq!(
            entries[0].frame,
            Some(json!({"auth": "<SECRET>", "cmd": "init"}))
        );
        assert_eq!(entries[0].note, None);

        assert_eq!(entries[1].dir, Direction::Marker);
        assert_eq!(entries[1].stream, "conn");
        assert_eq!(entries[1].note.as_deref(), Some("reconnecting"));

        assert_eq!(entries[2].dir, Direction::In);
        assert_eq!(entries[2].stream, "session:s1");
        assert_eq!(entries[2].frame, Some(json!({"result": "ok"})));

        Ok(())
    }

    #[test]
    fn note_is_redacted() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file_path = dir.path().join("test_note.jsonl");

        let redactor = Redactor::new(vec![
            ("s3cret".into(), "<SECRET>".into()),
            ("acme".into(), "<ORG>".into()),
        ]);
        let rec = Recorder::file(&file_path, redactor)?;

        rec.record_marker("conn", "encountered s3cret at ACME corp");
        rec.record(
            Direction::Marker,
            "stream",
            None,
            None,
            Some("another s3cret note".to_string()),
        );

        let file = std::fs::File::open(&file_path)?;
        let reader = std::io::BufReader::new(file);
        let entries: Vec<RecordEntry> = reader
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect();

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].note.as_deref(),
            Some("encountered <SECRET> at <ORG> corp")
        );
        assert_eq!(entries[1].note.as_deref(), Some("another <SECRET> note"));

        Ok(())
    }

    #[test]
    #[should_panic(
        expected = "probe recorder writing under fixtures/ requires a non-empty needle file"
    )]
    fn fixtures_recorder_panics_without_org_needles() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures_dir = dir.path().join("fixtures");
        let file_path = fixtures_dir.join("test.jsonl");

        let redactor = Redactor::default();
        let _ = Recorder::file(&file_path, redactor);
    }

    #[test]
    fn fixtures_recorder_succeeds_with_org_needles() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures_dir = dir.path().join("fixtures");
        let file_path = fixtures_dir.join("test.jsonl");

        let mut redactor = Redactor::default();
        redactor.add_org_needle("acme");
        let rec = Recorder::file(&file_path, redactor);
        assert!(rec.is_ok());
    }

    #[test]
    fn disabled_recorder_does_nothing() {
        let rec = Recorder::disabled();
        rec.record_marker("conn", "something");
        assert!(!rec.is_enabled());
    }
}
