use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemSpecs {
    pub os: String,
    pub arch: String,
    pub cpu_cores: usize,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskIoResult {
    pub write_speed_mb_s: f64,
    pub read_speed_mb_s: f64,
    pub duration_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeComputeResult {
    pub single_thread_ops_sec: f64,
    pub multi_thread_ops_sec: f64,
    pub speedup_factor: f64,
    pub cores_used: usize,
}

/// Returns basic host system specifications.
#[tauri::command]
pub fn get_system_specs(app: tauri::AppHandle) -> SystemSpecs {
    let call = crate::logging::CommandLog::start("get_system_specs", serde_json::json!({}));
    let specs = system_specs(&app);
    call.ok(&specs);
    specs
}

fn system_specs(app: &tauri::AppHandle) -> SystemSpecs {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    SystemSpecs {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_cores,
        app_version: app.package_info().version.to_string(),
    }
}

/// Measures sequential disk write and read speed using an 8 MB temporary buffer.
#[tauri::command]
pub async fn benchmark_disk_io() -> Result<DiskIoResult, String> {
    let call = crate::logging::CommandLog::start("benchmark_disk_io", serde_json::json!({}));
    call.finish(disk_io().await)
}

async fn disk_io() -> Result<DiskIoResult, String> {
    tokio::task::spawn_blocking(move || {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join(format!("jana2u_benchmark_{}.tmp", std::process::id()));

        let size_bytes = 8 * 1024 * 1024; // 8 MB
        let data = vec![0x55u8; size_bytes];

        let t_start = Instant::now();

        // 1. Write Test
        let t_write = Instant::now();
        {
            let mut file = File::create(&test_file).map_err(|e| e.to_string())?;
            file.write_all(&data).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        let write_dur = t_write.elapsed().as_secs_f64();
        let write_speed = (size_bytes as f64 / (1024.0 * 1024.0)) / write_dur.max(0.0001);

        // 2. Read Test
        let t_read = Instant::now();
        {
            let mut file = OpenOptions::new()
                .read(true)
                .open(&test_file)
                .map_err(|e| e.to_string())?;
            let mut buffer = vec![0u8; size_bytes];
            file.read_exact(&mut buffer).map_err(|e| e.to_string())?;
        }
        let read_dur = t_read.elapsed().as_secs_f64();
        let read_speed = (size_bytes as f64 / (1024.0 * 1024.0)) / read_dur.max(0.0001);

        // Cleanup
        let _ = fs::remove_file(test_file);

        let total_dur_ms = t_start.elapsed().as_secs_f64() * 1000.0;

        Ok(DiskIoResult {
            write_speed_mb_s: (write_speed * 10.0).round() / 10.0,
            read_speed_mb_s: (read_speed * 10.0).round() / 10.0,
            duration_ms: (total_dur_ms * 10.0).round() / 10.0,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Evaluates single-core vs multi-core mathematical compute throughput and speedup.
#[tauri::command]
pub async fn benchmark_native_compute() -> Result<NativeComputeResult, String> {
    let call = crate::logging::CommandLog::start("benchmark_native_compute", serde_json::json!({}));
    call.finish(native_compute().await)
}

async fn native_compute() -> Result<NativeComputeResult, String> {
    tokio::task::spawn_blocking(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        fn workload(iterations: u64) -> u64 {
            let mut count = 0;
            for i in 2..iterations {
                let mut is_prime = true;
                let limit = (i as f64).sqrt() as u64;
                for d in 2..=limit {
                    if i % d == 0 {
                        is_prime = false;
                        break;
                    }
                }
                if is_prime {
                    count += 1;
                }
            }
            count
        }

        // Single-core run
        let iters = 25000;
        let t_single = Instant::now();
        let _ = workload(iters);
        let single_dur = t_single.elapsed().as_secs_f64();
        let single_ops = iters as f64 / single_dur.max(0.0001);

        // Multi-core run
        let t_multi = Instant::now();
        let handles: Vec<_> = (0..cores)
            .map(|_| std::thread::spawn(move || workload(iters)))
            .collect();
        for h in handles {
            let _ = h.join();
        }
        let multi_dur = t_multi.elapsed().as_secs_f64();
        let total_work = (iters * cores as u64) as f64;
        let multi_ops = total_work / multi_dur.max(0.0001);

        let speedup = multi_ops / single_ops.max(0.0001);

        Ok(NativeComputeResult {
            single_thread_ops_sec: single_ops.round(),
            multi_thread_ops_sec: multi_ops.round(),
            speedup_factor: (speedup * 10.0).round() / 10.0,
            cores_used: cores,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}
