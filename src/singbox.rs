// 进程管理 + 共享状态 + 系统集成
// 负责：拉起/停止 data/sing-box.exe、捕获日志、管理员权限检测与重启
// 系统代理（Windows 注册表）、开机自启、流量持久化、启动失败诊断

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

// CREATE_NO_WINDOW：阻止控制台子进程弹出黑框（GUI 父进程拉起 console 子进程时必需）
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
use std::os::windows::process::CommandExt;
use std::os::windows::ffi::OsStrExt;

use crate::nodes::NodesStore;
use crate::STATE;

// MessageBoxW 常量（启动失败诊断弹窗用）
const MB_OK: u32 = 0x00000000;
const MB_ICONERROR: u32 = 0x00000010;

extern "system" {
    fn MessageBoxW(hWnd: *mut std::ffi::c_void, lpText: *const u16, lpCaption: *const u16, uType: u32) -> i32;
}

pub struct StatusData {
    pub running: bool,
    pub up_speed: u64,   // 字节/秒
    pub down_speed: u64, // 字节/秒
    pub up_total: u64,
    pub down_total: u64,
    pub conns: usize,
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub elevated: bool,

    // ---- sing-box 进程 ----
    child: Mutex<Option<Child>>,

    // ---- 运行时状态 ----
    pub status: Mutex<StatusData>,
    pub logs: Arc<Mutex<Vec<String>>>,
    pub log_seq: Arc<AtomicU64>,

    // ---- 节点管理 ----
    pub nodes: Mutex<NodesStore>,

    // ---- 系统集成 ----
    pub autostart: AtomicBool,
    pub system_proxy: AtomicBool,

    // ---- 节点延迟 ----
    pub latency: AtomicU64, // 最近一次延迟测试结果（毫秒），0=未测试，u64::MAX=失败
    // ---- 监听端口（默认 2080；被占用时可由 UI 换端口）----
    pub proxy_port: AtomicU16,

    // ---- 流量持久化 ----
    traffic_dirty: AtomicBool,

    // ---- 进程守护（watchdog） ----
    /// 连续 API 心跳失败次数
    wd_failures: AtomicU32,
    /// 连续心跳成功次数（稳定秒数），达到阈值后重置 wd_retries
    wd_stable_count: AtomicU32,
    /// 自动重启尝试次数（手动启动会重置）
    wd_retries: AtomicU32,
    /// 守护暂停到何时（UNIX 时间戳，毫秒）
    wd_suspended_until: AtomicU64,
    /// 当前 watchdog 是否正在自动重启中（避免并发）
    wd_restarting: AtomicBool,
}

impl AppState {
    pub fn new(data_dir: PathBuf) -> Self {
        // 先读取当前监听端口（在 data_dir 被 move 进结构体前借用）
        let proxy_port_init = Self::load_port(&data_dir);
        // 从注册表读取当前系统代理和开机自启状态
        let autostart = is_autostart_enabled();
        let system_proxy = is_system_proxy_enabled();

        // 加载节点列表
        let nodes = NodesStore::load(&data_dir);

        // 尝试从持久化文件恢复流量计数
        let (up_total, down_total) = load_traffic_file(&data_dir);

        let status = StatusData {
            running: false,
            up_speed: 0,
            down_speed: 0,
            up_total,
            down_total,
            conns: 0,
        };

        AppState {
            data_dir,
            elevated: is_elevated(),
            child: Mutex::new(None),
            status: Mutex::new(status),
            logs: Arc::new(Mutex::new(Vec::new())),
            log_seq: Arc::new(AtomicU64::new(0)),
            nodes: Mutex::new(nodes),
            autostart: AtomicBool::new(autostart),
            system_proxy: AtomicBool::new(system_proxy),
            traffic_dirty: AtomicBool::new(false),
            latency: AtomicU64::new(0),
            proxy_port: AtomicU16::new(proxy_port_init),
            wd_failures: AtomicU32::new(0),
            wd_stable_count: AtomicU32::new(0),
            wd_retries: AtomicU32::new(0),
            wd_suspended_until: AtomicU64::new(0),
            wd_restarting: AtomicBool::new(false),
        }
    }

    // ---- 端口持久化辅助 ----

    /// 读取「最终选定的监听端口」：优先专用 port.txt（跨启动保留用户选择），
    /// 回退到 config.json，最后默认 2080。这样不论别的电脑什么程序占了端口，
    /// 只要用户选过新端口，下次启动都会沿用最终选择。
    fn load_port(data_dir: &Path) -> u16 {
        // 1) 专用持久化文件（用户最终选择）
        let pf = data_dir.join("port.txt");
        if let Ok(t) = std::fs::read_to_string(&pf) {
            if let Ok(p) = t.trim().parse::<u16>() {
                if (1024..=65535).contains(&p) {
                    return p;
                }
            }
        }
        // 2) 回退：从已生成的 config.json 读
        Self::read_port_from_config(data_dir)
    }

    /// 将当前端口写入 port.txt（用户最终选择，跨启动保留）
    fn save_port(&self) {
        let port = self.proxy_port.load(Ordering::Relaxed);
        let path = self.data_dir.join("port.txt");
        let _ = std::fs::write(&path, port.to_string());
    }

