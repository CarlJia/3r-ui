//! xray 访问日志（access log）的解析与持久化。
//!
//! xray 被配置为 `log.access = "@stderr"` 时，每条连接请求都会在 stderr
//! 产生一行输出。`XraySupervisor` 把每一行 stderr 通过 mpsc 通道送进来，
//! 这里解析并把可识别的访问日志行落库到 `connection_logs` 表；无法识别
//! 的行（xray 的 error/warning 内部输出等）直接丢弃，不进入
//! `connection_logs`。
//!
//! xray 的 access log 格式随版本变化，典型形如：
//!   `2026/01/15 10:23:45 from 1.2.3.4:56789 accepted tcp:example.com:443 [inbound-1 -> direct] abc-uuid`
//!   `2026/01/15 10:23:45 from 1.2.3.4:56789 rejected tcp:example.com:443 [user not found] abc-uuid`
//! 解析失败或字段缺失时，对应列落 NULL；原始行始终保存在 `raw_message`。

use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::crypto::session::now_unix;
use crate::error::AppResult;

/// 30 天保留窗口（秒）。清理由 [`prune_loop`] 周期执行。
const RETENTION_SECS: i64 = 30 * 24 * 3600;

/// 一条解析后的访问日志。`uuid` / `src_addr` / `dest` / `reason` 缺失时为
/// `None`；`status` 一定非空（否则就不是访问日志行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAccess {
    pub logged_at: Option<i64>,
    pub uuid: Option<String>,
    pub src_addr: Option<String>,
    pub dest: Option<String>,
    pub status: String,
    pub reason: Option<String>,
}

/// 解析 `YYYY/MM/DD HH:MM:SS[.ffffff]` 格式的 xray 时间戳前缀为 Unix 秒。
/// xray-core 的 stderr logger 启用 `log.Lmicroseconds`，所以真实行末尾
/// 有 `.微秒` 片段；解析失败（如月 13 / 时 25）回退为 `None`，调用方
/// 回退到 `received_at`。xray 写本地时间（`time.Now()`，无 `log.LUTC`），
/// 因此面板必须在 spawn xray 时设 `TZ=UTC`（见 `process.rs`），否则
/// 解析出的 `logged_at` 会带主机 UTC 偏移、让 30 天保留窗口与日期筛选
/// 漂移最多 ±14h。
fn parse_xray_timestamp(line: &str) -> Option<i64> {
    let rest = line.trim_start();
    let (date, after_date) = rest.split_once(' ')?;
    let (time_str, _) = after_date.trim_start().split_once(' ')?;
    let (y, rest) = date.split_once('/')?;
    let (mo, d) = rest.split_once('/')?;
    let year: i32 = y.parse().ok()?;
    let month: u8 = mo.parse().ok()?;
    let day: u8 = d.parse().ok()?;
    let (h, mi_and_s) = time_str.split_once(':')?;
    let (mi, sec_frac) = mi_and_s.split_once(':')?;
    let (sec_str, _) = sec_frac.split_once('.').unwrap_or((sec_frac, "")); // microseconds 以 `.` 起头
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = mi.parse().ok()?;
    let second: u8 = sec_str.parse().ok()?;

    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let t = time::Time::from_hms(hour, minute, second).ok()?;
    Some(
        time::PrimitiveDateTime::new(date, t)
            .assume_utc()
            .unix_timestamp(),
    )
}

