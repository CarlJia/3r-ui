//! DDNS runner and background loop.

use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use crate::error::{AppError, AppResult};
use crate::proxy::cloudflare::CloudflareClient;
pub use crate::proxy::cloudflare::DnsSyncOutcome;

/// 触发 DDNS 立即同步的通知器
#[derive(Clone, Debug)]
pub struct DdnsNotifier(pub Arc<Notify>);

impl DdnsNotifier {
    pub fn new() -> Self {
        Self(Arc::new(Notify::new()))
    }

    pub fn notify_sync(&self) {
        self.0.notify_one();
    }
}

impl Default for DdnsNotifier {
    fn default() -> Self {
        Self::new()
    }
}

/// 执行一次完整的 DDNS 同步检查
pub async fn perform_ddns_sync(pool: &SqlitePool) -> AppResult<Vec<DnsSyncOutcome>> {
    let settings = crate::ddns::config::get_ddns_settings(pool).await?;
    if !settings.is_enabled() {
        tracing::debug!("DDNS is disabled, skipping sync");
        return Ok(vec![DnsSyncOutcome::Skipped {
            domain: String::new(),
            reason: "DDNS is disabled in settings".to_string(),
        }]);
    }

    let token = crate::ddns::config::resolve_effective_cf_token(pool, &settings).await?;
    if token.is_empty() {
        let err_msg = "未配置 Cloudflare API Token (且反代配置中无可用 Token)".to_string();
        tracing::warn!("{err_msg}");
        crate::ddns::config::update_ddns_status(
            pool,
            &settings.last_ipv4,
            &settings.last_ipv6,
            &format!("错误: {err_msg}"),
        )
        .await?;
        return Err(AppError::Proxy(err_msg));
    }

    let client = CloudflareClient::new(token);

    // 探测公网 IP
    let mut detected_v4 = None;
    if settings.is_ipv4_enabled() {
        detected_v4 = crate::net::detect_public_ipv4().await;
        if detected_v4.is_none() {
            tracing::warn!("failed to detect public IPv4 address");
        }
    }

    let mut detected_v6 = None;
    if settings.is_ipv6_enabled() {
        detected_v6 = crate::net::detect_public_ipv6().await;
        if detected_v6.is_none() {
            tracing::warn!("failed to detect public IPv6 address");
        }
    }

    let v4_domains = settings.parse_ipv4_domains();
    let v6_domains = settings.parse_ipv6_domains();

    let mut outcomes = Vec::new();
    let mut sync_success_domain = None;

    // 同步 IPv4
    if settings.is_ipv4_enabled() {
        for domain in &v4_domains {
            if let Some(ref ip) = detected_v4 {
                match client
                    .upsert_dns_record(domain, "A", ip, settings.is_cf_proxied())
                    .await
                {
                    Ok(outcome) => {
                        tracing::info!(domain = %domain, outcome = ?outcome, "DDNS IPv4 sync result");
                        if matches!(
                            outcome,
                            DnsSyncOutcome::Created { .. }
                                | DnsSyncOutcome::Updated { .. }
                                | DnsSyncOutcome::Unchanged { .. }
                        ) && sync_success_domain.is_none()
                        {
                            sync_success_domain = Some(domain.clone());
                        }
                        outcomes.push(outcome);
                    }
                    Err(e) => {
                        tracing::error!(domain = %domain, error = %e, "DDNS IPv4 sync failed");
                        outcomes.push(DnsSyncOutcome::Failed {
                            domain: domain.clone(),
                            error: e.to_string(),
                        });
                    }
                }
            } else {
                outcomes.push(DnsSyncOutcome::Failed {
                    domain: domain.clone(),
                    error: "未能探测到本机公网 IPv4 地址".to_string(),
                });
            }
        }
    }

    // 同步 IPv6
    if settings.is_ipv6_enabled() {
        for domain in &v6_domains {
            if let Some(ref ip) = detected_v6 {
                match client
                    .upsert_dns_record(domain, "AAAA", ip, settings.is_cf_proxied())
                    .await
                {
                    Ok(outcome) => {
                        tracing::info!(domain = %domain, outcome = ?outcome, "DDNS IPv6 sync result");
                        if sync_success_domain.is_none() {
                            sync_success_domain = Some(domain.clone());
                        }
                        outcomes.push(outcome);
                    }
                    Err(e) => {
                        tracing::error!(domain = %domain, error = %e, "DDNS IPv6 sync failed");
                        outcomes.push(DnsSyncOutcome::Failed {
                            domain: domain.clone(),
                            error: e.to_string(),
                        });
                    }
                }
            } else {
                outcomes.push(DnsSyncOutcome::Failed {
                    domain: domain.clone(),
                    error: "未能探测到本机公网 IPv6 地址".to_string(),
                });
            }
        }
    }

    // 生成整体状态文本
    let has_failure = outcomes
        .iter()
        .any(|o| matches!(o, DnsSyncOutcome::Failed { .. }));
    let has_update = outcomes.iter().any(|o| {
        matches!(
            o,
            DnsSyncOutcome::Created { .. } | DnsSyncOutcome::Updated { .. }
        )
    });

    let status_summary = if outcomes.is_empty() {
        "未配置任何有效域名".to_string()
    } else if has_failure {
        let first_err = outcomes
            .iter()
            .find_map(|o| match o {
                DnsSyncOutcome::Failed { error, .. } => Some(error.as_str()),
                _ => None,
            })
            .unwrap_or("部分同步失败");
        format!("同步异常: {first_err}")
    } else if has_update {
        "同步成功: DNS 记录已更新".to_string()
    } else {
        "同步成功: IP 未变动 (解析已是最新)".to_string()
    };

    let v4_str = detected_v4
        .as_deref()
        .unwrap_or(if settings.is_ipv4_enabled() {
            ""
        } else {
            &settings.last_ipv4
        });
    let v6_str = detected_v6
        .as_deref()
        .unwrap_or(if settings.is_ipv6_enabled() {
            ""
        } else {
            &settings.last_ipv6
        });

    crate::ddns::config::update_ddns_status(pool, v4_str, v6_str, &status_summary).await?;

    // 自动更新节点连接 Host
    if settings.is_auto_sync_public_host() {
        if let Some(target_host) = sync_success_domain {
            if let Err(e) = crate::net::update_system_settings(pool, &target_host).await {
                tracing::warn!(error = %e, host = %target_host, "failed to auto sync system_settings.public_host");
            } else {
                tracing::info!(host = %target_host, "auto updated system_settings.public_host");
            }
        }
    }

    Ok(outcomes)
}

