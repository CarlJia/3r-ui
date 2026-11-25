//! Database access and helpers for Pingora reverse proxy settings.

use sqlx::SqlitePool;

use crate::db::models::ProxySettings;
use crate::error::{AppError, AppResult};

/// 获取唯一的反向代理配置记录 (id = 1)。若不存在则插入默认值并返回。
pub async fn get_proxy_settings(pool: &SqlitePool) -> AppResult<ProxySettings> {
    let settings = sqlx::query_as::<_, ProxySettings>(
        r#"
        SELECT id, enabled, listen_port, domain, path, sub_path, cert_mode, cf_api_token, cf_zone_id,
               custom_cert_path, custom_key_path, cert_expires_at, last_renewed_at,
               last_error, updated_at
        FROM proxy_settings
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
                INSERT OR IGNORE INTO proxy_settings (id, enabled, listen_port, domain, path, sub_path, cert_mode)
                VALUES (1, 0, 8443, '', '', '/sub', 'cf_dns')
                "#,
            )
            .execute(pool)
            .await
            .map_err(AppError::Database)?;

            sqlx::query_as::<_, ProxySettings>(
                r#"
                SELECT id, enabled, listen_port, domain, path, sub_path, cert_mode, cf_api_token, cf_zone_id,
                       custom_cert_path, custom_key_path, cert_expires_at, last_renewed_at,
                       last_error, updated_at
                FROM proxy_settings
                WHERE id = 1
                "#,
            )
            .fetch_one(pool)
            .await
            .map_err(AppError::Database)
        }
    }
}

/// 更新反代配置参数。如果传入的 `cf_api_token` 为 None 或包含掩码 `*`，则保留原 Token。
#[allow(clippy::too_many_arguments)]
pub async fn update_proxy_settings(
    pool: &SqlitePool,
    enabled: bool,
    listen_port: u16,
    domain: &str,
    path: &str,
    sub_path: &str,
    cert_mode: &str,
    cf_api_token: Option<&str>,
    cf_zone_id: Option<&str>,
    custom_cert_path: &str,
    custom_key_path: &str,
) -> AppResult<ProxySettings> {
    let current = get_proxy_settings(pool).await?;

    let token_to_save = match cf_api_token {
        Some(t) if !t.trim().is_empty() && !t.contains('*') => t.trim().to_string(),
        _ => current.cf_api_token,
    };

    let zone_to_save = cf_zone_id
        .map(|z| z.trim().to_string())
        .unwrap_or(current.cf_zone_id);

    let clean_path = crate::db::models::normalize_panel_path(path);
    let clean_sub_path = crate::db::models::normalize_sub_path(sub_path);

    sqlx::query(
        r#"
        UPDATE proxy_settings
        SET enabled = ?,
            listen_port = ?,
            domain = ?,
            path = ?,
            sub_path = ?,
            cert_mode = ?,
            cf_api_token = ?,
            cf_zone_id = ?,
            custom_cert_path = ?,
            custom_key_path = ?,
            updated_at = unixepoch()
        WHERE id = 1
        "#,
    )
    .bind(if enabled { 1 } else { 0 })
    .bind(listen_port as i64)
    .bind(domain.trim())
    .bind(clean_path)
    .bind(clean_sub_path)
    .bind(cert_mode.trim())
    .bind(token_to_save)
    .bind(zone_to_save)
    .bind(custom_cert_path.trim())
    .bind(custom_key_path.trim())
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    get_proxy_settings(pool).await
}

/// 更新证书签发与到期状态
pub async fn update_cert_status(
    pool: &SqlitePool,
    cert_expires_at: Option<i64>,
    last_renewed_at: Option<i64>,
    last_error: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE proxy_settings
        SET cert_expires_at = COALESCE(?, cert_expires_at),
            last_renewed_at = COALESCE(?, last_renewed_at),
            last_error = ?,
            updated_at = unixepoch()
        WHERE id = 1
        "#,
    )
    .bind(cert_expires_at)
    .bind(last_renewed_at)
    .bind(last_error)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_token_behavior() {
        let mut s = ProxySettings {
            id: 1,
            enabled: 1,
            listen_port: 8443,
            domain: "example.com".into(),
            path: "".into(),
            sub_path: "/sub".into(),
            cert_mode: "cf_dns".into(),
            cf_api_token: "sk-1234567890abcdef".into(),
            cf_zone_id: "".into(),
            custom_cert_path: "".into(),
            custom_key_path: "".into(),
            cert_expires_at: None,
            last_renewed_at: None,
            last_error: None,
            updated_at: 0,
        };
        assert_eq!(s.masked_cf_token(), "sk-****cdef");

        s.cf_api_token = "short".into();
        assert_eq!(s.masked_cf_token(), "********");

        s.cf_api_token = "".into();
        assert_eq!(s.masked_cf_token(), "");
    }

    #[tokio::test]
    async fn db_migrations_and_proxy_settings_crud() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        // 默认行已由迁移初始化
        let initial = get_proxy_settings(&pool).await.unwrap();
        assert_eq!(initial.id, 1);
        assert_eq!(initial.enabled, 0);
        assert_eq!(initial.listen_port, 8443);
        assert_eq!(initial.path, "");
        assert_eq!(initial.sub_path, "/sub");
        assert_eq!(initial.cert_mode, "cf_dns");

        // 更新配置
        let updated = update_proxy_settings(
            &pool,
            true,
            9443,
            "panel.example.com",
            "/xxjjye",
            "/mysub",
            "cf_dns",
            Some("sk-secret-token-123"),
            Some("zone-abc"),
            "/etc/ssl/cert.pem",
            "/etc/ssl/key.pem",
        )
        .await
        .unwrap();

        assert_eq!(updated.enabled, 1);
        assert_eq!(updated.listen_port, 9443);
        assert_eq!(updated.domain, "panel.example.com");
        assert_eq!(updated.path, "/xxjjye");
        assert_eq!(updated.sub_path, "/mysub");
        assert_eq!(updated.cf_api_token, "sk-secret-token-123");
        assert_eq!(updated.cf_zone_id, "zone-abc");

        // 传掩码更新，确保 Token 不被覆盖
        let masked_update = update_proxy_settings(
            &pool,
            true,
            9443,
            "panel.example.com",
            "xxjjye/",
            "mysub/",
            "cf_dns",
            Some("sk-****-123"),
            None,
            "",
            "",
        )
        .await
        .unwrap();
        assert_eq!(masked_update.cf_api_token, "sk-secret-token-123");
        assert_eq!(masked_update.path, "/xxjjye");
        assert_eq!(masked_update.sub_path, "/mysub");

        // 更新证书状态
        update_cert_status(&pool, Some(1735689600), Some(1730000000), None)
            .await
            .unwrap();
        let with_cert = get_proxy_settings(&pool).await.unwrap();
        assert_eq!(with_cert.cert_expires_at, Some(1735689600));
        assert_eq!(with_cert.last_renewed_at, Some(1730000000));
        assert_eq!(with_cert.last_error, None);
    }
}
