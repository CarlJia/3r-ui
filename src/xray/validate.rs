//! Validation for the `REALITY dest` value the inbound form takes.
//!
//! `dest` is the host xray proxies a connection to when REALITY auth fails,
//! so any non-public host set here turns the proxy into an SSRF relay for
//! unauthenticated traffic. The form accepts free-form text today, which
//! means an operator can paste `127.0.0.1`, `localhost`, `10.0.0.1`, an
//! internal DNS name, or `https://...` and the panel writes it straight
//! into `xray/config.json`.
//!
//! This module rejects anything that is not a public hostname or IP literal
//! — including the obvious private/loopback/link-local ranges, IPv4-mapped
//! IPv6, IPv6 unique-local and documentation blocks — and resolves a
//! hostname before accepting it so a domain that points at an RFC1918
//! address is caught too. Resolution happens once at save time; the TOCTOU
//! window between validation and xray's own resolution is left to xray.
//!
//! `dest` is concatenated with `:443` downstream (see
//! `crate::xray::config::render_inbound`), so input that already contains
//! a port is rejected outright — otherwise the rendered value would be
//! `host:port:443` and xray would refuse to start.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use rand::RngCore;

use crate::error::AppResult;

/// Maximum DNS answers we accept before bailing. A malicious DNS response
/// with thousands of A/AAAA records should not pad the validator's memory.
const MAX_RESOLVED: usize = 32;

/// Validate `dest` and return the canonicalised form (no trimming, lower-cased
/// host — case differences matter only for `localhost`, which is rejected
/// anyway). The caller passes the form value through here before persisting
/// it to the `inbounds` row.
///
/// Errors are user-facing: they bubble up to the form, where the operator
/// sees them as the inbound-save error message.
pub async fn validate_dest(dest: &str) -> AppResult<String> {
    let raw = dest.trim();
    if raw.is_empty() {
        return Err(crate::error::AppError::Config(
            "REALITY dest 不能为空".into(),
        ));
    }

    // Reject anything that cannot be a hostname or IP literal: spaces, paths,
    // userinfo (`user@`), scheme markers, brackets, etc. IPv6 literals are
    // bracketed in URIs but xray takes the bare form; we accept either and
    // strip brackets later, so a stray bracket from copy-paste is still a
    // hard reject.
    if raw.contains(char::is_whitespace)
        || raw.contains('/')
        || raw.contains('@')
        || raw.contains('[')
        || raw.contains(']')
    {
        return Err(crate::error::AppError::Config(
            "REALITY dest 必须是主机名或 IP，不能含空格、路径、@ 或方括号".into(),
        ));
    }

    // Anything with a `:` is either an IPv6 literal or a `host:port` form.
    // The panel always appends `:443` downstream, so a user-supplied port
    // would silently produce `host:port:443` and a config that xray refuses
    // to start with.
    if raw.contains(':') {
        return Err(crate::error::AppError::Config(
            "REALITY dest 不能带端口（端口固定为 443）".into(),
        ));
    }

    // `localhost` parses as neither an IP literal nor a resolvable hostname
    // (depending on the resolver's `hosts` file), and is loopback by
    // definition regardless. Reject it before going further.
    if raw.eq_ignore_ascii_case("localhost") {
        return Err(crate::error::AppError::Config(
            "REALITY dest 不能是 localhost".into(),
        ));
    }

    // Hostnames get validated against RFC 1123 (letters/digits/hyphens,
    // dot-separated labels, ≤ 253 chars, each label ≤ 63). This catches
    // things the resolver might accept as a search-list trick (e.g.
    // `evil.example.com\0internal`) before the resolver sees them.
    if let Err(msg) = check_hostname_shape(raw) {
        return Err(crate::error::AppError::Config(msg));
    }

    // If it parses as an IP literal, check the IP directly. Otherwise resolve
    // and check every answer.
    if let Ok(ip) = raw.parse::<IpAddr>() {
        if ip_is_non_public(&ip) {
            return Err(crate::error::AppError::Config(format!(
                "REALITY dest {} 属于保留/私有范围（回环、RFC1918、链路本地、ULA 等）",
                ip
            )));
        }
        return Ok(raw.to_ascii_lowercase());
    }

    let addrs: Vec<IpAddr> = tokio::net::lookup_host(format!("{raw}:443"))
        .await
        .map_err(|e| crate::error::AppError::Config(format!("REALITY dest 域名解析失败: {e}")))?
        .take(MAX_RESOLVED)
        .map(|sa| sa.ip())
        .collect();

    if addrs.is_empty() {
        return Err(crate::error::AppError::Config(format!(
            "REALITY dest 域名 {raw} 解析无结果"
        )));
    }

    for ip in &addrs {
        if ip_is_non_public(ip) {
            return Err(crate::error::AppError::Config(format!(
                "REALITY dest 域名 {raw} 解析到保留/私有地址 {}",
                ip
            )));
        }
    }

    Ok(raw.to_ascii_lowercase())
}

