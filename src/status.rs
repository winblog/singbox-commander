// 状态/流量轮询 —— 直连 sing-box clash_api (默认 127.0.0.1:9090)
// 不依赖任何 HTTP 库，用 std::net::TcpStream 手写最小 HTTP/1.1 GET
// /traffic 与 /connections 都是流式/长连端点，故读到第一条完整 JSON 即返回，不等待连接关闭。

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const API_HOST: &str = "127.0.0.1";

/// 发起一次 GET，读到第一条完整 JSON 对象（{ ... }）即立即返回；避免在流式端点上死等。
fn http_get_first_json(path: &str, api_port: u16, timeout: Duration) -> Option<Value> {
    let mut stream = TcpStream::connect((API_HOST, api_port)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {API_HOST}:{api_port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // 定位 HTTP body 起点（跳过响应头）
                let body_start = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                    .unwrap_or(0);
                // 在 body 内找第一个完整 JSON 对象 { ... }
                if let Some(rel) = buf[body_start..].iter().position(|&b| b == b'{') {
                    let abs = body_start + rel;
                    // 用栈匹配大括号以支持嵌套（connections 返回的对象含数组/子对象）
                    let mut depth = 0i32;
                    let mut in_str = false;
                    let mut esc = false;
                    for (i, &b) in buf[abs..].iter().enumerate() {
                        if in_str {
                            if esc {
                                esc = false;
                            } else if b == b'\\' {
                                esc = true;
                            } else if b == b'"' {
                                in_str = false;
                            }
                            continue;
                        }
                        match b {
                            b'"' => in_str = true,
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    let s = String::from_utf8_lossy(&buf[abs..abs + i + 1]);
                                    if let Ok(v) = serde_json::from_str::<Value>(&s) {
                                        return Some(v);
                                    }
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if buf.len() > 2_000_000 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    None
}

/// 瞬时上下行速度（来自 /traffic，返回 (up_bytes_per_sec, down_bytes_per_sec)）
/// clash_api 的 /traffic 连接后立刻推一条 {up,down}（最近一秒流量），读到即返回。
pub fn fetch_traffic(api_port: u16) -> Option<(u64, u64)> {
    let v = http_get_first_json("/traffic", api_port, Duration::from_millis(1300))?;
    let up = v.get("up").and_then(|x| x.as_u64()).unwrap_or(0);
    let down = v.get("down").and_then(|x| x.as_u64()).unwrap_or(0);
    Some((up, down))
}

/// 累计流量与活跃连接数（来自 /connections）
/// 返回 (upload_total, download_total, 活跃连接数)
pub fn fetch_connections(api_port: u16) -> Option<(u64, u64, usize)> {
    let v = http_get_first_json("/connections", api_port, Duration::from_millis(1300))?;
    let up = v.get("uploadTotal").and_then(|x| x.as_u64()).unwrap_or(0);
    let down = v.get("downloadTotal").and_then(|x| x.as_u64()).unwrap_or(0);
    let count = v
        .get("connections")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    Some((up, down, count))
}
