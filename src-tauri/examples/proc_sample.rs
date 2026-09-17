//! Per-process CPU/memory sampler for the benchmark scripts.
//!
//! `ps -o time=` only resolves CPU to whole seconds, which reports 0.0 for a
//! short benchmark run. This uses the same `sysinfo` accounting as the app's
//! `benchmark_resource_sample` command, so CPU is accurate to a millisecond.
//!
//! Usage:
//!   proc_sample --csv out.csv [--interval-ms 500] backend=123 document-server=456
//!   proc_sample --csv out.csv --match log_pipe=log_pipe --interval-ms 500
//!
//! `--match NAME=SUBSTRING` also tracks processes discovered by command line,
//! so short-lived helpers started after the sampler are still measured (the
//! sampler excludes itself, or it would match its own arguments). `--once`
//! appends a single sample and exits, which the scripts use to take a closing
//! measurement before tearing the run down.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

fn main() {
    let mut csv_path = String::new();
    let mut interval_ms = 500u64;
    let mut targets: Vec<(String, u32)> = Vec::new();
    let mut matchers: Vec<(String, String)> = Vec::new();
    let mut once = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--csv" => csv_path = args.next().unwrap_or_default(),
            "--interval-ms" => {
                interval_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(500);
            }
            "--once" => once = true,
            "--match" => {
                if let Some(pair) = args.next() {
                    if let Some((name, needle)) = pair.split_once('=') {
                        matchers.push((name.to_string(), needle.to_string()));
                    }
                }
            }
            other => {
                if let Some((name, pid)) = other.split_once('=') {
                    if let Ok(pid) = pid.parse::<u32>() {
                        targets.push((name.to_string(), pid));
                    }
                }
            }
        }
    }

    if csv_path.is_empty() {
        eprintln!("proc_sample: --csv <path> is required");
        std::process::exit(2);
    }

    let mut file = match OpenOptions::new()
        .create(true)
        .write(true)
        .append(once)
        .truncate(!once)
        .open(&csv_path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!("proc_sample: cannot write {csv_path}: {err}");
            std::process::exit(1);
        }
    };
    // `--once` appends, so it writes the header only when starting a new file.
    let needs_header = !once
        || std::fs::metadata(&csv_path)
            .map(|meta| meta.len() == 0)
            .unwrap_or(true);
    if needs_header {
        let _ = writeln!(
            file,
            "timestamp_iso,pid,process_name,cpu_seconds,rss_mb,open_fds"
        );
    }
    let self_pid = std::process::id();

    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_cmd(UpdateKind::Always);

    loop {
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        let now = chrono::Local::now().to_rfc3339();

        // Named pids plus anything matching a --match substring, summed per
        // name so several helper processes report as one row.
        let mut rows: BTreeMap<String, (u32, u64, u64)> = BTreeMap::new();
        for (name, pid) in &targets {
            if let Some(process) = system.process(Pid::from_u32(*pid)) {
                rows.insert(
                    name.clone(),
                    (*pid, process.accumulated_cpu_time(), process.memory()),
                );
            }
        }
        for (name, needle) in &matchers {
            let mut cpu_ms = 0u64;
            let mut rss = 0u64;
            let mut first_pid = 0u32;
            for (pid, process) in system.processes() {
                let cmd = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ");
                // Never match ourselves: our own arguments contain the needle.
                if pid.as_u32() != self_pid && cmd.contains(needle) {
                    cpu_ms += process.accumulated_cpu_time();
                    rss += process.memory();
                    if first_pid == 0 {
                        first_pid = pid.as_u32();
                    }
                }
            }
            if first_pid != 0 {
                rows.insert(name.clone(), (first_pid, cpu_ms, rss));
            }
        }

        for (name, (pid, cpu_ms, rss)) in rows {
            let _ = writeln!(
                file,
                "{now},{pid},{name},{:.3},{:.2},0",
                cpu_ms as f64 / 1000.0,
                rss as f64 / (1024.0 * 1024.0)
            );
        }
        let _ = file.flush();
        if once {
            return;
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}