/// Minimum length of a REALITY shortId. Below this an attacker can enumerate
/// the candidate set quickly: a single hex char is 16 possibilities, and
/// every probe requires a full TLS 1.3 + X25519 + auth handshake. With 8
/// hex chars the search space is 2^32 and the per-probe cost keeps brute
/// force impractical. The form's `random` button generates 16 by default, so
/// raising the floor to 8 only affects operators who hand-roll a value.
pub const SHORT_ID_MIN_LEN: usize = 8;

/// Generate a fresh XHTTP path. Each path is `/r/<16hex>`, which looks like
/// an ordinary asset URL rather than the `path` field name itself — a
/// passive scanner that only tries `/` (the xray documentation default) will
/// not find the endpoint.
pub fn generate_xhttp_path() -> String {
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut hex = String::with_capacity(16);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    format!("/r/{hex}")
}

/// Validate a path an operator typed or pasted. Server paths and share-URI
/// paths have to agree byte-for-byte, so we restrict the alphabet to the
/// URL-safe RFC 3986 unreserved + reserved-sub-delims subset. URL-encoded
/// sequences (`%xx`) would round-trip cleanly in xray, but accepting them
/// makes the share URI hard to read and trips up some clients' URL parsers,
/// so we refuse them up front and let an operator paste the raw form.
pub fn validate_xhttp_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("xhttp path 不能为空".into());
    }
    if !path.starts_with('/') {
        return Err("xhttp path 必须以 '/' 开头".into());
    }
    let body = &path[1..];
    if body.is_empty() {
        return Err("xhttp path 不能只是 '/'".into());
    }
    if body.len() > 200 {
        return Err("xhttp path 长度不能超过 200 字符".into());
    }
    if body
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '~')))
    {
        return Err("xhttp path 只能包含字母、数字、/、-、_、.、~".into());
    }
    Ok(())
}

/// Check that `id` is 1..=16 ASCII hex characters. The previous minimum was
/// 1, which the 1-char space made trivially enumerable.
pub fn validate_short_id(id: &str) -> Result<(), String> {
    let trimmed = id.trim();
    if trimmed.len() < SHORT_ID_MIN_LEN {
        return Err(format!(
            "shortId 至少需要 {SHORT_ID_MIN_LEN} 位十六进制字符（1 位可被快速爆破）"
        ));
    }
    if trimmed.len() > 16 {
        return Err("shortId 最长 16 位十六进制字符".into());
    }
    if !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("shortId 必须是十六进制字符".into());
    }
    Ok(())
}

/// Return `Ok` if `host` is a syntactically valid RFC 1123 hostname; `Err`
/// with a user-facing message otherwise. Empty, oversized (> 253 chars),
/// label-oversized (> 63 chars), or with leading/trailing hyphens or
/// underscores in a label, or containing characters outside
/// `[a-zA-Z0-9.-]`, are all rejected.
///
/// `validate_dest` already rejects non-hostname shapes via the input
/// scanner, but `lookup_host` would happily resolve `127.0.0.1.example.com`
/// (search-list style) if the resolver is configured that way; checking the
/// shape here is a backstop.
fn check_hostname_shape(host: &str) -> Result<(), String> {
    if host.is_empty() || host.len() > 253 {
        return Err(format!(
            "REALITY dest 长度必须在 1-253 之间（got {}）",
            host.len()
        ));
    }
    if host.starts_with('.') || host.ends_with('.') {
        return Err("REALITY dest 不能以点开头或结尾".into());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("REALITY dest 标签长度必须在 1-63 之间（{label}）"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "REALITY dest 标签不能以连字符开头或结尾（{label}）"
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("REALITY dest 标签含非法字符（{label}）"));
        }
    }
    Ok(())
}

