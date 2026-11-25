//! Hysteria 2 URI construction.
//!
//! Format (per Hysteria 2 URI specification):
//!   `hysteria2://<auth>@<host>:<port>/?sni=<sni>&insecure=0#<name>`

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

    format!(
        "hysteria2://{auth}@{host}:{port}/?sni={sni}&insecure=0#{tag}",
        auth = client.uuid,
        host = host,
        port = inbound.listen_port,
        sni = encoded_sni,
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
    fn hy2_uri_format() {
        let uri = build_uri(
            &client("my-secret-pw", Some("test-hy2")),
            &inbound(),
            "panel.example.com",
        );
        assert!(uri.starts_with("hysteria2://my-secret-pw@panel.example.com:8443/?"));
        assert!(uri.contains("sni=hy2.example.com"));
        assert!(uri.contains("insecure=0"));
        assert!(uri.ends_with("#test-hy2"));
    }
}
