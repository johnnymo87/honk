use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub upstream_id: Option<u64>,
    pub frame: Value,
}

#[derive(Debug, PartialEq)]
pub enum Resume<'a> {
    Delta(&'a [Entry]),
    Reset(&'a [Entry]),
}

#[derive(Serialize, Deserialize)]
struct LogHeader {
    epoch: u64,
    resume_from: Option<u64>,
}

pub fn read_header_epoch(path: &Path) -> Result<u64> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line)?;
    let header: LogHeader = serde_json::from_str(&first_line)?;
    Ok(header.epoch)
}

#[derive(Serialize)]
struct BorrowedLogLine<'a> {
    seq: Option<u64>,
    id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame: Option<&'a Value>,
}

#[derive(Deserialize)]
struct OwnedLogLine {
    seq: Option<u64>,
    id: Option<u64>,
    #[serde(default)]
    frame: Option<Value>,
}

fn stamp_frame(frame: &mut Value, epoch: u64, seq: u64) {
    if !frame.is_object() {
        return;
    }
    let params = match frame.get_mut("params") {
        Some(p) => p,
        None => {
            frame
                .as_object_mut()
                .unwrap()
                .insert("params".into(), serde_json::json!({}));
            frame.get_mut("params").unwrap()
        }
    };
    if !params.is_object() {
        *params = serde_json::json!({});
    }
    let meta = match params.get_mut("_meta") {
        Some(m) => m,
        None => {
            params
                .as_object_mut()
                .unwrap()
                .insert("_meta".into(), serde_json::json!({}));
            params.get_mut("_meta").unwrap()
        }
    };
    if !meta.is_object() {
        *meta = serde_json::json!({});
    }
    meta.as_object_mut().unwrap().insert(
        "honk".into(),
        serde_json::json!({ "epoch": epoch, "seq": seq }),
    );
}

#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    file: std::fs::File,
    epoch: u64,
    resume_from: Option<u64>,
    entries: Vec<Entry>,
    last_processed_id: Option<u64>,
}