/// True if `ip` is any range that must not be set as a REALITY dest —
/// covering loopback, RFC1918 private, link-local, unique-local, multicast,
/// broadcast, unspecified, documentation, and the reserved 240.0.0.0/4 +
/// 0.0.0.0/8 blocks. The function is conservative on purpose: every check
/// uses `std`'s stable predicates where available, and explicit segment
/// math where Rust still gates the helper behind an unstable feature flag.
pub fn ip_is_non_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_non_public(v4),
        IpAddr::V6(v6) => v6_is_non_public(v6),
    }
}

fn v4_is_non_public(v4: &Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()           // 127.0.0.0/8
        || v4.is_private()      // 10/8, 172.16/12, 192.168/16, 0.0.0.0/8 (also unspecified)
        || v4.is_link_local()   // 169.254.0.0/16
        || v4.is_unspecified()  // 0.0.0.0
        || v4.is_broadcast()    // 255.255.255.255
        || v4.is_multicast()    // 224.0.0.0/4
        || v4.is_documentation() // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || o[0] == 0            // 0.0.0.0/8 - "this network"
        || o[0] >= 240 // 240.0.0.0/4 - reserved (future use)
}

fn v6_is_non_public(v6: &Ipv6Addr) -> bool {
    let segs = v6.segments();
    v6.is_loopback()                       // ::1
        || v6.is_unspecified()             // ::
        || v6.is_multicast()               // ff00::/8
        || (segs[0] == 0xfe80)             // fe80::/10 - link-local (handwritten: is_unicast_link_local is 1.84+)
        || (segs[0] & 0xfe00) == 0xfc00    // fc00::/7 - unique local (std::net is unstable)
        || (segs[0] == 0x2001 && segs[1] == 0x0db8) // 2001:db8::/32 - documentation (unstable)
        // IPv4-mapped IPv6 — unwrap and re-check as IPv4 so ::ffff:10.0.0.1 is also caught.
        // (IPv4-compatible IPv6, ::x.x.x.x, was deprecated by RFC 4291 and is ignored.)
        || v6.to_ipv4_mapped().is_some_and(|v4| v4_is_non_public(&v4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_loopback_ipv4() {
        assert!(validate_dest("127.0.0.1").await.is_err());
    }

    #[tokio::test]
    async fn rejects_private_ipv4() {
        for ip in ["10.0.0.1", "172.16.0.1", "172.31.255.1", "192.168.1.1"] {
            assert!(validate_dest(ip).await.is_err(), "{ip} should be rejected");
        }
    }

    #[tokio::test]
    async fn rejects_link_local_ipv4() {
        assert!(validate_dest("169.254.169.254").await.is_err()); // AWS metadata service
    }

    #[tokio::test]
    async fn rejects_ipv4_with_port() {
        assert!(validate_dest("1.1.1.1:443").await.is_err());
    }

    #[tokio::test]
    async fn rejects_ipv6_literal() {
        // IPv6 literals contain `:`, which we reject as `host:port`. The
        // operator is expected to use a hostname; if they need an IPv6 dest
        // they can use a hostname that resolves to it.
        assert!(validate_dest("::1").await.is_err());
        assert!(validate_dest("[::1]").await.is_err());
        assert!(validate_dest("2001:db8::1").await.is_err());
    }

    #[tokio::test]
    async fn rejects_localhost_alias() {
        assert!(validate_dest("localhost").await.is_err());
    }

    #[tokio::test]
    async fn rejects_hostname_with_port() {
        assert!(validate_dest("example.com:443").await.is_err());
    }

    #[tokio::test]
    async fn rejects_reserved_v4_blocks() {
        for ip in [
            "0.0.0.0",
            "0.1.2.3",
            "240.0.0.1",
            "255.255.255.255",
            "224.0.0.1",
        ] {
            assert!(validate_dest(ip).await.is_err(), "{ip} should be rejected");
        }
    }

    #[tokio::test]
    async fn rejects_documentation_ipv4() {
        for ip in ["192.0.2.1", "198.51.100.1", "203.0.113.1"] {
            assert!(validate_dest(ip).await.is_err(), "{ip} should be rejected");
        }
    }

    #[test]
    fn rejects_short_shortid() {
        assert!(validate_short_id("a").is_err());
        assert!(validate_short_id("abcd").is_err());
    }

    #[test]
    fn accepts_eight_char_shortid() {
        assert!(validate_short_id("abcd1234").is_ok());
    }

    #[test]
    fn rejects_non_hex_shortid() {
        assert!(validate_short_id("ghijklmn").is_err());
    }

    #[test]
    fn rejects_overlong_shortid() {
        assert!(validate_short_id("0123456789abcdef0").is_err());
    }

    #[test]
    fn rejects_empty_and_whitespace_shortid() {
        assert!(validate_short_id("").is_err());
        assert!(validate_short_id("   ").is_err());
    }

    #[test]
    fn ip_helper_catches_ipv4_mapped_ipv6() {
        // ::ffff:127.0.0.1 — an IPv6 form of loopback. Unwrap and reject.
        let mapped = "::ffff:127.0.0.1".parse::<IpAddr>().unwrap();
        assert!(ip_is_non_public(&mapped));
    }

    #[test]
    fn ip_helper_catches_unique_local_ipv6() {
        // fc00::/7 — ULA.
        let ula: IpAddr = "fc00::1".parse().unwrap();
        assert!(ip_is_non_public(&ula));
    }

    #[test]
    fn hostname_shape_rejects_search_list_tricks() {
        // The hostname scanner catches things the resolver might still accept
        // via its search list.
        assert!(check_hostname_shape("").is_err());
        assert!(check_hostname_shape(".").is_err());
        assert!(check_hostname_shape("a.").is_err());
        assert!(check_hostname_shape("-a.com").is_err());
        assert!(check_hostname_shape("a-.com").is_err());
        assert!(check_hostname_shape("a b.com").is_err());
        assert!(check_hostname_shape(&"a".repeat(64)).is_err()); // label too long
    }

    #[test]
    fn generated_xhttp_path_is_valid_and_non_root() {
        for _ in 0..32 {
            let path = generate_xhttp_path();
            assert_ne!(path, "/");
            assert!(path.starts_with("/r/"));
            assert_eq!(path.len(), "/r/".len() + 16);
            assert!(validate_xhttp_path(&path).is_ok(), "invalid: {path}");
        }
    }

    #[test]
    fn generated_paths_are_distinct() {
        let a = generate_xhttp_path();
        let b = generate_xhttp_path();
        let c = generate_xhttp_path();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn validate_xhttp_path_accepts_simple_paths() {
        assert!(validate_xhttp_path("/r/abcdef0123456789").is_ok());
        assert!(validate_xhttp_path("/some/path").is_ok());
        assert!(validate_xhttp_path("/a-b_c.d~e").is_ok());
    }

    #[test]
    fn validate_xhttp_path_rejects_root_and_empty() {
        assert!(validate_xhttp_path("").is_err());
        assert!(validate_xhttp_path("/").is_err());
    }

    #[test]
    fn validate_xhttp_path_rejects_missing_leading_slash() {
        assert!(validate_xhttp_path("r/abc").is_err());
        assert!(validate_xhttp_path("abc").is_err());
    }

    #[test]
    fn validate_xhttp_path_rejects_unsafe_chars() {
        // Percent-encoded sequences, query strings, fragments, and whitespace
        // are all rejected: keeping the path in a URL-safe subset avoids
        // client-parser differences when the URI is pasted into a QR code.
        assert!(validate_xhttp_path("/a%20b").is_err());
        assert!(validate_xhttp_path("/a?b=c").is_err());
        assert!(validate_xhttp_path("/a#b").is_err());
        assert!(validate_xhttp_path("/a b").is_err());
        assert!(validate_xhttp_path("/a\\b").is_err());
    }

    #[test]
    fn validate_xhttp_path_rejects_overlong_input() {
        let long = format!("/{}", "a".repeat(201));
        assert!(validate_xhttp_path(&long).is_err());
    }
}
