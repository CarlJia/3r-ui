//! Share link parser for proxy outbounds (VMess, VLESS, Shadowsocks, Socks5, HTTP).

use std::collections::HashMap;

use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedOutbound {
    pub tag: String,
    pub name: String,
    pub protocol: String,
    pub server_addr: String,
    pub server_port: u16,
    pub settings_json: String,
}

/// Parse a single proxy node share link (vmess://, vless://, ss://, socks5://, socks://, http://, https://)
pub fn parse_outbound_link(raw_link: &str) -> AppResult<ParsedOutbound> {
    let link = raw_link.trim();
    if link.is_empty() {
        return Err(AppError::Validation("分享链接不能为空".into()));
    }

    if link.starts_with("vmess://") {
        parse_vmess(link)
    } else if link.starts_with("vless://") {
        parse_vless(link)
    } else if link.starts_with("ss://") {
        parse_shadowsocks(link)
    } else if link.starts_with("socks5://") || link.starts_with("socks://") {
        parse_socks(link)
    } else if link.starts_with("http://") || link.starts_with("https://") {
        parse_http(link)
    } else {
        Err(AppError::Validation(
            "不支持的节点链接协议，目前仅支持 vmess://, vless://, ss://, socks5://, http://".into(),
        ))
    }
}

// ─── VMESS ───────────────────────────────────────────────────────────────────

fn parse_vmess(link: &str) -> AppResult<ParsedOutbound> {
    let b64 = link.trim_start_matches("vmess://").trim();
    let decoded_bytes = decode_base64_loose(b64)
        .map_err(|e| AppError::Validation(format!("VMess Base64 解码失败: {e}")))?;
    let json_str = String::from_utf8(decoded_bytes)
        .map_err(|_| AppError::Validation("VMess 配置内容不是有效的 UTF-8 字符串".into()))?;

    let v: Value = serde_json::from_str(&json_str)
        .map_err(|e| AppError::Validation(format!("VMess JSON 格式解析失败: {e}")))?;

    let server_addr = v
        .get("add")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Validation("VMess 节点缺少服务器地址 (add)".into()))?
        .to_string();

    let server_port = match v.get("port") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(443) as u16,
        Some(Value::String(s)) => s.parse::<u16>().unwrap_or(443),
        _ => 443,
    };

    let uuid = v
        .get("id")
        .and_then(|i| i.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Validation("VMess 节点缺少用户 ID (id)".into()))?
        .to_string();

    let alter_id = match v.get("aid") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    };

    let security = v
        .get("scy")
        .and_then(|s| s.as_str())
        .unwrap_or("auto")
        .to_string();

    let name = v
        .get("ps")
        .and_then(|p| p.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&server_addr)
        .to_string();

    let net = v.get("net").and_then(|n| n.as_str()).unwrap_or("tcp");
    let tls = v.get("tls").and_then(|t| t.as_str()).unwrap_or("");
    let host = v.get("host").and_then(|h| h.as_str()).unwrap_or("");
    let path = v.get("path").and_then(|p| p.as_str()).unwrap_or("");
    let sni = v.get("sni").and_then(|s| s.as_str()).unwrap_or(host);

    let mut stream_settings = json!({
        "network": net,
    });

    if tls == "tls" {
        stream_settings["security"] = json!("tls");
        if !sni.is_empty() {
            stream_settings["tlsSettings"] = json!({
                "serverName": sni,
            });
        }
    }

    if net == "ws" {
        let mut ws_obj = json!({});
        if !path.is_empty() {
            ws_obj["path"] = json!(path);
        }
        if !host.is_empty() {
            ws_obj["headers"] = json!({ "Host": host });
        }
        stream_settings["wsSettings"] = ws_obj;
    } else if net == "grpc" {
        let service_name = path;
        if !service_name.is_empty() {
            stream_settings["grpcSettings"] = json!({
                "serviceName": service_name,
            });
        }
    }

    let settings = json!({
        "uuid": uuid,
        "alter_id": alter_id,
        "security": security,
        "streamSettings": stream_settings,
    });

    let tag = generate_tag(&name, "vmess");

    Ok(ParsedOutbound {
        tag,
        name,
        protocol: "vmess".into(),
        server_addr,
        server_port,
        settings_json: serde_json::to_string_pretty(&settings).unwrap_or_default(),
    })
}

// ─── VLESS ───────────────────────────────────────────────────────────────────

