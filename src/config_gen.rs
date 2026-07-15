// 配置生成模块 —— 严格复刻 G:/自编译APK/box.html 的生成逻辑
// 1) baseConfig 模板（与网页 baseConfig 逐字一致）
// 2) parseLink：vmess / vless / trojan 链接解析为 outbound
// 3) generate_config：替换 proxy outbound + 更新 DNS 占位符，写出 data/config.json
//    use_tun=true 时沿用默认模板（含 tun-in）；=false 时移除 TUN 入站（仅 mixed 代理模式）

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use url::Url;

// ===== box.html 的 baseConfig（逐字搬运，仅去掉外层的 const baseConfig = / 分号）=====
const BASE_CONFIG: &str = r#"{
  "log": { "level": "info", "timestamp": true },
  "experimental": {
    "clash_api": {
      "external_controller": "127.0.0.1:9090",
      "external_ui_download_detour": "proxy",
      "default_mode": "rule"
    },
    "cache_file": {
      "enabled": true,
      "path": "cache.db"
    }
  },
  "dns": {
    "servers": [
      { "tag": "dns_direct", "type": "udp", "server": "119.29.29.29", "server_port": 53 },
      { "tag": "dns_proxy", "type": "https", "server": "1.1.1.1", "server_port": 443, "detour": "proxy" }
    ],
    "rules": [
      { "rule_set": "geosite-ads", "action": "predefined", "rcode": "NOERROR" },
      { "rule_set": "geosite-cn", "server": "dns_direct" },
      { "domain": ["node-placeholder.com"], "server": "dns_direct" },
      { "server": "dns_proxy" }
    ],
    "strategy": "ipv4_only"
  },
  "inbounds": [
    { "type": "mixed", "tag": "mixed-in", "listen": "127.0.0.1", "listen_port": 2080 },
    {
      "type": "tun", "tag": "tun-in",
      "interface_name": "sing-box-tun",
      "address": ["172.19.0.1/30"],
      "auto_route": true,
      "strict_route": true,
      "stack": "system"
    }
  ],
  "outbounds": [
    {
      "type": "vless",
      "tag": "proxy",
      "server": "node-placeholder.com",
      "server_port": 443,
      "uuid": "00000000-0000-0000-0000-000000000000"
    },
    { "type": "direct", "tag": "direct" },
    { "type": "block", "tag": "block" }
  ],
  "route": {
    "auto_detect_interface": true,
    "default_domain_resolver": "dns_direct",
    "final": "proxy",
    "rules": [
      { "port": [443], "protocol": ["quic"], "outbound": "block" },
      { "ip_cidr": ["174.132.167.252/32"], "outbound": "direct" },
      {
        "domain_suffix": [
          "microsoft.com", "msftconnecttest.com", "akamaitech.net",
          "edgesuite.net", "alidns.com", "doh.pub", "dot.pub",
          "360.cn", "onedns.net"
        ],
        "outbound": "direct"
      },
      { "inbound": ["mixed-in", "tun-in"], "action": "sniff" },
      { "protocol": "dns", "action": "hijack-dns" },
      { "protocol": "bittorrent", "outbound": "block" },
      { "rule_set": ["geosite-ads"], "outbound": "block" },
      { "rule_set": ["geosite-cn"], "outbound": "direct" },
      { "rule_set": ["geoip-cn"], "outbound": "direct" },
      { "ip_is_private": true, "outbound": "direct" }
    ],
    "rule_set": [
      {
        "tag": "geosite-ads",
        "type": "remote",
        "format": "binary",
        "url": "https://cdn.jsdelivr.net/gh/SagerNet/sing-geosite@rule-set/geosite-category-ads-all.srs",
        "download_detour": "proxy",
        "update_interval": "1d"
      },
      {
        "tag": "geosite-cn",
        "type": "remote",
        "format": "binary",
        "url": "https://cdn.jsdelivr.net/gh/SagerNet/sing-geosite@rule-set/geosite-cn.srs",
        "download_detour": "proxy",
        "update_interval": "1d"
      },
      {
        "tag": "geoip-cn",
        "type": "remote",
        "format": "binary",
        "url": "https://cdn.jsdelivr.net/gh/SagerNet/sing-geoip@rule-set/geoip-cn.srs",
        "download_detour": "proxy",
        "update_interval": "1d"
      }
    ]
  }
}"#;

