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

    // 优先通过 systemctl is-active fail2ban 判断服务是否处于活跃状态
    let systemctl_active = Command::new("systemctl")
        .args(["is-active", "--quiet", "fail2ban"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // 进一步通过 fail2ban-client ping 测试 daemon 响应
    let ping_output = Command::new(&client_path).arg("ping").output();
    let ping_pong = match ping_output {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
            s.contains("pong")
        }
        Err(_) => false,
    };

    let running = systemctl_active || ping_pong;

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

/// 自动探测系统的 SSH 监听端口 (优先 `sshd -T`，其次读取 `/etc/ssh/sshd_config`，兜底 22)
pub fn detect_ssh_port() -> u16 {
    if let Ok(out) = Command::new("sshd").args(["-T"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            for line in s.lines() {
                let trimmed = line.trim();
                if let Some(port_str) = trimmed.strip_prefix("port ") {
                    if let Ok(port) = port_str.trim().parse::<u16>() {
                        return port;
                    }
                }
            }
        }
    }
    if let Ok(content) = std::fs::read_to_string("/etc/ssh/sshd_config") {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some(port_str) = trimmed.strip_prefix("Port ") {
                if let Ok(port) = port_str.trim().parse::<u16>() {
                    return port;
                }
            }
        }
    }
    22
}

/// 生成 `/etc/fail2ban/jail.local` 配置内容
pub fn generate_jail_config(
    settings: &crate::db::models::SecuritySettings,
    ssh_port: u16,
) -> String {
    let mut ips = vec!["127.0.0.1/8".to_string(), "::1".to_string()];
    for item in settings.whitelist_ips.split([',', ' ']) {
        let trimmed = item.trim();
        if !trimmed.is_empty() && trimmed != "127.0.0.1/8" && trimmed != "::1" {
            ips.push(trimmed.to_string());
        }
    }
    let ignoreip = ips.join(" ");
    let maxretry = settings.maxretry.max(1);
    let findtime = settings.findtime_secs.max(60);
    let bantime = settings.bantime_secs.max(60);
    let ssh_enabled = if settings.is_fail2ban_enabled() && settings.is_ssh_protected() {
        "true"
    } else {
        "false"
    };
    let web_enabled = if settings.is_fail2ban_enabled() && settings.is_web_protected() {
        "true"
    } else {
        "false"
    };

    format!(
        r#"# 3R-UI Fail2ban Configuration
# Automatically managed by 3R-UI.

[DEFAULT]
backend = systemd
ignoreip = {ignoreip}
bantime = {bantime}s
findtime = {findtime}s
maxretry = {maxretry}

[sshd]
enabled = {ssh_enabled}
port = {ssh_port}
backend = systemd
maxretry = {maxretry}
bantime = {bantime}s

[3r-ui]
enabled = {web_enabled}
port = 8443,http,https
filter = 3r-ui
backend = systemd
journalmatch = _SYSTEMD_UNIT=3r-ui.service + SYSLOG_IDENTIFIER=3r-ui
maxretry = {maxretry}
findtime = {findtime}s
bantime = {bantime}s
"#
    )
}

/// 3R-UI 专属 filter 定义内容
pub const FILTER_3R_UI: &str = r#"# Fail2ban filter for 3R-UI Web Console
[Definition]
failregex = ^.*\[SECURITY\] Web login failed for user='.*' from ip='<HOST>'.*$
ignoreregex =
"#;

/// 将当前安全配置同步写入 Fail2ban 配置并重载服务
pub async fn sync_fail2ban_config(
    settings: &crate::db::models::SecuritySettings,
) -> Result<(), String> {
    use base64::Engine;

    let settings = settings.clone();
    tokio::task::spawn_blocking(move || {
        if which_fail2ban().is_none() {
            return Ok(());
        }

        let ssh_port = detect_ssh_port();
        let jail_content = generate_jail_config(&settings, ssh_port);

        let b64_filter = base64::prelude::BASE64_STANDARD.encode(FILTER_3R_UI.as_bytes());
        let b64_jail = base64::prelude::BASE64_STANDARD.encode(jail_content.as_bytes());

        // 由于 3r-ui 运行在 systemd 沙箱 (ProtectSystem=strict) 中，
        // 直接在进程内部写 /etc/fail2ban 会遭遇只读文件系统错误 (os error 30)。
        // 故优先通过 systemd-run 运行 oneshot 命令在 host 级别安全写入配置并重载。
        let sync_cmd = format!(
            "mkdir -p /etc/fail2ban/filter.d /etc/fail2ban/jail.d && \
             echo '{b64_filter}' | base64 -d > /etc/fail2ban/filter.d/3r-ui.conf && \
             rm -f /etc/fail2ban/jail.d/defaults-debian.conf && \
             echo '{b64_jail}' | base64 -d > /etc/fail2ban/jail.local && \
             (fail2ban-client reload || systemctl restart fail2ban)"
        );

        let has_systemd_run = Command::new("which")
            .arg("systemd-run")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let out = if has_systemd_run {
            Command::new("systemd-run")
                .args([
                    "--unit=3r-ui-sync-fail2ban",
                    "--service-type=oneshot",
                    "--wait",
                    "bash",
                    "-c",
                    &sync_cmd,
                ])
                .output()
        } else {
            Command::new("bash").args(["-c", &sync_cmd]).output()
        };

        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                let out_msg = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Err(format!("Failed to sync fail2ban config: {err} {out_msg}")
                    .trim()
                    .to_string())
            }
            Err(e) => Err(format!("Execution failed: {e}")),
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("Task spawn error: {e}")))
}

