// Realtime listeners of the sync agent. Two server-sent event streams turn
// "wait for the next timer" into "act now":
//
//   local backend  GET /api/sync/local/events  -> `outbox`  -> upload now
//   POS cloud      GET /api/sync/events        -> `change`  -> download now
//                                                 `hello` / `resync` too
//
// Events carry no record data, only "something changed"; the agent then runs
// its normal push / pull, so every rule (acks, cursors, conflicts) stays in one
// place. A dropped stream reconnects with backoff, and while either stream is
// down the agent falls back to its timer, so nothing here can lose a change.

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use serde_json::json;

use crate::logging::{Level, LogEvent};

use super::http::{CloudSyncApi, ErrorKind, LocalApi, SyncError};
use super::state::{LiveChannel, SyncControl};

/// A stream that sent nothing (not even the server's 15 s heartbeat) for this
/// long is treated as dead and reopened.
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
/// Reconnect delays: the local backend restarts in seconds; the cloud may be
/// unreachable for longer.
const LOCAL_RETRY: (Duration, Duration) = (Duration::from_millis(500), Duration::from_secs(10));
const CLOUD_RETRY: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(30));
/// A cloud without the events endpoint (older deployment): look again rarely.
const CLOUD_UNSUPPORTED_RETRY: Duration = Duration::from_secs(300);

/// One parsed server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Incremental `text/event-stream` parser: feed it bytes as they arrive and
/// it returns the events completed so far. Comment lines (heartbeats) are
/// dropped; `id`/`retry` fields are not needed here.
#[derive(Default)]
pub struct SseParser {
    buffer: String,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    out.push(SseEvent {
                        event: if self.event.is_empty() {
                            "message".to_string()
                        } else {
                            std::mem::take(&mut self.event)
                        },
                        data: self.data.join("\n"),
                    });
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "event" => self.event = value.to_string(),
                "data" => self.data.push(value.to_string()),
                _ => {}
            }
        }
        out
    }
}

/// What a listener does with each event.
type OnEvent = Box<dyn Fn(&SseEvent) + Send + Sync>;

/// Reads one opened stream until it ends, errors or goes silent.
async fn read_stream(mut resp: reqwest::Response, on_event: &OnEvent) -> Result<(), SyncError> {
    let mut parser = SseParser::default();
    loop {
        let chunk = match tokio::time::timeout(SILENCE_LIMIT, resp.chunk()).await {
            Err(_) => return Ok(()),
            Ok(Err(e)) => {
                return Err(SyncError::new(
                    ErrorKind::Offline,
                    "STREAM_ERROR",
                    e.without_url().to_string(),
                    0,
                ))
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(chunk))) => chunk,
        };
        for event in parser.feed(&chunk) {
            on_event(&event);
        }
    }
}

fn backoff(attempt: u32, (min, max): (Duration, Duration)) -> Duration {
    let base = min.as_secs_f64() * 2f64.powi(attempt.min(10) as i32);
    let jitter: f64 = rand::thread_rng().gen_range(0.8..1.2);
    Duration::from_secs_f64((base * jitter).min(max.as_secs_f64()))
}

/// Keeps the local `outbox` stream open for as long as the task lives; each
/// event (including the one sent on connect) asks for an upload.
pub async fn run_local(api: LocalApi, ctl: Arc<SyncControl>) {
    let on_event: OnEvent = {
        let ctl = ctl.clone();
        Box::new(move |event| {
            if event.event == "outbox" {
                ctl.wake_local();
            }
        })
    };
    let mut failures = 0;
    loop {
        match api.open_events().await {
            Ok(resp) => {
                failures = 0;
                ctl.set_live(LiveChannel::Local, true);
                let _ = read_stream(resp, &on_event).await;
                ctl.set_live(LiveChannel::Local, false);
            }
            Err(_) => failures += 1,
        }
        tokio::time::sleep(backoff(failures, LOCAL_RETRY)).await;
    }
}

/// Keeps the cloud change stream open for as long as the task lives. On
/// connect the cloud says `hello` (we may have missed changes while away), on
/// each change from elsewhere `change`, and `resync` if we fell behind: all
/// three ask for a download.
pub async fn run_cloud(api: CloudSyncApi, ctl: Arc<SyncControl>) {
    let on_event: OnEvent = {
        let ctl = ctl.clone();
        Box::new(move |event| {
            if matches!(event.event.as_str(), "hello" | "change" | "resync") {
                ctl.wake_remote();
            }
        })
    };
    let mut failures = 0;
    loop {
        let delay = match api.open_events().await {
            Ok(resp) => {
                failures = 0;
                ctl.set_live(LiveChannel::Cloud, true);
                LogEvent::shell("sync", "live.connected")
                    .level(Level::Debug)
                    .emit();
                let outcome = read_stream(resp, &on_event).await;
                ctl.set_live(LiveChannel::Cloud, false);
                LogEvent::shell("sync", "live.disconnected")
                    .level(Level::Debug)
                    .data(json!({ "code": outcome.err().map(|e| e.code) }))
                    .emit();
                backoff(0, CLOUD_RETRY)
            }
            // An older cloud without the endpoint: the timer keeps syncing.
            Err(e) if e.status == 404 || e.status == 405 => CLOUD_UNSUPPORTED_RETRY,
            Err(_) => {
                failures += 1;
                backoff(failures, CLOUD_RETRY)
            }
        };
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks_and_skips_heartbeats() {
        let mut p = SseParser::default();
        assert!(p.feed(b": ping\n\nevent: hel").is_empty());
        let out = p.feed(b"lo\ndata: {\"latestSeq\":7}\n\nevent: change\r\nid: 8\r\n");
        assert_eq!(
            out,
            vec![SseEvent {
                event: "hello".into(),
                data: "{\"latestSeq\":7}".into()
            }]
        );
        let out = p.feed(b"data: {\"seq\":8}\r\n\r\n");
        assert_eq!(out[0].event, "change");
        assert_eq!(out[0].data, "{\"seq\":8}");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert!(backoff(0, CLOUD_RETRY) <= Duration::from_millis(1200));
        assert!(backoff(20, CLOUD_RETRY) <= Duration::from_secs(30));
        assert!(backoff(20, CLOUD_RETRY) >= Duration::from_secs(24));
    }
}