    /// 从 data_dir/config.json 读取 mixed-in 监听端口；文件不存在或解析失败则返回默认 2080
    fn read_port_from_config(data_dir: &Path) -> u16 {
        let path = data_dir.join("config.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(arr) = v.get("inbounds").and_then(|x| x.as_array()) {
                    for ib in arr {
                        if ib.get("tag").and_then(|t| t.as_str()) == Some("mixed-in") {
                            if let Some(p) = ib.get("listen_port").and_then(|x| x.as_u64()) {
                                return p as u16;
                            }
                        }
                    }
                }
            }
        }
        2080
    }

    // ---- 进程管理 ----

    pub fn is_running(&self) -> bool {
        let mut g = self.child.lock().unwrap();
        match g.as_mut() {
            Some(c) => c.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }

    /// 检测端口是否被占用：bind 成功=空闲，失败=已被监听
    pub fn port_in_use(port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
    }

    /// 从 from+1 起寻找第一个「代理端口与 API 端口都空闲」的基准端口。
    /// 返回该基准（mixed 端口），对应的 API 端口 = 基准 + API_PORT_OFFSET。
    pub fn next_free_pair(from: u16) -> u16 {
        let mut p = from + 1;
        while p < 65535 {
            // 用 saturating_add 避免基准接近上限时 p+offset 溢出回绕导致配对探测错乱
            let api = p.saturating_add(Self::API_PORT_OFFSET);
            if api >= p && !Self::port_in_use(p) && !Self::port_in_use(api) {
                return p;
            }
            p += 1;
        }
        from + 1
    }

    /// 读取/设置当前监听端口
    pub fn get_proxy_port(&self) -> u16 {
        self.proxy_port.load(Ordering::Relaxed)
    }
    pub fn set_proxy_port(&self, port: u16) {
        self.proxy_port.store(port, Ordering::Relaxed);
        self.save_port(); // 持久化「最终选择端口」，下次启动沿用
    }

    /// clash_api 控制端口与 mixed 入站端口的固定偏移（默认 2080→9090）
    pub(crate) const API_PORT_OFFSET: u16 = 7010;

    /// 当前 clash_api 控制端口（随代理端口一起换，避免 9090 等被占用）
    pub fn api_port(&self) -> u16 {
        self.proxy_port.load(Ordering::Relaxed).saturating_add(Self::API_PORT_OFFSET)
    }

    // ---- 进程守护（watchdog） ----

    /// 记录一次 API 心跳成功：重置连续失败计数，并累计稳定秒数。
    /// 当连续稳定 60 秒后，认为本次 incident 结束，重置自动重启次数。
    pub fn wd_heartbeat_ok(&self) {
        self.wd_failures.store(0, Ordering::Relaxed);
        let stable = self.wd_stable_count.fetch_add(1, Ordering::Relaxed) + 1;
        if stable >= 60 {
            self.wd_stable_count.store(0, Ordering::Relaxed);
            self.wd_retries.store(0, Ordering::Relaxed);
        }
    }

    /// 记录一次 API 心跳失败；返回当前连续失败次数
    pub fn wd_heartbeat_fail(&self) -> u32 {
        self.wd_stable_count.store(0, Ordering::Relaxed);
        self.wd_failures.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 守护是否处于暂停冷却期
    fn wd_is_suspended(&self, now_ms: u64) -> bool {
        let until = self.wd_suspended_until.load(Ordering::Relaxed);
        until > 0 && now_ms < until
    }

    /// 手动启动成功后重置 watchdog 计数
    pub fn wd_reset(&self) {
        self.wd_failures.store(0, Ordering::Relaxed);
        self.wd_stable_count.store(0, Ordering::Relaxed);
        self.wd_retries.store(0, Ordering::Relaxed);
        self.wd_suspended_until.store(0, Ordering::Relaxed);
        self.wd_restarting.store(false, Ordering::Relaxed);
    }

    /// 尝试自动重启 sing-box。由后台轮询线程调用。
    /// 返回 (是否尝试重启, 日志信息)
    pub fn wd_try_restart(&self) -> (bool, String) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // 冷却期中不处理
        if self.wd_is_suspended(now_ms) {
            return (false, "watchdog 处于冷却期，跳过自动重启".to_string());
        }

        // 已经在重启中，或 sing-box 其实还活着（API 短暂不可用），不重复处理
        if self.wd_restarting.load(Ordering::Relaxed) || self.is_running() {
            return (false, "watchdog 检测到 sing-box 仍在运行，不自动重启".to_string());
        }

        // 重试过多：进入 5 分钟冷却
        let retries = self.wd_retries.load(Ordering::Relaxed);
        if retries >= 5 {
            self.wd_suspended_until
                .store(now_ms + 5 * 60 * 1000, Ordering::Relaxed);
            return (false, "watchdog 连续自动重启 5 次失败，进入 5 分钟冷却".to_string());
        }

        // 标记重启中，并保留当前重试计数（start_proxy 会重置，之后恢复）
        self.wd_restarting.store(true, Ordering::Relaxed);
        let next_retries = retries + 1;
        self.wd_retries.store(next_retries, Ordering::Relaxed);

        // 实际重启
        match self.restart_proxy() {
            Ok(_) => {
                self.wd_failures.store(0, Ordering::Relaxed);
                self.wd_restarting.store(false, Ordering::Relaxed);
                // 保留重试计数：启动成功后又很快崩溃时需要继续计数
                self.wd_retries.store(next_retries, Ordering::Relaxed);
                (true, format!("watchdog 已自动重启 sing-box（第 {} 次）", next_retries))
            }
            Err(e) => {
                self.wd_restarting.store(false, Ordering::Relaxed);
                // 失败冷却 30 秒
                self.wd_suspended_until.store(now_ms + 30_000, Ordering::Relaxed);
                (true, format!("watchdog 自动重启失败：{}，30 秒后重试", e))
            }
        }
    }

    // ---- 配置 ----
    pub fn regenerate_config(&self) -> Result<String, String> {
        let (link, use_tun, route_mode) = {
            let nodes = self.nodes.lock().unwrap();
            let n = nodes.current_node().ok_or("当前没有节点，请先导入链接")?;
            (n.link.clone(), nodes.use_tun(), nodes.route_mode().to_string())
        };
        let port = self.proxy_port.load(Ordering::Relaxed);
        crate::config_gen::generate_config(&link, &self.data_dir, use_tun, port, port.saturating_add(Self::API_PORT_OFFSET), &route_mode)
    }

    pub fn binary_exists(&self) -> bool {
        self.data_dir.join("sing-box.exe").exists()
    }

    pub fn config_exists(&self) -> bool {
        self.data_dir.join("config.json").exists()
    }

    /// 切换全局路由模式并重新生成配置（运行中自动重启生效）
    /// mode: "rule"（国内直连）| "global"（所有流量走代理）
    pub fn set_route_mode(&self, mode: &str) -> Result<String, String> {
        {
            let mut nodes = self.nodes.lock().unwrap();
            nodes.set_route_mode(mode, &self.data_dir);
        }
        // 已有配置才需要重写；否则等导入/启动时自然采用新模式
        if self.config_exists() {
            self.regenerate_config()?;
            if self.is_running() {
                self.restart_proxy()?;
            }
        }
        Ok(format!(
            "已切换为{}",
            if mode == "global" {
                "全局模式（所有流量走代理）"
            } else {
                "规则模式（国内直连）"
            }
        ))
    }

    pub fn push_log(&self, msg: impl Into<String>) {
        let mut lg = self.logs.lock().unwrap();
        lg.push(msg.into());
        let n = lg.len();
        if n > 500 {
            lg.drain(0..n - 500);
        }
        drop(lg);
        self.log_seq.fetch_add(1, Ordering::Relaxed);
    }

    /// 启动 sing-box，并执行 500ms 后存活诊断
    pub fn start_proxy(&self) -> Result<(), String> {
        if self.is_running() {
            return Err("sing-box 已在运行中".into());
        }
        let exe = self.data_dir.join("sing-box.exe");
        if !exe.exists() {
            return Err(format!(
                "未找到 {}\\sing-box.exe，请把它放进 data 目录",
                self.data_dir.display()
            ));
        }
        let config = self.data_dir.join("config.json");
        if !config.exists() {
            return Err("尚未生成配置，请先在控制台粘贴节点链接并点「应用配置」".into());
        }

        let mut child = Command::new(&exe)
            .arg("run")
            .arg("-c")
            .arg(&config)
            .current_dir(&self.data_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("启动 sing-box 失败: {}", e))?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pid = child.id();

        *self.child.lock().unwrap() = Some(child);

        // 更新运行状态，并清空 watchdog 计数（手动/自动启动成功都视为新周期）
        if let Ok(mut s) = self.status.lock() {
            s.running = true;
        }
        self.wd_reset();

        // 启动日志捕获线程
        let log_path = self.data_dir.join("sing-box.log");
        let logs = self.logs.clone();
        let seq = self.log_seq.clone();
        if let Some(out) = stdout {
            let lp = log_path.clone();
            let logs2 = logs.clone();
            let seq2 = seq.clone();
            thread::spawn(move || {
                read_pipe(out, &logs2, &seq2, &lp);
            });
        }

        // stderr 单独捕获：保留最后几行用于启动诊断
        let err_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(err) = stderr {
            let lp = log_path.clone();
            let logs2 = logs.clone();
            let seq2 = seq.clone();
            let err_lines2 = err_lines.clone();
            thread::spawn(move || {
                read_pipe_with_buffer(err, &logs2, &seq2, &lp, &err_lines2);
            });
        }

        // 启动失败诊断：等 800ms 后检查进程是否还活着
        let err_lines2 = err_lines.clone();
        let pid_check = pid;
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(800));
            let st = match STATE.get() {
                Some(s) => s,
                None => return,
            };
            // 检查进程存活
            let is_dead = {
                let mut g = st.child.lock().unwrap();
                match g.as_mut() {
                    Some(c) if c.id() == pid_check => {
                        c.try_wait().ok().flatten().is_some()
                    }
                    _ => true,
                }
            };
            if is_dead {
                st.push_log("⚠ sing-box 启动后立即退出，可能配置有误");
                let errs = err_lines2.lock().unwrap();
                let tail: Vec<String> = if !errs.is_empty() {
                    let n = errs.len();
                    let slice = if n > 5 { &errs[n - 5..] } else { &errs[..] };
                    slice.iter().map(|l| format!("  ERR> {}", l)).collect()
                } else {
                    Vec::new()
                };
                for line in &tail {
                    st.push_log(line.clone());
                }
                // 弹出明确错误提示，而不是只在日志里提示
                let detail = if tail.is_empty() {
                    "未捕获到 stderr，请检查 data/sing-box.log 文件。".to_string()
                } else {
                    tail.join("\n")
                };
                let content = format!("sing-box 启动后立即退出，可能配置有误或 sing-box.exe 不兼容。\n\n{}", detail);
                let caption = "启动失败";
                let cw: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();
                let capw: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
                unsafe { MessageBoxW(std::ptr::null_mut(), cw.as_ptr(), capw.as_ptr(), MB_OK | MB_ICONERROR); }
            }
        });

        Ok(())
    }

    /// 停止 sing-box
    pub fn stop_proxy(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Ok(mut s) = self.status.lock() {
            s.running = false;
            s.up_speed = 0;
            s.down_speed = 0;
        }
    }

    /// 重启代理（切换节点后调用）
    pub fn restart_proxy(&self) -> Result<(), String> {
        self.stop_proxy();
        // 等 200ms 让端口释放
        thread::sleep(Duration::from_millis(200));
        self.start_proxy()
    }

    // ---- 节点管理 ----

    /// 添加节点并自动生成配置
    pub fn add_node(&self, link: &str) -> Result<String, String> {
        let (route_mode, use_tun) = {
            let nodes = self.nodes.lock().unwrap();
            (nodes.route_mode().to_string(), nodes.use_tun())
        };
        crate::config_gen::generate_config(link, &self.data_dir, use_tun, self.proxy_port.load(Ordering::Relaxed), self.proxy_port.load(Ordering::Relaxed).saturating_add(Self::API_PORT_OFFSET), &route_mode)?;
        self.nodes
            .lock()
            .unwrap()
            .add(link, &self.data_dir);
        let name = self
            .nodes
            .lock()
            .unwrap()
            .current_node()
            .map(|n| n.name.clone())
            .unwrap_or_default();
        // 启动前先检查当前状态：如果之前运行中，自动重启
        if self.is_running() {
            let _ = self.restart_proxy();
        }
        Ok(format!("已添加节点「{}」并生成配置", name))
    }

    /// 切换到指定节点（重新生成配置 + 重启 sing-box）
    pub fn switch_node(&self, index: usize) -> Result<String, String> {
        let (link, name, use_tun, route_mode) = {
            let nodes = self.nodes.lock().unwrap();
            let n = nodes
                .nodes
                .get(index)
                .ok_or("节点索引不存在")?;
            (n.link.clone(), n.name.clone(), nodes.use_tun(), nodes.route_mode().to_string())
        };
        // 生成配置（use_tun 为全局设置，所有节点共用）
        crate::config_gen::generate_config(&link, &self.data_dir, use_tun, self.proxy_port.load(Ordering::Relaxed), self.proxy_port.load(Ordering::Relaxed).saturating_add(Self::API_PORT_OFFSET), &route_mode)?;
        // 更新 current
        self.nodes.lock().unwrap().set_current(index, &self.data_dir);
        // 重启
        if self.is_running() {
            self.restart_proxy()?;
        }
        Ok(format!("已切换到「{}」", name))
    }

    /// 删除节点
    pub fn delete_node(&self, index: usize) -> Result<String, String> {
        let name = {
            let nodes = self.nodes.lock().unwrap();
            nodes
                .nodes
                .get(index)
                .map(|n| n.name.clone())
                .unwrap_or_default()
        };
        let ok = self.nodes.lock().unwrap().remove(index, &self.data_dir);
        if !ok {
            return Err("删除失败：索引越界".into());
        }
        Ok(format!("已删除「{}」", name))
    }

    /// 修改全局虚拟网卡（接管所有应用）开关，重新生成配置并重启
    pub fn set_use_tun(&self, use_tun: bool) -> Result<String, String> {
        let (link, name, route_mode) = {
            let mut nodes = self.nodes.lock().unwrap();
            if nodes.use_tun() == use_tun {
                return Ok(nodes.current_node().map(|n| n.name.clone()).unwrap_or_default());
            }
            nodes.set_use_tun(use_tun, &self.data_dir);
            let link = nodes.current_node().ok_or("当前没有节点")?.link.clone();
            let name = nodes.current_node().map(|n| n.name.clone()).unwrap_or_default();
            let rm = nodes.route_mode().to_string();
            (link, name, rm)
        };
        crate::config_gen::generate_config(&link, &self.data_dir, use_tun, self.proxy_port.load(Ordering::Relaxed), self.proxy_port.load(Ordering::Relaxed).saturating_add(Self::API_PORT_OFFSET), &route_mode)?;
        if self.is_running() {
            self.restart_proxy()?;
        }
        Ok(name)
    }

    /// 测试当前节点延迟
    /// 直连节点服务器测 RTT（与代理是否启动无关）。
    /// 多次取样取最小，降低抖动噪声；单次超时 3s，任一次失败即判不可达。
    fn measure_latency(link: &str) -> Option<u64> {
        let server = extract_server_from_link(link)?;
        let sock_addr = format!("{}:{}", server.0, server.1).parse().ok()?;
        let mut best: Option<u64> = None;
        for _ in 0..3 {
            let start = std::time::Instant::now();
            match std::net::TcpStream::connect_timeout(&sock_addr, Duration::from_secs(3)) {
                Ok(_) => {
                    let e = start.elapsed().as_millis() as u64;
                    best = Some(best.map_or(e, |b| b.min(e)));
                }
                Err(_) => return None,
            }
        }
        best
    }

    /// 测当前节点延迟
    pub fn test_current_latency(&self) -> Option<u64> {
        let link = {
            let nodes = self.nodes.lock().unwrap();
            nodes.current_node()?.link.clone()
        };
        Self::measure_latency(&link)
    }

    /// 测指定索引节点延迟
    pub fn test_node_latency(&self, index: usize) -> Option<u64> {
        let link = {
            let nodes = self.nodes.lock().unwrap();
            nodes.nodes.get(index)?.link.clone()
        };
        Self::measure_latency(&link)
    }

    /// 节点总数
    pub fn node_count(&self) -> usize {
        self.nodes.lock().unwrap().nodes.len()
    }

    /// 把延迟结果写回节点（会话内临时，不持久化）
    pub fn set_node_latency(&self, index: usize, lat: u64) {
        let mut nodes = self.nodes.lock().unwrap();
        if let Some(n) = nodes.nodes.get_mut(index) {
            n.latency = lat;
        }
    }

    // ---- 系统代理 ----

    /// 通知系统「代理设置已变更」，让正在运行的浏览器（Edge/Chrome/IE）立即生效
    /// 仅改注册表不够——WinINET 应用通常在启动时读一次，需广播 INTERNET_OPTION_SETTINGS_CHANGED(39)
    /// + INTERNET_OPTION_REFRESH(37) 才能免重启生效。
    /// 改用 wininet!InternetSetOptionW 直接 FFI，去掉原先每次切换都拉起的 PowerShell 开销。
    fn notify_proxy_changed() {
        #[link(name = "wininet")]
        extern "system" {
            fn InternetSetOptionW(
                hInternet: *mut std::ffi::c_void,
                dwOption: u32,
                lpBuffer: *mut std::ffi::c_void,
                dwBufferLength: u32,
            ) -> i32;
        }
        unsafe {
            InternetSetOptionW(std::ptr::null_mut(), 39, std::ptr::null_mut(), 0);
            InternetSetOptionW(std::ptr::null_mut(), 37, std::ptr::null_mut(), 0);
        }
    }

    /// 开启/关闭系统代理（写入 Windows 注册表）
    /// 纯 WinAPI 实现：RegSetValueExW 写 ProxyEnable/ProxyServer + 广播 InternetSetOption，
    /// 不再依赖 PowerShell，切换无冷启动开销。
    pub fn set_system_proxy(&self, enable: bool) -> Result<(), String> {
        const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
        if enable {
            let addr = format!("127.0.0.1:{}", self.proxy_port.load(Ordering::Relaxed));
            reg_set_dword(SUB, "ProxyEnable", 1)?;
            reg_set_string(SUB, "ProxyServer", &addr)?;
        } else {
            reg_set_dword(SUB, "ProxyEnable", 0)?;
        }
        // 广播变更通知，让已运行的浏览器立即生效（无需重启）
        Self::notify_proxy_changed();
        self.system_proxy.store(enable, Ordering::Relaxed);
        Ok(())
    }

    // ---- 开机自启 ----

    /// 开启/关闭开机自启（写入 Windows 注册表 Run 键）
    /// 纯 WinAPI 实现（原为 PowerShell），切换无冷启动开销。
    pub fn set_autostart(&self, enable: bool) -> Result<(), String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("获取 exe 路径失败: {}", e))?;
        let exe_str = exe.display().to_string();
        const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
        if enable {
            reg_set_string(SUB, "SingBoxCommander", &exe_str)?;
        } else {
            reg_delete_value(SUB, "SingBoxCommander")?;
        }
        self.autostart.store(enable, Ordering::Relaxed);
        Ok(())
    }

    /// 退出时清理本程序设置的系统代理，避免残留"死代理"导致浏览器无法上网
    ///
    /// 仅当代理确实是本程序写入的 `127.0.0.1:2080` 时才清理，
    /// 以免误关用户通过其他代理工具（如 Clash）设置的代理。
    pub fn cleanup_proxy_on_exit(&self) {
        if !self.system_proxy.load(Ordering::Relaxed) {
            return;
        }
        const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
        let expected = format!("127.0.0.1:{}", self.proxy_port.load(Ordering::Relaxed));
        let is_ours = reg_get_string(SUB, "ProxyServer")
            .map(|v| v.eq_ignore_ascii_case(&expected))
            .unwrap_or(false);
        if is_ours {
            let _ = self.set_system_proxy(false);
        }
    }

    /// 登记「登录自检」：异常关机（未走正常退出流程）后，
    /// 若系统代理指向本机却无服务监听（死代理），下次登录会自动清理，
    /// 避免浏览器因残留代理设置而无法上网。
    /// 写入 HKCU\...\RunOnce，系统登录时执行一次后自动删除该键。
    /// 纯 WinAPI 实现（原为 PowerShell）。
    pub fn register_logon_cleanup() {
        if let Ok(exe) = std::env::current_exe() {
            let exe_str = exe.display().to_string();
            // 注册表值：带引号的 exe 路径 + --cleanup 参数
            const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
            let value = format!("\"{}\" --cleanup", exe_str);
            let _ = reg_set_string(SUB, "SingBoxCommander_Cleanup", &value);
        }
    }

    /// 正常停止/退出时撤销登录自检登记（代理已干净关闭，无需再清理）
    pub fn unregister_logon_cleanup() {
        const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
        let _ = reg_delete_value(SUB, "SingBoxCommander_Cleanup");
    }

    /// 清理「死代理」：系统代理指向本程序设置的回环地址，但没有任何服务在监听。
    /// 仅在代理确实是我们自己设置的 `127.0.0.1:<本程序端口>`、且端口无人监听时才关闭，
    /// 绝不动其他代理工具（如 Clash 用的不同回环端口）。
    pub fn cleanup_dangling_proxy(&self) {
        const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
        if !is_system_proxy_enabled() {
            return;
        }
        let server = match reg_get_string(SUB, "ProxyServer") {
            Some(s) => s,
            None => return,
        };
        // 精确匹配「本程序此刻使用的端口」，避免误关其他回环代理（Clash 等）
        let expected = format!("127.0.0.1:{}", self.proxy_port.load(Ordering::Relaxed));
        if !server.eq_ignore_ascii_case(&expected) {
            return;
        }
        let port = self.proxy_port.load(Ordering::Relaxed);
        let listening = std::net::TcpStream::connect_timeout(
            &([127, 0, 0, 1], port).into(),
            Duration::from_millis(600),
        )
        .is_ok();
        if !listening {
            // 代理设置还在，但服务已死 → 直接关闭系统代理，恢复浏览器上网
            let _ = self.set_system_proxy(false);
        }
    }

    // ---- 流量持久化 ----

    /// 标记流量数据变化（由状态轮询线程调用）
    pub fn mark_traffic_dirty(&self) {
        self.traffic_dirty.store(true, Ordering::Relaxed);
    }

    /// 将流量数据持久化到文件（如果标记了变化）
    pub fn flush_traffic_if_dirty(&self) {
        if !self.traffic_dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let s = self.status.lock().unwrap();
        let data = format!(
            r#"{{"up_total":{},"down_total":{}}}"#,
            s.up_total, s.down_total
        );
        drop(s);
        let path = self.data_dir.join("traffic.json");
        let _ = fs::write(&path, data);
    }
}

