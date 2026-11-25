//! Pingora reverse proxy service implementation for 3r-ui panel.

use async_trait::async_trait;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::Error;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

pub struct PanelProxy {
    /// 本地 Axum 面板监听地址，例如 "127.0.0.1:2053"
    pub upstream_addr: String,
    /// 允许访问的绑定域名，例如 "panel.example.com"（全小写）
    pub allowed_domain: String,
    /// 允许访问的自定义面板路径前缀，例如 "/xxjjye"（若未配置则为空）
    pub panel_path: String,
    /// 允许访问的自定义节点订阅路径前缀，例如 "/sub" 或 "/mysub"
    pub sub_path: String,
}

impl PanelProxy {
    pub fn new(
        upstream_addr: String,
        allowed_domain: String,
        panel_path: String,
        sub_path: String,
    ) -> Self {
        Self {
            upstream_addr,
            allowed_domain: allowed_domain.trim().trim_end_matches('.').to_lowercase(),
            panel_path: crate::db::models::normalize_panel_path(&panel_path),
            sub_path: crate::db::models::normalize_sub_path(&sub_path),
        }
    }
}

/// 检查传入的请求路径是否被允许。
/// 当配置了 `panel_path` 时，仅允许精准匹配或其子路径，或公共订阅端点（`sub_path` 及标准的 `/sub`）。
pub fn is_path_allowed(req_path: &str, panel_path: &str, sub_path: &str) -> bool {
    let norm_sub = if sub_path.is_empty() {
        "/sub"
    } else {
        sub_path
    };
    // 允许客户端订阅请求（匹配配置的 sub_path 或标准的 /sub）
    if req_path == norm_sub
        || req_path.starts_with(&format!("{norm_sub}/"))
        || req_path == "/sub"
        || req_path.starts_with("/sub/")
    {
        return true;
    }

    if panel_path.is_empty() {
        return true;
    }

    // 允许面板登录入口及面板内子路由
    if req_path == panel_path {
        return true;
    }
    if let Some(rest) = req_path.strip_prefix(panel_path) {
        if rest.starts_with('/') {
            return true;
        }
    }
    false
}

/// 将请求路径中的 `panel_path` 前缀剔除，获得转发至内部 Axum 面板的目标路径。
pub fn strip_path_prefix<'a>(req_path: &'a str, panel_path: &str) -> Option<&'a str> {
    if panel_path.is_empty() {
        return None;
    }
    if req_path == panel_path {
        return Some("/");
    }
    if let Some(rest) = req_path.strip_prefix(panel_path) {
        if rest.starts_with('/') {
            return Some(rest);
        }
    }
    None
}

/// 重写上游重定向头 `Location`，确保浏览器自动重定向到带 `panel_path` 的完整路径。
pub fn rewrite_location(location: &str, panel_path: &str) -> Option<String> {
    if panel_path.is_empty() {
        return None;
    }
    if location.starts_with('/') && !location.starts_with(panel_path) {
        if location == "/" {
            Some(panel_path.to_string())
        } else {
            Some(format!("{panel_path}{location}"))
        }
    } else {
        None
    }
}

