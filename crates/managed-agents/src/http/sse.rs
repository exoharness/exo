use std::{io, time::Duration};

use anyhow::{Result, bail};
use bytes::Bytes;
use exoharness::{Event, EventStream};
use futures::{Stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

pub const HEARTBEAT: &[u8] = b":\n\n";

pub fn encode_event(event: &impl serde::Serialize) -> Result<Bytes> {
    let data = serde_json::to_string(event)?;
    Ok(Bytes::from(format!("event: exo_event\ndata: {data}\n\n")))
}

pub fn encode_error(error: &str) -> Bytes {
    Bytes::from(format!(
        "event: error\ndata: {}\n\n",
        serde_json::json!({ "error": error })
    ))
}

pub fn with_heartbeats<S, E>(
    events: S,
    heartbeat_interval: Duration,
) -> impl Stream<Item = std::result::Result<Bytes, E>>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Unpin,
{
    let heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_interval,
        heartbeat_interval,
    );
    futures::stream::unfold(
        (events, heartbeat),
        |(mut events, mut heartbeat)| async move {
            tokio::select! {
                event = events.next() => event.map(|event| (event, (events, heartbeat))),
                _ = heartbeat.tick() => Some((
                    Ok(Bytes::from_static(HEARTBEAT)),
                    (events, heartbeat),
                )),
            }
        },
    )
}

pub fn encode(events: EventStream) -> impl Stream<Item = Result<Bytes>> {
    with_heartbeats(
        events.map(|event| match event {
            Ok(event) => encode_event(&event),
            Err(error) => Ok(encode_error(&error.to_string())),
        }),
        HEARTBEAT_INTERVAL,
    )
}

pub fn decode<S, E>(bytes: S) -> EventStream
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let reader = StreamReader::new(Box::pin(bytes.map_err(io::Error::other)));
    let lines = BufReader::new(reader).lines();
    Box::pin(futures::stream::try_unfold(lines, |mut lines| async move {
        let mut name = String::new();
        let mut data = String::new();
        loop {
            let Some(line) = lines.next_line().await? else {
                if !data.is_empty() {
                    bail!("runtime event stream ended in an incomplete event");
                }
                return Ok(None);
            };
            if line.is_empty() {
                match name.as_str() {
                    "exo_event" => {
                        let event: Event = serde_json::from_str(&data)?;
                        return Ok(Some((event, lines)));
                    }
                    "error" => {
                        #[derive(Deserialize)]
                        struct StreamError {
                            error: String,
                        }
                        let error: StreamError = serde_json::from_str(&data)?;
                        bail!("runtime event stream failed: {}", error.error);
                    }
                    _ => {
                        name.clear();
                        data.clear();
                    }
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => name = value.to_owned(),
                "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
                _ => {}
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::{EventData, Uuid7};

    #[tokio::test]
    async fn decodes_fragmented_utf8_multiline_data_and_crlf() -> Result<()> {
        let event = Event {
            id: Uuid7::now(),
            thread_id: Uuid7::now(),
            session_id: None,
            turn_id: None,
            created_at: "2026-09-13T00:00:00Z".parse()?,
            data: EventData::Error {
                message: "café".into(),
                metadata: None,
            },
        };
        let data = serde_json::to_string_pretty(&event)?;
        let frame = format!(
            ": heartbeat\r\n\r\nevent: unknown\r\ndata: ignored\r\n\r\nevent: exo_event\r\n{}\r\n",
            data.lines()
                .map(|line| format!("data: {line}\r\n"))
                .collect::<String>()
        );
        let chunks: Vec<_> = frame
            .bytes()
            .map(|b| Ok::<_, io::Error>(Bytes::from(vec![b])))
            .collect();
        let mut decoded = decode(futures::stream::iter(chunks));
        let actual = decoded.next().await.transpose()?.expect("event");
        assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(&event)?);
        assert!(decoded.next().await.is_none());
        let mut roundtrip = decode(futures::stream::iter([Ok::<_, io::Error>(encode_event(
            &event,
        )?)]));
        assert_eq!(
            roundtrip.next().await.transpose()?.expect("event").id,
            event.id
        );
        Ok(())
    }

    #[tokio::test]
    async fn propagates_server_errors_broken_connections_and_incomplete_events() {
        for frame in [
            encode_error("permission revoked"),
            Bytes::from_static(b"event: exo_event\ndata: {}"),
            Bytes::from_static(b"event: exo_event\ndata: invalid\n\n"),
        ] {
            let mut decoded = decode(futures::stream::iter([Ok::<_, io::Error>(frame)]));
            assert!(decoded.next().await.expect("error item").is_err());
        }
        let mut decoded = decode(futures::stream::iter([Err::<Bytes, _>(io::Error::other(
            "connection closed",
        ))]));
        assert!(
            decoded
                .next()
                .await
                .expect("error item")
                .unwrap_err()
                .to_string()
                .contains("connection closed")
        );
    }
}
