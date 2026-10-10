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

impl Recorder {
    /// Creates a disabled recorder that discards all recorded events.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Creates a recorder that appends JSONL lines to `path`.
    pub fn file(path: impl AsRef<Path>, redactor: Redactor) -> anyhow::Result<Self> {
        let path = path.as_ref();
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
    fn disabled_recorder_does_nothing() {
        let rec = Recorder::disabled();
        rec.record_marker("conn", "something");
        assert!(!rec.is_enabled());
    }
}
