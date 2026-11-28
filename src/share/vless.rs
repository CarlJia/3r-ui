//! VLESS URI construction.
//!
//! Format (per xray-core VLESS spec, REALITY transport):
//!   `vless://<uuid>@<host>:<port>?type=xhttp&security=reality&pbk=<pb>&sni=<sni>&sid=<sid>&path=<path>&fp=chrome#<name>`
//!
//! The query parameters used here match what xray-core ≥ 1.8 expects: `type`
//! (transport) is `xhttp`; `security` is `reality`; `pbk` is the REALITY
//! public key base64url; `sni` and `sid` are passed through verbatim;
//! `path` is the inbound's `xhttpSettings.path` so a client hits the same
//! non-root path the server serves; `fp=chrome` is the uTLS fingerprint that
//! REALITY clients impersonate.

use urlencoding::encode;

use crate::db::models::{Client, Inbound};

/// Build a VLESS URI for a single client. `host` is the public hostname or
/// IP the operator wants the share link to point at — it is not derived from
/// the inbound `listen_port` because the inbound may bind to a private
/// interface that is unreachable from a remote client.
pub fn build_uri(client: &Client, inbound: &Inbound, host: &str) -> String {
    let name = client.name.clone().unwrap_or_else(|| "3r-ui".to_string());
    let tag = encode(&tag_for(&name));
    // The rendered config uses `effective_xhttp_path`, so the share link must
    // use the same value or the client would request a path the inbound
    // never answers on (`/` by default) and hang on connect.
    let path = encode(&crate::xray::config::effective_xhttp_path(inbound));
    format!(
        "vless://{uuid}@{host}:{port}?type=xhttp&security=reality&pbk={pbk}&sni={sni}&sid={sid}&path={path}&fp=chrome#{tag}",
        uuid = client.uuid,
        host = host,
        port = inbound.listen_port,
        pbk = inbound.public_key,
        sni = encode(&inbound.sni),
        sid = encode(&inbound.short_id),
        path = path,
        tag = tag,
    )
}

/// Strip characters VLESS URI fragments do not tolerate. The xray spec is
/// strict — `#` would close the fragment, `?` would start a new query. The
/// rest (whitespace, control bytes, etc.) trips up clients.
fn tag_for(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '#' | '?' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// Query parameter values that live outside the fragment may contain `=`/`
/// after urlencoding — but xray client parsers expect the `sni` and `sid`
/// fields verbatim. We percent-encode them through [`urlencoding`] for the
/// URI level, while leaving the `/` separator in the userinfo untouched.
mod urlencoding {
    pub fn encode(input: &str) -> String {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbound() -> Inbound {
        Inbound {
            id: 1,
            listen_ip: "0.0.0.0".into(),
            listen_port: 443,
            dest: "www.apple.com".into(),
            sni: "www.apple.com".into(),
            server_name: None,
            private_key: "PRIVATE".into(),
            public_key: "PUBKEY".into(),
            short_id: "abcd".into(),
            xhttp_path: Some("/r/abcdef0123456789".into()),
            protocol: "vless".into(),
            cert_mode: "reality".into(),
            custom_cert_path: "".into(),
            custom_key_path: "".into(),
            masquerade_url: "".into(),
            port_range: "".into(),
            transport_protocol: "TCP".into(),
            skip_cert_verify: 0,
            udp: 1,
            status: "active".into(),
            outbound_id: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn client(uuid: &str, name: Option<&str>) -> Client {
        Client {
            id: 1,
            inbound_id: 1,
            uuid: uuid.into(),
            name: name.map(|s| s.to_string()),
            quota_bytes: 0,
            expiry_at: None,
            status: "active".into(),
            sub_token: Some("token123".into()),
            mieru_username: Some("user1".into()),
            mieru_password: Some("pass1".into()),
            created_at: None,
        }
    }

    #[test]
    fn uri_has_all_required_query_params() {
        let uri = build_uri(
            &client("uuid-abc", Some("alice")),
            &inbound(),
            "host.example.com",
        );
        assert!(uri.starts_with("vless://uuid-abc@host.example.com:443?"));
        for needle in [
            "type=xhttp",
            "security=reality",
            "pbk=PUBKEY",
            "sni=www.apple.com",
            "sid=abcd",
            "path=%2Fr%2Fabcdef0123456789",
            "fp=chrome",
            "#alice",
        ] {
            assert!(uri.contains(needle), "{needle} missing in {uri}");
        }
    }

    #[test]
    fn blank_name_falls_back_to_a_default_tag() {
        let uri = build_uri(&client("uuid-abc", None), &inbound(), "h");
        assert!(uri.ends_with("#3r-ui"), "got {uri}");
    }

    #[test]
    fn tag_strips_uri_breaking_chars() {
        let uri = build_uri(&client("u", Some("a#b?c d")), &inbound(), "h");
        // The fragment can only contain one '#' — the first one we emit is
        // the fragment separator; subsequent '#' / '?' are turned into '_',
        // and the space is percent-encoded.
        assert!(uri.ends_with("#a_b_c%20d"), "got {uri}");
    }
}
