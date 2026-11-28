//! Cloudflare DNS API client for ACME DNS-01 challenge management.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::{AppError, AppResult};

const DEFAULT_CF_API_BASE: &str = "https://api.cloudflare.com/client/v4";
const DEFAULT_DOH_URLS: &[&str] = &[
    "https://cloudflare-dns.com/dns-query",
    "https://dns.alidns.com/resolve",
    "https://doh.pub/dns-query",
    "https://1.1.1.1/dns-query",
];

#[derive(Clone, Debug)]
pub struct CloudflareClient {
    client: reqwest::Client,
    api_token: String,
    base_url: String,
    doh_urls: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CfApiResponse<T> {
    success: bool,
    errors: Option<Vec<CfApiError>>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct CfApiError {
    code: u64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct CfZone {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CfDnsRecord {
    pub id: String,
    pub zone_id: Option<String>,
    #[serde(rename = "type")]
    pub record_type: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default)]
    pub ttl: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DnsSyncOutcome {
    Created {
        domain: String,
        ip: String,
    },
    Updated {
        domain: String,
        old_ip: String,
        new_ip: String,
    },
    Unchanged {
        domain: String,
        ip: String,
    },
    Skipped {
        domain: String,
        reason: String,
    },
    Failed {
        domain: String,
        error: String,
    },
}

#[derive(Debug, Serialize)]
struct CreateOrUpdateDnsRecordRequest<'a> {
    #[serde(rename = "type")]
    record_type: &'a str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
    proxied: bool,
}

#[derive(Debug, Serialize)]
struct CreateDnsRecordRequest<'a> {
    #[serde(rename = "type")]
    record_type: &'a str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
}

#[derive(Debug, Deserialize)]
struct DnsRecordResult {
    id: String,
}

#[derive(Debug, Deserialize)]
struct DohResponse {
    #[serde(rename = "Status")]
    status: u32,
    #[serde(rename = "Answer")]
    answer: Option<Vec<DohAnswer>>,
}

#[derive(Debug, Deserialize)]
struct DohAnswer {
    data: String,
}

impl CloudflareClient {
    pub fn new(api_token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            client,
            api_token,
            base_url: DEFAULT_CF_API_BASE.to_string(),
            doh_urls: DEFAULT_DOH_URLS.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[cfg(test)]
    pub fn with_endpoints(mut self, base_url: String, doh_url: String) -> Self {
        self.base_url = base_url;
        self.doh_urls = vec![doh_url];
        self
    }

    /// 根据完整域名查找所属的 Cloudflare Zone ID。
    /// 例如对于 `panel.sub.example.com`，依次尝试：
    /// 1. `panel.sub.example.com`
    /// 2. `sub.example.com`
    /// 3. `example.com`
    pub async fn get_zone_id(&self, domain: &str) -> AppResult<String> {
        let clean_domain = domain.trim().trim_end_matches('.');
        let parts: Vec<&str> = clean_domain.split('.').collect();
        if parts.len() < 2 {
            return Err(AppError::Proxy(format!(
                "invalid domain name for DNS lookup: {domain}"
            )));
        }

        for i in 0..=parts.len() - 2 {
            let candidate_zone = parts[i..].join(".");
            let url = format!(
                "{}/zones?name={}&status=active",
                self.base_url, candidate_zone
            );
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.api_token)
                .send()
                .await
                .map_err(|e| {
                    AppError::Proxy(format!(
                        "failed to query Cloudflare zone for {candidate_zone}: {e}"
                    ))
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(AppError::Proxy(format!(
                    "Cloudflare API returned error status {status}: {body}"
                )));
            }

            let parsed: CfApiResponse<Vec<CfZone>> = resp.json().await.map_err(|e| {
                AppError::Proxy(format!("failed to parse Cloudflare zone response: {e}"))
            })?;

            if let Some(zones) = parsed.result {
                if let Some(zone) = zones.into_iter().find(|z| z.name == candidate_zone) {
                    return Ok(zone.id);
                }
            }
        }

        Err(AppError::Proxy(format!(
            "no active Cloudflare zone found for domain: {domain}"
        )))
    }

    /// 在指定 Zone 中创建 TXT 记录（用于 ACME DNS-01 验证）。
    /// 返回创建成功的 record_id。
    pub async fn create_txt_record(
        &self,
        zone_id: &str,
        name: &str,
        content: &str,
    ) -> AppResult<String> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone_id);
        let req_body = CreateDnsRecordRequest {
            record_type: "TXT",
            name,
            content,
            ttl: 120, // 2 minutes
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_token)
            .json(&req_body)
            .send()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to create Cloudflare TXT record: {e}")))?;

