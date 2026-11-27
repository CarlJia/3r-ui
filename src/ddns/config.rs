//! Cloudflare DDNS configuration management and database operations.

use sqlx::SqlitePool;

use crate::db::models::DdnsSettings;
use crate::error::{AppError, AppResult};

/// 获取单例 DDNS 配置 (`id = 1`)。若数据库尚未初始化则自动插入默认值。
pub async fn get_ddns_settings(pool: &SqlitePool) -> AppResult<DdnsSettings> {
    let settings = sqlx::query_as::<_, DdnsSettings>(
        r#"
        SELECT id, enabled, cf_api_token, ipv4_enabled, ipv4_domains,
               ipv6_enabled, ipv6_domains, check_interval_mins, cf_proxied,
               auto_sync_public_host, last_ipv4, last_ipv6, last_sync_at,
               last_status, updated_at
        FROM ddns_settings
        WHERE id = 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)?;

    match settings {
        Some(s) => Ok(s),
        None => {
            sqlx::query(
                r#"
                INSERT OR IGNORE INTO ddns_settings (
                    id, enabled, cf_api_token, ipv4_enabled, ipv4_domains,
                    ipv6_enabled, ipv6_domains, check_interval_mins, cf_proxied,
                    auto_sync_public_host, last_ipv4, last_ipv6, last_sync_at, last_status
                ) VALUES (
                    1, 0, '', 1, '',
                    0, '', 5, 0,
                    1, '', '', 0, ''
                )
                "#,
            )
            .execute(pool)
            .await
            .map_err(AppError::Database)?;

            sqlx::query_as::<_, DdnsSettings>(
                r#"
                SELECT id, enabled, cf_api_token, ipv4_enabled, ipv4_domains,
                       ipv6_enabled, ipv6_domains, check_interval_mins, cf_proxied,
                       auto_sync_public_host, last_ipv4, last_ipv6, last_sync_at,
                       last_status, updated_at
                FROM ddns_settings
                WHERE id = 1
                "#,
            )
            .fetch_one(pool)
            .await
            .map_err(AppError::Database)
        }
    }
}

/// 更新 DDNS 配置参数。如果传入的 `cf_api_token` 为 None 或包含掩码 `*`，则保留原 Token。
#[allow(clippy::too_many_arguments)]
pub async fn update_ddns_settings(
    pool: &SqlitePool,
    enabled: bool,
    cf_api_token: Option<&str>,
    ipv4_enabled: bool,
    ipv4_domains: &str,
    ipv6_enabled: bool,
    ipv6_domains: &str,
    check_interval_mins: i64,
    cf_proxied: bool,
    auto_sync_public_host: bool,
) -> AppResult<DdnsSettings> {
    let current = get_ddns_settings(pool).await?;

    let token_to_save = match cf_api_token {
        Some(t) if !t.trim().is_empty() && !t.contains('*') => t.trim().to_string(),
        _ => current.cf_api_token,
    };

    let interval = check_interval_mins.clamp(1, 1440);

    sqlx::query(
        r#"
        UPDATE ddns_settings
        SET enabled = ?,
            cf_api_token = ?,
            ipv4_enabled = ?,
            ipv4_domains = ?,
            ipv6_enabled = ?,
            ipv6_domains = ?,
            check_interval_mins = ?,
            cf_proxied = ?,
            auto_sync_public_host = ?,
            updated_at = unixepoch()
        WHERE id = 1
        "#,
    )
    .bind(if enabled { 1 } else { 0 })
    .bind(token_to_save)
    .bind(if ipv4_enabled { 1 } else { 0 })
    .bind(ipv4_domains.trim())
    .bind(if ipv6_enabled { 1 } else { 0 })
    .bind(ipv6_domains.trim())
    .bind(interval)
    .bind(if cf_proxied { 1 } else { 0 })
    .bind(if auto_sync_public_host { 1 } else { 0 })
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    get_ddns_settings(pool).await
}

/// 更新 DDNS 执行状态记录（IP、时间戳及结果状态）。
pub async fn update_ddns_status(
    pool: &SqlitePool,
    last_ipv4: &str,
    last_ipv6: &str,
    last_status: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE ddns_settings
        SET last_ipv4 = ?,
            last_ipv6 = ?,
            last_sync_at = unixepoch(),
            last_status = ?
        WHERE id = 1
        "#,
    )
    .bind(last_ipv4.trim())
    .bind(last_ipv6.trim())
    .bind(last_status.trim())
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(())
}

/// 解析当前生效的 Cloudflare API Token。
/// 优先使用 DDNS 自身配置的 Token；若未填写，则尝试继承反向代理已配置的 Token。
pub async fn resolve_effective_cf_token(
    pool: &SqlitePool,
    ddns: &DdnsSettings,
) -> AppResult<String> {
    if !ddns.cf_api_token.trim().is_empty() {
        return Ok(ddns.cf_api_token.trim().to_string());
    }

    // 尝试继承反向代理配置中的 Token
    let proxy = crate::proxy::config::get_proxy_settings(pool).await?;
    if !proxy.cf_api_token.trim().is_empty() {
        return Ok(proxy.cf_api_token.trim().to_string());
    }

    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ddns_settings_crud_and_masking() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let initial = get_ddns_settings(&pool).await.unwrap();
        assert_eq!(initial.enabled, 0);
        assert_eq!(initial.check_interval_mins, 5);

        // Update settings with real token
        let updated = update_ddns_settings(
            &pool,
            true,
            Some("sk-my-secret-token-12345"),
            true,
            "vps.example.com",
            false,
            "",
            10,
            false,
            true,
        )
        .await
        .unwrap();

        assert_eq!(updated.enabled, 1);
        assert_eq!(updated.cf_api_token, "sk-my-secret-token-12345");
        assert_eq!(updated.ipv4_domains, "vps.example.com");
        assert_eq!(updated.check_interval_mins, 10);

        // Update with masked token retains original
        let masked_update = update_ddns_settings(
            &pool,
            true,
            Some("sk-***2345"),
            true,
            "vps.example.com",
            false,
            "",
            10,
            false,
            true,
        )
        .await
        .unwrap();

        assert_eq!(masked_update.cf_api_token, "sk-my-secret-token-12345");

        // Update status
        update_ddns_status(&pool, "203.0.113.1", "", "Success")
            .await
            .unwrap();

        let after_status = get_ddns_settings(&pool).await.unwrap();
        assert_eq!(after_status.last_ipv4, "203.0.113.1");
        assert_eq!(after_status.last_status, "Success");
        assert!(after_status.last_sync_at > 0);
    }
}
