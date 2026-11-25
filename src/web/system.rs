//! System resource metrics: CPU, memory, disk, network I/O, load average, uptime.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;

use super::middleware::CurrentAdmin;
use super::AppState;

/// `GET /admin/api/system-status` — return JSON view of system resource usage
pub async fn system_status_api(
    State(state): State<AppState>,
    Extension(_admin): Extension<CurrentAdmin>,
) -> Response {
    let metrics = state.system_tracker.collect(&state.config.data_dir);
    (StatusCode::OK, axum::Json(metrics)).into_response()
}

/// 实时系统运行监控指标
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub os_name: String,
    pub arch: String,
    pub hostname: String,
    pub uptime_seconds: u64,
    pub uptime_formatted: String,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub load_formatted: String,
    pub cpu_percent: f64,
    pub cpu_cores: usize,
    pub mem_total_bytes: u64,
    pub mem_used_bytes: u64,
    pub mem_percent: f64,
    pub mem_total_formatted: String,
    pub mem_used_formatted: String,
    pub disk_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_percent: f64,
    pub disk_total_formatted: String,
    pub disk_used_formatted: String,
    pub net_in_speed_bytes: u64,
    pub net_out_speed_bytes: u64,
    pub net_in_speed_formatted: String,
    pub net_out_speed_formatted: String,
    pub net_total_in_bytes: u64,
    pub net_total_out_bytes: u64,
    pub net_total_in_formatted: String,
    pub net_total_out_formatted: String,
    pub app_panel_mem_bytes: u64,
    pub app_panel_mem_formatted: String,
    pub app_xray_mem_bytes: u64,
    pub app_xray_mem_formatted: String,
    pub app_mieru_mem_bytes: u64,
    pub app_mieru_mem_formatted: String,
    pub app_total_mem_bytes: u64,
    pub app_total_mem_formatted: String,
}

pub struct SystemTracker {
    last_cpu: Mutex<Option<(Instant, u64, u64)>>,
    last_net: Mutex<Option<(Instant, u64, u64)>>,
}

impl Default for SystemTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemTracker {
    pub fn new() -> Self {
        Self {
            last_cpu: Mutex::new(None),
            last_net: Mutex::new(None),
        }
    }

