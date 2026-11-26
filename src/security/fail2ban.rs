//! Fail2ban management: status detection, banned IP enumeration, unbanning, and auto-install.

use serde::{Deserialize, Serialize};
use std::process::Command;

/// Fail2ban 服务整体运行状态
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fail2banStatus {
    /// 系统是否安装了 fail2ban
    pub installed: bool,
    /// fail2ban-server 守护进程是否在运行
    pub running: bool,
    /// 各个已配置的 Jail 状态
    pub jails: Vec<JailStatus>,
}

/// 单个 Jail (例如 sshd 或 3r-ui) 的运行状态与封禁列表
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JailStatus {
    pub name: String,
    pub enabled: bool,
    pub currently_banned: i64,
    pub total_banned: i64,
    pub banned_ips: Vec<String>,
}

/// 检测系统当前 Fail2ban 的整体状态
pub async fn get_status() -> Fail2banStatus {
    tokio::task::spawn_blocking(get_status_sync)
        .await
        .unwrap_or(Fail2banStatus {
            installed: false,
            running: false,
            jails: Vec::new(),
        })
}

fn get_status_sync() -> Fail2banStatus {
    // 检查 fail2ban-client 是否在 PATH 中
    let client_path = match which_fail2ban() {
        Some(p) => p,
        None => {
            return Fail2banStatus {
                installed: false,
                running: false,
                jails: Vec::new(),
            };
        }
    };

    // 执行 fail2ban-client ping 测试服务运行
    let ping_output = Command::new(&client_path).arg("ping").output();
    let running = match ping_output {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
            s.contains("pong")
        }
        Err(_) => false,
    };

    if !running {
        return Fail2banStatus {
            installed: true,
            running: false,
            jails: Vec::new(),
        };
    }

    // 获取激活的 jail 列表: fail2ban-client status
    let mut jails = Vec::new();
    let jail_names = match Command::new(&client_path).arg("status").output() {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            parse_jail_list(&s)
        }
        Err(_) => Vec::new(),
    };

    for jail in jail_names {
        if let Some(j_status) = get_jail_status_sync(&client_path, &jail) {
            jails.push(j_status);
        }
    }

    Fail2banStatus {
        installed: true,
        running: true,
        jails,
    }
}

