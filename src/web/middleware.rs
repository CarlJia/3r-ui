//! Route middleware: session authentication and CSRF verification.
//!
//! `require_session` is the single gate for `/admin/*`. It resolves the
//! session cookie to a [`CurrentAdmin`] and rejects anything that does not
//! resolve. For state-changing verbs it additionally requires the CSRF token
//! to be presented — either in the `X-CSRF-Token` header (what htmx sends)
//! or as a `csrf` field in a form body (what a plain HTML form can send) —
//! matched against the CSRF cookie in constant time.
//!
//! Both cookies are `SameSite=Strict`, so a cross-site submission carries
//! neither; the double-submit check is the second layer, not the first.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::crypto::session::{constant_time_eq, now_unix};
use crate::db::models::Admin;
use crate::web::{AppState, CSRF_COOKIE, SESSION_COOKIE};

/// The authenticated admin, injected into handlers by [`require_session`].
///
/// Handlers read this through `Extension<CurrentAdmin>`, which the session gate
/// inserts. Using `Extension` rather than a hand-written extractor keeps the
/// "this route must be behind the gate" invariant in one place: the gate is the
/// only code that writes the extension, and a handler that reads it without the
/// gate gets a 500 from axum rather than silently trusting a missing value.
#[derive(Clone, Debug)]
pub struct CurrentAdmin(pub Admin);

/// 访问路径前缀（如通过反代访问时设置的 `/xxjjye`），注入到请求 Extension 中。
#[derive(Clone, Debug, Default)]
pub struct BasePath(pub String);

/// 从请求头 `X-Forwarded-Prefix` 中提取并规范化路径前缀。
pub fn base_path_from_headers(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .map(crate::db::models::normalize_panel_path)
        .unwrap_or_default()
}

/// 客户端真实 IP 地址包装结构体，存储在请求 Extension 中
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIp(pub std::net::IpAddr);

impl std::fmt::Display for ClientIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 从请求头中提取真实客户端 IP（支持 X-Forwarded-For, X-Real-IP 等，回退至 127.0.0.1）
pub fn extract_client_ip(headers: &axum::http::HeaderMap) -> std::net::IpAddr {
    // 优先读取 X-Forwarded-For: client, proxy1, proxy2
    if let Some(forwarded_for) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = forwarded_for.split(',').next() {
            if let Ok(ip) = first.trim().parse::<std::net::IpAddr>() {
                return ip;
            }
        }
    }

    // 其次读取 X-Real-IP
    if let Some(real_ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        if let Ok(ip) = real_ip.trim().parse::<std::net::IpAddr>() {
            return ip;
        }
    }

    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

/// 全局前缀处理中间件：
/// 1. 识别 `X-Forwarded-Prefix` 或根据数据库中配置的 `proxy_settings.path` 自动适配请求路径；
/// 2. 若请求 URI 带有自定义 path 前缀，自动剥离前缀以便 Axum 路由正常匹配；
/// 3. 将 `BasePath` 存入请求扩展；
/// 4. 针对 3xx 重定向响应自动补充 `base_path` 前缀。
pub async fn base_path_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut base_path = base_path_from_headers(request.headers());

    if base_path.is_empty() {
        if let Ok(settings) = crate::proxy::config::get_proxy_settings(&state.pool).await {
            let p = settings.normalized_path();
            if !p.is_empty() {
                let req_path = request.uri().path();
                if req_path == p || req_path.starts_with(&format!("{p}/")) {
                    base_path = p;
                }
            }
        }
    }

    if !base_path.is_empty() {
        let uri = request.uri().clone();
        let path = uri.path();
        if let Some(target) = crate::proxy::server::strip_path_prefix(path, &base_path) {
            let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
            let new_path_and_query = format!("{}{query}", target);
            if let Ok(new_uri) = new_path_and_query.parse::<axum::http::Uri>() {
                *request.uri_mut() = new_uri;
            }
        }
    }

    // 若配置了自定义订阅路径 (如 /mysub)，将请求 URI 映射至内部 /sub/:token 路由
    if let Ok(settings) = crate::proxy::config::get_proxy_settings(&state.pool).await {
        let sub_p = settings.normalized_sub_path();
        if sub_p != "/sub" {
            let req_path = request.uri().path().to_string();
            if let Some(rest) = req_path.strip_prefix(&sub_p) {
                if rest.is_empty() || rest.starts_with('/') {
                    let query = request
                        .uri()
                        .query()
                        .map(|q| format!("?{q}"))
                        .unwrap_or_default();
                    let new_path_and_query = format!("/sub{rest}{query}");
                    if let Ok(new_uri) = new_path_and_query.parse::<axum::http::Uri>() {
                        *request.uri_mut() = new_uri;
                    }
                }
            }
        }
    }

    request.extensions_mut().insert(BasePath(base_path.clone()));

    let client_ip = extract_client_ip(request.headers());
    request.extensions_mut().insert(ClientIp(client_ip));

    let mut response = next.run(request).await;

    if !base_path.is_empty() {
        if let Some(location) = response.headers().get(axum::http::header::LOCATION) {
            if let Ok(loc_str) = location.to_str() {
                if let Some(new_loc) = crate::proxy::server::rewrite_location(loc_str, &base_path) {
                    if let Ok(header_val) = new_loc.parse() {
                        response
                            .headers_mut()
                            .insert(axum::http::header::LOCATION, header_val);
                    }
                }
            }
        }
    }

    response
}

