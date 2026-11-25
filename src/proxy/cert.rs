//! Certificate storage, expiration inspection, and automated renewal loop.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use x509_parser::pem::Pem;
use x509_parser::prelude::*;

use crate::crypto::session::now_unix;
use crate::error::{AppError, AppResult};
use crate::proxy::config::get_proxy_settings;

/// 证书自动续期间隔阈值（天数）：剩余少于 30 天时触发自动续签。
pub const RENEW_THRESHOLD_DAYS: i64 = 30;

/// 证书目录中的单个文件信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertFileInfo {
    pub name: String,
    pub rel_path: String,
    pub abs_path: String,
    pub file_type: String,
    pub size_bytes: u64,
    pub size_formatted: String,
    pub modified_at: String,
    pub permissions: String,
    pub is_key: bool,
}

/// 单个域名目录下的证书与密钥信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertDomainGroup {
    pub domain: String,
    pub dir_path: String,
    pub has_cert: bool,
    pub has_key: bool,
    pub cert_status_badge: String,
    pub cert_status_text: String,
    pub subject: String,
    pub issuer: String,
    pub san_list: Vec<String>,
    pub not_before: String,
    pub not_after: String,
    pub days_remaining: Option<i64>,
    pub cert_pem_preview: String,
    pub files: Vec<CertFileInfo>,
}

/// 证书根目录全景视图
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertsDirView {
    pub dir_path: String,
    pub exists: bool,
    pub total_domains: usize,
    pub groups: Vec<CertDomainGroup>,
}

/// 格式化文件大小为易读字符串 (B / KB / MB)
pub fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 证书解析详情
#[derive(Clone, Debug)]
pub struct ParsedCertInfo {
    pub subject: String,
    pub issuer: String,
    pub san_list: Vec<String>,
    pub not_before: i64,
    pub not_after: i64,
    pub not_before_formatted: String,
    pub not_after_formatted: String,
    pub days_remaining: i64,
    pub is_expired: bool,
    pub is_expiring_soon: bool,
}

/// 解析证书 PEM 文本中的完整元数据
pub fn inspect_cert_pem_details(pem_str: &str) -> AppResult<ParsedCertInfo> {
    for pem_res in Pem::iter_from_buffer(pem_str.as_bytes()) {
        let pem = pem_res.map_err(|e| AppError::Proxy(format!("PEM parse error: {e}")))?;
        if pem.label == "CERTIFICATE" {
            let (_, cert) = parse_x509_certificate(&pem.contents)
                .map_err(|e| AppError::Proxy(format!("X.509 parse error: {e}")))?;
            let subject = cert.subject().to_string();
            let issuer = cert.issuer().to_string();
            let not_before = cert.validity().not_before.timestamp();
            let not_after = cert.validity().not_after.timestamp();

            let mut san_list = Vec::new();
            if let Ok(Some(ext)) = cert.subject_alternative_name() {
                for name in &ext.value.general_names {
                    match name {
                        x509_parser::extensions::GeneralName::DNSName(d) => {
                            san_list.push(d.to_string())
                        }
                        x509_parser::extensions::GeneralName::IPAddress(ip) => {
                            san_list.push(format!("{:?}", ip))
                        }
                        _ => san_list.push(format!("{:?}", name)),
                    }
                }
            }

            let now = now_unix();
            let days_remaining = (not_after - now) / 86400;
            let is_expired = not_after <= now;
            let is_expiring_soon = is_cert_expiring_soon(not_after, RENEW_THRESHOLD_DAYS);

            return Ok(ParsedCertInfo {
                subject,
                issuer,
                san_list,
                not_before,
                not_after,
                not_before_formatted: crate::web::logs::timestamp_to_iso(not_before),
                not_after_formatted: crate::web::logs::timestamp_to_iso(not_after),
                days_remaining,
                is_expired,
                is_expiring_soon,
            });
        }
    }
    Err(AppError::Proxy(
        "no CERTIFICATE block found in PEM".to_string(),
    ))
}

