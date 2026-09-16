//! Runs the desktop log pipeline outside Tauri, for the command-line logging
//! benchmark (`scripts/bench-logging.sh`).
//!
//! It reads a sidecar's stdout on its own stdin, normalizes each line with the
//! same `logging::ingest` the shell uses, and writes through the same hub and
//! writer — so the measured CPU, memory and disk are the real pipeline's, just
//! without a window.
//!
//! Usage: `jana2u-backend | log_pipe --source backend --dir /tmp/logs`
//!
//! Every line is also echoed to stdout, so the caller can keep the raw output.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use app_lib::logging::hub;
use app_lib::logging::ingest::{self, Stream};

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(current) = args.next() {
        if current == name {
            return args.next();
        }
        if let Some(value) = current.strip_prefix(&format!("{name}=")) {
            return Some(value.to_string());
        }
    }
    None
}

fn main() {
    let source = arg("--source").unwrap_or_else(|| "sidecar".to_string());
    let dir = PathBuf::from(arg("--dir").unwrap_or_else(|| {
        eprintln!("log_pipe: --dir <logs dir> is required");
        std::process::exit(2);
    }));

    let hub = hub::init();
    hub.attach_dir(dir, env!("CARGO_PKG_VERSION"));

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    let mut reader = stdin.lock();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // the sidecar closed its stdout
            Ok(_) => {
                let _ = stdout.write_all(line.as_bytes());
                if let Some(event) =
                    ingest::from_sidecar_line(&source, Stream::Stdout, line.as_bytes())
                {
                    event.emit();
                }
            }
            Err(err) => {
                eprintln!("log_pipe: read error: {err}");
                break;
            }
        }
    }

    let _ = stdout.flush();
    hub.flush_blocking(Duration::from_secs(5));
    let counters = hub.counters();
    // The benchmark script parses this line.
    println!(
        "log_pipe_summary source={source} events={} bytes={} dropped={}",
        counters.events_written, counters.bytes_written, counters.dropped
    );
}