/// 解析一条 xray access log 行。返回 `None` 表示不是访问日志行（如
/// xray 的 error/warning 内部输出——那些行没有 `accepted` / `rejected`
/// 关键字）。
///
/// 识别规则（best-effort，具体格式随 xray 版本变化）：行内含
/// `accepted` 或 `rejected` 关键字即视为访问日志行；UUID 优先取
/// `email:<uuid>` 标记，其次取行尾 token；src 取 `from <addr>` 的
/// 地址；dest 取 status 后的 `<proto>:<dest>` 并剥离 proto 前缀；
/// reason 仅在 `rejected` 时取行内 `[...]` 括号内容。
pub fn parse_access_line(line: &str) -> Option<ParsedAccess> {
    // `accepted` / `rejected` 关键字定位。用单词边界（前后是空格或行首尾）
    // 避免误匹配 dest 中包含该子串的情况。
    let (status, after_status) = find_status_keyword(line)?;
    let logged_at = parse_xray_timestamp(line);

    // src: `from <ip:port>` 的地址。`from` 通常紧跟时间戳之后。
    let src_addr = {
        let mut found = None;
        let mut prev: Option<&str> = None;
        for token in line.split_whitespace() {
            if prev == Some("from") {
                found = Some(token.to_string());
                break;
            }
            prev = Some(token);
        }
        found
    };

    // dest: status 后的第一个 token，形如 `tcp:example.com:443` 或
    // `example.com:443`；剥离 `tcp:` / `udp:` proto 前缀。
    let dest = after_status
        .split_whitespace()
        .next()
        .map(|t| {
            t.split_once(':')
                .filter(|(proto, _)| matches!(*proto, "tcp" | "udp"))
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| t.to_string())
        })
        .filter(|s| !s.is_empty());

    // uuid: 优先 `email:<uuid>` 标记；否则取行尾 token（xray 的 access log
    // 把 email 放在行尾）。
    let uuid = line
        .split_whitespace()
        .find_map(|t| t.strip_prefix("email:").map(|s| s.to_string()))
        .or_else(|| {
            line.split_whitespace()
                .next_back()
                .filter(|t| looks_like_uuid(t))
                .map(|t| t.to_string())
        });

    // reason: 仅在 rejected 时取 `[...]` 括号内容（accepted 时 `[...]` 是
    // detour 标签，不是 reason）。
    let reason = if status == "rejected" {
        after_status
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    Some(ParsedAccess {
        logged_at,
        uuid,
        src_addr,
        dest,
        status: status.to_string(),
        reason,
    })
}

/// 轻量判断：一行 stderr 是否是 access log 行（含 `accepted` / `rejected`
/// 关键字）。`XraySupervisor` 用它把 access 行的面板日志降级到 debug，
/// error/warning 行保持 warn 可见性。
pub fn is_access_log_line(line: &str) -> bool {
    find_status_keyword(line).is_some()
}

/// 在行内定位 `accepted` / `rejected` 关键字（以空格或行首尾为界）。
/// 返回 `(status, status 之后剩余的行文本)`。
fn find_status_keyword(line: &str) -> Option<(&str, &str)> {
    for keyword in ["accepted", "rejected"] {
        if let Some(idx) = line.find(keyword) {
            let before_ok = idx == 0 || line.as_bytes()[idx - 1] == b' ';
            let after_idx = idx + keyword.len();
            let after_ok = after_idx == line.len() || line.as_bytes()[after_idx] == b' ';
            if before_ok && after_ok {
                return Some((&line[idx..after_idx], &line[after_idx..]));
            }
        }
    }
    None
}

/// 宽松的 UUID 形状判断：8-4-4-4-12 的 hex 段。xray 把 email 写在行尾；
/// 我们在 VLESS client 里把 email 设为 UUID，所以行尾 token 若长得像
/// UUID 就是它。
fn looks_like_uuid(token: &str) -> bool {
    let parts: Vec<&str> = token.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(lens)
        .all(|(p, len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 把一批解析结果在单个事务内落库，摊销 fsync 与写锁开销（访问日志
/// 高频时逐行 INSERT 会让每次写入都独占数据库）。
pub async fn persist_batch(pool: &SqlitePool, batch: &[(ParsedAccess, String)]) -> AppResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let now = now_unix();
    let mut tx = pool
        .begin()
        .await
        .map_err(crate::error::AppError::Database)?;
    for (parsed, raw) in batch {
        sqlx::query(
            "INSERT INTO connection_logs \
                (logged_at, uuid, src_addr, dest, status, reason, raw_message, received_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(parsed.logged_at.unwrap_or(now))
        .bind(parsed.uuid.clone().unwrap_or_default())
        .bind(parsed.src_addr.clone())
        .bind(parsed.dest.clone())
        .bind(&parsed.status)
        .bind(parsed.reason.clone())
        .bind(raw)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(crate::error::AppError::Database)?;
    }
    tx.commit()
        .await
        .map_err(crate::error::AppError::Database)?;
    Ok(())
}

/// 消费 stderr 行通道：攒批解析并落库。批触发条件：攒满 [`BATCH_SIZE`]
/// 行，或通道暂时无更多可读行（try_recv 清空即冲刷）。通道关闭（面板
/// shutdown）时退出循环。
const BATCH_SIZE: usize = 50;

pub async fn run_log_line_loop(pool: SqlitePool, mut rx: mpsc::Receiver<String>) {
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };
        let mut lines = vec![first];
        while lines.len() < BATCH_SIZE {
            match rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(_) => break,
            }
        }

        let batch: Vec<(ParsedAccess, String)> = lines
            .into_iter()
            .filter_map(|line| parse_access_line(&line).map(|parsed| (parsed, line)))
            .collect();
        if batch.is_empty() {
            continue;
        }
        if let Err(e) = persist_batch(&pool, &batch).await {
            tracing::warn!(error = %e, count = batch.len(), "connection log batch insert failed");
        }
    }
}