/// 扫描证书数据目录，获取所有域名目录、证书文件列表与解析详情
pub fn scan_certs_dir(data_dir: &Path) -> CertsDirView {
    let certs_dir = data_dir.join("certs");
    let dir_path = certs_dir.display().to_string();

    if !certs_dir.is_dir() {
        return CertsDirView {
            dir_path,
            exists: false,
            total_domains: 0,
            groups: Vec::new(),
        };
    }

    let mut groups = Vec::new();
    if let Ok(entries) = fs::read_dir(&certs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let domain = entry.file_name().to_string_lossy().to_string();
                let dir_path_str = path.display().to_string();

                let mut files = Vec::new();
                let mut has_cert = false;
                let mut has_key = false;
                let mut cert_pem_content = String::new();

                if let Ok(file_entries) = fs::read_dir(&path) {
                    for f in file_entries.flatten() {
                        let fp = f.path();
                        if fp.is_file() {
                            let fname = f.file_name().to_string_lossy().to_string();
                            let is_key = fname.contains("key") || fname.ends_with(".key");
                            let is_cert = fname.contains("chain")
                                || fname.contains("cert")
                                || (fname.ends_with(".crt") || fname.ends_with(".pem")) && !is_key;

                            let (size_bytes, size_formatted, modified_at, permissions) =
                                match fs::metadata(&fp) {
                                    Ok(m) => {
                                        let s = m.len();
                                        let sf = format_file_size(s);
                                        let mod_time = m
                                            .modified()
                                            .ok()
                                            .map(|st| {
                                                let ts = st
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_secs()
                                                    as i64;
                                                crate::web::logs::timestamp_to_iso(ts)
                                            })
                                            .unwrap_or_else(|| "-".to_string());
                                        #[cfg(unix)]
                                        let perm =
                                            format!("{:04o}", m.permissions().mode() & 0o7777);
                                        #[cfg(not(unix))]
                                        let perm = "-".to_string();

                                        (s, sf, mod_time, perm)
                                    }
                                    Err(_) => {
                                        (0, "0 B".to_string(), "-".to_string(), "-".to_string())
                                    }
                                };

                            let file_type = if is_cert {
                                has_cert = true;
                                if fname == "fullchain.pem" || cert_pem_content.is_empty() {
                                    if let Ok(content) = fs::read_to_string(&fp) {
                                        cert_pem_content = content;
                                    }
                                }
                                "证书 (公钥链)".to_string()
                            } else if is_key {
                                has_key = true;
                                "私钥 (密钥文件)".to_string()
                            } else {
                                "其它文件".to_string()
                            };

                            files.push(CertFileInfo {
                                name: fname.clone(),
                                rel_path: format!("{domain}/{fname}"),
                                abs_path: fp.display().to_string(),
                                file_type,
                                size_bytes,
                                size_formatted,
                                modified_at,
                                permissions,
                                is_key,
                            });
                        }
                    }
                }

                // 排序：证书文件在前，私钥文件在后，其余字母排序
                files.sort_by(|a, b| {
                    if a.is_key && !b.is_key {
                        std::cmp::Ordering::Greater
                    } else if !a.is_key && b.is_key {
                        std::cmp::Ordering::Less
                    } else {
                        a.name.cmp(&b.name)
                    }
                });

                // 解析证书详情
                let parsed = if !cert_pem_content.is_empty() {
                    inspect_cert_pem_details(&cert_pem_content).ok()
                } else {
                    None
                };

                let is_fallback = domain == "_fallback";
                let (badge, text, subject, issuer, san_list, not_before, not_after, days_remaining) =
                    match parsed {
                        Some(p) => {
                            let badge = if is_fallback {
                                "fallback".to_string()
                            } else if p.is_expired {
                                "expired".to_string()
                            } else if p.is_expiring_soon {
                                "expiring".to_string()
                            } else {
                                "valid".to_string()
                            };

                            let text = if is_fallback {
                                "自签兜底证书 (内部保底用)".to_string()
                            } else if p.is_expired {
                                format!("已过期 ({})", p.not_after_formatted)
                            } else {
                                format!("有效 (剩余 {} 天)", p.days_remaining)
                            };

                            (
                                badge,
                                text,
                                p.subject,
                                p.issuer,
                                p.san_list,
                                p.not_before_formatted,
                                p.not_after_formatted,
                                Some(p.days_remaining),
                            )
                        }
                        None => {
                            if has_cert && has_key {
                                (
                                    "valid".to_string(),
                                    "证书文件正常 (未解析)".to_string(),
                                    "-".to_string(),
                                    "-".to_string(),
                                    vec![],
                                    "-".to_string(),
                                    "-".to_string(),
                                    None,
                                )
                            } else {
                                (
                                    "incomplete".to_string(),
                                    "文件不全 (证书或私钥缺失)".to_string(),
                                    "-".to_string(),
                                    "-".to_string(),
                                    vec![],
                                    "-".to_string(),
                                    "-".to_string(),
                                    None,
                                )
                            }
                        }
                    };

                groups.push(CertDomainGroup {
                    domain,
                    dir_path: dir_path_str,
                    has_cert,
                    has_key,
                    cert_status_badge: badge,
                    cert_status_text: text,
                    subject,
                    issuer,
                    san_list,
                    not_before,
                    not_after,
                    days_remaining,
                    cert_pem_preview: cert_pem_content,
                    files,
                });
            }
        }
    }

    // 域名排序：正式域名在前，_fallback 兜底置底
    groups.sort_by(|a, b| {
        if a.domain == "_fallback" && b.domain != "_fallback" {
            std::cmp::Ordering::Greater
        } else if a.domain != "_fallback" && b.domain == "_fallback" {
            std::cmp::Ordering::Less
        } else {
            a.domain.cmp(&b.domain)
        }
    });

    let total_domains = groups.len();
    CertsDirView {
        dir_path,
        exists: true,
        total_domains,
        groups,
    }
}

