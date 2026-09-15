// Normalizes foreign log output into `LogEvent`s. This is what makes a new
// sidecar (in any language) loggable with zero shell changes: it only has to
// print one JSON object per line on stdout. Understood shapes, in order:
//   1. the unified schema itself (`category` + `event` [+ `ts`, `data`, …]);
//   2. `tracing-subscriber` JSON (`timestamp`, `level`, `fields`/flattened,
//      `target`, `span`/`spans`) — what the Rust sidecars emit;
//   3. anything else: the raw text line (ANSI stripped) as `sidecar/stdout`.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::event::{Level, LogEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Keys that map onto top-level schema fields (never copied into `data`).
const RESERVED: &[&str] = &[
    "v",
    "seq",
    "ts",
    "ts_utc",
    "timestamp",
    "time",
    "tz",
    "source",
    "level",
    "category",
    "event",
    "msg",
    "message",
    "boot_id",
    "installation_id",
    "app_version",
    "os",
    "session_user",
    "route",
    "request_id",
    "data",
    "fields",
    "span",
    "spans",
];

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap())
}

/// One stdout/stderr line from a sidecar. Returns `None` for blank lines.
pub fn from_sidecar_line(source: &str, stream: Stream, raw: &[u8]) -> Option<LogEvent> {
    let text = String::from_utf8_lossy(raw);
    let text = ansi_re().replace_all(text.trim_end_matches(['\r', '\n']), "");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) {
            return Some(from_json(source, obj));
        }
    }
    let (event, mut level) = match stream {
        Stream::Stdout => ("stdout", Level::Info),
        Stream::Stderr => ("stderr", Level::Warn),
    };
    if trimmed.contains("panicked at") {
        level = Level::Error;
    }
    Some(
        LogEvent::new(source, "sidecar", event)
            .level(level)
            .msg(trimmed.to_string()),
    )
}

/// A JSON object from any producer, attributed to `source` (the transport
/// decides the source; a producer cannot impersonate another one).
pub fn from_json(source: &str, mut obj: Map<String, Value>) -> LogEvent {
    // Non-flattened tracing JSON nests the event fields under `fields`.
    if let Some(Value::Object(fields)) = obj.remove("fields") {
        for (k, v) in fields {
            obj.entry(k).or_insert(v);
        }
    }

    let target = str_field(&obj, "target");
    let category = str_field(&obj, "category")
        .or_else(|| target.as_deref().map(category_from_target))
        .unwrap_or_else(|| "app".into());
    let event_name = str_field(&obj, "event")
        .or_else(|| target.as_deref().map(event_from_target))
        .unwrap_or_else(|| "log".into());

    let mut event = LogEvent::new(source, &category, &event_name);
    event.ts = str_field(&obj, "ts")
        .or_else(|| str_field(&obj, "timestamp"))
        .or_else(|| str_field(&obj, "time"))
        .unwrap_or(event.ts);
    event.ts_utc = str_field(&obj, "ts_utc").unwrap_or_default();
    event.level = str_field(&obj, "level")
        .map(|l| Level::parse(&l))
        .unwrap_or_default();
    event.msg = str_field(&obj, "msg").or_else(|| str_field(&obj, "message"));
    event.route = str_field(&obj, "route");

    // Request/user context: the event itself, else the current span, else
    // any span in the list (innermost last).
    let span_ctx = span_context(&obj);
    event.request_id =
        str_field(&obj, "request_id").or_else(|| span_ctx.get("request_id").cloned());
    event.session_user = str_field(&obj, "session_user")
        .or_else(|| str_field(&obj, "user_id"))
        .or_else(|| span_ctx.get("user_id").cloned());

    // Producers may pass these along (a sidecar knows LOG_BOOT_ID); the hub
    // fills whichever are still missing.
    event.boot_id = str_field(&obj, "boot_id").unwrap_or_default();
    event.installation_id = str_field(&obj, "installation_id");
    event.app_version = str_field(&obj, "app_version").unwrap_or_default();

    let mut data = match obj.remove("data") {
        Some(Value::Object(map)) => map,
        Some(Value::Null) | None => Map::new(),
        Some(other) => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    if let Some(span) = obj.get("span").cloned() {
        data.entry("span").or_insert(span);
    }
    for (k, v) in obj {
        if !RESERVED.contains(&k.as_str()) {
            data.entry(k).or_insert(v);
        }
    }
    // Services log bodies as JSON *strings* (tracing fields are flat); expand
    // them back into structured values so the viewer can browse them.
    for (key, value) in data.iter_mut() {
        if !key.ends_with("body") {
            continue;
        }
        if let Value::String(raw) = value {
            let trimmed = raw.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                    *value = parsed;
                }
            }
        }
    }
    event.data = if data.is_empty() {
        Value::Null
    } else {
        Value::Object(data)
    };
    event
}