/// 删除 `logged_at` 早于 `threshold_unix` 的所有行；返回删除行数。
pub async fn prune_older_than(pool: &SqlitePool, threshold_unix: i64) -> AppResult<u64> {
    let res = sqlx::query("DELETE FROM connection_logs WHERE logged_at < ?")
        .bind(threshold_unix)
        .execute(pool)
        .await
        .map_err(crate::error::AppError::Database)?;
    Ok(res.rows_affected())
}

/// 后台清理任务：每小时一次，删除 30 天前的条目。与 session cleanup
/// 的节奏一致；面板 shutdown 时由调用方 `.abort()`。
pub async fn prune_loop(pool: SqlitePool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let threshold = now_unix() - RETENTION_SECS;
        match prune_older_than(&pool, threshold).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(deleted = n, "pruned expired connection logs"),
            Err(e) => tracing::warn!(error = %e, "connection log prune failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_accepted_vless_line() {
        let line = "2026/01/15 10:23:45 from 1.2.3.4:56789 accepted tcp:example.com:443 \
                    [inbound-1 -> direct] 01234567-89ab-cdef-0123-456789abcdef";
        let p = parse_access_line(line).expect("access line");
        assert_eq!(p.status, "accepted");
        assert_eq!(
            p.uuid.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(p.src_addr.as_deref(), Some("1.2.3.4:56789"));
        assert_eq!(p.dest.as_deref(), Some("example.com:443"));
        assert_eq!(p.reason, None);
        assert!(p.logged_at.is_some());
    }

    #[test]
    fn parses_rejected_line_with_reason() {
        let line = "2026/01/15 10:23:45 from 10.0.0.1:443 rejected tcp:example.com:443 \
                    [user not found] 01234567-89ab-cdef-0123-456789abcdef";
        let p = parse_access_line(line).expect("access line");
        assert_eq!(p.status, "rejected");
        assert_eq!(p.reason.as_deref(), Some("user not found"));
        assert_eq!(
            p.uuid.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
    }

    #[test]
    fn parses_email_prefixed_uuid() {
        let line = "2026/01/15 10:23:45 from 1.2.3.4:1 accepted tcp:a.com:443 [x -> y] \
                    email:abc-uuid";
        let p = parse_access_line(line).expect("access line");
        assert_eq!(p.uuid.as_deref(), Some("abc-uuid"));
    }

    #[test]
    fn drops_error_log_lines() {
        // xray 内部 error/warning 行没有 accepted/rejected 关键字。
        assert!(parse_access_line("2026/01/15 10:23:45 [Warning] failed to read config").is_none());
        assert!(parse_access_line("Xray 1.8.4 started").is_none());
    }

    #[test]
    fn uuid_extraction_falls_back_to_none_for_non_uuid_tail() {
        let line = "2026/01/15 10:23:45 from 1.2.3.4:1 accepted tcp:a.com:443 [x -> y] -";
        let p = parse_access_line(line).expect("access line");
        assert_eq!(p.uuid, None);
    }

    #[test]
    fn timestamp_parses_civil_datetime() {
        // 2026-01-15 10:23:45 UTC = 1768472625
        let ts = parse_xray_timestamp("2026/01/15 10:23:45 rest").expect("timestamp");
        assert_eq!(ts, 1768472625);
    }

    #[test]
    fn timestamp_parses_microsecond_suffix() {
        // xray-core 的 stderr logger 启用 `log.Lmicroseconds`，真实行
        // 末尾有 `.微秒` 片段；解析应忽略它而不是当作坏数据拒绝。
        let ts =
            parse_xray_timestamp("2026/01/15 10:23:45.123456 rest").expect("with microseconds");
        assert_eq!(ts, 1768472625);
    }
}