/// 获取指定域名的证书完整链路径
pub fn cert_path(data_dir: &Path, domain: &str) -> PathBuf {
    data_dir.join("certs").join(domain).join("fullchain.pem")
}

/// 获取指定域名的私钥路径
pub fn key_path(data_dir: &Path, domain: &str) -> PathBuf {
    data_dir.join("certs").join(domain).join("privkey.pem")
}

/// 将证书与私钥写入数据目录，并设置 0600 安全权限
pub fn save_cert_files(
    data_dir: &Path,
    domain: &str,
    cert_pem: &str,
    key_pem: &str,
) -> AppResult<(PathBuf, PathBuf)> {
    let domain_dir = data_dir.join("certs").join(domain);
    fs::create_dir_all(&domain_dir).map_err(AppError::Io)?;

    let cp = domain_dir.join("fullchain.pem");
    let kp = domain_dir.join("privkey.pem");

    fs::write(&cp, cert_pem).map_err(AppError::Io)?;
    fs::write(&kp, key_pem).map_err(AppError::Io)?;

    // 限制私钥文件权限仅自身可读写
    let perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(&kp, perms).map_err(AppError::Io)?;

    Ok((cp, kp))
}

/// 从 PEM 字符串中解析证书过期时间戳 (unix epoch 秒)。
pub fn parse_cert_not_after(pem_str: &str) -> AppResult<i64> {
    for pem_res in Pem::iter_from_buffer(pem_str.as_bytes()) {
        let pem = pem_res.map_err(|e| AppError::Proxy(format!("PEM parse error: {e}")))?;
        if pem.label == "CERTIFICATE" {
            let (_, cert) = parse_x509_certificate(&pem.contents)
                .map_err(|e| AppError::Proxy(format!("X.509 parse error: {e}")))?;
            return Ok(cert.validity().not_after.timestamp());
        }
    }
    Err(AppError::Proxy(
        "no CERTIFICATE block found in PEM".to_string(),
    ))
}

/// 检查给定过期时间戳是否距离现在不足 `threshold_days` 天。
pub fn is_cert_expiring_soon(expires_at: i64, threshold_days: i64) -> bool {
    let now = now_unix();
    let threshold_secs = threshold_days * 86400;
    expires_at <= now + threshold_secs
}