#[async_trait]
impl ProxyHttp for PanelProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    /// 校验请求 Host 是否匹配绑定域名，以及请求 Path 是否匹配自定义面板访问路径
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

        if !self.panel_path.is_empty() {
            let req_path = session.req_header().uri.path();
            if !is_path_allowed(req_path, &self.panel_path, &self.sub_path) {
                tracing::warn!(
                    req_path,
                    expected_prefix = %self.panel_path,
                    sub_prefix = %self.sub_path,
                    "proxy blocking request not matching custom panel or sub path"
                );
                session.respond_error(404).await?;
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

    /// 向发往本地 Axum 的请求注入 X-Forwarded-* 头并重写请求 URI
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
        if !self.panel_path.is_empty() {
            let _ = upstream_request.insert_header("X-Forwarded-Prefix", &self.panel_path);

            // 将请求路径中的自定义 path 前缀剔除，使本地 Axum 接收原生标准路径
            let current_uri = upstream_request.uri.clone();
            let current_path = current_uri.path();
            if let Some(target_path) = strip_path_prefix(current_path, &self.panel_path) {
                let query = current_uri
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                let new_path_and_query = format!("{}{query}", target_path);
                if let Ok(new_uri) = new_path_and_query.parse::<http::Uri>() {
                    upstream_request.set_uri(new_uri);
                }
            }
        }

        // 若配置了自定义订阅路径 (如 /mysub)，向上游转发时映射回标准的 /sub 路由
        if !self.sub_path.is_empty() && self.sub_path != "/sub" {
            let current_uri = upstream_request.uri.clone();
            let current_path = current_uri.path();
            if let Some(rest) = current_path.strip_prefix(&self.sub_path) {
                if rest.is_empty() || rest.starts_with('/') {
                    let query = current_uri
                        .query()
                        .map(|q| format!("?{q}"))
                        .unwrap_or_default();
                    let new_path_and_query = format!("/sub{rest}{query}");
                    if let Ok(new_uri) = new_path_and_query.parse::<http::Uri>() {
                        upstream_request.set_uri(new_uri);
                    }
                }
            }
        }

        Ok(())
    }

    /// 拦截上游响应，针对 3xx 重定向的 Location 头自动注入自定义 path 前缀
    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<(), Box<Error>> {
        if !self.panel_path.is_empty() {
            if let Some(location) = upstream_response.headers.get(axum::http::header::LOCATION) {
                if let Ok(loc_str) = location.to_str() {
                    if let Some(new_loc) = rewrite_location(loc_str, &self.panel_path) {
                        let _ = upstream_response.insert_header("Location", new_loc);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_path_normalization() {
        let proxy = PanelProxy::new(
            "127.0.0.1:2053".into(),
            "Panel.Example.COM.".into(),
            "/xxjjye/".into(),
            "mysub/".into(),
        );
        assert_eq!(proxy.allowed_domain, "panel.example.com");
        assert_eq!(proxy.panel_path, "/xxjjye");
        assert_eq!(proxy.sub_path, "/mysub");

        let root_proxy = PanelProxy::new(
            "127.0.0.1:2053".into(),
            "example.com".into(),
            "/".into(),
            "".into(),
        );
        assert_eq!(root_proxy.panel_path, "");
        assert_eq!(root_proxy.sub_path, "/sub");
    }

    #[test]
    fn path_filtering() {
        let prefix = "/xxjjye";
        let sub = "/mysub";
        // 精确匹配与子路径应放行
        assert!(is_path_allowed("/xxjjye", prefix, sub));
        assert!(is_path_allowed("/xxjjye/", prefix, sub));
        assert!(is_path_allowed("/xxjjye/admin", prefix, sub));
        assert!(is_path_allowed("/xxjjye/login", prefix, sub));
        assert!(is_path_allowed("/xxjjye/admin/inbound", prefix, sub));

        // 订阅路径应放行（无论是自定义的 /mysub 还是标准的 /sub）
        assert!(is_path_allowed("/mysub", prefix, sub));
        assert!(is_path_allowed("/mysub/token123", prefix, sub));
        assert!(is_path_allowed("/sub", prefix, sub));
        assert!(is_path_allowed("/sub/token123", prefix, sub));

        // 根路径、其他前缀或非法访问应阻断
        assert!(!is_path_allowed("/", prefix, sub));
        assert!(!is_path_allowed("/admin", prefix, sub));
        assert!(!is_path_allowed("/login", prefix, sub));
        assert!(!is_path_allowed("/xxjjye_other", prefix, sub));
        assert!(!is_path_allowed("/other", prefix, sub));
    }

    #[test]
    fn uri_path_stripping() {
        let prefix = "/xxjjye";
        assert_eq!(strip_path_prefix("/xxjjye", prefix), Some("/"));
        assert_eq!(strip_path_prefix("/xxjjye/", prefix), Some("/"));
        assert_eq!(strip_path_prefix("/xxjjye/admin", prefix), Some("/admin"));
        assert_eq!(
            strip_path_prefix("/xxjjye/admin/inbound", prefix),
            Some("/admin/inbound")
        );
        assert_eq!(strip_path_prefix("/other", prefix), None);
        assert_eq!(strip_path_prefix("/xxjjye_other", prefix), None);
    }

    #[test]
    fn location_rewriting() {
        let prefix = "/xxjjye";
        assert_eq!(rewrite_location("/", prefix), Some("/xxjjye".to_string()));
        assert_eq!(
            rewrite_location("/admin", prefix),
            Some("/xxjjye/admin".to_string())
        );
        assert_eq!(
            rewrite_location("/admin/settings", prefix),
            Some("/xxjjye/admin/settings".to_string())
        );
        // 已经带有 prefix 的不重复添加
        assert_eq!(rewrite_location("/xxjjye/admin", prefix), None);
        // 外部绝对 URL 不修改
        assert_eq!(rewrite_location("https://google.com", prefix), None);
    }
}
