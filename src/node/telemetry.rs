#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::time::Instant;

use crate::node::protocol::{NodeTelemetry, ServiceStatusItem};

#[allow(dead_code)]
static PREV_CPU: Mutex<Option<(u64, u64, Instant)>> = Mutex::new(None);

pub fn collect_system_telemetry(services: Vec<ServiceStatusItem>) -> NodeTelemetry {
    let (mem_used, mem_total) = read_memory();
    let (disk_used, disk_total) = read_disk("/");
    let cpu_percent = read_cpu();

    NodeTelemetry {
        cpu_percent,
        memory_used_bytes: mem_used,
        memory_total_bytes: mem_total,
        disk_used_bytes: disk_used,
        disk_total_bytes: disk_total,
        services,
    }
}

fn read_memory() -> (i64, i64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(file) = File::open("/proc/meminfo") {
            let reader = BufReader::new(file);
            let mut total_kb: Option<i64> = None;
            let mut avail_kb: Option<i64> = None;

            for line in reader.lines().flatten() {
                if line.starts_with("MemTotal:") {
                    total_kb = parse_kb_line(&line);
                } else if line.starts_with("MemAvailable:") {
                    avail_kb = parse_kb_line(&line);
                }
                if total_kb.is_some() && avail_kb.is_some() {
                    break;
                }
            }

            if let (Some(total), Some(avail)) = (total_kb, avail_kb) {
                let used = (total - avail) * 1024;
                return (used, total * 1024);
            }
        }
    }

    // Default / Mock for non-linux or fallback
    (512 * 1024 * 1024, 2048 * 1024 * 1024)
}

#[allow(dead_code)]
fn parse_kb_line(line: &str) -> Option<i64> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse::<i64>().ok()
    } else {
        None
    }
}

fn read_disk(path: &str) -> (i64, i64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;

        if let Ok(c_path) = CString::new(path) {
            let mut stat = MaybeUninit::<libc::statvfs>::uninit();
            let res = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
            if res == 0 {
                let stat = unsafe { stat.assume_init() };
                let total = (stat.f_blocks as i64) * (stat.f_frsize as i64);
                let free = (stat.f_bavail as i64) * (stat.f_frsize as i64);
                let used = total.saturating_sub(free);
                return (used, total);
            }
        }
    }

    (20 * 1024 * 1024 * 1024, 50 * 1024 * 1024 * 1024)
}

fn read_cpu() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(file) = File::open("/proc/stat") {
            let reader = BufReader::new(file);
            if let Some(Ok(first_line)) = reader.lines().next() {
                if first_line.starts_with("cpu ") {
                    let parts: Vec<u64> = first_line
                        .split_whitespace()
                        .skip(1)
                        .filter_map(|s| s.parse().ok())
                        .collect();

                    if parts.len() >= 4 {
                        let idle = parts[3];
                        let total: u64 = parts.iter().sum();
                        let now = Instant::now();

                        let mut lock = PREV_CPU.lock().unwrap();
                        if let Some((prev_total, prev_idle, _prev_instant)) = *lock {
                            let total_diff = total.saturating_sub(prev_total);
                            let idle_diff = idle.saturating_sub(prev_idle);
                            *lock = Some((total, idle, now));

                            if total_diff > 0 {
                                let usage = (1.0 - (idle_diff as f64 / total_diff as f64)) * 100.0;
                                return usage.clamp(0.0, 100.0);
                            }
                        } else {
                            *lock = Some((total, idle, now));
                        }
                    }
                }
            }
        }
    }

    15.5
}