/// 解析 `fail2ban-client status` 输出中的 Jail list:
pub fn parse_jail_list(output: &str) -> Vec<String> {
    for line in output.lines() {
        if let Some(pos) = line.find("Jail list:") {
            let list_str = &line[pos + 10..];
            return list_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    Vec::new()
}

/// 获取指定 Jail 的详细状态 (fail2ban-client status <jail>)
fn get_jail_status_sync(client_path: &str, jail: &str) -> Option<JailStatus> {
    let out = Command::new(client_path)
        .args(["status", jail])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(parse_single_jail_status(jail, &s))
}

/// 解析单个 Jail 的输出字符串
pub fn parse_single_jail_status(jail: &str, output: &str) -> JailStatus {
    let mut currently_banned = 0;
    let mut total_banned = 0;
    let mut banned_ips = Vec::new();

    for line in output.lines() {
        if let Some(pos) = line.find("Currently banned:") {
            let val = &line[pos + 17..];
            currently_banned = val.trim().parse().unwrap_or(0);
        } else if let Some(pos) = line.find("Total banned:") {
            let val = &line[pos + 13..];
            total_banned = val.trim().parse().unwrap_or(0);
        } else if let Some(pos) = line.find("Banned IP list:") {
            let val = &line[pos + 15..];
            banned_ips = val
                .split_whitespace()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    JailStatus {
        name: jail.to_string(),
        enabled: true,
        currently_banned,
        total_banned,
        banned_ips,
    }
}

/// 解除指定 Jail 中封禁的 IP (fail2ban-client set <jail> unbanip <ip>)
pub async fn unban_ip(jail: &str, ip: &str) -> Result<(), String> {
    let jail = jail.to_string();
    let ip = ip.to_string();
    tokio::task::spawn_blocking(move || {
        let client_path =
            which_fail2ban().ok_or_else(|| "fail2ban is not installed".to_string())?;

        // 安全检查：校验 jail 与 ip 的字符合法性，防止命令注入
        if !jail
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("Invalid jail name".to_string());
        }
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err("Invalid IP address".to_string());
        }

        let out = Command::new(&client_path)
            .args(["set", &jail, "unbanip", &ip])
            .output()
            .map_err(|e| format!("Failed to execute fail2ban-client: {e}"))?;

        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task spawn error: {e}")))
}

/// 自动安装并配置 Fail2ban (调用系统包管理器)
pub async fn install_fail2ban() -> Result<String, String> {
    tokio::task::spawn_blocking(|| {
        // 由于 3r-ui.service 启用了 systemd 安全加固沙箱 (ProtectSystem=strict)，
        // 当前进程内部无法直接向 /var/lib/dpkg /usr /etc 等系统目录执行 apt-get 写入操作。
        // 因此优先使用 `systemd-run` 创建临时的 host 级别 oneshot service 运行安装指令；
        // 若系统不支持 systemd-run，则回退执行直接安装。
        let has_systemd_run = Command::new("which")
            .arg("systemd-run")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let out = if has_systemd_run {
            Command::new("systemd-run")
                .args([
                    "--unit=3r-ui-install-fail2ban",
                    "--service-type=oneshot",
                    "--wait",
                    "bash",
                    "-c",
                    "DEBIAN_FRONTEND=noninteractive apt-get update -y && DEBIAN_FRONTEND=noninteractive apt-get install -y fail2ban",
                ])
                .output()
                .map_err(|e| format!("systemd-run failed: {e}"))?
        } else {
            Command::new("bash")
                .args([
                    "-c",
                    "DEBIAN_FRONTEND=noninteractive apt-get update -y && DEBIAN_FRONTEND=noninteractive apt-get install -y fail2ban",
                ])
                .output()
                .map_err(|e| format!("apt-get install fail2ban failed: {e}"))?
        };

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let out_str = String::from_utf8_lossy(&out.stdout);
            return Err(format!("{err}\n{out_str}").trim().to_string());
        }

        // 重新启动服务
        let _ = Command::new("systemctl")
            .args(["enable", "fail2ban"])
            .output();
        let _ = Command::new("systemctl")
            .args(["restart", "fail2ban"])
            .output();

        Ok("Fail2ban 安装成功并已自动启动。".to_string())
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task error: {e}")))
}

fn which_fail2ban() -> Option<String> {
    for path in [
        "/usr/bin/fail2ban-client",
        "/usr/local/bin/fail2ban-client",
        "fail2ban-client",
    ] {
        if Command::new("which")
            .arg(path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || std::path::Path::new(path).exists()
        {
            return Some(path.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_jail_list() {
        let output = r#"
Status
|- Number of jail:      2
`- Jail list:   sshd, 3r-ui
"#;
        let jails = parse_jail_list(output);
        assert_eq!(jails, vec!["sshd", "3r-ui"]);
    }

    #[test]
    fn test_parse_single_jail_status() {
        let output = r#"
Status for the jail: sshd
|- Filter
|  |- Currently failed: 1
|  |- Total failed:     5
|  `- Journal matches:  _SYSTEMD_UNIT=sshd.service
`- Actions
   |- Currently banned: 2
   |- Total banned:     3
   `- Banned IP list:   192.168.1.100 10.0.0.50
"#;
        let st = parse_single_jail_status("sshd", output);
        assert_eq!(st.name, "sshd");
        assert_eq!(st.currently_banned, 2);
        assert_eq!(st.total_banned, 3);
        assert_eq!(st.banned_ips, vec!["192.168.1.100", "10.0.0.50"]);
    }
}