    /// 收集当前系统的所有资源与网络状态
    pub fn collect(&self, data_dir: &Path) -> SystemMetrics {
        let now = Instant::now();
        let cpu_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        // 1. 系统负载 Load Average
        let mut loads = [0.0f64; 3];
        #[cfg(unix)]
        unsafe {
            libc::getloadavg(loads.as_mut_ptr(), 3);
        }
        let (load_1, load_5, load_15) = (
            (loads[0] * 100.0).round() / 100.0,
            (loads[1] * 100.0).round() / 100.0,
            (loads[2] * 100.0).round() / 100.0,
        );
        let load_formatted = format!("{:.2}, {:.2}, {:.2}", load_1, load_5, load_15);

        // 2. 运行时间 Uptime
        let uptime_seconds = get_uptime();
        let uptime_formatted = format_uptime(uptime_seconds);

        // 3. 内存使用量
        let (mem_total_bytes, mem_used_bytes) = get_memory();
        let mem_percent = if mem_total_bytes > 0 {
            let p = (mem_used_bytes as f64 / mem_total_bytes as f64) * 100.0;
            (p * 10.0).round() / 10.0
        } else {
            0.0
        };

        // 4. 硬盘使用量
        let (disk_total_bytes, disk_used_bytes) = get_disk(data_dir);
        let disk_percent = if disk_total_bytes > 0 {
            let p = (disk_used_bytes as f64 / disk_total_bytes as f64) * 100.0;
            (p * 10.0).round() / 10.0
        } else {
            0.0
        };

        // 5. CPU 使用率
        let cpu_ticks = get_cpu_ticks();
        let cpu_percent = {
            let mut guard = self.last_cpu.lock().unwrap();
            match (*guard, cpu_ticks) {
                (Some((prev_time, prev_idle, prev_total)), Some((curr_idle, curr_total))) => {
                    *guard = Some((now, curr_idle, curr_total));
                    let dt = now.duration_since(prev_time).as_secs_f64();
                    let d_idle = curr_idle.saturating_sub(prev_idle);
                    let d_total = curr_total.saturating_sub(prev_total);
                    if dt > 0.1 && d_total > 0 {
                        let usage = 100.0 * (1.0 - (d_idle as f64 / d_total as f64));
                        (usage.clamp(0.0, 100.0) * 10.0).round() / 10.0
                    } else {
                        // Fallback: estimate from 1m load average
                        ((load_1 / cpu_cores as f64) * 100.0).clamp(0.0, 100.0)
                    }
                }
                (_, Some((curr_idle, curr_total))) => {
                    *guard = Some((now, curr_idle, curr_total));
                    ((load_1 / cpu_cores as f64) * 100.0).clamp(0.0, 100.0)
                }
                _ => ((load_1 / cpu_cores as f64) * 100.0).clamp(0.0, 100.0),
            }
        };

        // 6. 网络吞吐与速率
        let (net_total_in_bytes, net_total_out_bytes) = get_net_bytes();
        let (net_in_speed_bytes, net_out_speed_bytes) = {
            let mut guard = self.last_net.lock().unwrap();
            match *guard {
                Some((prev_time, prev_rx, prev_tx)) => {
                    *guard = Some((now, net_total_in_bytes, net_total_out_bytes));
                    let dt = now.duration_since(prev_time).as_secs_f64();
                    if dt > 0.1 {
                        let rx_rate =
                            (net_total_in_bytes.saturating_sub(prev_rx) as f64 / dt) as u64;
                        let tx_rate =
                            (net_total_out_bytes.saturating_sub(prev_tx) as f64 / dt) as u64;
                        (rx_rate, tx_rate)
                    } else {
                        (0, 0)
                    }
                }
                None => {
                    *guard = Some((now, net_total_in_bytes, net_total_out_bytes));
                    (0, 0)
                }
            }
        };

        // 7. OS 与 Hostname
        let os_name = get_os_pretty_name();
        let arch = std::env::consts::ARCH.to_string();
        let hostname = get_hostname();

        // 8. 程序自身及核心子进程专属内存采集 (3R-UI Panel + Xray)
        let panel_pid = std::process::id() as i32;
        let app_panel_mem_bytes = get_process_rss(panel_pid);

        let pid_file = data_dir.join("xray.pid");
        let app_xray_mem_bytes = if let Ok(content) = std::fs::read_to_string(&pid_file) {
            if let Ok(xray_pid) = content.trim().parse::<i32>() {
                get_process_rss(xray_pid)
            } else {
                0
            }
        } else {
            0
        };

        let mieru_pid_file = data_dir.join("mieru.pid");
        let app_mieru_mem_bytes = if let Ok(content) = std::fs::read_to_string(&mieru_pid_file) {
            if let Ok(mieru_pid) = content.trim().parse::<i32>() {
                get_process_rss(mieru_pid)
            } else {
                0
            }
        } else {
            0
        };

        let app_total_mem_bytes = app_panel_mem_bytes
            .saturating_add(app_xray_mem_bytes)
            .saturating_add(app_mieru_mem_bytes);

        SystemMetrics {
            os_name,
            arch,
            hostname,
            uptime_seconds,
            uptime_formatted,
            load_1,
            load_5,
            load_15,
            load_formatted,
            cpu_percent,
            cpu_cores,
            mem_total_bytes,
            mem_used_bytes,
            mem_percent,
            mem_total_formatted: format_bytes_human(mem_total_bytes),
            mem_used_formatted: format_bytes_human(mem_used_bytes),
            disk_total_bytes,
            disk_used_bytes,
            disk_percent,
            disk_total_formatted: format_bytes_human(disk_total_bytes),
            disk_used_formatted: format_bytes_human(disk_used_bytes),
            net_in_speed_bytes,
            net_out_speed_bytes,
            net_in_speed_formatted: format_rate_human(net_in_speed_bytes),
            net_out_speed_formatted: format_rate_human(net_out_speed_bytes),
            net_total_in_bytes,
            net_total_out_bytes,
            net_total_in_formatted: format_bytes_human(net_total_in_bytes),
            net_total_out_formatted: format_bytes_human(net_total_out_bytes),
            app_panel_mem_bytes,
            app_panel_mem_formatted: format_bytes_human(app_panel_mem_bytes),
            app_xray_mem_bytes,
            app_xray_mem_formatted: format_bytes_human(app_xray_mem_bytes),
            app_mieru_mem_bytes,
            app_mieru_mem_formatted: format_bytes_human(app_mieru_mem_bytes),
            app_total_mem_bytes,
            app_total_mem_formatted: format_bytes_human(app_total_mem_bytes),
        }
    }
}

