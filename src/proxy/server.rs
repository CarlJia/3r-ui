//! Pingora reverse proxy service implementation for 3r-ui panel.

use async_trait::async_trait;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Error;
use pingora_http::RequestHeader;
use pingora_proxy::{ProxyHttp, Session};

pub struct PanelProxy {
    /// 本地 Axum 面板监听地址，例如 "127.0.0.1:2053"
    pub upstream_addr: String,
    /// 允许访问的绑定域名，例如 "panel.example.com"（全小写）
    pub allowed_domain: String,
}

impl PanelProxy {
    pub fn new(upstream_addr: String, allowed_domain: String) -> Self {
        Self {
            upstream_addr,
            allowed_domain: allowed_domain.trim().trim_end_matches('.').to_lowercase(),
        }
    }
}

#[async_trait]
impl ProxyHttp for PanelProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    /// 校验请求 Host 是否匹配绑定域名；不匹配则返回 403 阻断
    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool, Box<Error>> {
        if !self.allowed_domain.is_empty() {
            let req_header = session.req_header();
            let host_opt = req_header
                .headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .or_else(|| req_header.uri.host());

            let is_match = match host_opt {
                Some(host) => {
                    let clean_host = host.split(':').next().unwrap_or(host).trim().to_lowercase();
                    clean_host == self.allowed_domain
                }
                None => false,
            };

            if !is_match {
                tracing::warn!(
                    req_host = ?host_opt,
                    allowed = %self.allowed_domain,
                    "proxy blocking request with mismatched Host header"
                );
                session.respond_error(403).await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// 定义代理后端 upstream（指向本地 Axum HTTP 服务）
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>, Box<Error>> {
        let (host, port) = match self.upstream_addr.split_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().unwrap_or(2053)),
            None => ("127.0.0.1", 2053),
        };
        // 纯 HTTP 反向代理至本地 Axum
        let peer = HttpPeer::new((host, port), false, String::new());
        Ok(Box::new(peer))
    }

    /// 向发往本地 Axum 的请求注入 X-Forwarded-* 头
    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<(), Box<Error>> {
        if let Some(client_addr) = session.client_addr() {
            let client_ip = match client_addr.as_inet() {
                Some(inet) => inet.ip().to_string(),
                None => String::new(),
            };
            if !client_ip.is_empty() {
                let _ = upstream_request.insert_header("X-Forwarded-For", client_ip);
            }
        }
        let _ = upstream_request.insert_header("X-Forwarded-Proto", "https");
        if !self.allowed_domain.is_empty() {
            let _ = upstream_request.insert_header("X-Forwarded-Host", &self.allowed_domain);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_normalization() {
        let proxy = PanelProxy::new("127.0.0.1:2053".into(), "Panel.Example.COM.".into());
        assert_eq!(proxy.allowed_domain, "panel.example.com");
    }
}
