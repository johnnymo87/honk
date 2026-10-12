use anyhow::Context;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use serde_json::Value;

/// One SSE event: its JSON payload and its `id:` field if this event set a new one.
#[derive(Debug, Clone, PartialEq)]
pub struct SseFrame {
    pub id: Option<u64>,
    pub data: Value,
}

/// Transforms a stream of bytes into a stream of parsed JSON [`Value`]s.
///
/// Skips events with empty or whitespace-only data (such as keepalives).
/// Returns an error item for non-JSON payloads or underlying stream errors.
pub fn sse_to_json_stream<S, E>(byte_stream: S) -> BoxStream<'static, anyhow::Result<Value>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    sse_to_frames(byte_stream)
        .map(|r| r.map(|f| f.data))
        .boxed()
}

/// Like [`sse_to_json_stream`], but keeps each event's id (patched goose stamps one).
pub fn sse_to_frames<S, E>(byte_stream: S) -> BoxStream<'static, anyhow::Result<SseFrame>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut previous_id = String::new();
    byte_stream
        .eventsource()
        .filter_map(move |event_res| {
            let item = match event_res {
                Ok(event) => {
                    // eventsource-stream carries the last `id:` onto later events that
                    // set none; only an id that differs from the previous one is new.
                    let id = if event.id != previous_id {
                        event.id.trim().parse().ok()
                    } else {
                        None
                    };
                    previous_id.clone_from(&event.id);
                    let data = event.data.trim();
                    if data.is_empty() {
                        None
                    } else {
                        Some(
                            serde_json::from_str::<Value>(data)
                                .with_context(|| {
                                    format!(
                                        "failed to parse SSE data as JSON (length: {})",
                                        data.len()
                                    )
                                })
                                .map(|data| SseFrame { id, data }),
                        )
                    }
                }
                Err(err) => Some(Err(anyhow::anyhow!("SSE stream error: {err}"))),
            };
            futures::future::ready(item)
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[tokio::test]
    async fn keepalive_only_yields_nothing() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("data:\n\n")),
            Ok(Bytes::from("data:   \n\n")),
            Ok(Bytes::from(": keepalive comment\n\n")),
        ];
        let stream = futures::stream::iter(chunks);
        let mut json_stream = sse_to_json_stream(stream);

        let item = json_stream.next().await;
        assert!(item.is_none());
    }

    #[tokio::test]
    async fn one_json_frame_yields_value() {
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from("data: {\"key\":\"value\"}\n\n"))];
        let stream = futures::stream::iter(chunks);
        let mut json_stream = sse_to_json_stream(stream);

        let item = json_stream
            .next()
            .await
            .expect("expected one item")
            .expect("expected ok");
        assert_eq!(item, json!({"key": "value"}));
        assert!(json_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn frame_split_across_two_chunks_yields_one_value() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("data: {\"split\":")),
            Ok(Bytes::from(" true}\n\n")),
        ];
        let stream = futures::stream::iter(chunks);
        let mut json_stream = sse_to_json_stream(stream);

        let item = json_stream
            .next()
            .await
            .expect("expected one item")
            .expect("expected ok");
        assert_eq!(item, json!({"split": true}));
        assert!(json_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn two_frames_in_one_chunk_yields_two_values() {
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"))];
        let stream = futures::stream::iter(chunks);
        let mut json_stream = sse_to_json_stream(stream);

        let first = json_stream.next().await.expect("first item").expect("ok");
        let second = json_stream.next().await.expect("second item").expect("ok");
        assert_eq!(first, json!({"a": 1}));
        assert_eq!(second, json!({"b": 2}));
        assert!(json_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn non_json_data_yields_error_not_panic() {
        let raw_payload = "super_secret_not_json";
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from(format!("data: {raw_payload}\n\n")))];
        let stream = futures::stream::iter(chunks);
        let mut json_stream = sse_to_json_stream(stream);

        let item = json_stream.next().await.expect("expected error item");
        let err = item.expect_err("expected error");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains(&format!("length: {}", raw_payload.len())),
            "expected error to mention length {}, got: {err_msg}",
            raw_payload.len()
        );
        assert!(
            !err_msg.contains(raw_payload),
            "error should not leak raw data, got: {err_msg}"
        );
        assert!(json_stream.next().await.is_none());
    }

    #[tokio::test]
    async fn frame_ids_are_parsed_and_absent_ids_are_none() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("data: {\"a\":1}\n\n")),
            Ok(Bytes::from("id: 7\ndata: {\"b\":2}\n\n")),
            Ok(Bytes::from("data:\n\n")),
        ];
        let frames: Vec<SseFrame> = sse_to_frames(futures::stream::iter(chunks))
            .map(|r| r.unwrap())
            .collect()
            .await;
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    id: None,
                    data: json!({"a": 1})
                },
                SseFrame {
                    id: Some(7),
                    data: json!({"b": 2})
                },
            ]
        );
    }

    #[tokio::test]
    async fn an_inherited_id_is_not_reported_as_new() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("id: 5\ndata: {\"a\":1}\n\n")),
            Ok(Bytes::from("data: {\"b\":2}\n\n")),
            Ok(Bytes::from("id: 6\ndata: {\"c\":3}\n\n")),
        ];
        let ids: Vec<Option<u64>> = sse_to_frames(futures::stream::iter(chunks))
            .map(|r| r.unwrap().id)
            .collect()
            .await;
        assert_eq!(ids, vec![Some(5), None, Some(6)]);
    }
}