/// 自动安装并配置 Fail2ban (调用系统包管理器)
pub async fn install_fail2ban() -> Result<String, String> {
    tokio::task::spawn_blocking(|| {
        let ssh_port = detect_ssh_port();

        // 构建初始化安装并在 Debian 12 上修复 backend=systemd 及缺少 rsyslog/iptables 的安装指令
        let install_script = format!(
            r#"set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y fail2ban python3-systemd rsyslog iptables
systemctl enable --now rsyslog 2>/dev/null || true
mkdir -p /etc/fail2ban/filter.d /etc/fail2ban/jail.d /var/run/fail2ban /run/fail2ban

cat <<'EOF' > /etc/fail2ban/filter.d/3r-ui.conf
# Fail2ban filter for 3R-UI Web Console
[Definition]
failregex = ^.*\[SECURITY\] Web login failed for user='.*' from ip='<HOST>'.*$
ignoreregex =
EOF

# 移除 Debian 12 默认自带易导致冲突的 defaults-debian.conf
rm -f /etc/fail2ban/jail.d/defaults-debian.conf

# 写入全局 jail.local (优先级最高，确保 backend=systemd)
cat <<EOF > /etc/fail2ban/jail.local
# 3R-UI Fail2ban Configuration
[DEFAULT]
backend = systemd
ignoreip = 127.0.0.1/8 ::1
bantime = 1d
findtime = 10m
maxretry = 5

[sshd]
enabled = true
port = {ssh_port}
backend = systemd
maxretry = 3
bantime = 1d

[3r-ui]
enabled = true
port = 8443,http,https
filter = 3r-ui
backend = systemd
journalmatch = _SYSTEMD_UNIT=3r-ui.service + SYSLOG_IDENTIFIER=3r-ui
maxretry = 5
findtime = 10m
bantime = 1d
EOF

systemctl daemon-reload
systemctl enable fail2ban
systemctl restart fail2ban
"#
        );

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
                    &install_script,
                ])
                .output()
                .map_err(|e| format!("systemd-run failed: {e}"))?
        } else {
            Command::new("bash")
                .args(["-c", &install_script])
                .output()
                .map_err(|e| format!("install fail2ban failed: {e}"))?
        };

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let out_str = String::from_utf8_lossy(&out.stdout);
            // 抓取失败时的 fail2ban 状态和日志协助定位
            let journal = Command::new("journalctl")
                .args(["-u", "fail2ban", "-n", "20", "--no-pager"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();

            return Err(format!("{err}\n{out_str}\nJournal:\n{journal}")
                .trim()
                .to_string());
        }

        // 校验 fail2ban 服务状态
        let status = Command::new("systemctl")
            .args(["is-active", "--quiet", "fail2ban"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if status {
            Ok(format!(
                "Fail2ban 安装并配置成功，已成功启动守护进程（SSH 端口：{ssh_port}）。"
            ))
        } else {
            let journal = Command::new("journalctl")
                .args(["-u", "fail2ban", "-n", "20", "--no-pager"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            Err(format!(
                "Fail2ban 已安装，但启动失败，日志输出：\n{journal}"
            ))
        }
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

    #[test]
    fn test_generate_jail_config() {
        let settings = crate::db::models::SecuritySettings {
            id: 1,
            fail2ban_enabled: 1,
            ssh_protection: 1,
            web_protection: 1,
            maxretry: 3,
            findtime_secs: 300,
            bantime_secs: 3600,
            whitelist_ips: "1.2.3.4, 5.6.7.8".to_string(),
            updated_at: 0,
        };

        let conf = generate_jail_config(&settings, 2222);
        assert!(conf.contains("backend = systemd"));
        assert!(conf.contains("ignoreip = 127.0.0.1/8 ::1 1.2.3.4 5.6.7.8"));
        assert!(conf.contains("port = 2222"));
        assert!(conf.contains("bantime = 3600s"));
        assert!(conf.contains("maxretry = 3"));
        assert!(conf.contains("[3r-ui]"));
        assert!(conf.contains("enabled = true"));
    }
}
