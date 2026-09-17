// Read side of the log folder for the Settings → Logs viewer: day listing,
// disk stats, filtered queries across sources (plain and gzipped files) and
// ZIP export. Pure filesystem code, no hub state, so it is unit-testable.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::event::Level;
use super::retention::parse_day;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayInfo {
    pub day: String,
    pub sources: Vec<String>,
    pub bytes: u64,
    pub compressed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogStats {
    pub logs_dir: String,
    pub total_bytes: u64,
    pub day_count: usize,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogQuery {
    /// Inclusive `YYYY-MM-DD` bounds; missing = open-ended.
    pub from_day: Option<String>,
    pub to_day: Option<String>,
    /// Empty = all sources.
    pub sources: Vec<String>,
    pub min_level: Option<Level>,
    /// Empty = all categories.
    pub categories: Vec<String>,
    /// Case-insensitive substring over the raw line.
    pub text: Option<String>,
    pub request_id: Option<String>,
    pub boot_id: Option<String>,
    /// Number of newest matches to skip (pagination).
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub entries: Vec<Value>,
    /// `Some(offset)` for the next (older) page, `None` when exhausted.
    pub next_offset: Option<usize>,
}

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;

/// `(source, index)` from a log file name, or `None` for foreign files.
fn parse_file_name(name: &str) -> Option<(String, bool)> {
    let (stem, compressed) = match name.strip_suffix(".jsonl.gz") {
        Some(s) => (s, true),
        None => (name.strip_suffix(".jsonl")?, false),
    };
    // `backend.1` → `backend`
    let source = match stem.rsplit_once('.') {
        Some((base, idx)) if idx.chars().all(|c| c.is_ascii_digit()) => base,
        _ => stem,
    };
    Some((source.to_string(), compressed))
}

/// Day folders, newest first.
pub fn list_days(logs_dir: &Path) -> Vec<DayInfo> {
    let mut days: Vec<DayInfo> = fs::read_dir(logs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let day = entry.file_name().to_string_lossy().into_owned();
            parse_day(&day)?;
            let mut info = DayInfo {
                day,
                sources: vec![],
                bytes: 0,
                compressed: false,
            };
            for file in fs::read_dir(entry.path()).ok()?.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                let Some((source, compressed)) = parse_file_name(&name) else {
                    continue;
                };
                info.bytes += file.metadata().map(|m| m.len()).unwrap_or(0);
                info.compressed |= compressed;
                if !info.sources.contains(&source) {
                    info.sources.push(source);
                }
            }
            info.sources.sort();
            Some(info)
        })
        .collect();
    days.sort_by(|a, b| b.day.cmp(&a.day));
    days
}

pub fn stats(logs_dir: &Path) -> LogStats {
    let days = list_days(logs_dir);
    LogStats {
        logs_dir: logs_dir.to_string_lossy().into_owned(),
        total_bytes: days.iter().map(|d| d.bytes).sum(),
        day_count: days.len(),
        first_day: days.last().map(|d| d.day.clone()),
        last_day: days.first().map(|d| d.day.clone()),
    }
}

fn day_files(logs_dir: &Path, day: &str, sources: &[String]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(logs_dir.join(day))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| {
            let name = f.file_name().to_string_lossy().into_owned();
            let (source, _) = parse_file_name(&name)?;
            (sources.is_empty() || sources.contains(&source)).then(|| f.path())
        })
        .collect();
    files.sort();
    files
}

fn open_lines(path: &Path) -> std::io::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Ok(Box::new(BufReader::new(flate2::read::GzDecoder::new(file))))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

fn matches(line: &str, value: &Value, q: &LogQuery, text_lower: Option<&str>) -> bool {
    if let Some(min) = q.min_level {
        let level = value
            .get("level")
            .and_then(Value::as_str)
            .map(Level::parse)
            .unwrap_or_default();
        if level < min {
            return false;
        }
    }
    if !q.categories.is_empty() {
        let cat = value.get("category").and_then(Value::as_str).unwrap_or("");
        if !q.categories.iter().any(|c| c == cat) {
            return false;
        }
    }
    if let Some(rid) = q.request_id.as_deref().filter(|s| !s.is_empty()) {
        if value.get("request_id").and_then(Value::as_str) != Some(rid) {
            return false;
        }
    }
    if let Some(boot) = q.boot_id.as_deref().filter(|s| !s.is_empty()) {
        if value.get("boot_id").and_then(Value::as_str) != Some(boot) {
            return false;
        }
    }
    if let Some(needle) = text_lower {
        if !line.to_lowercase().contains(needle) {
            return false;
        }
    }
    true
}

/// Newest-first page of matching records. Days are scanned newest to oldest
/// and scanning stops once the page is full, so recent queries stay cheap
/// even with years of history on disk.
pub fn query(logs_dir: &Path, q: &LogQuery) -> LogPage {
    let limit = if q.limit == 0 {
        DEFAULT_LIMIT
    } else {
        q.limit.min(MAX_LIMIT)
    };
    let wanted = q.offset + limit;
    let text_lower = q
        .text
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let mut collected: Vec<Value> = Vec::new();
    let mut exhausted = true;
    for day in list_days(logs_dir) {
        if q.to_day.as_deref().is_some_and(|to| day.day.as_str() > to) {
            continue;
        }
        if q.from_day
            .as_deref()
            .is_some_and(|from| day.day.as_str() < from)
        {
            break;
        }
        let mut day_matches: Vec<Value> = Vec::new();
        for path in day_files(logs_dir, &day.day, &q.sources) {
            let Ok(reader) = open_lines(&path) else {
                continue;
            };
            // A torn final line after a crash fails to parse and is skipped.
            for line in reader.lines().map_while(Result::ok) {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if matches(&line, &value, q, text_lower.as_deref()) {
                    day_matches.push(value);
                }
            }
        }
        day_matches.sort_by_key(|v| std::cmp::Reverse(sort_key(v)));
        collected.extend(day_matches);
        if collected.len() > wanted {
            exhausted = false;
            break;
        }
    }

    let has_more = !exhausted || collected.len() > wanted;
    let entries: Vec<Value> = collected.into_iter().skip(q.offset).take(limit).collect();
    LogPage {
        next_offset: has_more.then_some(q.offset + entries.len()),
        entries,
    }
}