// ---- 辅助函数 ----

/// 日志轮转：单个日志文件超过 5MB 时保留最多 3 个备份（sing-box.log.1 ~ .3）。
fn rotate_log_file(log_path: &Path) {
    const MAX_SIZE: u64 = 5 * 1024 * 1024;
    const MAX_BACKUPS: usize = 3;
    let meta = match fs::metadata(log_path) {
        Ok(m) if m.len() > MAX_SIZE => m,
        _ => return,
    };

    // 如果文件大小异常或为空，跳过轮转
    if meta.len() == 0 {
        return;
    }

    // 删除最旧的备份，依次后移
    let oldest = log_path.with_extension("log.3");
    let _ = fs::remove_file(&oldest);
    for i in (1..MAX_BACKUPS).rev() {
        let src = log_path.with_extension(format!("log.{}", i));
        let dst = log_path.with_extension(format!("log.{}", i + 1));
        let _ = fs::rename(&src, &dst);
    }
    let _ = fs::rename(log_path, log_path.with_extension("log.1"));
}

/// 读管道核心：把每一行写入日志文件 + 追加到共享缓冲（保留末尾 500 行）。
/// err_buffer 可选，用于启动诊断保留最后若干行 stderr。
fn read_pipe_core<R: std::io::Read>(
    reader: R,
    logs: &Arc<Mutex<Vec<String>>>,
    log_seq: &Arc<AtomicU64>,
    log_path: &Path,
    err_buffer: Option<&Arc<Mutex<Vec<String>>>>,
) {
    // 每次打开日志前先轮转，避免单个文件无限增长
    rotate_log_file(log_path);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok();
    let mut br = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match br.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "{}", trimmed);
                    }
                    if let Some(buf) = err_buffer {
                        if let Ok(mut errs) = buf.lock() {
                            errs.push(trimmed.to_string());
                            if errs.len() > 20 {
                                errs.remove(0);
                            }
                        }
                    }
                    if let Ok(mut lg) = logs.lock() {
                        lg.push(trimmed.to_string());
                        let n = lg.len();
                        if n > 500 {
                            lg.drain(0..n - 500);
                        }
                        drop(lg);
                        log_seq.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

fn read_pipe<R: std::io::Read>(
    reader: R,
    logs: &Arc<Mutex<Vec<String>>>,
    log_seq: &Arc<AtomicU64>,
    log_path: &Path,
) {
    read_pipe_core(reader, logs, log_seq, log_path, None)
}

fn read_pipe_with_buffer<R: std::io::Read>(
    reader: R,
    logs: &Arc<Mutex<Vec<String>>>,
    log_seq: &Arc<AtomicU64>,
    log_path: &Path,
    err_buffer: &Arc<Mutex<Vec<String>>>,
) {
    read_pipe_core(reader, logs, log_seq, log_path, Some(err_buffer))
}

/// 检测当前进程是否以管理员身份运行（纯 WinAPI，避免启动时阻塞于 PowerShell）
///
/// 原实现用 `powershell -Command ...IsInRole(Administrator)`，每次冷启动都要
/// 加载 PowerShell 运行时（约数百毫秒）。这里改用 shell32!IsUserAnAdmin，
/// 直接读取当前进程令牌，微秒级返回，与前者判定逻辑等价。
pub fn is_elevated() -> bool {
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

/// 以管理员身份重启本程序（UAC 提权），随后退出当前进程
pub fn restart_as_admin(exe_path: &Path, cwd: &Path) -> Result<(), String> {
    let ps = format!(
        "Start-Process -FilePath '{}' -Verb RunAs -WorkingDirectory '{}'",
        exe_path.display(),
        cwd.display()
    );
    Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("提权重启失败: {}", e))?;
    std::process::exit(0);
}

/// 检测系统代理是否已开启（纯 WinAPI 读注册表，微秒级）
fn is_system_proxy_enabled() -> bool {
    const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    reg_get_dword(SUB, "ProxyEnable").map(|v| v == 1).unwrap_or(false)
}

/// 检测开机自启是否已设置（纯 WinAPI 读注册表，微秒级）
fn is_autostart_enabled() -> bool {
    const SUB: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    // 仅判断 Run 键下是否存在本程序条目（值存在即视为已开启）
    reg_get_string(SUB, "SingBoxCommander").is_some()
}

/// 从流量持久化文件中恢复累计值
fn load_traffic_file(data_dir: &Path) -> (u64, u64) {
    let path = data_dir.join("traffic.json");
    match fs::read_to_string(&path) {
        Ok(s) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                let up = v.get("up_total").and_then(|x| x.as_u64()).unwrap_or(0);
                let down = v.get("down_total").and_then(|x| x.as_u64()).unwrap_or(0);
                return (up, down);
            }
            (0, 0)
        }
        Err(_) => (0, 0),
    }
}