pub async fn require_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let base_path = base_path_from_headers(request.headers());
    let jar = axum_extra::extract::cookie::CookieJar::from_headers(request.headers());
    let expected = jar.get(CSRF_COOKIE).map(|c| c.value().to_string());

    let Some(admin) = resolve_admin(&state, &jar).await else {
        let redirect_target = if base_path.is_empty() {
            "/".to_string()
        } else {
            base_path
        };
        return Redirect::to(&redirect_target).into_response();
    };

    let (mut parts, body) = request.into_parts();

    // A state-changing request with no CSRF cookie at all cannot be validated,
    // so it is rejected rather than waved through. (An authenticated browser
    // always has one, because login sets it.)
    if !is_safe_method(&parts.method) {
        let Some(expected) = expected.as_deref() else {
            return (StatusCode::FORBIDDEN, "missing CSRF cookie").into_response();
        };
        // Buffer so a form-encoded `csrf` field can be inspected, then hand the
        // unmodified bytes back to the handler.
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .unwrap_or_default();
        if !csrf_matches(expected, &parts.headers, &bytes) {
            return (StatusCode::FORBIDDEN, "missing or invalid CSRF token").into_response();
        }
        parts.extensions.insert(admin);
        return next
            .run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    }

    parts.extensions.insert(admin);
    next.run(Request::from_parts(parts, body)).await
}

/// Accept the token from the `X-CSRF-Token` header, or from a `csrf=` field in
/// an `application/x-www-form-urlencoded` body.
fn csrf_matches(expected: &str, headers: &axum::http::HeaderMap, body: &[u8]) -> bool {
    if let Some(presented) = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()) {
        return constant_time_eq(expected, presented);
    }
    let is_form = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return false;
    }
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    text.split('&').any(|pair| {
        let Some((k, v)) = pair.split_once('=') else {
            return false;
        };
        k == "csrf" && percent_decode(v) == expected
    })
}

/// Minimal percent-decoding for a single form value. `+` is a space in
/// urlencoded bodies but never appears in our base64url token alphabet.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn resolve_admin(
    state: &AppState,
    jar: &axum_extra::extract::cookie::CookieJar,
) -> Option<CurrentAdmin> {
    let session_id = jar.get(SESSION_COOKIE)?.value().to_string();

    let (admin_id, expires_at): (i64, i64) =
        sqlx::query_as("SELECT admin_id, expires_at FROM sessions WHERE session_id = ?")
            .bind(&session_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()?;

    if expires_at <= now_unix() {
        return None;
    }

    let admin = sqlx::query_as::<_, Admin>(
        "SELECT id, username, password_hash, created_at FROM admins WHERE id = ?",
    )
    .bind(admin_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()?;

    Some(CurrentAdmin(admin))
}

fn is_safe_method(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    )
}

/// Read the CSRF token out of the request cookies, for embedding in a form.
pub fn csrf_from_cookies(jar: &axum_extra::extract::cookie::CookieJar) -> String {
    jar.get(CSRF_COOKIE)
        .map(|c| c.value().to_string())
        .unwrap_or_default()
}

/// Security headers applied to every rendered page.
pub const SECURITY_HEADERS: [(&str, &str); 4] = [
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "same-origin"),
    (
        "content-security-policy",
        "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'",
    ),
];

/// Render an HTML page with the standard security headers and any Set-Cookie
/// values from `jar` appended. Shared by the admin page handlers so each one
/// doesn't hand-maintain the cookie-forwarding + header decoration.
pub(crate) fn render_page(
    jar: axum_extra::extract::cookie::CookieJar,
    status: StatusCode,
    body: String,
) -> Response {
    let mut response = (status, axum::response::Html(body)).into_response();
    for cookie in jar.iter() {
        if let Ok(v) = cookie.encoded().to_string().parse() {
            response
                .headers_mut()
                .append(axum::http::header::SET_COOKIE, v);
        }
    }
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("static header name"),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

/// Minimal percent-encoding for query-string values in template-rendered
/// links. Shared by the share-link builder and the logs page's pagination.
pub(crate) fn url_encode(input: &str) -> String {
    input
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::server::{rewrite_location, strip_path_prefix};
    use axum::http::HeaderMap;

    #[test]
    fn test_base_path_from_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(base_path_from_headers(&headers), "");

        headers.insert("x-forwarded-prefix", "/xxjjye".parse().unwrap());
        assert_eq!(base_path_from_headers(&headers), "/xxjjye");

        headers.insert("x-forwarded-prefix", "/xxjjye/".parse().unwrap());
        assert_eq!(base_path_from_headers(&headers), "/xxjjye");

        headers.insert("x-forwarded-prefix", "/".parse().unwrap());
        assert_eq!(base_path_from_headers(&headers), "");
    }

    #[test]
    fn test_strip_path_prefix() {
        assert_eq!(
            strip_path_prefix("/xxjjye/admin", "/xxjjye"),
            Some("/admin")
        );
        assert_eq!(strip_path_prefix("/xxjjye", "/xxjjye"), Some("/"));
        assert_eq!(strip_path_prefix("/xxjjye/", "/xxjjye"), Some("/"));
        assert_eq!(strip_path_prefix("/other/admin", "/xxjjye"), None);
    }

    #[test]
    fn test_rewrite_location() {
        assert_eq!(
            rewrite_location("/admin", "/xxjjye"),
            Some("/xxjjye/admin".to_string())
        );
        assert_eq!(
            rewrite_location("/", "/xxjjye"),
            Some("/xxjjye".to_string())
        );
        assert_eq!(rewrite_location("/xxjjye/admin", "/xxjjye"), None);
        assert_eq!(rewrite_location("https://google.com", "/xxjjye"), None);
    }
}