impl SessionLog {
    /// New generation: write header + frames to a tmp file, fsync, rename. Used on every
    /// new connection and on `Acp-Replay: gap` (frames = export rebuild, ids None, plus any
    /// buffered live frames with their ids).
    pub fn create(
        path: &Path,
        epoch: u64,
        resume_from: Option<u64>,
        frames: Vec<(Option<u64>, Value)>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("session.jsonl");
        let tmp_path = path.with_file_name(format!(".{file_name}.tmp"));

        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut tmp_file = opts.open(&tmp_path)?;

        let header = LogHeader { epoch, resume_from };
        let mut buf = serde_json::to_string(&header)?;
        buf.push('\n');

        let mut entries = Vec::with_capacity(frames.len());
        let mut last_upstream_id = resume_from;
        let mut seq = 0u64;

        for (upstream_id, mut frame) in frames {
            debug_assert_eq!(
                crate::rpc::kind(&frame),
                crate::rpc::Kind::Notification,
                "logged frame must be a notification (has method, no id)"
            );
            seq += 1;
            stamp_frame(&mut frame, epoch, seq);
            if let Some(id) = upstream_id {
                last_upstream_id = Some(last_upstream_id.map_or(id, |prev| prev.max(id)));
            }
            let line = BorrowedLogLine {
                seq: Some(seq),
                id: upstream_id,
                frame: Some(&frame),
            };
            buf.push_str(&serde_json::to_string(&line)?);
            buf.push('\n');
            entries.push(Entry {
                seq,
                upstream_id,
                frame,
            });
        }

        tmp_file.write_all(buf.as_bytes())?;
        tmp_file.sync_all()?;
        drop(tmp_file);

        std::fs::rename(&tmp_path, path)?;

        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            let dir_file = std::fs::File::open(parent)?;
            dir_file.sync_all()?;
        }

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
            epoch,
            resume_from,
            entries,
            last_processed_id: last_upstream_id,
        })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        let metadata = file.metadata()?;
        let file_len = metadata.len() as usize;

        let mut data = Vec::with_capacity(file_len);
        file.read_to_end(&mut data)?;

        if data.is_empty() {
            anyhow::bail!("log file {} is empty", path.display());
        }

        let header_end = data
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| anyhow::anyhow!("log file missing header newline"))?;

        let header_bytes = &data[..header_end];
        let header: LogHeader = serde_json::from_slice(header_bytes)?;
        let mut offset = header_end + 1;

        let epoch = header.epoch;
        let resume_from = header.resume_from;
        let mut last_upstream_id = resume_from;
        let mut entries = Vec::new();
        let mut valid_file_len = offset;

        while offset < data.len() {
            let line_start = offset;
            let has_newline = data[offset..].iter().position(|&b| b == b'\n');
            let Some(pos) = has_newline else {
                // Final partial line (no trailing \n) -> truncate it
                break;
            };
            let line_end = offset + pos;
            let line_bytes = &data[line_start..line_end];
            let more_bytes_follow = line_end + 1 < data.len();

            match serde_json::from_slice::<OwnedLogLine>(line_bytes) {
                Ok(line) => {
                    if let Some(id) = line.id {
                        last_upstream_id = Some(last_upstream_id.map_or(id, |prev| prev.max(id)));
                    }
                    if let (Some(seq), Some(frame)) = (line.seq, line.frame) {
                        entries.push(Entry {
                            seq,
                            upstream_id: line.id,
                            frame,
                        });
                    }
                    valid_file_len = line_end + 1;
                    offset = line_end + 1;
                }
                Err(e) => {
                    if more_bytes_follow {
                        anyhow::bail!(
                            "corrupt log line in middle of log file {} at offset {line_start}: {e}",
                            path.display()
                        );
                    } else {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "final complete log line is unparseable; truncating"
                        );
                        break;
                    }
                }
            }
        }

        if valid_file_len < data.len() {
            file.set_len(valid_file_len as u64)?;
        }
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
            epoch,
            resume_from,
            entries,
            last_processed_id: last_upstream_id,
        })
    }

    /// Stamp params._meta.honk = {epoch, seq} and append as ONE write. Returns the stamped entries.
    pub fn append(&mut self, frames: Vec<(Option<u64>, Value)>) -> Result<&[Entry]> {
        if frames.is_empty() {
            return Ok(&[]);
        }
        let start_idx = self.entries.len();
        let mut seq = self.head().seq;
        let mut buf = String::new();

        for (upstream_id, mut frame) in frames {
            debug_assert_eq!(
                crate::rpc::kind(&frame),
                crate::rpc::Kind::Notification,
                "appended frame must be a notification (has method, no id)"
            );
            seq += 1;
            stamp_frame(&mut frame, self.epoch, seq);
            if let Some(id) = upstream_id {
                self.last_processed_id =
                    Some(self.last_processed_id.map_or(id, |prev| prev.max(id)));
            }
            let line = BorrowedLogLine {
                seq: Some(seq),
                id: upstream_id,
                frame: Some(&frame),
            };
            buf.push_str(&serde_json::to_string(&line)?);
            buf.push('\n');
            self.entries.push(Entry {
                seq,
                upstream_id,
                frame,
            });
        }

        self.file.write_all(buf.as_bytes())?;
        Ok(&self.entries[start_idx..])
    }

    /// Id-only line: a session-stream frame that was processed but not logged.
    pub fn mark_processed(&mut self, id: u64) -> Result<()> {
        self.last_processed_id = Some(self.last_processed_id.map_or(id, |prev| prev.max(id)));
        let line = BorrowedLogLine {
            seq: None,
            id: Some(id),
            frame: None,
        };
        let mut buf = serde_json::to_string(&line)?;
        buf.push('\n');
        self.file.write_all(buf.as_bytes())?;
        Ok(())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn head(&self) -> honk_wire::Cursor {
        honk_wire::Cursor {
            epoch: self.epoch,
            seq: self.entries.last().map_or(0, |e| e.seq),
        }
    }

    /// Last processed session-stream event id: max(resume_from, last id on any line, incl. id-only).
    pub fn last_upstream_id(&self) -> Option<u64> {
        self.last_processed_id
    }

    pub fn resume(&self, after: Option<&honk_wire::Cursor>) -> Resume<'_> {
        match after {
            Some(cursor) if cursor.epoch == self.epoch && cursor.seq <= self.head().seq => {
                let idx = self
                    .entries
                    .iter()
                    .position(|e| e.seq > cursor.seq)
                    .unwrap_or(self.entries.len());
                Resume::Delta(&self.entries[idx..])
            }
            _ => Resume::Reset(&self.entries),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resume_from(&self) -> Option<u64> {
        self.resume_from
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

pub fn last_usage_frame(entries: &[Entry]) -> Option<&Value> {
    entries.iter().rev().find_map(|entry| {
        if entry.frame.get("method").and_then(Value::as_str) != Some("session/update") {
            return None;
        }
        let update = entry.frame.pointer("/params/update")?;
        honk_wire::usage(update)?;
        Some(&entry.frame)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(msg_id: &str, text: &str) -> Value {
        crate::rpc::notification(
            "session/update",
            json!({
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": msg_id,
                    "content": {
                        "type": "text",
                        "text": text,
                    }
                }
            }),
        )
    }

    fn usage_frame(used: u64, size: u64) -> Value {
        crate::rpc::notification(
            "session/update",
            json!({
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": used,
                    "size": size,
                }
            }),
        )
    }

    #[test]
    fn test_last_usage_frame() {
        let entries = vec![
            Entry {
                seq: 1,
                upstream_id: Some(1),
                frame: chunk("m1", "hello"),
            },
            Entry {
                seq: 2,
                upstream_id: Some(2),
                frame: usage_frame(100, 200),
            },
            Entry {
                seq: 3,
                upstream_id: Some(3),
                frame: chunk("m2", "world"),
            },
            Entry {
                seq: 4,
                upstream_id: Some(4),
                frame: usage_frame(150, 200),
            },
            Entry {
                seq: 5,
                upstream_id: Some(5),
                frame: chunk("m3", "!"),
            },
        ];

        let found = last_usage_frame(&entries);
        assert!(found.is_some());
        let val = found.unwrap();
        assert_eq!(
            val.pointer("/params/update/used").and_then(Value::as_u64),
            Some(150)
        );

        let empty: Vec<Entry> = vec![];
        assert_eq!(last_usage_frame(&empty), None);

        let chunks_only = vec![Entry {
            seq: 1,
            upstream_id: Some(1),
            frame: chunk("m1", "hello"),
        }];
        assert_eq!(last_usage_frame(&chunks_only), None);

        // Frame with method != "session/update" must be ignored
        let mut entries_with_custom = entries.clone();
        entries_with_custom.push(Entry {
            seq: 6,
            upstream_id: Some(6),
            frame: crate::rpc::notification(
                "_goose/unstable/session/update",
                json!({
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "usage_update",
                        "used": 250,
                        "size": 200,
                    }
                }),
            ),
        });
        let found_custom = last_usage_frame(&entries_with_custom);
        assert_eq!(
            found_custom
                .unwrap()
                .pointer("/params/update/used")
                .and_then(Value::as_u64),
            Some(150),
            "custom method must be ignored, returning previous session/update frame"
        );

        let custom_only = vec![Entry {
            seq: 1,
            upstream_id: Some(1),
            frame: crate::rpc::notification(
                "_goose/unstable/session/update",
                json!({
                    "sessionId": "s",
                    "update": {
                        "sessionUpdate": "usage_update",
                        "used": 250,
                        "size": 200,
                    }
                }),
            ),
        }];
        assert_eq!(last_usage_frame(&custom_only), None);
    }

    #[test]
    fn reopen_keeps_epoch_seq_and_last_id_so_cursors_stay_valid() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut log = SessionLog::create(&p, 3, None, vec![]).unwrap();
        log.append(vec![
            (Some(10), chunk("m", "a")),
            (Some(11), chunk("m", "b")),
        ])
        .unwrap();
        let cur = log.head();
        drop(log);
        let log = SessionLog::open(&p).unwrap();
        assert_eq!(log.epoch(), 3);
        assert_eq!(read_header_epoch(&p).unwrap(), 3);
        assert_eq!(log.head(), cur);
        assert_eq!(log.last_upstream_id(), Some(11));
        assert!(matches!(
            log.resume(Some(&honk_wire::Cursor { epoch: 3, seq: 1 })),
            Resume::Delta(e) if e.len() == 1
        ));
        assert!(matches!(
            log.resume(Some(&honk_wire::Cursor { epoch: 2, seq: 1 })),
            Resume::Reset(_)
        ));
    }

    #[test]
    fn partial_trailing_line_is_truncated_on_open() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut log = SessionLog::create(&p, 1, None, vec![]).unwrap();
        log.append(vec![(Some(1), chunk("m", "a"))]).unwrap();
        drop(log);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"{\"seq\":2,\"id\":2,\"fra")
            .unwrap();
        let mut log = SessionLog::open(&p).unwrap();
        assert_eq!(log.last_upstream_id(), Some(1));
        log.append(vec![(Some(2), chunk("m", "b"))]).unwrap(); // seq 2 reused, file stays parseable
        assert_eq!(SessionLog::open(&p).unwrap().head().seq, 2);
    }

    #[test]
    fn create_from_export_records_resume_point_and_new_epoch() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let log =
            SessionLog::create(&p, 7, Some(40), vec![(None, chunk("m", "committed"))]).unwrap();
        assert_eq!(
            (log.epoch(), log.last_upstream_id(), log.head().seq),
            (7, Some(40), 1)
        );
    }

    #[test]
    fn id_only_lines_advance_last_processed_id_but_not_seq() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut log = SessionLog::create(&p, 1, None, vec![]).unwrap();
        log.append(vec![(Some(5), chunk("m", "a"))]).unwrap();
        log.mark_processed(6).unwrap(); // e.g. a prompt reply
        drop(log);
        let log = SessionLog::open(&p).unwrap();
        assert_eq!(log.last_upstream_id(), Some(6));
        assert_eq!(log.head().seq, 1);
    }

    #[test]
    fn resume_edge_cases_reset_on_none_or_future_seq() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut log = SessionLog::create(&p, 5, None, vec![]).unwrap();
        log.append(vec![(Some(1), chunk("m", "a")), (Some(2), chunk("m", "b"))])
            .unwrap();
        assert_eq!(log.head().seq, 2);

        // None -> Reset(all)
        match log.resume(None) {
            Resume::Reset(all) => assert_eq!(all.len(), 2),
            Resume::Delta(_) => panic!("expected Reset on None"),
        }

        // seq > head -> Reset(all)
        match log.resume(Some(&honk_wire::Cursor { epoch: 5, seq: 99 })) {
            Resume::Reset(all) => assert_eq!(all.len(), 2),
            Resume::Delta(_) => panic!("expected Reset on seq > head"),
        }

        // seq == head -> Delta(empty)
        match log.resume(Some(&honk_wire::Cursor { epoch: 5, seq: 2 })) {
            Resume::Delta(delta) => assert_eq!(delta.len(), 0),
            Resume::Reset(_) => panic!("expected Delta on seq == head"),
        }
    }

    #[test]
    fn frames_are_stamped_with_epoch_and_seq() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut log = SessionLog::create(&p, 4, None, vec![(None, chunk("m", "init"))]).unwrap();
        assert_eq!(
            honk_wire::frame_cursor(&log.entries[0].frame),
            Some(honk_wire::Cursor { epoch: 4, seq: 1 })
        );

        let entries = log
            .append(vec![(Some(10), chunk("m", "appended"))])
            .unwrap();
        assert_eq!(
            honk_wire::frame_cursor(&entries[0].frame),
            Some(honk_wire::Cursor { epoch: 4, seq: 2 })
        );
    }

    #[test]
    fn middle_corruption_refuses_to_open() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sessions").join("s.jsonl");
        let mut log = SessionLog::create(&p, 1, None, vec![]).unwrap();
        log.append(vec![(Some(1), chunk("m", "a")), (Some(2), chunk("m", "b"))])
            .unwrap();
        drop(log);

        // Check permissions of parent (sessions) and file
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(p.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        // Corrupt middle line (line 1 of 2 entries)
        let contents = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);
        lines[1] = "{\"seq\":1,not-valid-json}";
        let corrupted_data = lines.join("\n") + "\n";
        std::fs::write(&p, corrupted_data).unwrap();

        let res = SessionLog::open(&p);
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("middle"),
            "expected middle corruption error, got: {err}"
        );
    }

    #[test]
    fn final_complete_unparseable_line_is_truncated() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut log = SessionLog::create(&p, 1, None, vec![]).unwrap();
        log.append(vec![(Some(1), chunk("m", "a"))]).unwrap();
        drop(log);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"{\"seq\":2,invalid}\n")
            .unwrap();
        let log = SessionLog::open(&p).unwrap();
        assert_eq!(log.head().seq, 1);
    }
}