/// 从链接中提取 (host:port) 用于延迟测试
fn extract_server_from_link(link: &str) -> Option<(String, u16)> {
    // vless://uuid@host:port?...
    // trojan://pw@host:port?...
    if let Some(rest) = link
        .strip_prefix("vless://")
        .or_else(|| link.strip_prefix("trojan://"))
    {
        let after_at = rest.split('@').nth(1)?;
        let host_port = after_at.split('?').next()?;
        let mut parts = host_port.split(':');
        let host = parts.next()?.to_string();
        let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(443);
        return Some((host, port));
    }
    // vmess://base64{...} — 尝试解码后提取
    if let Some(b64) = link.strip_prefix("vmess://") {
        let cleaned = b64.split('#').next()?;
        let decoded = B64.decode(cleaned).ok()?;
        let json_str = String::from_utf8(decoded).ok()?;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_str) {
            let host = v.get("add").and_then(|x| x.as_str())?.to_string();
            let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(443) as u16;
            return Some((host, port));
        }
    }
    None
}

// ---- 纯 WinAPI 快速读取（替代启动期的 PowerShell 调用）----
//
// 启动路径原本会串行调用 3 次 `powershell.exe`（管理员判定、系统代理、开机自启），
// 每次 PowerShell 冷启动都要加载运行时，3 次叠加约数百毫秒~1.8 秒，
// 直接拖慢程序首屏出现。改为直接走 advapi32/shell32 的 FFI，微秒级返回。

