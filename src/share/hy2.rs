//! Hysteria 2 URI construction.
//!
//! Format (per Hysteria 2 URI specification & modern client extensions):
//!   `hysteria2://<auth>@<host>:<port>/?sni=<sni>&insecure=0&udp=1#<name>`
//!   or with skip-cert-verify:
//!   `hysteria2://<auth>@<host>:<port>/?sni=<sni>&insecure=1&skip-cert-verify=1&udp=1#<name>`

use crate::db::models::{Client, Inbound};

/// Build a Hysteria 2 URI for a single client.
pub fn build_uri(client: &Client, inbound: &Inbound, host: &str) -> String {
    let name = client.name.clone().unwrap_or_else(|| "3r-ui".to_string());
    let tag = urlencoding::encode(&tag_for(&name));
    let sni = if !inbound.sni.trim().is_empty() {
        inbound.sni.trim()
    } else {
        host
    };
    let encoded_sni = urlencoding::encode(sni);

    let insecure = if inbound.skip_cert_verify == 1 { 1 } else { 0 };
    let udp = if inbound.udp == 1 { 1 } else { 0 };

    let mut params = vec![
        format!("sni={encoded_sni}"),
        format!("insecure={insecure}"),
    ];
    if insecure == 1 {
        params.push("skip-cert-verify=1".to_string());
    }
    if udp == 1 {
        params.push("udp=1".to_string());
    }
    let query = params.join("&");

    format!(
        "hysteria2://{auth}@{host}:{port}/?{query}#{tag}",
        auth = client.uuid,
        host = host,
        port = inbound.listen_port,
        query = query,
        tag = tag,
    )
}

fn tag_for(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '#' | '?' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

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
            id: 2,
            listen_port: 8443,
            dest: "".into(),
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
        }
    }

    fn client(uuid: &str, name: Option<&str>) -> Client {
        Client {
            id: 1,
            inbound_id: 2,
            uuid: uuid.into(),
            name: name.map(|s| s.to_string()),
            quota_bytes: 0,
            expiry_at: None,
            status: "active".into(),
            sub_token: Some("tok123".into()),
            mieru_username: Some("user1".into()),
            mieru_password: Some("pass1".into()),
            created_at: None,
        }
    }

    #[test]
    fn hy2_uri_format_default() {
        let uri = build_uri(
            &client("my-secret-pw", Some("test-hy2")),
            &inbound(),
            "panel.example.com",
        );
        assert!(uri.starts_with("hysteria2://my-secret-pw@panel.example.com:8443/?"));
        assert!(uri.contains("sni=hy2.example.com"));
        assert!(uri.contains("insecure=0"));
        assert!(uri.contains("udp=1"));
        assert!(!uri.contains("skip-cert-verify"));
        assert!(uri.ends_with("#test-hy2"));
    }

    #[test]
    fn hy2_uri_format_skip_cert_verify() {
        let mut ib = inbound();
        ib.skip_cert_verify = 1;
        let uri = build_uri(
            &client("my-secret-pw", Some("test-hy2")),
            &ib,
            "panel.example.com",
        );
        assert!(uri.contains("insecure=1"));
        assert!(uri.contains("skip-cert-verify=1"));
        assert!(uri.contains("udp=1"));
    }

    #[test]
    fn hy2_uri_format_udp_disabled() {
        let mut ib = inbound();
        ib.udp = 0;
        let uri = build_uri(
            &client("my-secret-pw", Some("test-hy2")),
            &ib,
            "panel.example.com",
        );
        assert!(uri.contains("insecure=0"));
        assert!(!uri.contains("udp=1"));
    }
}
