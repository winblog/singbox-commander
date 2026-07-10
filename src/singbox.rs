// 进程管理 + 共享状态 + 系统集成
// 负责：拉起/停止 data/sing-box.exe、捕获日志、管理员权限检测与重启
// 系统代理（Windows 注册表）、开机自启、流量持久化、启动失败诊断

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
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

    /// 基于当前节点重新生成 config.json（换端口或 TUN 变化后调用）
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

        // 更新运行状态
        if let Ok(mut s) = self.status.lock() {
            s.running = true;
        }

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
                if !errs.is_empty() {
                    let tail = if errs.len() > 5 {
                        &errs[errs.len() - 5..]
                    } else {
                        &errs[..]
                    };
                    for line in tail {
                        st.push_log(format!("  ERR> {}", line));
                    }
                }
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
    /// + INTERNET_OPTION_REFRESH(37) 才能免重启生效
    fn notify_proxy_changed() {
        let ps = r#"
$code = @'
[DllImport("wininet.dll", SetLastError=true)]
public static extern bool InternetSetOption(IntPtr hInternet, int dwOption, IntPtr lpBuffer, int dwBufferLength);
'@
$wininet = Add-Type -MemberDefinition $code -Name wininet -Namespace Win32 -PassThru
$wininet::InternetSetOption([IntPtr]::Zero, 39, [IntPtr]::Zero, 0) | Out-Null
$wininet::InternetSetOption([IntPtr]::Zero, 37, [IntPtr]::Zero, 0) | Out-Null
"#;
        let _ = run_ps(ps); // 通知失败不影响主要功能
    }

    /// 开启/关闭系统代理（写入 Windows 注册表）
    pub fn set_system_proxy(&self, enable: bool) -> Result<(), String> {
        if enable {
            let addr = format!("127.0.0.1:{}", self.proxy_port.load(Ordering::Relaxed));
            let ps = format!(
                r#"$path='HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings'; Set-ItemProperty -Path $path -Name ProxyEnable -Value 1; Set-ItemProperty -Path $path -Name ProxyServer -Value '{}'"#,
                addr
            );
            run_ps(&ps)?;
        } else {
            let ps = r#"Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -Name ProxyEnable -Value 0"#;
            run_ps(ps)?;
        }
        // 广播变更通知，让已运行的浏览器立即生效（无需重启）
        Self::notify_proxy_changed();
        self.system_proxy.store(enable, Ordering::Relaxed);
        Ok(())
    }

    // ---- 开机自启 ----

    /// 开启/关闭开机自启（写入 Windows 注册表 Run 键）
    pub fn set_autostart(&self, enable: bool) -> Result<(), String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("获取 exe 路径失败: {}", e))?;
        let exe_str = exe.display().to_string();
        if enable {
            let ps = format!(
                r#"Set-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SingBoxGUI' -Value '{}'"#,
                exe_str.replace('\'', "''")
            );
            run_ps(&ps)?;
        } else {
            // Remove-ItemProperty 在键不存在时会报错，用 -ErrorAction SilentlyContinue 抑制
            let ps = r#"Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SingBoxGUI' -ErrorAction SilentlyContinue"#;
            run_ps(ps)?;
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

/// 读管道核心：把每一行写入日志文件 + 追加到共享缓冲（保留末尾 500 行）。
/// err_buffer 可选，用于启动诊断保留最后若干行 stderr。
fn read_pipe_core<R: std::io::Read>(
    reader: R,
    logs: &Arc<Mutex<Vec<String>>>,
    log_seq: &Arc<AtomicU64>,
    log_path: &Path,
    err_buffer: Option<&Arc<Mutex<Vec<String>>>>,
) {
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

/// 执行 PowerShell 命令，隐藏窗口。命令失败（非零退出码）返回错误。
fn run_ps(cmd: &str) -> Result<(), String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("执行 PowerShell 失败: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.lines().next().unwrap_or("").trim().to_string();
        Err(if msg.is_empty() { "PowerShell 命令执行失败".into() } else { msg })
    }
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
    reg_get_string(SUB, "SingBoxGUI").is_some()
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
}

/// HKCU 预定义句柄（与 Windows 头文件一致）
const HKEY_CURRENT_USER: *mut std::ffi::c_void = 0x8000_0001 as *mut std::ffi::c_void;
/// 只接受 REG_DWORD
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
/// 只接受 REG_SZ
const RRF_RT_REG_SZ: u32 = 0x0000_0002;

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