        let status = resp.status();
        let parsed: CfApiResponse<DnsRecordResult> = resp.json().await.map_err(|e| {
            AppError::Proxy(format!(
                "failed to parse Cloudflare create record response: {e}"
            ))
        })?;

        if !parsed.success {
            let err_msg = parsed
                .errors
                .unwrap_or_default()
                .into_iter()
                .map(|e| format!("{}({})", e.message, e.code))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::Proxy(format!(
                "Cloudflare failed to create TXT record (status {status}): {err_msg}"
            )));
        }

        match parsed.result {
            Some(res) => Ok(res.id),
            None => Err(AppError::Proxy(
                "Cloudflare returned success without record id".to_string(),
            )),
        }
    }

    /// 清理已完成验证的 TXT 记录。
    pub async fn delete_txt_record(&self, zone_id: &str, record_id: &str) -> AppResult<()> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone_id, record_id
        );
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to delete Cloudflare TXT record: {e}")))?;

        let status = resp.status();
        let parsed: CfApiResponse<serde_json::Value> = resp.json().await.map_err(|e| {
            AppError::Proxy(format!(
                "failed to parse Cloudflare delete record response: {e}"
            ))
        })?;

        if !parsed.success {
            let err_msg = parsed
                .errors
                .unwrap_or_default()
                .into_iter()
                .map(|e| format!("{}({})", e.message, e.code))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::Proxy(format!(
                "Cloudflare failed to delete TXT record (status {status}): {err_msg}"
            )));
        }

        Ok(())
    }

    /// 轮询 DoH 权威 DNS，确认 TXT 记录已成功在公网生效，避免 ACME 提前验证失败。
    /// 自动在多个主流 DoH 服务（Cloudflare / 阿里 / 腾讯）之间回退查询。
    pub async fn wait_dns_propagation(
        &self,
        name: &str,
        expected_content: &str,
        timeout: Duration,
    ) -> AppResult<()> {
        let start = std::time::Instant::now();
        let clean_expected = expected_content.trim().trim_matches('"');
        let check_interval = Duration::from_secs(3);

        while start.elapsed() < timeout {
            for doh_url in &self.doh_urls {
                let url = format!("{doh_url}?name={name}&type=TXT");
                let resp = self
                    .client
                    .get(&url)
                    .header("accept", "application/dns-json")
                    .timeout(Duration::from_secs(4))
                    .send()
                    .await;

                if let Ok(res) = resp {
                    if let Ok(doh) = res.json::<DohResponse>().await {
                        if doh.status == 0 {
                            if let Some(answers) = doh.answer {
                                for ans in answers {
                                    let ans_data = ans.data.trim().trim_matches('"');
                                    if ans_data == clean_expected {
                                        tracing::info!(
                                            name = %name,
                                            doh = %doh_url,
                                            elapsed = ?start.elapsed(),
                                            "DNS TXT record propagation verified"
                                        );
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(check_interval).await;
        }

        Err(AppError::Proxy(format!(
            "timed out after {}s waiting for DNS TXT record {name} to propagate",
            timeout.as_secs()
        )))
    }

    /// 检查指定 Zone 中的 TXT 记录是否已在 Cloudflare 权威数据库中就绪。
    pub async fn verify_txt_record_in_zone(
        &self,
        zone_id: &str,
        name: &str,
        expected_content: &str,
    ) -> AppResult<bool> {
        let clean_expected = expected_content.trim().trim_matches('"');
        let url = format!(
            "{}/zones/{zone_id}/dns_records?type=TXT&name={name}",
            self.base_url
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to query Cloudflare TXT records: {e}")))?;

        if resp.status().is_success() {
            if let Ok(parsed) = resp.json::<CfApiResponse<Vec<serde_json::Value>>>().await {
                if let Some(records) = parsed.result {
                    for r in records {
                        if let Some(content) = r.get("content").and_then(|v| v.as_str()) {
                            if content.trim().trim_matches('"') == clean_expected {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// 查询指定 Zone 下符合指定域名与类型的 DNS 记录列表
    pub async fn list_dns_records(
        &self,
        zone_id: &str,
        record_type: &str,
        name: &str,
    ) -> AppResult<Vec<CfDnsRecord>> {
        let clean_name = name.trim().trim_end_matches('.');
        let url = format!(
            "{}/zones/{zone_id}/dns_records?type={record_type}&name={clean_name}",
            self.base_url
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to query Cloudflare DNS records: {e}")))?;

        let status = resp.status();
        let parsed: CfApiResponse<Vec<CfDnsRecord>> = resp.json().await.map_err(|e| {
            AppError::Proxy(format!(
                "failed to parse Cloudflare list DNS records response: {e}"
            ))
        })?;

        if !parsed.success {
            let err_msg = parsed
                .errors
                .unwrap_or_default()
                .into_iter()
                .map(|e| format!("{}({})", e.message, e.code))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::Proxy(format!(
                "Cloudflare failed to list DNS records (status {status}): {err_msg}"
            )));
        }

        Ok(parsed.result.unwrap_or_default())
    }

    /// 在指定 Zone 中创建 DNS 记录 (A 或 AAAA)
    #[allow(clippy::too_many_arguments)]
    pub async fn create_dns_record(
        &self,
        zone_id: &str,
        record_type: &str,
        name: &str,
        content: &str,
        proxied: bool,
        ttl: u32,
    ) -> AppResult<String> {
        let url = format!("{}/zones/{zone_id}/dns_records", self.base_url);
        let req_body = CreateOrUpdateDnsRecordRequest {
            record_type,
            name: name.trim().trim_end_matches('.'),
            content: content.trim(),
            ttl,
            proxied,
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_token)
            .json(&req_body)
            .send()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to create Cloudflare DNS record: {e}")))?;

        let status = resp.status();
        let parsed: CfApiResponse<DnsRecordResult> = resp.json().await.map_err(|e| {
            AppError::Proxy(format!(
                "failed to parse Cloudflare create DNS record response: {e}"
            ))
        })?;

        if !parsed.success {
            let err_msg = parsed
                .errors
                .unwrap_or_default()
                .into_iter()
                .map(|e| format!("{}({})", e.message, e.code))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::Proxy(format!(
                "Cloudflare failed to create DNS record (status {status}): {err_msg}"
            )));
        }

        match parsed.result {
            Some(res) => Ok(res.id),
            None => Err(AppError::Proxy(
                "Cloudflare returned success without DNS record id".to_string(),
            )),
        }
    }

    /// 更新指定 Zone 中的现有 DNS 记录
    #[allow(clippy::too_many_arguments)]
    pub async fn update_dns_record(
        &self,
        zone_id: &str,
        record_id: &str,
        record_type: &str,
        name: &str,
        content: &str,
        proxied: bool,
        ttl: u32,
    ) -> AppResult<()> {
        let url = format!("{}/zones/{zone_id}/dns_records/{record_id}", self.base_url);
        let req_body = CreateOrUpdateDnsRecordRequest {
            record_type,
            name: name.trim().trim_end_matches('.'),
            content: content.trim(),
            ttl,
            proxied,
        };

        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.api_token)
            .json(&req_body)
            .send()
            .await
            .map_err(|e| AppError::Proxy(format!("failed to update Cloudflare DNS record: {e}")))?;

        let status = resp.status();
        let parsed: CfApiResponse<serde_json::Value> = resp.json().await.map_err(|e| {
            AppError::Proxy(format!(
                "failed to parse Cloudflare update DNS record response: {e}"
            ))
        })?;

        if !parsed.success {
            let err_msg = parsed
                .errors
                .unwrap_or_default()
                .into_iter()
                .map(|e| format!("{}({})", e.message, e.code))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AppError::Proxy(format!(
                "Cloudflare failed to update DNS record (status {status}): {err_msg}"
            )));
        }

        Ok(())
    }

    /// 智能同步（Upsert）域名映射。若已有记录且 IP/代理相同则跳过，若不同则更新，若不存在则创建。
    pub async fn upsert_dns_record(
        &self,
        domain: &str,
        record_type: &str,
        ip: &str,
        proxied: bool,
    ) -> AppResult<DnsSyncOutcome> {
        let clean_domain = domain.trim().trim_end_matches('.');
        let clean_ip = ip.trim();
        if clean_domain.is_empty() {
            return Ok(DnsSyncOutcome::Skipped {
                domain: domain.to_string(),
                reason: "domain is empty".to_string(),
            });
        }
        if clean_ip.is_empty() {
            return Ok(DnsSyncOutcome::Skipped {
                domain: clean_domain.to_string(),
                reason: "ip is empty".to_string(),
            });
        }

        let zone_id = self.get_zone_id(clean_domain).await?;
        let existing_records = self
            .list_dns_records(&zone_id, record_type, clean_domain)
            .await?;

        if let Some(existing) = existing_records
            .into_iter()
            .find(|r| r.name.eq_ignore_ascii_case(clean_domain))
        {
            if existing.content == clean_ip && existing.proxied == proxied {
                return Ok(DnsSyncOutcome::Unchanged {
                    domain: clean_domain.to_string(),
                    ip: clean_ip.to_string(),
                });
            }

            self.update_dns_record(
                &zone_id,
                &existing.id,
                record_type,
                clean_domain,
                clean_ip,
                proxied,
                1,
            )
            .await?;
            Ok(DnsSyncOutcome::Updated {
                domain: clean_domain.to_string(),
                old_ip: existing.content,
                new_ip: clean_ip.to_string(),
            })
        } else {
            // If attempting to create A or AAAA record, check for an existing CNAME record
            // (e.g. Cloudflare Tunnel or external alias) to avoid breaking tunnels or triggering 81053 error.
            if record_type == "A" || record_type == "AAAA" {
                let cnames = self
                    .list_dns_records(&zone_id, "CNAME", clean_domain)
                    .await?;
                if let Some(cname) = cnames
                    .into_iter()
                    .find(|r| r.name.eq_ignore_ascii_case(clean_domain))
                {
                    return Ok(DnsSyncOutcome::Skipped {
                        domain: clean_domain.to_string(),
                        reason: format!(
                            "已有 CNAME 记录指向 '{}' (疑似 Cloudflare Tunnel 隧道或别名)，跳过创建 {} 记录以防冲突",
                            cname.content, record_type
                        ),
                    });
                }
            }

            self.create_dns_record(&zone_id, record_type, clean_domain, clean_ip, proxied, 1)
                .await?;
            Ok(DnsSyncOutcome::Created {
                domain: clean_domain.to_string(),
                ip: clean_ip.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn zone_id_discovery() {
        let mock_server = MockServer::start().await;

        // 子域未找到
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "panel.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": []
            })))
            .mount(&mock_server)
            .await;

        // 根域找到
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [
                    { "id": "zone-123456", "name": "example.com" }
                ]
            })))
            .mount(&mock_server)
            .await;

        let cf = CloudflareClient::new("test-token".into()).with_endpoints(
            mock_server.uri(),
            format!("{}/dns-query", mock_server.uri()),
        );

        let zone_id = cf.get_zone_id("panel.example.com").await.unwrap();
        assert_eq!(zone_id, "zone-123456");
    }

    #[tokio::test]
    async fn create_and_delete_txt_record() {
        let mock_server = MockServer::start().await;

        // POST 创建
        Mock::given(method("POST"))
            .and(path("/zones/zone-123/dns_records"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": { "id": "rec-999" }
            })))
            .mount(&mock_server)
            .await;

        // DELETE 删除
        Mock::given(method("DELETE"))
            .and(path("/zones/zone-123/dns_records/rec-999"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": { "id": "rec-999" }
            })))
            .mount(&mock_server)
            .await;

        let cf = CloudflareClient::new("test-token".into()).with_endpoints(
            mock_server.uri(),
            format!("{}/dns-query", mock_server.uri()),
        );

        let rec_id = cf
            .create_txt_record("zone-123", "_acme-challenge.example.com", "challenge-data")
            .await
            .unwrap();
        assert_eq!(rec_id, "rec-999");

        cf.delete_txt_record("zone-123", &rec_id).await.unwrap();
    }

    #[tokio::test]
    async fn doh_propagation_success() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/dns-query"))
            .and(query_param("name", "_acme-challenge.example.com"))
            .and(query_param("type", "TXT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Status": 0,
                "Answer": [
                    { "name": "_acme-challenge.example.com.", "type": 16, "TTL": 120, "data": "\"token-abc-xyz\"" }
                ]
            })))
            .mount(&mock_server)
            .await;

        let cf = CloudflareClient::new("test-token".into()).with_endpoints(
            mock_server.uri(),
            format!("{}/dns-query", mock_server.uri()),
        );

        let result = cf
            .wait_dns_propagation(
                "_acme-challenge.example.com",
                "token-abc-xyz",
                Duration::from_secs(5),
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dns_record_crud_and_upsert() {
        let mock_server = MockServer::start().await;

        // Subdomain zone not found
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "vps.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": []
            })))
            .mount(&mock_server)
            .await;

        // Root zone found
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [{ "id": "zone-ddns-123", "name": "example.com" }]
            })))
            .mount(&mock_server)
            .await;

        // First query: no record exists
        Mock::given(method("GET"))
            .and(path("/zones/zone-ddns-123/dns_records"))
            .and(query_param("type", "A"))
            .and(query_param("name", "vps.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": []
            })))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // No CNAME exists for vps.example.com
        Mock::given(method("GET"))
            .and(path("/zones/zone-ddns-123/dns_records"))
            .and(query_param("type", "CNAME"))
            .and(query_param("name", "vps.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": []
            })))
            .mount(&mock_server)
            .await;

        // Create record
        Mock::given(method("POST"))
            .and(path("/zones/zone-ddns-123/dns_records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": { "id": "rec-a-1" }
            })))
            .mount(&mock_server)
            .await;

        let cf = CloudflareClient::new("cf-secret-token".into()).with_endpoints(
            mock_server.uri(),
            format!("{}/dns-query", mock_server.uri()),
        );

        // Upsert should create record
        let outcome1 = cf
            .upsert_dns_record("vps.example.com", "A", "203.0.113.1", false)
            .await
            .unwrap();
        assert_eq!(
            outcome1,
            DnsSyncOutcome::Created {
                domain: "vps.example.com".to_string(),
                ip: "203.0.113.1".to_string()
            }
        );

        // Second query: record exists with 203.0.113.1
        Mock::given(method("GET"))
            .and(path("/zones/zone-ddns-123/dns_records"))
            .and(query_param("type", "A"))
            .and(query_param("name", "vps.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [{
                    "id": "rec-a-1",
                    "type": "A",
                    "name": "vps.example.com",
                    "content": "203.0.113.1",
                    "proxied": false,
                    "ttl": 1
                }]
            })))
            .mount(&mock_server)
            .await;

        // Upsert with same IP should be Unchanged
        let outcome2 = cf
            .upsert_dns_record("vps.example.com", "A", "203.0.113.1", false)
            .await
            .unwrap();
        assert_eq!(
            outcome2,
            DnsSyncOutcome::Unchanged {
                domain: "vps.example.com".to_string(),
                ip: "203.0.113.1".to_string()
            }
        );

        // Update record mock
        Mock::given(method("PUT"))
            .and(path("/zones/zone-ddns-123/dns_records/rec-a-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": { "id": "rec-a-1" }
            })))
            .mount(&mock_server)
            .await;

        // Upsert with new IP should be Updated
        let outcome3 = cf
            .upsert_dns_record("vps.example.com", "A", "203.0.113.2", false)
            .await
            .unwrap();
        assert_eq!(
            outcome3,
            DnsSyncOutcome::Updated {
                domain: "vps.example.com".to_string(),
                old_ip: "203.0.113.1".to_string(),
                new_ip: "203.0.113.2".to_string()
            }
        );

        // Test CNAME conflict avoidance: if domain has existing CNAME, creating A record is skipped
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "tunnel.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": []
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/zones/zone-ddns-123/dns_records"))
            .and(query_param("type", "A"))
            .and(query_param("name", "tunnel.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": []
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/zones/zone-ddns-123/dns_records"))
            .and(query_param("type", "CNAME"))
            .and(query_param("name", "tunnel.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "errors": [],
                "result": [{
                    "id": "rec-cname-1",
                    "type": "CNAME",
                    "name": "tunnel.example.com",
                    "content": "abc-tunnel.cfargotunnel.com",
                    "proxied": true,
                    "ttl": 1
                }]
            })))
            .mount(&mock_server)
            .await;

        let outcome4 = cf
            .upsert_dns_record("tunnel.example.com", "A", "203.0.113.2", false)
            .await
            .unwrap();
        assert!(matches!(outcome4, DnsSyncOutcome::Skipped { .. }));
        if let DnsSyncOutcome::Skipped { domain, reason } = outcome4 {
            assert_eq!(domain, "tunnel.example.com");
            assert!(reason.contains("abc-tunnel.cfargotunnel.com"));
        }
    }
}