fn parse_vless(link: &str) -> AppResult<ParsedOutbound> {
    let url =
        Url::parse(link).map_err(|e| AppError::Validation(format!("VLESS 链接格式错误: {e}")))?;

    let uuid = url.username().to_string();
    if uuid.is_empty() {
        return Err(AppError::Validation("VLESS 链接缺少 UUID".into()));
    }

    let server_addr = url
        .host_str()
        .ok_or_else(|| AppError::Validation("VLESS 链接缺少服务器地址".into()))?
        .to_string();

    let server_port = url.port().unwrap_or(443);

    let raw_name = url.fragment().unwrap_or("");
    let name = if raw_name.is_empty() {
        server_addr.clone()
    } else {
        percent_decode(raw_name)
    };

    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();

    let net = params
        .get("type")
        .or_else(|| params.get("transport"))
        .map(|s| s.as_str())
        .unwrap_or("tcp");
    let security = params.get("security").map(|s| s.as_str()).unwrap_or("none");
    let flow = params.get("flow").cloned().unwrap_or_default();
    let sni = params.get("sni").map(|s| s.as_str()).unwrap_or("");
    let host = params.get("host").map(|s| s.as_str()).unwrap_or("");
    let path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let pbk = params.get("pbk").map(|s| s.as_str()).unwrap_or("");
    let sid = params.get("sid").map(|s| s.as_str()).unwrap_or("");
    let fp = params.get("fp").map(|s| s.as_str()).unwrap_or("");

    let mut stream_settings = json!({
        "network": net,
    });

    if security == "tls" {
        stream_settings["security"] = json!("tls");
        let mut tls_obj = json!({});
        if !sni.is_empty() {
            tls_obj["serverName"] = json!(sni);
        }
        if !fp.is_empty() {
            tls_obj["fingerprint"] = json!(fp);
        }
        stream_settings["tlsSettings"] = tls_obj;
    } else if security == "reality" {
        stream_settings["security"] = json!("reality");
        let mut reality_obj = json!({
            "publicKey": pbk,
            "shortId": sid,
        });
        if !sni.is_empty() {
            reality_obj["serverName"] = json!(sni);
        }
        if !fp.is_empty() {
            reality_obj["fingerprint"] = json!(fp);
        }
        stream_settings["realitySettings"] = reality_obj;
    }

    if net == "ws" {
        let mut ws_obj = json!({});
        if !path.is_empty() {
            ws_obj["path"] = json!(path);
        }
        if !host.is_empty() {
            ws_obj["headers"] = json!({ "Host": host });
        }
        stream_settings["wsSettings"] = ws_obj;
    } else if net == "grpc" && !path.is_empty() {
        stream_settings["grpcSettings"] = json!({
            "serviceName": path,
        });
    }

    let mut settings = json!({
        "uuid": uuid,
        "encryption": "none",
        "streamSettings": stream_settings,
    });

    if !flow.is_empty() {
        settings["flow"] = json!(flow);
    }

    let tag = generate_tag(&name, "vless");

    Ok(ParsedOutbound {
        tag,
        name,
        protocol: "vless".into(),
        server_addr,
        server_port,
        settings_json: serde_json::to_string_pretty(&settings).unwrap_or_default(),
    })
}

// ─── SHADOWSOCKS ─────────────────────────────────────────────────────────────

