// 节点管理模块 —— 多节点列表的持久化与操作
// 数据结构：data/nodes.json → { nodes: [{name, link}], current: index, use_tun, route_mode }
// 节点名称优先从链接 #fragment 提取（URL-decode），否则自动命名为 "节点1"、"节点2"…
// use_tun（虚拟网卡/接管所有应用）是全局设置，对所有节点统一生效，不存于单节点

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 单个节点条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub link: String,
    /// 最近一次延迟测速（毫秒）；0=未测，u64::MAX=不可达。不持久化（会话内临时数据）。
    #[serde(skip)]
    pub latency: u64,
}

/// 节点集合（JSON 持久化格式）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodesStore {
    pub nodes: Vec<NodeInfo>,
    pub current: usize, // 当前选中节点索引，无节点时默认 0
    #[serde(default = "default_use_tun")]
    pub use_tun: bool, // 全局虚拟网卡（接管所有应用）开关，对所有节点统一生效
    #[serde(default = "default_route_mode")]
    pub route_mode: String, // 路由模式："rule"（国内直连）| "global"（所有流量走代理）
}

/// serde 默认值：虚拟网卡默认开启
fn default_use_tun() -> bool {
    true
}

/// serde 默认值：路由模式缺省为规则模式
fn default_route_mode() -> String {
    "rule".to_string()
}

impl NodesStore {
    /// 从 data_dir/nodes.json 加载，不存在则返回空
    pub fn load(data_dir: &Path) -> Self {
        let path = path(data_dir);
        let mut store = match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        // 防御：钳制 current 不超过节点数，避免损坏的 nodes.json 使 current_node() 越界返回 None
        if store.nodes.is_empty() {
            store.current = 0;
        } else if store.current >= store.nodes.len() {
            store.current = store.nodes.len() - 1;
        }
        store
    }

    /// 保存到 data_dir/nodes.json
    pub fn save(&self, data_dir: &Path) {
        let path = path(data_dir);
        let _ = fs::create_dir_all(data_dir);
        // 兜底：current 不能越界
        let mut to_save = self.clone();
        if to_save.nodes.is_empty() || to_save.current >= to_save.nodes.len() {
            to_save.current = 0;
        }
        if let Ok(s) = serde_json::to_string_pretty(&to_save) {
            let _ = fs::write(&path, s);
        }
    }

    /// 添加节点（自动命名：优先从 #fragment 提取，否则用索引命名）
    /// use_tun 为全局设置，不再存于单节点，故此处不再接收该参数
    pub fn add(&mut self, link: &str, data_dir: &Path) {
        let name = extract_name(link, self.nodes.len() + 1);
        let node = NodeInfo {
            name,
            link: link.to_string(),
            latency: 0,
        };
        self.nodes.push(node);
        self.current = self.nodes.len() - 1; // 新节点自动选中
        self.save(data_dir);
    }

    /// 读取全局虚拟网卡开关
    pub fn use_tun(&self) -> bool {
        self.use_tun
    }

    /// 设置并持久化全局虚拟网卡开关
    pub fn set_use_tun(&mut self, v: bool, data_dir: &Path) {
        self.use_tun = v;
        self.save(data_dir);
    }

    /// 删除节点，自动调整 current 索引
    pub fn remove(&mut self, index: usize, data_dir: &Path) -> bool {
        if index >= self.nodes.len() {
            return false;
        }
        self.nodes.remove(index);
        if !self.nodes.is_empty() && self.current >= self.nodes.len() {
            self.current = self.nodes.len() - 1;
        }
        self.save(data_dir);
        true
    }

    /// 设置为当前节点
    pub fn set_current(&mut self, index: usize, data_dir: &Path) -> bool {
        if index >= self.nodes.len() {
            return false;
        }
        self.current = index;
        self.save(data_dir);
        true
    }

    /// 获取当前节点的引用
    pub fn current_node(&self) -> Option<&NodeInfo> {
        self.nodes.get(self.current)
    }

    /// 获取当前节点的可变引用
    pub fn current_node_mut(&mut self) -> Option<&mut NodeInfo> {
        self.nodes.get_mut(self.current)
    }

    /// 读取当前路由模式（统一归一为 "rule" / "global"）
    pub fn route_mode(&self) -> &str {
        if self.route_mode == "global" { "global" } else { "rule" }
    }