/// 后台自动检测与续期任务。
/// 每 12 小时检测一次证书是否需要续期（到期前 30 天自动触发），并在成功续期后触发反代热重载。
pub async fn run_renewal_loop(
    pool: SqlitePool,
    data_dir: PathBuf,
    proxy_tx: tokio::sync::mpsc::Sender<crate::proxy::supervisor::ProxyCommand>,
    cert_tracker: Option<Arc<crate::proxy::tracker::CertProgressTracker>>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(43200));
    // 启动时延迟 10 秒，避免与面板启动时的并发竞争
    tokio::time::sleep(Duration::from_secs(10)).await;

    loop {
        interval.tick().await;

        let settings = match get_proxy_settings(&pool).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "proxy renewal loop: failed to load settings");
                continue;
            }
        };

        if !settings.is_enabled()
            || settings.cert_mode != "cf_dns"
            || settings.domain.is_empty()
            || settings.cf_api_token.is_empty()
        {
            continue;
        }

        let should_renew = match settings.cert_expires_at {
            Some(exp) => is_cert_expiring_soon(exp, RENEW_THRESHOLD_DAYS),
            None => {
                let cp = cert_path(&data_dir, &settings.domain);
                let kp = key_path(&data_dir, &settings.domain);
                !cp.is_file() || !kp.is_file()
            }
        };

        if should_renew {
            tracing::info!(
                domain = %settings.domain,
                "proxy renewal loop: certificate is expiring or absent, triggering automated renewal"
            );
            let pool_clone = pool.clone();
            let data_dir_clone = data_dir.clone();
            let domain = settings.domain.clone();
            let cf_token = settings.cf_api_token.clone();
            let cf_zone_id = if settings.cf_zone_id.is_empty() {
                None
            } else {
                Some(settings.cf_zone_id.clone())
            };
            let proxy_tx_clone = proxy_tx.clone();
            let tracker = cert_tracker.clone();

            match crate::proxy::acme::request_certificate(
                &pool_clone,
                &data_dir_clone,
                &domain,
                &cf_token,
                cf_zone_id.as_deref(),
                false,
                tracker.as_deref(),
            )
            .await
            {
                Ok(expires_at) => {
                    tracing::info!(
                        domain = %domain,
                        expires_at = expires_at,
                        "automatic certificate renewal succeeded, reloading proxy"
                    );
                    let _ = proxy_tx_clone
                        .send(crate::proxy::supervisor::ProxyCommand::Reload)
                        .await;
                }
                Err(e) => {
                    let err_msg = format!("ACME 自动续期失败: {e}");
                    tracing::error!(%err_msg);
                    let _ = crate::proxy::config::update_cert_status(
                        &pool_clone,
                        None,
                        None,
                        Some(&err_msg),
                    )
                    .await;
                }
            }
        }
    }
}

/// 生成自签名证书并返回 (cert_pem, key_pem)
pub fn generate_self_signed_cert(domain: &str) -> AppResult<(String, String)> {
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, domain);
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| AppError::Proxy(format!("failed to generate key pair: {e}")))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| AppError::Proxy(format!("failed to generate self-signed cert: {e}")))?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