fn str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn span_context(obj: &Map<String, Value>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut absorb = |span: &Value| {
        if let Value::Object(map) = span {
            for key in ["request_id", "user_id"] {
                if let Some(Value::String(s)) = map.get(key) {
                    out.insert(key.to_string(), s.clone());
                }
            }
        }
    };
    if let Some(Value::Array(spans)) = obj.get("spans") {
        spans.iter().for_each(&mut absorb);
    }
    if let Some(span) = obj.get("span") {
        absorb(span);
    }
    out
}

fn category_from_target(target: &str) -> String {
    if target.starts_with("tower_http") {
        "http".into()
    } else if target.starts_with("sqlx") {
        "db".into()
    } else if target == "domain" || target.starts_with("domain::") {
        "domain".into()
    } else {
        "app".into()
    }
}

fn event_from_target(target: &str) -> String {
    if target.starts_with("sqlx::query") {
        "query".into()
    } else {
        target.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_line_strips_ansi_and_maps_stream() {
        let e =
            from_sidecar_line("backend", Stream::Stderr, b"\x1b[32mINFO\x1b[0m hello\n").unwrap();
        assert_eq!(e.msg.as_deref(), Some("INFO hello"));
        assert_eq!(
            (e.category.as_str(), e.event.as_str(), e.level),
            ("sidecar", "stderr", Level::Warn)
        );
        assert!(from_sidecar_line("backend", Stream::Stdout, b"   \n").is_none());
    }

    #[test]
    fn panic_line_is_error() {
        let e = from_sidecar_line(
            "backend",
            Stream::Stderr,
            b"thread 'main' panicked at src/main.rs:1",
        )
        .unwrap();
        assert_eq!(e.level, Level::Error);
    }

    #[test]
    fn tracing_json_flattened_with_span() {
        let line = json!({
            "timestamp": "2026-09-15T10:22:01.123456+05:30",
            "level": "WARN",
            "message": "request finished",
            "target": "tower_http::trace::on_response",
            "status": 404,
            "span": {"name": "request", "request_id": "req_1", "user_id": "usr_9"}
        })
        .to_string();
        let e = from_sidecar_line("backend", Stream::Stdout, line.as_bytes()).unwrap();
        assert_eq!(e.ts, "2026-09-15T10:22:01.123456+05:30");
        assert_eq!(e.level, Level::Warn);
        assert_eq!(e.category, "http");
        assert_eq!(e.request_id.as_deref(), Some("req_1"));
        assert_eq!(e.session_user.as_deref(), Some("usr_9"));
        assert_eq!(e.data["status"], 404);
        assert_eq!(e.data["target"], "tower_http::trace::on_response");
    }

    #[test]
    fn tracing_json_nested_fields_and_domain_event() {
        let line = json!({
            "timestamp": "2026-09-15T10:22:01+05:30",
            "level": "INFO",
            "target": "domain",
            "fields": {"message": "sale created", "category": "domain", "event": "sale.created", "invoice_key": "inv_1"}
        })
        .to_string();
        let e = from_sidecar_line("backend", Stream::Stdout, line.as_bytes()).unwrap();
        assert_eq!(
            (e.category.as_str(), e.event.as_str()),
            ("domain", "sale.created")
        );
        assert_eq!(e.msg.as_deref(), Some("sale created"));
        assert_eq!(e.data["invoice_key"], "inv_1");
    }

    /// Exact line shape the backend prints with `LOG_FORMAT=json`
    /// (captured from a running binary): flattened fields, `spans` list.
    #[test]
    fn real_backend_request_line() {
        let line = r#"{"timestamp":"2026-09-15T23:45:49.146691+05:30","level":"INFO","message":"POST /api/auth/login","category":"http","event":"request","method":"POST","path":"/api/auth/login","content_type":"application/json","content_length":"47","user_agent":"curl/8.7.1","body":"{\"email\":\"nobody@shop.lk\",\"password\":\"[REDACTED]\"}","target":"http","spans":[{"method":"POST","path":"/api/auth/login","request_id":"req_e2e-login","name":"request"}]}"#;
        let e = from_sidecar_line("backend", Stream::Stdout, line.as_bytes()).unwrap();
        assert_eq!(e.ts, "2026-09-15T23:45:49.146691+05:30");
        assert_eq!((e.category.as_str(), e.event.as_str()), ("http", "request"));
        assert_eq!(e.request_id.as_deref(), Some("req_e2e-login"));
        assert_eq!(e.msg.as_deref(), Some("POST /api/auth/login"));
        assert_eq!(e.data["body"]["email"], "nobody@shop.lk");
        assert_eq!(e.data["body"]["password"], "[REDACTED]");
        assert_eq!(e.data["status"], Value::Null);
    }

    /// sqlx statement lines carry no category; the target maps them to `db`.
    #[test]
    fn real_sqlx_line_maps_to_db() {
        let line = r#"{"timestamp":"2026-09-15T23:45:49.147709+05:30","level":"INFO","summary":"SELECT * FROM users …","db.statement":"\n\nSELECT * FROM users WHERE LOWER(email) = LOWER($1)\n","rows_returned":0,"elapsed_secs":0.000124042,"target":"sqlx::query","spans":[{"request_id":"req_e2e-login","name":"request"}]}"#;
        let e = from_sidecar_line("backend", Stream::Stdout, line.as_bytes()).unwrap();
        assert_eq!((e.category.as_str(), e.event.as_str()), ("db", "query"));
        assert_eq!(e.request_id.as_deref(), Some("req_e2e-login"));
        assert!(e.data["db.statement"]
            .as_str()
            .unwrap()
            .contains("FROM users"));
    }

    #[test]
    fn json_string_bodies_are_expanded() {
        let line = json!({
            "timestamp": "2026-09-15T10:22:01+05:30", "level": "INFO", "target": "http",
            "category": "http", "event": "request",
            "body": "{\"email\":\"a@b.c\",\"password\":\"[REDACTED]\"}",
            "response_body": "not json {"
        })
        .to_string();
        let e = from_sidecar_line("backend", Stream::Stdout, line.as_bytes()).unwrap();
        assert_eq!(e.data["body"]["email"], "a@b.c");
        assert_eq!(e.data["response_body"], "not json {");
    }

    #[test]
    fn unified_schema_passthrough_keeps_data() {
        let obj = json!({
            "ts": "2026-09-15T10:00:00.000+05:30", "level": "info", "category": "ui", "event": "click",
            "msg": "Clicked Save", "route": "/billing", "request_id": "req_2",
            "data": {"label": "Save"}, "source": "backend"
        });
        let Value::Object(map) = obj else {
            unreachable!()
        };
        let e = from_json("frontend", map);
        assert_eq!(e.source, "frontend", "transport decides the source");
        assert_eq!(e.route.as_deref(), Some("/billing"));
        assert_eq!(e.data["label"], "Save");
        assert!(e.data.get("source").is_none());
    }
}