#[link(name = "advapi32")]
extern "system" {
    /// RegGetValueW：自动定位/重定向注册表键并读取值（WIN7+ 可用）
    fn RegGetValueW(
        hkey: *mut std::ffi::c_void,
        lpSubKey: *const u16,
        lpValue: *const u16,
        dwFlags: u32,
        pdwType: *mut u32,
        pvData: *mut std::ffi::c_void,
        pcbData: *mut u32,
    ) -> i32; // 0 = ERROR_SUCCESS

    /// RegOpenKeyExW：以指定权限打开（或创建）注册表子键
    fn RegOpenKeyExW(
        hKey: *mut std::ffi::c_void,
        lpSubKey: *const u16,
        ulOptions: u32,
        samDesired: u32,
        phkResult: *mut *mut std::ffi::c_void,
    ) -> i32;

    /// RegSetValueExW：写入/覆盖注册表值（支持 REG_DWORD / REG_SZ）
    fn RegSetValueExW(
        hKey: *mut std::ffi::c_void,
        lpValueName: *const u16,
        Reserved: u32,
        dwType: u32,
        lpData: *const std::ffi::c_void,
        cbData: u32,
    ) -> i32;

    /// RegDeleteValueW：删除注册表值
    fn RegDeleteValueW(hKey: *mut std::ffi::c_void, lpValueName: *const u16) -> i32;

    /// RegCloseKey：关闭打开的注册表键句柄
    fn RegCloseKey(hKey: *mut std::ffi::c_void) -> i32;
}