/// 解析入站协议（如 Hysteria 2）所需的 TLS 证书与私钥路径。
/// 如果是 custom 模式，使用 custom_cert_path / custom_key_path。
/// 如果是 auto 模式，优先使用 proxy_domain 的证书，其次 inbound.sni 的证书，
/// 若都不存在则生成/复用 _fallback 自签证书以确保 Xray 顺利启动。
pub fn resolve_inbound_cert(
    data_dir: &Path,
    inbound: &crate::db::models::Inbound,
    proxy_domain: Option<&str>,
) -> AppResult<(PathBuf, PathBuf)> {
    if inbound.cert_mode == "custom" {
        let cp = PathBuf::from(&inbound.custom_cert_path);
        let kp = PathBuf::from(&inbound.custom_key_path);
        if cp.exists() && kp.exists() {
            return Ok((cp, kp));
        }
        tracing::warn!(
            cert_path = %inbound.custom_cert_path,
            key_path = %inbound.custom_key_path,
            "custom cert/key file not found, falling back to auto/self-signed cert"
        );
    }

    if let Some(domain) = proxy_domain {
        if !domain.is_empty() {
            let cp = cert_path(data_dir, domain);
            let kp = key_path(data_dir, domain);
            if cp.exists() && kp.exists() {
                return Ok((cp, kp));
            }
        }
    }

    if !inbound.sni.is_empty() {
        let cp = cert_path(data_dir, &inbound.sni);
        let kp = key_path(data_dir, &inbound.sni);
        if cp.exists() && kp.exists() {
            return Ok((cp, kp));
        }
    }

    let fb_cp = cert_path(data_dir, "_fallback");
    let fb_kp = key_path(data_dir, "_fallback");
    if fb_cp.exists() && fb_kp.exists() {
        return Ok((fb_cp, fb_kp));
    }

    let cn = if !inbound.sni.is_empty() {
        &inbound.sni
    } else if let Some(d) = proxy_domain {
        if !d.is_empty() {
            d
        } else {
            "localhost"
        }
    } else {
        "localhost"
    };

    let (cert_pem, key_pem) = generate_self_signed_cert(cn)?;
    save_cert_files(data_dir, "_fallback", &cert_pem, &key_pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_certs_dir_and_inspect() {
        let temp_dir = tempfile::tempdir().unwrap();
        let domain = "scan.example.com";
        let (cert_pem, key_pem) = generate_self_signed_cert(domain).unwrap();
        save_cert_files(temp_dir.path(), domain, &cert_pem, &key_pem).unwrap();

        let view = scan_certs_dir(temp_dir.path());
        assert!(view.exists);
        assert_eq!(view.total_domains, 1);
        let group = &view.groups[0];
        assert_eq!(group.domain, domain);
        assert!(group.has_cert);
        assert!(group.has_key);
        assert_eq!(group.cert_status_badge, "valid");
        assert_eq!(group.subject, format!("CN={domain}"));
        assert_eq!(group.files.len(), 2);
    }

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(500), "500 B");
        assert_eq!(format_file_size(2048), "2.0 KB");
        assert_eq!(format_file_size(1048576 * 3), "3.0 MB");
    }

    #[test]
    fn expiring_soon_logic() {
        let now = now_unix();
        // 剩余 10 天 -> 需要续期
        assert!(is_cert_expiring_soon(now + 10 * 86400, 30));
        // 已过期 -> 需要续期
        assert!(is_cert_expiring_soon(now - 10, 30));
        // 剩余 40 天 -> 暂不需要续期
        assert!(!is_cert_expiring_soon(now + 40 * 86400, 30));
    }

    #[test]
    fn save_and_parse_test_cert() {
        let temp_dir = tempfile::tempdir().unwrap();
        let domain = "test.example.com";

        // 使用 rcgen 生成自签测试证书
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, domain);
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        let (cp, kp) = save_cert_files(temp_dir.path(), domain, &cert_pem, &key_pem).unwrap();
        assert!(cp.exists());
        assert!(kp.exists());

        let expires_at = parse_cert_not_after(&cert_pem).unwrap();
        assert!(expires_at > now_unix());
    }

    #[test]
    fn resolve_inbound_cert_fallback() {
        let temp_dir = tempfile::tempdir().unwrap();
        let inbound = crate::db::models::Inbound {
            id: 1,
            listen_port: 8443,
            dest: "example.com".into(),
            sni: "hy2.example.com".into(),
            server_name: None,
            private_key: "".into(),
            public_key: "".into(),
            short_id: "".into(),
            xhttp_path: None,
            protocol: "hysteria2".into(),
            cert_mode: "auto".into(),
            custom_cert_path: "".into(),
            custom_key_path: "".into(),
            masquerade_url: "".into(),
            port_range: "".into(),
            transport_protocol: "TCP".into(),
            skip_cert_verify: 0,
            udp: 1,
            status: "active".into(),
            created_at: None,
            updated_at: None,
        };

        let (cp, kp) = resolve_inbound_cert(temp_dir.path(), &inbound, None).unwrap();
        assert!(cp.exists());
        assert!(kp.exists());
    }
}