/// Chronological key: UTC time, then boot, then per-boot sequence.
fn sort_key(v: &Value) -> (String, String, u64) {
    (
        v.get("ts_utc")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        v.get("boot_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        v.get("seq").and_then(Value::as_u64).unwrap_or(0),
    )
}

/// Zip the selected day folders (all when both bounds are `None`) plus
/// `logging.json`, preserving the on-disk layout. Returns files written.
pub fn export_zip(
    logs_dir: &Path,
    from_day: Option<&str>,
    to_day: Option<&str>,
    dest: &Path,
) -> Result<usize, String> {
    use zip::write::SimpleFileOptions;

    let file = File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut count = 0;
    let mut buf = Vec::new();
    for day in list_days(logs_dir) {
        if to_day.is_some_and(|to| day.day.as_str() > to)
            || from_day.is_some_and(|from| day.day.as_str() < from)
        {
            continue;
        }
        for path in day_files(logs_dir, &day.day, &[]) {
            let name = format!(
                "{}/{}",
                day.day,
                path.file_name().unwrap().to_string_lossy()
            );
            buf.clear();
            File::open(&path)
                .and_then(|mut f| f.read_to_end(&mut buf))
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            zip.start_file(name, options).map_err(|e| e.to_string())?;
            zip.write_all(&buf).map_err(|e| e.to_string())?;
            count += 1;
        }
    }
    let config = logs_dir.join("logging.json");
    if config.exists() {
        zip.start_file("logging.json", options)
            .map_err(|e| e.to_string())?;
        zip.write_all(&fs::read(config).unwrap_or_default())
            .map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::hub::random_hex;

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("myrologic-store-{}", random_hex(8)));
        for (day, source, lines) in [
            (
                "2026-09-14",
                "backend",
                vec![
                    r#"{"ts_utc":"2026-09-14T01:00:00Z","seq":1,"boot_id":"b1","level":"info","category":"http","event":"request","request_id":"r1"}"#,
                ],
            ),
            (
                "2026-09-15",
                "frontend",
                vec![
                    r#"{"ts_utc":"2026-09-15T01:00:00Z","seq":2,"boot_id":"b2","level":"info","category":"ui","event":"click","msg":"Complete Sale","request_id":"r2"}"#,
                    r#"{"ts_utc":"2026-09-15T01:00:02Z","seq":4,"boot_id":"b2","level":"error","category":"http","event":"response","request_id":"r2"}"#,
                    "{\"torn",
                ],
            ),
            (
                "2026-09-15",
                "backend",
                vec![
                    r#"{"ts_utc":"2026-09-15T01:00:01Z","seq":3,"boot_id":"b2","level":"info","category":"http","event":"request","request_id":"r2"}"#,
                ],
            ),
        ] {
            fs::create_dir_all(dir.join(day)).unwrap();
            let mut body = lines.join("\n");
            body.push('\n');
            let path = dir.join(day).join(format!("{source}.jsonl"));
            let mut existing = fs::read_to_string(&path).unwrap_or_default();
            existing.push_str(&body);
            fs::write(path, existing).unwrap();
        }
        dir
    }

    #[test]
    fn lists_days_newest_first_with_sources() {
        let dir = fixture();
        let days = list_days(&dir);
        assert_eq!(days[0].day, "2026-09-15");
        assert_eq!(days[0].sources, vec!["backend", "frontend"]);
        assert_eq!(stats(&dir).first_day.as_deref(), Some("2026-09-14"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn query_merges_sources_newest_first_and_filters() {
        let dir = fixture();
        let all = query(&dir, &LogQuery::default());
        let seqs: Vec<u64> = all
            .entries
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![4, 3, 2, 1]);
        assert!(all.next_offset.is_none());

        let traced = query(
            &dir,
            &LogQuery {
                request_id: Some("r2".into()),
                ..Default::default()
            },
        );
        assert_eq!(traced.entries.len(), 3);

        let errors = query(
            &dir,
            &LogQuery {
                min_level: Some(Level::Error),
                ..Default::default()
            },
        );
        assert_eq!(errors.entries.len(), 1);

        let text = query(
            &dir,
            &LogQuery {
                text: Some("complete sale".into()),
                ..Default::default()
            },
        );
        assert_eq!(text.entries[0]["event"], "click");

        let page = query(
            &dir,
            &LogQuery {
                limit: 2,
                ..Default::default()
            },
        );
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next_offset, Some(2));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rollover_and_gz_names_parse_to_source() {
        assert_eq!(
            parse_file_name("backend.1.jsonl"),
            Some(("backend".into(), false))
        );
        assert_eq!(
            parse_file_name("document-server.jsonl.gz"),
            Some(("document-server".into(), true))
        );
        assert_eq!(parse_file_name("notes.txt"), None);
    }
}
