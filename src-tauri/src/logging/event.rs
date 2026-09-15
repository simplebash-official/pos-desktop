// The unified log record (schema v1) shared by every source — shell,
// frontend, each sidecar and the installer. One record is one JSON line on
// disk. See `docs/logging.md` for the contract other producers must follow.

use chrono::{DateTime, FixedOffset, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA_VERSION: u8 = 1;

/// Severity, ordered from least to most severe so filters can compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    /// Lenient parse: accepts any casing plus the aliases other loggers emit
    /// (`WARNING`, `critical`, `log`, …). Unknown strings fall back to info.
    pub fn parse(raw: &str) -> Level {
        match raw.trim().to_ascii_lowercase().as_str() {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" | "err" => Level::Error,
            "fatal" | "critical" | "panic" => Level::Fatal,
            _ => Level::Info,
        }
    }

    pub fn from_log(level: log::Level) -> Level {
        match level {
            log::Level::Trace => Level::Trace,
            log::Level::Debug => Level::Debug,
            log::Level::Info => Level::Info,
            log::Level::Warn => Level::Warn,
            log::Level::Error => Level::Error,
        }
    }
}

/// One log record. Optional context fields are filled by the hub at write
/// time when the producer did not know them (e.g. `installation_id` for
/// events emitted before the installation record was loaded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub v: u8,
    #[serde(default)]
    pub seq: u64,
    /// Origin time, RFC 3339 with the local UTC offset.
    pub ts: String,
    #[serde(default)]
    pub ts_utc: String,
    #[serde(default)]
    pub tz: String,
    pub source: String,
    pub level: Level,
    pub category: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    #[serde(default)]
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl LogEvent {
    /// Start a record stamped with the current local time.
    pub fn new(source: &str, category: &str, event: &str) -> Self {
        let now = Local::now();
        LogEvent {
            v: SCHEMA_VERSION,
            seq: 0,
            ts: format_local(&now),
            ts_utc: format_utc(&now.with_timezone(&Utc)),
            tz: String::new(),
            source: source.to_string(),
            level: Level::Info,
            category: category.to_string(),
            event: event.to_string(),
            msg: None,
            boot_id: String::new(),
            installation_id: None,
            app_version: String::new(),
            os: String::new(),
            session_user: None,
            route: None,
            request_id: None,
            data: Value::Null,
        }
    }

    /// Shorthand for a shell-originated record.
    pub fn shell(category: &str, event: &str) -> Self {
        Self::new("shell", category, event)
    }

    pub fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    pub fn msg(mut self, msg: impl Into<String>) -> Self {
        self.msg = Some(msg.into());
        self
    }

    pub fn data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    /// Queue the record on the global hub (never blocks).
    pub fn emit(self) {
        super::hub::emit(self);
    }

    /// Local calendar day (`YYYY-MM-DD`) the record belongs to — taken from
    /// the origin timestamp so a late-flushed event lands in its own day.
    pub fn day(&self) -> String {
        match DateTime::parse_from_rfc3339(&self.ts) {
            Ok(dt) => dt.format("%Y-%m-%d").to_string(),
            Err(_) => Local::now().format("%Y-%m-%d").to_string(),
        }
    }

    /// Reconcile `ts` / `ts_utc` so both are present and valid. Producers
    /// that send a naive local time (the NSIS installer) or a UTC time get
    /// the local offset applied here.
    pub fn normalize_time(&mut self) {
        let parsed = parse_any_time(&self.ts).or_else(|| parse_any_time(&self.ts_utc));
        let dt = parsed.unwrap_or_else(|| Local::now().fixed_offset());
        let local = dt.with_timezone(&Local);
        self.ts = format_local(&local);
        self.ts_utc = format_utc(&dt.with_timezone(&Utc));
    }
}

pub fn format_local(dt: &DateTime<Local>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Micros, false)
}

pub fn format_utc(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Accepts RFC 3339 (any offset) or a naive `YYYY-MM-DD[ T]HH:MM:SS[.f]`
/// interpreted in the machine's local zone.
pub fn parse_any_time(raw: &str) -> Option<DateTime<FixedOffset>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt);
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            if let Some(local) = naive.and_local_timezone(Local).earliest() {
                return Some(local.fixed_offset());
            }
        }
    }
    None
}

/// IANA name of the machine's zone (e.g. `Asia/Colombo`), or the numeric
/// offset when the OS does not expose one.
pub fn local_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| Local::now().format("%:z").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parse_is_lenient_and_ordered() {
        assert_eq!(Level::parse("WARNING"), Level::Warn);
        assert_eq!(Level::parse("ERROR"), Level::Error);
        assert_eq!(Level::parse("nonsense"), Level::Info);
        assert!(Level::Fatal > Level::Error && Level::Trace < Level::Debug);
    }

    #[test]
    fn local_timestamp_carries_offset() {
        let e = LogEvent::shell("system", "test");
        let dt = DateTime::parse_from_rfc3339(&e.ts).expect("rfc3339");
        assert_eq!(
            dt.offset().local_minus_utc(),
            Local::now().offset().local_minus_utc()
        );
        assert!(e.ts_utc.ends_with('Z'));
    }

    #[test]
    fn normalize_time_accepts_naive_and_utc_inputs() {
        let mut e = LogEvent::shell("system", "test");
        e.ts = "2026-09-15 10:22:01".into();
        e.ts_utc.clear();
        e.normalize_time();
        assert!(DateTime::parse_from_rfc3339(&e.ts).is_ok());
        assert_eq!(e.day(), "2026-09-15");

        let mut u = LogEvent::shell("system", "test");
        u.ts = "2026-09-15T04:52:01.000Z".into();
        u.normalize_time();
        assert_eq!(u.ts_utc, "2026-09-15T04:52:01.000000Z");
    }
}