/// 生成配置并写入 data_dir/config.json，返回格式化后的 JSON 字符串
/// use_tun=true：保留默认 TUN 入站（全局接管）；=false：仅 mixed 代理模式，移除 tun-in
/// listen_port：mixed 入站端口；api_port：clash_api 控制端口（二者一起避开占用）
/// route_mode："rule"（默认，中国域名/IP 直连、其余走代理）| "global"（除局域网/本机外所有流量走代理）
pub fn generate_config(link: &str, data_dir: &Path, use_tun: bool, listen_port: u16, api_port: u16, route_mode: &str) -> Result<String, String> {
    let mut cfg: Value = serde_json::from_str(BASE_CONFIG)
        .map_err(|e| format!("内置模板解析失败: {}", e))?;

    let proxy = parse_link(link)?;

    // 找到 proxy outbound 并替换，同时记录旧 server 以便更新 DNS 占位符
    let mut old_server: Option<String> = None;
    if let Some(outbounds) = cfg.get_mut("outbounds").and_then(|v| v.as_array_mut()) {
        for o in outbounds.iter() {
            if o.get("tag").and_then(|t| t.as_str()) == Some("proxy") {
                old_server = o.get("server").and_then(|s| s.as_str()).map(|s| s.to_string());
                break;
            }
        }
        for o in outbounds.iter_mut() {
            if o.get("tag").and_then(|t| t.as_str()) == Some("proxy") {
                *o = proxy.clone();
            }
        }
    }

    // 设置 mixed 入站监听端口（默认 2080，端口冲突时可换）
    if let Some(inbounds) = cfg.get_mut("inbounds").and_then(|v| v.as_array_mut()) {
        for ib in inbounds.iter_mut() {
            if ib.get("tag").and_then(|t| t.as_str()) == Some("mixed-in") {
                if let Some(obj) = ib.as_object_mut() {
                    obj.insert("listen_port".into(), json!(listen_port));
                }
            }
        }
    }

    // 设置 clash_api 控制端口（默认 9090，与 listen_port 同步避开占用）
    if let Some(exp) = cfg.get_mut("experimental").and_then(|v| v.as_object_mut()) {
        if let Some(api) = exp.get_mut("clash_api").and_then(|v| v.as_object_mut()) {
            api.insert("external_controller".into(), json!(format!("127.0.0.1:{}", api_port)));
        }
    }

    // 更新 DNS 规则里的占位域名 node-placeholder.com
    if let (Some(old), Some(new_server)) = (old_server, proxy.get("server").and_then(|s| s.as_str())) {
        if let Some(dns) = cfg.get_mut("dns") {
            if let Some(rules) = dns.get_mut("rules").and_then(|r| r.as_array_mut()) {
                for rule in rules.iter_mut() {
                    if let Some(domain) = rule.get_mut("domain").and_then(|d| d.as_array_mut()) {
                        for d in domain.iter_mut() {
                            if d.as_str() == Some(old.as_str()) {
                                *d = json!(new_server);
                            }
                        }
                    }
                }
            }
        }
    }

    // 不使用 TUN 时：移除 tun 入站，并清理路由中对 tun-in 的引用（退回纯 mixed 代理模式）
    if !use_tun {
        if let Some(inbounds) = cfg.get_mut("inbounds").and_then(|v| v.as_array_mut()) {
            inbounds.retain(|i| i.get("type").and_then(|t| t.as_str()) != Some("tun"));
        }
        if let Some(rules) = cfg
            .get_mut("route")
            .and_then(|r| r.get_mut("rules"))
            .and_then(|r| r.as_array_mut())
        {
            for rule in rules.iter_mut() {
                if let Some(inbound) = rule.get_mut("inbound").and_then(|i| i.as_array_mut()) {
                    inbound.retain(|i| i.as_str() != Some("tun-in"));
                    // 若 inbound 列表被清空（理论上不会发生，mixed-in 仍在），则删除该键以免全局生效
                    if inbound.is_empty() {
                        if let Some(obj) = rule.as_object_mut() {
                            obj.remove("inbound");
                        }
                    }
                }
            }
        }
    }

    // 全局模式：除局域网/本机地址外，所有流量都经代理出站（移除中国域名/IP 的直连规则）
    if route_mode == "global" {
        if let Some(rules) = cfg
            .get_mut("route")
            .and_then(|r| r.get_mut("rules"))
            .and_then(|r| r.as_array_mut())
        {
            // 仅移除「中国域名/中国 IP → direct」这两条规则；保留：
            //  - ip_is_private → direct（局域网/本机必须直连，否则代理自身会回环）
            //  - quic/bt → block、geosite-ads → block 等协议/广告拦截
            //  - sniff / hijack-dns 等 action 类规则
            rules.retain(|rule| {
                let is_cn_direct = rule
                    .get("rule_set")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .any(|x| x.as_str() == Some("geosite-cn") || x.as_str() == Some("geoip-cn"))
                    })
                    .unwrap_or(false);
                !is_cn_direct
            });
        }
        // DNS：中国域名改为经代理解析（统一走代理出口，避免解析污染）
        if let Some(dns_rules) = cfg
            .get_mut("dns")
            .and_then(|d| d.get_mut("rules"))
            .and_then(|r| r.as_array_mut())
        {
            for rule in dns_rules.iter_mut() {
                if rule.get("rule_set").and_then(|v| v.as_str()) == Some("geosite-cn") {
                    if let Some(obj) = rule.as_object_mut() {
                        obj.insert("server".into(), json!("dns_proxy"));
                    }
                }
            }
        }
        // clash_api 指示为全局模式（供面板显示）
        if let Some(exp) = cfg.get_mut("experimental").and_then(|v| v.as_object_mut()) {
            if let Some(api) = exp.get_mut("clash_api").and_then(|v| v.as_object_mut()) {
                api.insert("default_mode".into(), json!("global"));
            }
        }
    }

    let out = serde_json::to_string_pretty(&cfg).map_err(|e| format!("序列化失败: {}", e))?;
    let path = data_dir.join("config.json");
    fs::write(&path, &out).map_err(|e| format!("写入 {} 失败: {}", path.display(), e))?;
    Ok(out)
}

