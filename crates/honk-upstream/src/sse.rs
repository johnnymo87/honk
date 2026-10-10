use anyhow::Context;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use serde_json::Value;

/// Transforms a stream of bytes into a stream of parsed JSON [`Value`]s.
///
/// Skips events with empty or whitespace-only data (such as keepalives).
/// Returns an error item for non-JSON payloads or underlying stream errors.
pub fn sse_to_json_stream<S, E>(byte_stream: S) -> BoxStream<'static, anyhow::Result<Value>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    byte_stream
        .eventsource()
        .filter_map(|event_res| async move {
            match event_res {
                Ok(event) => {
                    let data = event.data.trim();
                    if data.is_empty() {
                        None
                    } else {
                        Some(
                            serde_json::from_str::<Value>(data).with_context(|| {
                                format!("failed to parse SSE data as JSON: {data}")
                            }),
                        )
                    }
                }
                Err(err) => Some(Err(anyhow::anyhow!("SSE stream error: {err}"))),
            }
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
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::from("data: not json at all\n\n"))];
        let stream = futures::stream::iter(chunks);
        let mut json_stream = sse_to_json_stream(stream);

        let item = json_stream.next().await.expect("expected error item");
        assert!(item.is_err());
        assert!(json_stream.next().await.is_none());
    }
}