/// HKCU 预定义句柄（与 Windows 头文件一致）
const HKEY_CURRENT_USER: *mut std::ffi::c_void = 0x8000_0001 as *mut std::ffi::c_void;
/// 只接受 REG_DWORD
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
/// 只接受 REG_SZ
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
/// 注册表写入权限（设置/删除值需要）
const KEY_SET_VALUE: u32 = 0x0002;
/// REG_DWORD 类型
const REG_DWORD: u32 = 4;
/// REG_SZ 类型
const REG_SZ: u32 = 1;

/// 读取 REG_DWORD 类型值，不存在或非目标类型返回 None
fn reg_get_dword(subkey: &str, value: &str) -> Option<u32> {
    let sub_w: Vec<u16> = std::ffi::OsStr::new(subkey).encode_wide().chain(std::iter::once(0)).collect();
    let val_w: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().chain(std::iter::once(0)).collect();
    let mut data: u32 = 0;
    let mut len: u32 = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub_w.as_ptr(),
            val_w.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut std::ffi::c_void,
            &mut len,
        )
    };
    if r == 0 { Some(data) } else { None }
}

/// 读取 REG_SZ 类型值，不存在/空值返回 None
fn reg_get_string(subkey: &str, value: &str) -> Option<String> {
    let sub_w: Vec<u16> = std::ffi::OsStr::new(subkey).encode_wide().chain(std::iter::once(0)).collect();
    let val_w: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().chain(std::iter::once(0)).collect();
    // 先查询所需缓冲区大小
    let mut len: u32 = 0;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub_w.as_ptr(),
            val_w.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if r != 0 || len == 0 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0u16; (len as usize) / 2 + 1];
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub_w.as_ptr(),
            val_w.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            &mut len,
        )
    };
    if r != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..end]).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// 以可写方式打开 HKCU 子键（KEY_SET_VALUE），失败返回 None（调用方负责 RegCloseKey）