/// 解析单条节点链接，返回与 box.html parseLink 等价的 outbound 对象
pub fn parse_link(link: &str) -> Result<Value, String> {
    let link = link.trim();
    if link.is_empty() {
        return Err("链接为空".into());
    }

    // 等价于 box.html 的 buildTls
    let build_tls = |server_name: &str,
                     fingerprint: &str,
                     security_type: Option<&str>,
                     pbk: Option<&str>,
                     sid: Option<&str>,
                     alpns: &[String]|
     -> Value {
        let mut tls = Map::new();
        tls.insert("enabled".into(), json!(true));
        tls.insert("server_name".into(), json!(server_name));
        tls.insert(
            "utls".into(),
            json!({ "enabled": true, "fingerprint": fingerprint }),
        );
        if !alpns.is_empty() {
            tls.insert("alpn".into(), json!(alpns));
        }
        if security_type == Some("reality") {
            let mut reality = Map::new();
            reality.insert("enabled".into(), json!(true));
            reality.insert("public_key".into(), json!(pbk.unwrap_or("")));
            reality.insert("short_id".into(), json!(sid.unwrap_or("")));
            tls.insert("reality".into(), Value::Object(reality));
        }
        Value::Object(tls)
    };

    // 1. VMess
    if link.to_lowercase().starts_with("vmess://") {
        let b64 = link
            .strip_prefix("vmess://")
            .unwrap()
            .split('#')
            .next()
            .unwrap()
            .to_string();
        let decoded = decode_base64(&b64)?;
        let m: Map<String, Value> = serde_json::from_str(&decoded)
            .map_err(|_| "VMess 解析失败：节点数据格式异常".to_string())?;

        let get = |k: &str| m.get(k);
        let add = get("add").and_then(|x| x.as_str()).ok_or("VMess 缺少 add")?;
        let port = get("port").and_then(|x| x.as_i64()).unwrap_or(443) as u16;
        let id = get("id").and_then(|x| x.as_str()).ok_or("VMess 缺少 id")?;
        let scy = get("scy").and_then(|x| x.as_str()).unwrap_or("auto");

        let mut out = Map::new();
        out.insert("type".into(), json!("vmess"));
        out.insert("tag".into(), json!("proxy"));
        out.insert("server".into(), json!(add));
        out.insert("server_port".into(), json!(port));
        out.insert("uuid".into(), json!(id));
        out.insert("security".into(), json!(scy));
        out.insert("packet_encoding".into(), json!("xudp"));
        out.insert("tcp_fast_open".into(), json!(true));

        if get("tls").and_then(|x| x.as_str()) == Some("tls") {
            let sni = get("sni")
                .or_else(|| get("host"))
                .and_then(|x| x.as_str())
                .unwrap_or(add);
            let fp = get("fp").and_then(|x| x.as_str()).unwrap_or("chrome");
            let alpn = get("alpn")
                .and_then(|x| x.as_str())
                .map(|a| a.split(',').map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap_or_else(|| vec!["h2".into(), "http/1.1".into()]);
            out.insert("tls".into(), build_tls(sni, fp, None, None, None, &alpn));
        }

        match get("net").and_then(|x| x.as_str()).unwrap_or("tcp") {
            "ws" => {
                out.insert(
                    "transport".into(),
                    json!({
                        "type": "ws",
                        "path": get("path").and_then(|x| x.as_str()).unwrap_or("/"),
                        "headers": if let Some(h) = get("host").and_then(|x| x.as_str()) { json!({"Host": h}) } else { json!({}) }
                    }),
                );
            }
            "grpc" => {
                out.insert(
                    "transport".into(),
                    json!({ "type": "grpc", "service_name": get("path").and_then(|x| x.as_str()).unwrap_or("") }),
                );
            }
            "h2" | "http" => {
                out.insert(
                    "transport".into(),
                    json!({
                        "type": "http",
                        "host": [get("host").and_then(|x| x.as_str()).unwrap_or(add)],
                        "path": get("path").and_then(|x| x.as_str()).unwrap_or("/"),
                        "method": get("method").and_then(|x| x.as_str()).unwrap_or("GET")
                    }),
                );
            }
            _ => {}
        }

        return Ok(Value::Object(out));
    }

    // 2. VLESS / Trojan
    if link.to_lowercase().starts_with("vless://") || link.to_lowercase().starts_with("trojan://") {
        let url = Url::parse(link).map_err(|_| "链接格式无效，请检查是否完整".to_string())?;
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        let sni = params
            .get("sni")
            .cloned()
            .unwrap_or_else(|| url.host_str().unwrap_or("").to_string());
        let fp = params.get("fp").cloned().unwrap_or_else(|| "chrome".into());
        let security = params.get("security").map(|s| s.as_str()).unwrap_or("");
        let is_trojan = link.to_lowercase().starts_with("trojan://");

        let mut out = Map::new();
        out.insert("tag".into(), json!("proxy"));
        out.insert("server".into(), json!(url.host_str().unwrap_or("").to_string()));
        out.insert("server_port".into(), json!(url.port().unwrap_or(443)));
        out.insert("packet_encoding".into(), json!("xudp"));
        out.insert("tcp_fast_open".into(), json!(true));

        if is_trojan {
            out.insert("type".into(), json!("trojan"));
            out.insert("password".into(), json!(url.username().to_string()));
        } else {
            out.insert("type".into(), json!("vless"));
            out.insert("uuid".into(), json!(url.username().to_string()));
            if let Some(flow) = params.get("flow") {
                out.insert("flow".into(), json!(flow));
            }
        }

        let needs_tls =
            security == "tls" || security == "reality" || (is_trojan && security != "none");
        if needs_tls {
            let alpn = params
                .get("alpn")
                .map(|a| a.split(',').map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap_or_else(|| vec!["h2".into(), "http/1.1".into()]);
            let sec = if security.is_empty() { None } else { Some(security) };
            out.insert(
                "tls".into(),
                build_tls(
                    &sni,
                    &fp,
                    sec,
                    params.get("pbk").map(|s| s.as_str()),
                    params.get("sid").map(|s| s.as_str()),
                    &alpn,
                ),
            );
        }

        match params.get("type").map(|s| s.as_str()).unwrap_or("") {
            "ws" => {
                out.insert(
                    "transport".into(),
                    json!({
                        "type": "ws",
                        "path": params.get("path").cloned().unwrap_or_else(|| "/".into()),
                        "headers": if let Some(h) = params.get("host") { json!({"Host": h}) } else { json!({}) }
                    }),
                );
            }
            "grpc" => {
                out.insert(
                    "transport".into(),
                    json!({ "type": "grpc", "service_name": params.get("serviceName").cloned().or_else(|| params.get("path").cloned()).unwrap_or_default() }),
                );
            }
            "h2" | "http" => {
                out.insert(
                    "transport".into(),
                    json!({
                        "type": "http",
                        "host": [params.get("host").cloned().unwrap_or_else(|| url.host_str().unwrap_or("").to_string())],
                        "path": params.get("path").cloned().unwrap_or_else(|| "/".into()),
                        "method": params.get("method").cloned().unwrap_or_else(|| "GET".into())
                    }),
                );
            }
            _ => {}
        }

        return Ok(Value::Object(out));
    }

    Err("格式未能识别！目前仅支持 vmess://、vless:// 或 trojan:// 分享链接".into())
}

/// 等价于 box.html 的 decodeBase64：清理非 base64 字符、处理 URL-safe、补 padding
fn decode_base64(s: &str) -> Result<String, String> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii() && (c.is_alphanumeric() || *c == '+' || *c == '/' || *c == '=' || *c == '-' || *c == '_'))
        .collect();
    let mapped = cleaned.replace('-', "+").replace('_', "/");
    let padded = if !mapped.len().is_multiple_of(4) {
        format!("{}{}", mapped, "=".repeat(4 - mapped.len() % 4))
    } else {
        mapped
    };
    let bytes = B64
        .decode(&padded)
        .map_err(|e| format!("Base64 解码失败，请检查节点链接是否完整正确: {}", e))?;
    String::from_utf8(bytes).map_err(|e| format!("节点内容不是有效 UTF-8: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_data() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sbtest_{}_{}",
            std::process::id(),
            rand_suffix()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    fn parsed(link: &str) -> Value {
        parsed_with(link, true, "rule")
    }

    fn parsed_with(link: &str, use_tun: bool, route_mode: &str) -> Value {
        let dir = tmp_data();
        let out = generate_config(link, &dir, use_tun, 2080, 9090, route_mode).expect("generate failed");
        let v: Value = serde_json::from_str(&out).expect("invalid json");
        // 清理临时目录
        let _ = fs::remove_dir_all(&dir);
        v
    }

    #[test]
    fn vless_ws_tls() {
        let link = "vless://11111111-2222-3333-4444-555555555555@1.2.3.4:443?type=ws&security=tls&sni=example.com&path=%2Fws&host=cdn.example.com&fp=chrome#MyNode";
        let v = parsed(link);
        let outbounds = v.get("outbounds").unwrap().as_array().unwrap();
        let proxy = outbounds
            .iter()
            .find(|o| o.get("tag").unwrap() == "proxy")
            .unwrap();
        assert_eq!(proxy.get("type").unwrap(), "vless");
        assert_eq!(proxy.get("server").unwrap(), "1.2.3.4");
        assert_eq!(proxy.get("server_port").unwrap(), 443);
        assert_eq!(proxy.get("uuid").unwrap(), "11111111-2222-3333-4444-555555555555");
        assert!(proxy.get("tls").is_some());
        assert_eq!(proxy["tls"]["server_name"], "example.com");
        assert_eq!(proxy["transport"]["type"], "ws");
        assert_eq!(proxy["transport"]["path"], "/ws");
        // DNS 占位符被替换
        let dns_rules = v["dns"]["rules"].as_array().unwrap();
        assert!(dns_rules.iter().any(|r| r.get("domain").map(|d| d[0] == "1.2.3.4").unwrap_or(false)));
    }

    #[test]
    fn trojan_basic() {
        let link = "trojan://secretpass@5.6.7.8:8443?sni=edge.net&security=tls#TrojanNode";
        let v = parsed(link);
        let outbounds = v.get("outbounds").unwrap().as_array().unwrap();
        let proxy = outbounds
            .iter()
            .find(|o| o.get("tag").unwrap() == "proxy")
            .unwrap();
        assert_eq!(proxy.get("type").unwrap(), "trojan");
        assert_eq!(proxy.get("password").unwrap(), "secretpass");
        assert_eq!(proxy.get("server").unwrap(), "5.6.7.8");
        assert!(proxy.get("tls").is_some());
    }

    #[test]
    fn vmess_b64() {
        // {"add":"vm.example.com","port":443,"id":"uuid-1","aid":0,"scy":"auto","net":"ws","path":"/v","host":"h.example.com","tls":"tls","sni":"vm.example.com","fp":"chrome"}
        let raw = r#"{"add":"vm.example.com","port":443,"id":"uuid-1","aid":0,"scy":"auto","net":"ws","path":"/v","host":"h.example.com","tls":"tls","sni":"vm.example.com","fp":"chrome"}"#;
        let b64 = B64.encode(raw);
        let link = format!("vmess://{}#VMNode", b64);
        let v = parsed(&link);
        let outbounds = v.get("outbounds").unwrap().as_array().unwrap();
        let proxy = outbounds
            .iter()
            .find(|o| o.get("tag").unwrap() == "proxy")
            .unwrap();
        assert_eq!(proxy.get("type").unwrap(), "vmess");
        assert_eq!(proxy.get("server").unwrap(), "vm.example.com");
        assert_eq!(proxy.get("uuid").unwrap(), "uuid-1");
        assert_eq!(proxy["transport"]["type"], "ws");
        assert!(proxy.get("tls").is_some());
    }

    #[test]
    fn invalid_link_errors() {
        assert!(parse_link("").is_err());
        assert!(parse_link("http://not-a-node").is_err());
    }

    #[test]
    fn api_port_written() {
        let link = "vless://11111111-2222-3333-4444-555555555555@1.2.3.4:443?sni=example.com#MyNode";
        let dir = tmp_data();
        let out = generate_config(link, &dir, true, 2081, 9091, "rule").expect("generate failed");
        let v: Value = serde_json::from_str(&out).expect("invalid json");
        assert_eq!(
            v["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:9091",
            "clash_api external_controller 未使用传入的 api 端口"
        );
        let inbounds = v.get("inbounds").unwrap().as_array().unwrap();
        let mixed = inbounds
            .iter()
            .find(|i| i.get("tag").unwrap() == "mixed-in")
            .unwrap();
        assert_eq!(mixed.get("listen_port").unwrap(), 2081);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_remark_field_in_outbound() {
        // sing-box 1.13 的 outbounds 不支持 remark（v2rayN 写法），必须剔除
        let vless = "vless://11111111-2222-3333-4444-555555555555@1.2.3.4:443?sni=example.com#MyNode";
        let trojan = "trojan://secretpass@5.6.7.8:8443?sni=edge.net#TrojanNode";
        let vm = {
            let raw = r#"{"add":"vm.example.com","port":443,"id":"uuid-1","aid":0,"scy":"auto","net":"ws","path":"/v","host":"h.example.com","tls":"tls","sni":"vm.example.com","fp":"chrome"}"#;
            format!("vmess://{}#VMNode", B64.encode(raw))
        };
        for link in [vless, trojan, &vm] {
            let v = parsed(link);
            let proxy = v["outbounds"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o.get("tag").unwrap() == "proxy")
                .unwrap();
            assert!(
                proxy.get("remark").is_none(),
                "proxy outbound 仍含 remark 字段（link={}）",
                link
            );
            // tag 必须保持 proxy，否则路由规则引用失效
            assert_eq!(proxy.get("tag").unwrap(), "proxy");
        }
    }

    #[test]
    fn tun_off_removes_tun_inbound() {
        let link = "vless://11111111-2222-3333-4444-555555555555@1.2.3.4:443?sni=example.com#MyNode";
        // use_tun = false：不应再出现 tun 入站
        let v = parsed_with(link, false, "rule");
        let inbounds = v.get("inbounds").unwrap().as_array().unwrap();
        assert!(
            !inbounds
                .iter()
                .any(|i| i.get("type").and_then(|t| t.as_str()) == Some("tun")),
            "关闭 TUN 后仍残留 tun 入站"
        );
        // 路由 sniff 规则里不应再引用 tun-in
        let rules = v["route"]["rules"].as_array().unwrap();
        let sniff = rules
            .iter()
            .find(|r| r.get("action").and_then(|a| a.as_str()) == Some("sniff"));
        if let Some(sniff) = sniff {
            let inbound = sniff.get("inbound").and_then(|i| i.as_array());
            assert!(
                inbound.map(|arr| arr.iter().all(|x| x.as_str() != Some("tun-in"))).unwrap_or(true),
                "关闭 TUN 后路由仍引用 tun-in"
            );
        }
        // use_tun = true：保留 tun 入站
        let v2 = parsed_with(link, true, "rule");
        let inbounds2 = v2.get("inbounds").unwrap().as_array().unwrap();
        assert!(
            inbounds2
                .iter()
                .any(|i| i.get("type").and_then(|t| t.as_str()) == Some("tun")),
            "开启 TUN 时未保留 tun 入站"
        );
    }

    #[test]
    fn global_mode_routes_all_through_proxy() {
        let link = "vless://11111111-2222-3333-4444-555555555555@1.2.3.4:443?sni=example.com#MyNode";
        let dir = tmp_data();
        let out = generate_config(link, &dir, true, 2080, 9090, "global").expect("generate failed");
        let v: Value = serde_json::from_str(&out).expect("invalid json");
        let rules = v["route"]["rules"].as_array().unwrap();

        // 中国域名/IP 的直连规则应被移除
        assert!(
            !rules.iter().any(|r| r
                .get("rule_set")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().any(|e| e.as_str() == Some("geosite-cn") || e.as_str() == Some("geoip-cn")))
                .unwrap_or(false)),
            "全局模式下仍残留中国直连规则"
        );
        // 兜底仍为 proxy（全局=所有流量走代理）
        assert_eq!(v["route"]["final"], "proxy");
        // 私有网段直连必须保留（否则代理自身回环 / 局域网不可达）
        assert!(
            rules.iter().any(|r| r.get("ip_is_private").and_then(|x| x.as_bool()).unwrap_or(false)),
            "全局模式不应移除私有网段直连"
        );
        // clash_api default_mode 应为 global
        assert_eq!(v["experimental"]["clash_api"]["default_mode"], "global");
        // DNS：中国域名改为经代理解析
        let dns_rules = v["dns"]["rules"].as_array().unwrap();
        let cn_dns = dns_rules
            .iter()
            .find(|r| r.get("rule_set").and_then(|x| x.as_str()) == Some("geosite-cn"));
        assert_eq!(
            cn_dns.and_then(|r| r.get("server")).and_then(|s| s.as_str()),
            Some("dns_proxy"),
            "全局模式下中国域名 DNS 未改走代理"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
