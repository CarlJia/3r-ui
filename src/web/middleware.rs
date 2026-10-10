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

pub async fn require_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let jar = axum_extra::extract::cookie::CookieJar::from_headers(request.headers());
    let expected = jar.get(CSRF_COOKIE).map(|c| c.value().to_string());

    let Some(admin) = resolve_admin(&state, &jar).await else {
        return Redirect::to("/").into_response();
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