fn reg_open_writable(subkey: &str) -> Option<*mut std::ffi::c_void> {
    let sub_w: Vec<u16> = std::ffi::OsStr::new(subkey)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut hk: *mut std::ffi::c_void = std::ptr::null_mut();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub_w.as_ptr(), 0, KEY_SET_VALUE, &mut hk) };
    if r == 0 { Some(hk) } else { None }
}

/// 写入 REG_DWORD 值（不存在则创建）
fn reg_set_dword(subkey: &str, value: &str, data: u32) -> Result<(), String> {
    let hk = reg_open_writable(subkey).ok_or_else(|| "打开注册表键失败".to_string())?;
    let val_w: Vec<u16> = std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let r = unsafe {
        RegSetValueExW(
            hk,
            val_w.as_ptr(),
            0,
            REG_DWORD,
            &data as *const u32 as *const std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    unsafe { RegCloseKey(hk); }
    if r == 0 {
        Ok(())
    } else {
        Err(format!("写入注册表失败 (code {})", r))
    }
}

/// 写入 REG_SZ 值（不存在则创建；自动补 NUL 结尾）
fn reg_set_string(subkey: &str, value: &str, data: &str) -> Result<(), String> {
    let hk = reg_open_writable(subkey).ok_or_else(|| "打开注册表键失败".to_string())?;
    let val_w: Vec<u16> = std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut data_w: Vec<u16> = std::ffi::OsStr::new(data).encode_wide().collect();
    data_w.push(0);
    let r = unsafe {
        RegSetValueExW(
            hk,
            val_w.as_ptr(),
            0,
            REG_SZ,
            data_w.as_ptr() as *const std::ffi::c_void,
            (data_w.len() * std::mem::size_of::<u16>()) as u32,
        )
    };
    unsafe { RegCloseKey(hk); }
    if r == 0 {
        Ok(())
    } else {
        Err(format!("写入注册表失败 (code {})", r))
    }
}

/// 删除注册表值（值不存在视为成功，与 PowerShell -ErrorAction SilentlyContinue 等价）
fn reg_delete_value(subkey: &str, value: &str) -> Result<(), String> {
    let hk = reg_open_writable(subkey).ok_or_else(|| "打开注册表键失败".to_string())?;
    let val_w: Vec<u16> = std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let r = unsafe { RegDeleteValueW(hk, val_w.as_ptr()) };
    unsafe { RegCloseKey(hk); }
    // ERROR_FILE_NOT_FOUND = 2：键/值本就不存在，幂等成功
    if r == 0 || r == 2 {
        Ok(())
    } else {
        Err(format!("删除注册表值失败 (code {})", r))
    }
}