pub fn format_bytes_human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_rate_human(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes_human(bytes_per_sec))
}

pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}天 {hours}小时 {mins}分")
    } else if hours > 0 {
        format!("{hours}小时 {mins}分")
    } else {
        format!("{mins}分钟")
    }
}

fn get_uptime() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/uptime") {
            if let Some(first) = content.split_whitespace().next() {
                if let Ok(sec) = first.parse::<f64>() {
                    return sec as u64;
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    unsafe {
        let mut boottime = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut size = std::mem::size_of::<libc::timeval>();
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut boottime as *mut _ as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        ) == 0
        {
            let now = libc::time(std::ptr::null_mut());
            if now > boottime.tv_sec {
                return (now - boottime.tv_sec) as u64;
            }
        }
    }

    0
}

fn get_memory() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
            let mut total_kb: u64 = 0;
            let mut avail_kb: u64 = 0;
            let mut free_kb: u64 = 0;
            let mut buffers_kb: u64 = 0;
            let mut cached_kb: u64 = 0;

            for line in content.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
                    if let Ok(num) = val.parse::<u64>() {
                        match key {
                            "MemTotal:" => total_kb = num,
                            "MemAvailable:" => avail_kb = num,
                            "MemFree:" => free_kb = num,
                            "Buffers:" => buffers_kb = num,
                            "Cached:" => cached_kb = num,
                            _ => {}
                        }
                    }
                }
            }

            if total_kb > 0 {
                let available = if avail_kb > 0 {
                    avail_kb
                } else {
                    free_kb + buffers_kb + cached_kb
                };
                let used = total_kb.saturating_sub(available);
                return (total_kb * 1024, used * 1024);
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    unsafe {
        let mut mem_total: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
        let _ = libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut mem_total as *mut _ as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        );

        let mut vm_stat: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::vm_statistics64>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let host = libc::mach_host_self();
        if libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            &mut vm_stat as *mut _ as *mut _,
            &mut count,
        ) == libc::KERN_SUCCESS
        {
            let page_size = libc::sysconf(libc::_SC_PAGESIZE) as u64;
            let free = (vm_stat.free_count as u64) * page_size;
            let used = mem_total.saturating_sub(free);
            return (mem_total, used);
        }
    }

    (0, 0)
}

fn get_disk(data_dir: &Path) -> (u64, u64) {
    let check_path = if data_dir.exists() {
        data_dir
    } else {
        Path::new("/")
    };
    let c_path = std::ffi::CString::new(check_path.to_string_lossy().as_bytes())
        .unwrap_or_else(|_| std::ffi::CString::new("/").unwrap());

    #[cfg(unix)]
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
            let block_size = if stat.f_frsize > 0 {
                stat.f_frsize
            } else {
                stat.f_bsize
            } as u64;
            let total = stat.f_blocks as u64 * block_size;
            let free = stat.f_bavail as u64 * block_size;
            let used = total.saturating_sub(free);
            return (total, used);
        }
    }

    (0, 0)
}

