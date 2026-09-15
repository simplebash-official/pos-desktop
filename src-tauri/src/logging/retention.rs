// Retention: logs are never deleted (shop owner's decision). Days older than
// `COMPRESS_AFTER_DAYS` are gzipped in place to keep disk use sane, and files
// dropped into `logs/inbox/` by external producers (the Windows installer)
// are ingested once and removed.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use chrono::{Local, NaiveDate};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::Value;

use super::event::{Level, LogEvent};
use super::ingest;

pub const COMPRESS_AFTER_DAYS: i64 = 7;

pub fn parse_day(name: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()
}

/// gzip every plain `.jsonl` in day folders older than the threshold.
pub fn compact_old_days(logs_dir: &Path) {
    let today = Local::now().date_naive();
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = parse_day(&name) else {
            continue;
        };
        if (today - day).num_days() <= COMPRESS_AFTER_DAYS {
            continue;
        }
        let Ok(files) = fs::read_dir(entry.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Err(err) = gzip_file(&path) {
                LogEvent::shell("system", "log.compress_failed")
                    .level(Level::Warn)
                    .msg(format!("could not compress {}: {err}", path.display()))
                    .emit();
            }
        }
    }
}

fn gzip_file(path: &Path) -> std::io::Result<()> {
    let gz_path = path.with_extension("jsonl.gz");
    let tmp_path = path.with_extension("jsonl.gz.tmp");
    {
        let mut input = File::open(path)?;
        let mut encoder = GzEncoder::new(File::create(&tmp_path)?, Compression::default());
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?.flush()?;
    }
    fs::rename(&tmp_path, &gz_path)?;
    fs::remove_file(path)
}

/// Read and remove every `inbox/*.jsonl`. The file-name prefix before the
/// first `-` is the default source (`installer-1726…jsonl` → `installer`).
pub fn drain_inbox(logs_dir: &Path) -> Vec<LogEvent> {
    let inbox = logs_dir.join("inbox");
    let mut events = Vec::new();
    let Ok(entries) = fs::read_dir(&inbox) else {
        return events;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();
    for path in paths {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("inbox");
        let default_source = stem.split('-').next().unwrap_or("inbox").to_string();
        let Ok(file) = File::open(&path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event = match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Object(obj)) => {
                    let source = obj
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or(&default_source)
                        .to_string();
                    ingest::from_json(&source, obj)
                }
                _ => LogEvent::new(&default_source, "system", "inbox.raw").msg(trimmed.to_string()),
            };
            events.push(event);
        }
        let _ = fs::remove_file(&path);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::hub::random_hex;
    use std::io::Read;

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jana2u-retention-{}", random_hex(8)));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn compresses_only_old_days() {
        let dir = tmp_dir();
        let old = (Local::now() - chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string();
        let today = Local::now().format("%Y-%m-%d").to_string();
        for day in [&old, &today] {
            fs::create_dir_all(dir.join(day)).unwrap();
            fs::write(dir.join(day).join("shell.jsonl"), "{\"a\":1}\n").unwrap();
        }
        compact_old_days(&dir);
        assert!(dir.join(&old).join("shell.jsonl.gz").exists());
        assert!(!dir.join(&old).join("shell.jsonl").exists());
        assert!(dir.join(&today).join("shell.jsonl").exists());

        let mut out = String::new();
        flate2::read::GzDecoder::new(File::open(dir.join(&old).join("shell.jsonl.gz")).unwrap())
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "{\"a\":1}\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn inbox_is_ingested_and_removed() {
        let dir = tmp_dir();
        fs::create_dir_all(dir.join("inbox")).unwrap();
        fs::write(
            dir.join("inbox").join("installer-1.jsonl"),
            "{\"ts\":\"2026-09-15 10:00:00\",\"category\":\"lifecycle\",\"event\":\"install.start\"}\nnot json\n",
        )
        .unwrap();
        let events = drain_inbox(&dir);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, "installer");
        assert_eq!(events[0].event, "install.start");
        assert_eq!(events[1].event, "inbox.raw");
        assert!(!dir.join("inbox").join("installer-1.jsonl").exists());
        let _ = fs::remove_dir_all(dir);
    }
}