    /// 设置并持久化路由模式（"global" 以外一律视为 "rule"）
    pub fn set_route_mode(&mut self, mode: &str, data_dir: &Path) {
        self.route_mode = if mode == "global" { "global".to_string() } else { "rule".to_string() };
        self.save(data_dir);
    }
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("nodes.json")
}

/// 从链接中提取节点名称：
/// 优先用 #fragment（URL-decode），否则生成 "节点1"、"节点2" 等
fn extract_name(link: &str, auto_index: usize) -> String {
    // 尝试解析 fragment（# 后面的部分）
    if let Some(hash_pos) = link.rfind('#') {
        let raw = &link[hash_pos + 1..];
        // 简单的 URL-decode：只处理 %XX
        let decoded = url_decode(raw);
        if !decoded.is_empty() {
            return decoded;
        }
    }
    // 回退：根据协议前缀 + 服务器简写
    let server = extract_server_hint(link);
    format!("节点{} ({})", auto_index, server)
}

/// 从链接中提取服务器地址简写（用于自动命名展示）
fn extract_server_hint(link: &str) -> String {
    // 先去掉协议头
    let body = link
        .trim_start_matches("vmess://")
        .trim_start_matches("vless://")
        .trim_start_matches("trojan://");
    // 取 @ 后面的部分（vless/trojan），或直接取前 20 字符（vmess）
    if let Some(at_pos) = body.find('@') {
        let host_part = &body[at_pos + 1..];
        // 去掉端口和参数
        let host = host_part
            .split(':')
            .next()
            .unwrap_or(host_part)
            .split('?')
            .next()
            .unwrap_or(host_part)
            .split('#')
            .next()
            .unwrap_or(host_part);
        if !host.is_empty() {
            return host.to_string();
        }
    }
    // vmess 是 base64 JSON，取前 15 字符
    let preview: String = body.chars().take(15).collect();
    if preview.is_empty() {
        "未知".to_string()
    } else {
        preview
    }
}

/// 简单 URL decode（仅处理 %XX 十六进制转义，正确处理 UTF-8）
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex_str = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(hex) = u8::from_str_radix(hex_str, 16) {
                out.push(hex);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_name_from_fragment() {
        assert_eq!(
            extract_name("vless://uuid@1.2.3.4:443?sni=test.com#Tokyo-01", 1),
            "Tokyo-01"
        );
    }

    #[test]
    fn extract_name_url_encoded() {
        assert_eq!(
            extract_name("vless://uuid@1.2.3.4:443?sni=test.com#%E4%B8%9C%E4%BA%AC", 1),
            "东京"
        );
    }

    #[test]
    fn extract_name_auto() {
        let name = extract_name("vmess://dGhpcyBpcyBhIHRlc3Q=#", 3);
        assert!(name.starts_with("节点3"));
    }

    #[test]
    fn add_and_switch() {
        let dir = std::env::temp_dir().join(format!("sbtest_nodes_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut store = NodesStore::default();
        store.add("vless://uuid@1.2.3.4:443?sni=a.com#NodeA", &dir);
        assert_eq!(store.nodes.len(), 1);
        assert_eq!(store.current, 0);
        assert_eq!(store.current_node().unwrap().name, "NodeA");

        store.add("trojan://pw@5.6.7.8:8443?sni=b.com#NodeB", &dir);
        assert_eq!(store.nodes.len(), 2);
        assert_eq!(store.current, 1);

        // 切换回第一个
        store.set_current(0, &dir);
        assert_eq!(store.current, 0);

        // 删除节点
        store.remove(0, &dir);
        assert_eq!(store.nodes.len(), 1);
        assert_eq!(store.current, 0);
        assert_eq!(store.current_node().unwrap().name, "NodeB");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sbtest_nodes_persist_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut store = NodesStore::default();
        store.add("vless://uuid@1.2.3.4:443?sni=a.com#NodeA", &dir);
        store.add("trojan://pw@5.6.7.8:8443?sni=b.com#NodeB", &dir);
        store.set_current(1, &dir);

        // 重新加载验证
        let loaded = NodesStore::load(&dir);
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.current, 1);
        assert_eq!(loaded.current_node().unwrap().name, "NodeB");

        let _ = fs::remove_dir_all(&dir);
    }
}