/// DDNS 异步后台守护循环
pub async fn run_ddns_loop(pool: SqlitePool, notifier: DdnsNotifier) {
    tracing::info!("DDNS background runner started");

    // 启动后稍作等待，让网络和应用初始化完成
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 启动时自动执行一轮同步
    if let Err(e) = perform_ddns_sync(&pool).await {
        tracing::debug!(error = %e, "initial DDNS sync skipped or failed");
    }

    loop {
        // 读取当前设置的间隔
        let interval_mins = match crate::ddns::config::get_ddns_settings(&pool).await {
            Ok(s) => s.check_interval_mins.clamp(1, 1440) as u64,
            Err(_) => 5,
        };

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval_mins * 60)) => {
                if let Err(e) = perform_ddns_sync(&pool).await {
                    tracing::warn!(error = %e, "scheduled DDNS sync failed");
                }
            }
            _ = notifier.0.notified() => {
                tracing::info!("DDNS sync triggered by notifier");
                if let Err(e) = perform_ddns_sync(&pool).await {
                    tracing::warn!(error = %e, "triggered DDNS sync failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_perform_ddns_sync_disabled() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        // Initially disabled
        let outcomes = perform_ddns_sync(&pool).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], DnsSyncOutcome::Skipped { .. }));
    }

    #[tokio::test]
    async fn test_perform_ddns_sync_no_token() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        // Enable DDNS but without token
        crate::ddns::config::update_ddns_settings(
            &pool,
            true,
            Some(""),
            true,
            "vps.example.com",
            false,
            "",
            5,
            false,
            true,
        )
        .await
        .unwrap();

        let result = perform_ddns_sync(&pool).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ddns_notifier() {
        let notifier = DdnsNotifier::new();
        let notified = notifier.0.clone();

        let handle = tokio::spawn(async move {
            notified.notified().await;
            true
        });

        notifier.notify_sync();
        let success = handle.await.unwrap();
        assert!(success);
    }
}