fn parse_shadowsocks(link: &str) -> AppResult<ParsedOutbound> {
    let raw = link.trim_start_matches("ss://").trim();
    let (body, fragment) = match raw.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (raw, None),
    };

    let name = fragment
        .map(percent_decode)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ss-node".to_string());

    let (method, password, server_addr, server_port) = if body.contains('@') {
        // SIP002: base64(method:password)@host:port
        let (userinfo_b64, hostport) = body.split_once('@').unwrap();
        let userinfo_bytes = decode_base64_loose(userinfo_b64).map_err(|e| {
            AppError::Validation(format!("Shadowsocks 用户信息 Base64 解码失败: {e}"))
        })?;
        let userinfo = String::from_utf8(userinfo_bytes)
            .map_err(|_| AppError::Validation("Shadowsocks 用户信息不是合法 UTF-8".into()))?;
        let (m, p) = userinfo.split_once(':').ok_or_else(|| {
            AppError::Validation("Shadowsocks 用户信息缺少密码 (method:pass)".into())
        })?;

        let (h, port_str) = hostport
            .rsplit_once(':')
            .ok_or_else(|| AppError::Validation("Shadowsocks 缺少端口号 (host:port)".into()))?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| AppError::Validation("Shadowsocks 端口号不合法".into()))?;
        (m.to_string(), p.to_string(), h.to_string(), port)
    } else {
        // Legacy: base64(method:password@host:port)
        let decoded_bytes = decode_base64_loose(body)
            .map_err(|e| AppError::Validation(format!("Shadowsocks Base64 解码失败: {e}")))?;
        let decoded = String::from_utf8(decoded_bytes)
            .map_err(|_| AppError::Validation("Shadowsocks 内容不是合法 UTF-8".into()))?;

        let (userinfo, hostport) = decoded
            .split_once('@')
            .ok_or_else(|| AppError::Validation("Shadowsocks 链接缺少 @ 字符".into()))?;
        let (m, p) = userinfo
            .split_once(':')
            .ok_or_else(|| AppError::Validation("Shadowsocks 缺少密码".into()))?;
        let (h, port_str) = hostport
            .rsplit_once(':')
            .ok_or_else(|| AppError::Validation("Shadowsocks 缺少端口号".into()))?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| AppError::Validation("Shadowsocks 端口号不合法".into()))?;
        (m.to_string(), p.to_string(), h.to_string(), port)
    };

    let settings = json!({
        "method": method,
        "password": password,
    });

    let tag = generate_tag(&name, "ss");

    Ok(ParsedOutbound {
        tag,
        name,
        protocol: "shadowsocks".into(),
        server_addr,
        server_port,
        settings_json: serde_json::to_string_pretty(&settings).unwrap_or_default(),
    })
}

// ─── SOCKS5 ──────────────────────────────────────────────────────────────────

fn parse_socks(link: &str) -> AppResult<ParsedOutbound> {
    let url =
        Url::parse(link).map_err(|e| AppError::Validation(format!("Socks 链接格式错误: {e}")))?;

    let server_addr = url
        .host_str()
        .ok_or_else(|| AppError::Validation("Socks 链接缺少服务器地址".into()))?
        .to_string();

    let server_port = url.port().unwrap_or(1080);
    let name = url
        .fragment()
        .map(percent_decode)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| server_addr.clone());

    let user = url.username();
    let pass = url.password().unwrap_or("");

    let settings = if !user.is_empty() || !pass.is_empty() {
        json!({
            "users": [
                {
                    "user": user,
                    "pass": pass,
                }
            ]
        })
    } else {
        json!({})
    };

    let tag = generate_tag(&name, "socks");

    Ok(ParsedOutbound {
        tag,
        name,
        protocol: "socks".into(),
        server_addr,
        server_port,
        settings_json: serde_json::to_string_pretty(&settings).unwrap_or_default(),
    })
}

// ─── HTTP ────────────────────────────────────────────────────────────────────

fn parse_http(link: &str) -> AppResult<ParsedOutbound> {
    let url = Url::parse(link)
        .map_err(|e| AppError::Validation(format!("HTTP 代理链接格式错误: {e}")))?;

    let server_addr = url
        .host_str()
        .ok_or_else(|| AppError::Validation("HTTP 链接缺少服务器地址".into()))?
        .to_string();

    let server_port = url
        .port()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let name = url
        .fragment()
        .map(percent_decode)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| server_addr.clone());

    let user = url.username();
    let pass = url.password().unwrap_or("");

    let settings = if !user.is_empty() || !pass.is_empty() {
        json!({
            "users": [
                {
                    "user": user,
                    "pass": pass,
                }
            ]
        })
    } else {
        json!({})
    };

    let tag = generate_tag(&name, "http");

    Ok(ParsedOutbound {
        tag,
        name,
        protocol: "http".into(),
        server_addr,
        server_port,
        settings_json: serde_json::to_string_pretty(&settings).unwrap_or_default(),
    })
}

// ─── HELPERS ─────────────────────────────────────────────────────────────────

fn decode_base64_loose(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let mut s = input.trim().to_string();
    while s.len() % 4 != 0 {
        s.push('=');
    }

    STANDARD
        .decode(&s)
        .or_else(|_| URL_SAFE.decode(&s))
        .or_else(|_| {
            let s_nopad = input.trim().trim_end_matches('=');
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(s_nopad)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s_nopad))
        })
}

fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(c1), Some(c2)) = (h1, h2) {
                if let (Some(v1), Some(v2)) = (hex_val(c1), hex_val(c2)) {
                    bytes.push((v1 << 4) | v2);
                    continue;
                }
                bytes.push(b'%');
                bytes.push(c1);
                bytes.push(c2);
                continue;
            }
            bytes.push(b'%');
            if let Some(c1) = h1 {
                bytes.push(c1);
            }
            continue;
        } else if b == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn generate_tag(name: &str, protocol: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = sanitized.trim_matches('-');
    let suffix = if cleaned.is_empty() {
        protocol
    } else {
        cleaned
    };
    let candidate = format!("out-{}", suffix.to_lowercase());
    if candidate.len() > 32 {
        candidate[..32].trim_end_matches('-').to_string()
    } else {
        candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vmess() {
        let vmess_raw = r#"{"v":"2","ps":"HK-VMess-01","add":"hk.example.com","port":"443","id":"a8b9c0d1-1234-5678-9abc-def012345678","aid":"0","scy":"auto","net":"ws","type":"none","host":"hk.example.com","path":"/vmessws","tls":"tls"}"#;
        let b64 = STANDARD.encode(vmess_raw);
        let link = format!("vmess://{}", b64);

        let parsed = parse_outbound_link(&link).expect("should parse vmess link");
        assert_eq!(parsed.protocol, "vmess");
        assert_eq!(parsed.name, "HK-VMess-01");
        assert_eq!(parsed.server_addr, "hk.example.com");
        assert_eq!(parsed.server_port, 443);
        assert!(parsed
            .settings_json
            .contains("a8b9c0d1-1234-5678-9abc-def012345678"));
        assert!(parsed.settings_json.contains("/vmessws"));
    }

    #[test]
    fn test_parse_vless() {
        let link = "vless://12345678-1234-1234-1234-123456789abc@jp.example.com:443?type=ws&security=tls&sni=jp.example.com&path=%2Fvlessws#JP%20VLESS%20Node";
        let parsed = parse_outbound_link(link).expect("should parse vless link");
        assert_eq!(parsed.protocol, "vless");
        assert_eq!(parsed.name, "JP VLESS Node");
        assert_eq!(parsed.server_addr, "jp.example.com");
        assert_eq!(parsed.server_port, 443);
        assert!(parsed
            .settings_json
            .contains("12345678-1234-1234-1234-123456789abc"));
        assert!(parsed.settings_json.contains("/vlessws"));
        assert!(parsed.settings_json.contains("jp.example.com"));
    }

    #[test]
    fn test_parse_shadowsocks_sip002() {
        let userinfo = STANDARD.encode("aes-256-gcm:mypassword123");
        let link = format!("ss://{}@us.example.com:8388#US-SS-01", userinfo);
        let parsed = parse_outbound_link(&link).expect("should parse ss sip002 link");
        assert_eq!(parsed.protocol, "shadowsocks");
        assert_eq!(parsed.name, "US-SS-01");
        assert_eq!(parsed.server_addr, "us.example.com");
        assert_eq!(parsed.server_port, 8388);
        assert!(parsed.settings_json.contains("aes-256-gcm"));
        assert!(parsed.settings_json.contains("mypassword123"));
    }

    #[test]
    fn test_parse_socks5() {
        let link = "socks5://user1:pass1@sg.example.com:1080#SG-Socks5";
        let parsed = parse_outbound_link(link).expect("should parse socks5 link");
        assert_eq!(parsed.protocol, "socks");
        assert_eq!(parsed.name, "SG-Socks5");
        assert_eq!(parsed.server_addr, "sg.example.com");
        assert_eq!(parsed.server_port, 1080);
        assert!(parsed.settings_json.contains("user1"));
        assert!(parsed.settings_json.contains("pass1"));
    }

    #[test]
    fn test_parse_http() {
        let link = "http://myuser:mypass@proxy.example.com:8080#Corporate-HTTP";
        let parsed = parse_outbound_link(link).expect("should parse http link");
        assert_eq!(parsed.protocol, "http");
        assert_eq!(parsed.name, "Corporate-HTTP");
        assert_eq!(parsed.server_addr, "proxy.example.com");
        assert_eq!(parsed.server_port, 8080);
        assert!(parsed.settings_json.contains("myuser"));
        assert!(parsed.settings_json.contains("mypass"));
    }
}
