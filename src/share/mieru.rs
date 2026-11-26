//! Mieru node share URI generator.

use crate::db::models::{Client, Inbound};

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

/// Build human-readable `mierus://` URI format.
///
/// Example: `mierus://username:password@example.com?port=2012-2022&protocol=TCP&mtu=1400#NodeTag`
pub fn build_uri(client: &Client, inbound: &Inbound, host: &str) -> String {
    let fallback_user;
    let username = if let Some(u) = client
        .mieru_username
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| client.name.as_deref().filter(|s| !s.trim().is_empty()))
    {
        u
    } else {
        fallback_user = format!("user_{}", &client.uuid[..8.min(client.uuid.len())]);
        &fallback_user
    };

    let password = client
        .mieru_password
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&client.uuid);

    let port_str = if !inbound.port_range.trim().is_empty() {
        inbound.port_range.trim().to_string()
    } else {
        inbound.listen_port.to_string()
    };

    let proto = if !inbound.transport_protocol.trim().is_empty() {
        inbound.transport_protocol.trim()
    } else {
        "TCP"
    };

    let tag = client.name.as_deref().unwrap_or("3R-UI Mieru");
    let tag_encoded = urlencoding::encode(tag);
    let username_encoded = urlencoding::encode(username);
    let password_encoded = urlencoding::encode(password);

    format!(
        "mierus://{username_encoded}:{password_encoded}@{host}?port={port_str}&protocol={proto}&mtu=1400#{tag_encoded}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mieru_uri_builder() {
        let client = Client {
            id: 1,
            inbound_id: 3,
            uuid: "7c90d0c0-1234-4abc-9def-111122223333".into(),
            name: Some("mieru-node".into()),
            quota_bytes: 0,
            expiry_at: None,
            status: "active".into(),
            sub_token: Some("sub1".into()),
            mieru_username: Some("alice".into()),
            mieru_password: Some("secretpass".into()),
            created_at: None,
        };

        let inbound = Inbound {
            id: 3,
            listen_ip: "0.0.0.0".into(),
            listen_port: 2027,
            dest: "".into(),
            sni: "".into(),
            server_name: None,
            private_key: "".into(),
            public_key: "".into(),
            short_id: "".into(),
            xhttp_path: None,
            protocol: "mieru".into(),
            cert_mode: "auto".into(),
            custom_cert_path: "".into(),
            custom_key_path: "".into(),
            masquerade_url: "".into(),
            port_range: "2012-2022".into(),
            transport_protocol: "TCP,UDP".into(),
            skip_cert_verify: 0,
            udp: 1,
            status: "active".into(),
            created_at: None,
            updated_at: None,
        };

        let uri = build_uri(&client, &inbound, "vpn.example.com");
        assert!(uri.starts_with("mierus://alice:secretpass@vpn.example.com?"));
        assert!(uri.contains("port=2012-2022"));
        assert!(uri.contains("protocol=TCP,UDP"));
        assert!(uri.contains("#mieru-node"));
    }

    #[test]
    fn test_mieru_uri_builder_encodes_credentials_and_aligns_fallback() {
        let inbound = Inbound {
            id: 3,
            listen_ip: "0.0.0.0".into(),
            listen_port: 8588,
            dest: "".into(),
            sni: "".into(),
            server_name: None,
            private_key: "".into(),
            public_key: "".into(),
            short_id: "".into(),
            xhttp_path: None,
            protocol: "mieru".into(),
            cert_mode: "auto".into(),
            custom_cert_path: "".into(),
            custom_key_path: "".into(),
            masquerade_url: "".into(),
            port_range: "".into(),
            transport_protocol: "TCP".into(),
            skip_cert_verify: 0,
            udp: 0,
            status: "active".into(),
            created_at: None,
            updated_at: None,
        };

        // Case 1: Special & non-ASCII characters in username/password are URL encoded
        let client_special = Client {
            id: 1,
            inbound_id: 3,
            uuid: "7c90d0c0-1234-4abc-9def-111122223333".into(),
            name: Some("carljia-日本 - Mieru".into()),
            quota_bytes: 0,
            expiry_at: None,
            status: "active".into(),
            sub_token: Some("sub1".into()),
            mieru_username: Some("carljia-日本".into()),
            mieru_password: Some("p@ss:w/rd#1?".into()),
            created_at: None,
        };
        let uri = build_uri(&client_special, &inbound, "120.253.251.134");
        assert!(uri.starts_with(
            "mierus://carljia-%E6%97%A5%E6%9C%AC:p%40ss%3Aw%2Frd%231%3F@120.253.251.134?"
        ));

        // Case 2: Fallback username without mieru_username or name aligns with config.rs (`user_{uuid..8}`)
        let client_fallback = Client {
            id: 2,
            inbound_id: 3,
            uuid: "7c90d0c0-1234-4abc-9def-111122223333".into(),
            name: None,
            quota_bytes: 0,
            expiry_at: None,
            status: "active".into(),
            sub_token: Some("sub2".into()),
            mieru_username: None,
            mieru_password: None,
            created_at: None,
        };
        let uri_fallback = build_uri(&client_fallback, &inbound, "120.253.251.134");
        assert!(uri_fallback.starts_with(
            "mierus://user_7c90d0c0:7c90d0c0-1234-4abc-9def-111122223333@120.253.251.134?"
        ));
    }
}