fn get_cpu_ticks() -> Option<(u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/stat") {
            if let Some(line) = content.lines().find(|l| l.starts_with("cpu ")) {
                let parts: Vec<u64> = line
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if parts.len() >= 4 {
                    let user = parts[0];
                    let nice = parts[1];
                    let system = parts[2];
                    let idle = parts[3];
                    let iowait = parts.get(4).copied().unwrap_or(0);
                    let irq = parts.get(5).copied().unwrap_or(0);
                    let softirq = parts.get(6).copied().unwrap_or(0);
                    let steal = parts.get(7).copied().unwrap_or(0);

                    let idle_time = idle + iowait;
                    let total_time = user + nice + system + idle + iowait + irq + softirq + steal;
                    return Some((idle_time, total_time));
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    unsafe {
        let mut cpu_load: libc::host_cpu_load_info = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::host_cpu_load_info>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let host = libc::mach_host_self();
        if libc::host_statistics64(
            host,
            libc::HOST_CPU_LOAD_INFO,
            &mut cpu_load as *mut _ as *mut _,
            &mut count,
        ) == libc::KERN_SUCCESS
        {
            let user = cpu_load.cpu_ticks[libc::CPU_STATE_USER as usize] as u64;
            let system = cpu_load.cpu_ticks[libc::CPU_STATE_SYSTEM as usize] as u64;
            let idle = cpu_load.cpu_ticks[libc::CPU_STATE_IDLE as usize] as u64;
            let nice = cpu_load.cpu_ticks[libc::CPU_STATE_NICE as usize] as u64;
            let idle_time = idle;
            let total_time = user + system + idle + nice;
            return Some((idle_time, total_time));
        }
    }

    None
}

fn get_net_bytes() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/net/dev") {
            let mut total_rx: u64 = 0;
            let mut total_tx: u64 = 0;

            for line in content.lines().skip(2) {
                if let Some((iface, data)) = line.split_once(':') {
                    if iface.trim() == "lo" {
                        continue;
                    }
                    let parts: Vec<&str> = data.split_whitespace().collect();
                    if parts.len() >= 9 {
                        if let (Ok(rx), Ok(tx)) = (parts[0].parse::<u64>(), parts[8].parse::<u64>())
                        {
                            total_rx = total_rx.saturating_add(rx);
                            total_tx = total_tx.saturating_add(tx);
                        }
                    }
                }
            }
            return (total_rx, total_tx);
        }
    }

    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
            let mut total_rx: u64 = 0;
            let mut total_tx: u64 = 0;
            let mut curr = ifap;
            while !curr.is_null() {
                let ifa = *curr;
                if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family == libc::AF_LINK as u8 {
                    let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                    if !name.starts_with("lo") && !ifa.ifa_data.is_null() {
                        let ifd = ifa.ifa_data as *const libc::if_data;
                        total_rx = total_rx.saturating_add((*ifd).ifi_ibytes as u64);
                        total_tx = total_tx.saturating_add((*ifd).ifi_obytes as u64);
                    }
                }
                curr = ifa.ifa_next;
            }
            libc::freeifaddrs(ifap);
            return (total_rx, total_tx);
        }
    }

    (0, 0)
}

fn get_os_pretty_name() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                    return val.trim_matches('"').to_string();
                }
            }
        }
        "Linux".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "macOS".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        std::env::consts::OS.to_string()
    }
}

fn get_hostname() -> String {
    #[cfg(unix)]
    unsafe {
        let mut buf = [0u8; 256];
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            if let Ok(s) = std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char).to_str() {
                return s.to_string();
            }
        }
    }
    "localhost".to_string()
}

/// 获取指定进程的物理常驻内存大小 (RSS，单位为字节)
pub fn get_process_rss(pid: i32) -> u64 {
    if pid <= 0 {
        return 0;
    }

    #[cfg(target_os = "linux")]
    {
        let statm_path = format!("/proc/{pid}/statm");
        if let Ok(content) = std::fs::read_to_string(&statm_path) {
            if let Some(rss_pages) = content
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u64>().ok())
            {
                let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 };
                return rss_pages * page_size;
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if pid == std::process::id() as i32 {
            #[allow(deprecated)]
            unsafe {
                let mut info: libc::mach_task_basic_info = std::mem::zeroed();
                let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
                    / std::mem::size_of::<libc::natural_t>())
                    as libc::mach_msg_type_number_t;
                let task = libc::mach_task_self();
                if libc::task_info(
                    task,
                    libc::MACH_TASK_BASIC_INFO,
                    &mut info as *mut _ as *mut _,
                    &mut count,
                ) == libc::KERN_SUCCESS
                {
                    return info.resident_size;
                }
            }
        } else {
            #[allow(deprecated)]
            unsafe {
                if libc::kill(pid, 0) == 0 {
                    let mut proc_info: libc::proc_taskinfo = std::mem::zeroed();
                    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
                    if libc::proc_pidinfo(
                        pid,
                        libc::PROC_PIDTASKINFO,
                        0,
                        &mut proc_info as *mut _ as *mut _,
                        size,
                    ) == size
                    {
                        return proc_info.pti_resident_size;
                    }
                }
            }
        }
    }

    0
}
