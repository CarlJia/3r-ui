//! ACME DNS-01 certificate provisioning via Cloudflare DNS.

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus,
};
use sqlx::SqlitePool;
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::crypto::session::now_unix;
use crate::error::{AppError, AppResult};
use crate::proxy::cert::{parse_cert_not_after, save_cert_files};
use crate::proxy::cloudflare::CloudflareClient;
use crate::proxy::config::update_cert_status;
use crate::proxy::tracker::{CertProgressTracker, CertRequestStage};

/// 向 Let's Encrypt 申请/续期证书（使用 Cloudflare DNS-01 验证）。
/// 成功后将证书与私钥落盘并更新数据库，返回证书到期时间戳。
pub async fn request_certificate(
    pool: &SqlitePool,
    data_dir: &Path,
    domain: &str,
    cf_token: &str,
    cf_zone_id: Option<&str>,
    use_staging: bool,
    tracker: Option<&CertProgressTracker>,
) -> AppResult<i64> {
    let clean_domain = domain.trim().trim_end_matches('.');
    if clean_domain.is_empty() {
        if let Some(t) = tracker {
            t.failure("域名不能为空").await;
        }
        return Err(AppError::Proxy("domain cannot be empty for ACME".into()));
    }
    if cf_token.trim().is_empty() {
        if let Some(t) = tracker {
            t.failure("未配置 Cloudflare API Token").await;
        }
        return Err(AppError::Proxy(
            "Cloudflare API Token is required for ACME DNS-01".into(),
        ));
    }

    if let Some(t) = tracker {
        t.log(
            CertRequestStage::LocatingZone,
            "查询 Cloudflare Zone ID",
            "info",
            &format!("正在定位域名 {clean_domain} 对应的 Cloudflare Zone…"),
        )
        .await;
    }

    let cf_client = CloudflareClient::new(cf_token.trim().to_string());
    let zone_id = match cf_zone_id.filter(|z| !z.trim().is_empty()) {
        Some(z) => {
            if let Some(t) = tracker {
                t.log(
                    CertRequestStage::LocatingZone,
                    "已指定 Zone ID",
                    "info",
                    &format!("使用指定的 Zone ID: {z}"),
                )
                .await;
            }
            z.trim().to_string()
        }
        None => match cf_client.get_zone_id(clean_domain).await {
            Ok(zid) => {
                if let Some(t) = tracker {
                    t.log(
                        CertRequestStage::LocatingZone,
                        "Zone ID 获取成功",
                        "info",
                        &format!("成功解析到 Zone ID: {zid}"),
                    )
                    .await;
                }
                zid
            }
            Err(e) => {
                if let Some(t) = tracker {
                    t.failure(&format!(
                        "无法找到域名 {clean_domain} 的 Cloudflare Zone: {e}"
                    ))
                    .await;
                }
                return Err(e);
            }
        },
    };

    let acme_dir_url = if use_staging {
        LetsEncrypt::Staging.url()
    } else {
        LetsEncrypt::Production.url()
    };

    // 1. 加载或创建 ACME 账户私钥
    if let Some(t) = tracker {
        t.log(
            CertRequestStage::PreparingAccount,
            "准备 ACME 账户",
            "info",
            "正在加载或注册 Let's Encrypt 账户凭证…",
        )
        .await;
    }

    let acme_account_dir = data_dir.join("proxy").join("acme");
    fs::create_dir_all(&acme_account_dir).map_err(AppError::Io)?;
    let account_file = acme_account_dir.join("account.json");

    let account = if account_file.exists() {
        let creds_json = fs::read_to_string(&account_file).map_err(AppError::Io)?;
        let creds: AccountCredentials = serde_json::from_str(&creds_json).map_err(|e| {
            AppError::Proxy(format!("failed to parse ACME account credentials: {e}"))
        })?;
        Account::from_credentials(creds)
            .await
            .map_err(|e| AppError::Proxy(format!("invalid ACME credentials: {e}")))?
    } else {
        let (account, creds) = Account::create(
            &NewAccount {
                contact: &[],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            acme_dir_url,
            None,
        )
        .await
        .map_err(|e| AppError::Proxy(format!("failed to create ACME account: {e}")))?;

        let creds_json = serde_json::to_string_pretty(&creds)
            .map_err(|e| AppError::Proxy(format!("failed to serialize ACME credentials: {e}")))?;
        fs::write(&account_file, creds_json).map_err(AppError::Io)?;
        account
    };

    // 2. 创建订单
    if let Some(t) = tracker {
        t.log(
            CertRequestStage::SubmittingOrder,
            "提交 ACME 证书订单",
            "info",
            &format!("向 Let's Encrypt 提交订单，申请域名: {clean_domain}"),
        )
        .await;
    }

    let identifier = Identifier::Dns(clean_domain.to_string());
    let mut order = account
        .new_order(&NewOrder {
            identifiers: &[identifier],
        })
        .await
        .map_err(|e| AppError::Proxy(format!("failed to create ACME order: {e}")))?;

    // 3. 处理授权与 DNS-01 验证
    let authorizations = order
        .authorizations()
        .await
        .map_err(|e| AppError::Proxy(format!("failed to fetch authorizations: {e}")))?;

    for authz in &authorizations {
        if authz.status == AuthorizationStatus::Valid {
            continue;
        }

        let challenge = authz
            .challenges
            .iter()
            .find(|c| c.r#type == ChallengeType::Dns01)
            .ok_or_else(|| {
                AppError::Proxy("no DNS-01 challenge available in authorization".into())
            })?;

        let challenge_name = format!("_acme-challenge.{}", clean_domain);
        let challenge_value = order.key_authorization(challenge).dns_value();

        if let Some(t) = tracker {
            t.log(
                CertRequestStage::CreatingDnsRecord,
                "创建 DNS TXT 记录",
                "info",
                &format!("正在 Cloudflare 创建 TXT 记录: {challenge_name}"),
            )
            .await;
        }

        let rec_id = match cf_client
            .create_txt_record(&zone_id, &challenge_name, &challenge_value)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                if let Some(t) = tracker {
                    t.failure(&format!("在 Cloudflare 创建 TXT 记录失败: {e}"))
                        .await;
                }
                return Err(e);
            }
        };

        // 无论后续是否成功，必须确保在结束时清理 TXT 记录
        let cf_cleanup = async {
            let _ = cf_client.delete_txt_record(&zone_id, &rec_id).await;
        };

        // 等待 DNS 生效
        if let Some(t) = tracker {
            t.log(
                CertRequestStage::WaitingPropagation,
                "等待 DNS 解析全球生效",
                "info",
                "已写入 TXT 记录，正在通过权威及公网 DoH 节点轮询检测 DNS 解析生效…",
            )
            .await;
        }

        let prop_res = cf_client
            .wait_dns_propagation(&challenge_name, &challenge_value, Duration::from_secs(60))
            .await;

        if let Err(e) = prop_res {
            tracing::warn!(error = %e, "DoH propagation check timed out, checking Cloudflare authoritative zone directly");
            let in_zone = cf_client
                .verify_txt_record_in_zone(&zone_id, &challenge_name, &challenge_value)
                .await
                .unwrap_or(false);

            if in_zone {
                if let Some(t) = tracker {
                    t.log(
                        CertRequestStage::WaitingPropagation,
                        "Cloudflare 权威记录确认",
                        "info",
                        "公网 DoH 节点暂未同步，但 Cloudflare 权威已确认 TXT 记录就绪，等待 5 秒权威生效…",
                    )
                    .await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                cf_cleanup.await;
                if let Some(t) = tracker {
                    t.failure(&format!("等待 DNS TXT 记录生效超时: {e}")).await;
                }
                return Err(e);
            }
        }

        if let Some(t) = tracker {
            t.log(
                CertRequestStage::ValidatingChallenge,
                "触发 ACME 质询验证",
                "info",
                "DNS TXT 记录已生效，通知 Let's Encrypt 进行验证…",
            )
            .await;
        }

        // 通知 ACME 服务端质询已就绪
        if let Err(e) = order.set_challenge_ready(&challenge.url).await {
            cf_cleanup.await;
            if let Some(t) = tracker {
                t.failure(&format!("通知 ACME 质询就绪失败: {e}")).await;
            }
            return Err(AppError::Proxy(format!(
                "failed to set challenge ready: {e}"
            )));
        }

        // 轮询订单或质询状态直至 Ready / Valid
        let mut validated = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            match order.challenge(&challenge.url).await {
                Ok(c) => {
                    let status_str = format!("{:?}", c.status);
                    if status_str == "Valid" {
                        validated = true;
                        if let Some(t) = tracker {
                            t.log(
                                CertRequestStage::ValidatingChallenge,
                                "质询验证通过",
                                "info",
                                "Let's Encrypt DNS-01 验证成功通过！",
                            )
                            .await;
                        }
                        break;
                    } else if status_str == "Invalid" {
                        cf_cleanup.await;
                        let detail = c.error.map(|err| format!("{:?}", err)).unwrap_or_default();
                        let err_msg = format!("ACME challenge rejected: {detail}");
                        if let Some(t) = tracker {
                            t.failure(&err_msg).await;
                        }
                        return Err(AppError::Proxy(err_msg));
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "error checking challenge status");
                }
            }
        }

        cf_cleanup.await;

        if !validated {
            let err_msg = "ACME challenge verification timed out".to_string();
            if let Some(t) = tracker {
                t.failure(&err_msg).await;
            }
            return Err(AppError::Proxy(err_msg));
        }
    }

    // 4. 等待订单状态进入 Ready
    for _ in 0..20 {
        let state = order
            .refresh()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to refresh order state: {e}")))?;
        if state.status == OrderStatus::Ready || state.status == OrderStatus::Valid {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // 5. 生成私钥和 CSR
    if let Some(t) = tracker {
        t.log(
            CertRequestStage::GeneratingCsr,
            "生成证书请求与密钥",
            "info",
            "正在生成 ECDSA 私钥与证书签名请求 (CSR)…",
        )
        .await;
    }

    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| AppError::Proxy(format!("failed to generate ECDSA key pair: {e}")))?;

    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, clean_domain);
    let csr = params
        .serialize_request(&key_pair)
        .map_err(|e| AppError::Proxy(format!("failed to serialize CSR: {e}")))?;
    let csr_der = csr.der();

    // 6. 提交 CSR 完成订单
    if let Some(t) = tracker {
        t.log(
            CertRequestStage::FinalizingOrder,
            "完成证书签发",
            "info",
            "向 CA 提交 CSR 请求签发证书…",
        )
        .await;
    }

    order
        .finalize(csr_der)
        .await
        .map_err(|e| AppError::Proxy(format!("failed to finalize ACME order: {e}")))?;

    // 轮询订单直到可下载证书
    let mut cert_ready = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let state = order
            .refresh()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to refresh order: {e}")))?;
        if state.status == OrderStatus::Valid {
            cert_ready = true;
            break;
        }
    }

    if !cert_ready {
        let err_msg = "order finalization did not become valid".to_string();
        if let Some(t) = tracker {
            t.failure(&err_msg).await;
        }
        return Err(AppError::Proxy(err_msg));
    }

    if let Some(t) = tracker {
        t.log(
            CertRequestStage::DownloadingCert,
            "下载并安装证书",
            "info",
            "正在下载证书链并安全保存至本地磁盘…",
        )
        .await;
    }

    let cert_pem = order
        .certificate()
        .await
        .map_err(|e| AppError::Proxy(format!("failed to download certificate: {e}")))?
        .ok_or_else(|| AppError::Proxy("order returned empty certificate chain".into()))?;

    let key_pem = key_pair.serialize_pem();

    // 7. 存储证书文件并解析有效期
    let _ = save_cert_files(data_dir, clean_domain, &cert_pem, &key_pem)?;
    let expires_at = parse_cert_not_after(&cert_pem)?;

    // 8. 更新数据库记录
    update_cert_status(pool, Some(expires_at), Some(now_unix()), None).await?;

    if let Some(t) = tracker {
        t.success(expires_at).await;
    }

    tracing::info!(
        domain = %clean_domain,
        expires_at = expires_at,
        "successfully issued and installed ACME TLS certificate"
    );

    Ok(expires_at)
}
