#![windows_subsystem = "windows"]

// Sing-box 后台代理管理器（native-windows-gui，无 webview）
//
// 单窗口控制台：整合实时状态、节点管理、链接导入、运行日志
//
// 托盘右键菜单：
//   [控制台]
//   [开机自启]
//   ──────────────
//   [启动] [停止]
//   ──────────────
//   [以管理员身份重启] [退出]

use native_windows_gui as nwg;
use native_windows_gui::NativeUi;
use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

mod config_gen;
mod nodes;
mod singbox;
mod status;

use singbox::AppState;

// Win32 MessageBoxW：端口冲突时询问「是否换端口」（nwg 无稳定对话框导出，直接用系统 API）
#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(
        hwnd: *mut std::ffi::c_void,
        lpText: *const u16,
        lpCaption: *const u16,
        uType: u32,
    ) -> i32;
}

pub static STATE: OnceLock<Arc<AppState>> = OnceLock::new();

fn fmt_bytes(n: u64) -> String {
    if n < 1024 { return format!("{} B", n); }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 { return format!("{:.1} KB", kb); }
    let mb = kb / 1024.0;
    if mb < 1024.0 { return format!("{:.1} MB", mb); }
    format!("{:.2} GB", mb / 1024.0)
}

fn fmt_speed(n: u64) -> String { format!("{}/s", fmt_bytes(n)) }

fn log_line(msg: &str) {
    if let Some(st) = STATE.get() { st.push_log(msg); }
}

// 纯 WinAPI 读取剪贴板文本，避免每次打开控制台都同步调用 PowerShell（约数百毫秒卡顿）
#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hOwner: *mut std::ffi::c_void) -> i32;
    fn CloseClipboard() -> i32;
    fn GetClipboardData(uFormat: u32) -> *mut std::ffi::c_void;
    fn FindWindowW(
        lpClassName: *const u16,
        lpWindowName: *const u16,
    ) -> *mut std::ffi::c_void;
    fn ShowWindow(hWnd: *mut std::ffi::c_void, nCmdShow: i32) -> i32;
    fn SetForegroundWindow(hWnd: *mut std::ffi::c_void) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalLock(hMem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(hMem: *mut std::ffi::c_void) -> i32;
    fn CreateMutexW(
        lpMutexAttributes: *mut std::ffi::c_void,
        bInitialOwner: i32,
        lpName: *const u16,
    ) -> *mut std::ffi::c_void;
    fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
}

const CF_UNICODETEXT: u32 = 13;

// 单实例互斥体名称（Global\ 前缀跨会话可见，防多开产生多个托盘图标）
const SINGLE_INSTANCE_MUTEX: &str = "Global\\SingBoxGUI_Weys_SingleInstance";
const ERROR_ALREADY_EXISTS: u32 = 183;
const SW_RESTORE: i32 = 9;

// Win32 MessageBox 常量
const MB_OK: u32 = 0x0000;
const MB_ICONINFORMATION: u32 = 0x0040;
const MB_YESNO: u32 = 0x0004;
const MB_ICONWARNING: u32 = 0x0030;
const IDYES: i32 = 6;

fn read_clipboard() -> Option<String> {
    unsafe {
        // 剪贴板可能被其他进程占用，打开失败则静默降级（与旧版 PowerShell 失败行为一致）
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT);
        let result = if h.is_null() {
            None
        } else {
            let p = GlobalLock(h) as *const u16;
            if p.is_null() {
                None
            } else {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(p, len);
                let s = String::from_utf16_lossy(slice).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            }
        };
        if !h.is_null() {
            let _ = GlobalUnlock(h);
        }
        let _ = CloseClipboard();
        result
    }
}

#[derive(Default)]
pub struct App {
    window: nwg::MessageWindow,
    icon_green: nwg::Icon,
    icon_red: nwg::Icon,

    // ---- 托盘 ----
    tray: nwg::TrayNotification,
    tray_menu: nwg::Menu,
    m_console: nwg::MenuItem,
    m_autostart: nwg::MenuItem,
    m_sep1: nwg::MenuSeparator,
    m_start: nwg::MenuItem,
    m_stop: nwg::MenuItem,
    m_sep2: nwg::MenuSeparator,
    m_admin: nwg::MenuItem,
    m_exit: nwg::MenuItem,

    // ---- 控制台窗口（集成所有功能）----
    console: nwg::Window,
    // 状态区
    lbl_title: nwg::Label,
    lbl_running: nwg::Label,
    lbl_latency: nwg::Label,
    lbl_up: nwg::Label,
    lbl_down: nwg::Label,
    lbl_up_total: nwg::Label,
    lbl_down_total: nwg::Label,
    lbl_conns: nwg::Label,
    lbl_proxy: nwg::Label,
    // 节点管理区
    lbl_sep_nodes: nwg::Label,
    node_list: nwg::TextBox,
    lbl_node_idx: nwg::Label,
    node_index_input: nwg::TextBox,
    btn_node_switch: nwg::Button,
    btn_node_delete: nwg::Button,
    btn_latency: nwg::Button,
    btn_latency_all: nwg::Button,
    lbl_nodes_msg: nwg::Label,
    // 导入区
    lbl_sep_import: nwg::Label,
    link_input: nwg::TextBox,
    // 路由方式（二选一，互斥）：虚拟网卡模式 / 系统代理模式
    chk_tun: nwg::CheckBox,
    chk_proxy: nwg::CheckBox,
    btn_apply: nwg::Button,
    btn_start: nwg::Button,
    btn_stop: nwg::Button,
    chk_route: nwg::CheckBox,
    lbl_import_msg: nwg::Label,
    // 日志区
    lbl_sep_log: nwg::Label,
    log_box: nwg::TextBox,
    lbl_made: nwg::Label,
    btn_console_close: nwg::Button,

    timer: nwg::Timer,
    last_log_seq: Cell<u64>,
    // 节点列表上一次渲染的文字——跳过无变化的 set_text，避免每秒重置用户选区
    last_node_text: RefCell<String>,
    // 统一 UI 字体（解决中文显示别扭）
    font: nwg::Font,
}

impl App {
    fn show_menu(&self) {
        let st = STATE.get().unwrap();
        let running = st.is_running();
        self.m_start.set_enabled(!running);
        self.m_stop.set_enabled(running);
        self.m_admin.set_enabled(!st.elevated);
        // 勾选标记显示开机自启当前状态（系统代理开关已由控制台二选一模式接管，托盘不再重复）
        self.m_autostart.set_checked(st.autostart.load(Ordering::Relaxed));
        // 悬浮提示由 on_tick 每秒统一刷新，此处无需再设（避免被立即覆盖的冗余调用）
        let (x, y) = nwg::GlobalCursor::position();
        self.tray_menu.popup(x, y);
    }

    fn on_menu(&self, handle: &nwg::ControlHandle) {
        let st = STATE.get().unwrap();
        if *handle == self.m_console.handle {
            self.refresh_all(); // 刷新节点列表和状态
            self.console.set_visible(true);
            self.console.set_focus();
            } else if *handle == self.m_autostart.handle {
            let cur = st.autostart.load(Ordering::Relaxed);
            match st.set_autostart(!cur) {
                Ok(_) => log_line(&format!("开机自启已{}", if !cur { "开启" } else { "关闭" })),
                Err(e) => log_line(&format!("开机自启切换失败：{}", e)),
            }
        } else if *handle == self.m_start.handle {
            self.on_start();
        } else if *handle == self.m_stop.handle {
            self.on_stop();
        } else if *handle == self.m_admin.handle {
            if let Ok(exe) = std::env::current_exe() {
                if let Some(cwd) = exe.parent() {
                    let _ = singbox::restart_as_admin(&exe, cwd);
                }
            }
        } else if *handle == self.m_exit.handle {
            // 退出时清理本程序遗留的系统代理（写回注册表 + 广播），
            // 否则残留的"死代理"会让浏览器持续走代理死端口而无法上网
            st.cleanup_proxy_on_exit();
            st.stop_proxy(); nwg::stop_thread_dispatch();
        }
    }

    /// 打开控制台时刷新所有动态内容
    fn refresh_all(&self) {
        self.refresh_node_list();
        self.refresh_route_mode();
        // 同步路由方式（二选一）到两个互斥复选框
        if let Some(st) = STATE.get() {
            let ut = st.nodes.lock().unwrap().use_tun();
            self.chk_tun.set_check_state(if ut { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
            self.chk_proxy.set_check_state(if ut { nwg::CheckBoxState::Unchecked } else { nwg::CheckBoxState::Checked });
        }
        self.lbl_nodes_msg.set_text("");
        self.lbl_import_msg.set_text("");
        // 自动读取剪贴板：只有内容看起来像订阅链接时才填入导入框，避免普通文字误填
        if let Some(clip) = read_clipboard() {
            let looks_like_link = clip.lines().any(|line| {
                let line = line.trim();
                line.starts_with("http://") || line.starts_with("https://") ||
                line.starts_with("vmess://") || line.starts_with("vless://") ||
                line.starts_with("ss://") || line.starts_with("ssr://") ||
                line.starts_with("trojan://") || line.starts_with("hysteria://") ||
                line.starts_with("hysteria2://") || line.starts_with("hy2://") ||
                line.starts_with("tuic://") || line.starts_with("socks://") ||
                line.starts_with("ssh://")
            });
            if looks_like_link { self.link_input.set_text(&clip); }
        }
    }

    /// 同步路由模式开关到当前持久化状态（打开控制台 / 切换模式后调用）
    fn refresh_route_mode(&self) {
        if let Some(st) = STATE.get() {
            let global = st.nodes.lock().unwrap().route_mode() == "global";
            self.chk_route.set_check_state(if global { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
        }
    }

    /// 切换「所有网站都走代理（全局路由）」开关
    fn on_route_changed(&self) {
        let st = STATE.get().unwrap();
        let global = self.chk_route.check_state() == nwg::CheckBoxState::Checked;
        match st.set_route_mode(if global { "global" } else { "rule" }) {
            Ok(msg) => { self.lbl_import_msg.set_text(&msg); log_line(&msg); }
            Err(e) => {
                // 失败则回滚勾选状态
                let rm = st.nodes.lock().unwrap().route_mode().to_string();
                self.chk_route.set_check_state(if rm == "global" { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked });
                self.lbl_import_msg.set_text(&format!("切换失败：{}", e));
            }
        }
    }

    /// 构造节点列表文字（含每条延迟）。接受已持锁的 NodesStore 引用，避免重复加锁
    fn node_list_text_from(nodes: &nodes::NodesStore) -> String {
        let cur = nodes.current;
        // 接管所有应用 是全局设置，所有节点共用同一标识
        let use_tun = nodes.use_tun();
        let mut lines = Vec::new();
        for (i, n) in nodes.nodes.iter().enumerate() {
            let m = if i == cur { "▶" } else { "  " };
            let t = if use_tun { "[全应用]" } else { "[代理]" };
            let lat = match n.latency {
                0 => String::new(),
                u64::MAX => " ⏱不可达".to_string(),
                ms => format!(" ⏱{}ms", ms),
            };
            lines.push(format!("{} {} {}  {}{}", m, i, t, n.name, lat));
        }
        if lines.is_empty() { "（暂无节点）".to_string() } else { lines.join("\r\n") }
    }

    fn node_list_text(&self) -> String {
        let st = STATE.get().unwrap();
        Self::node_list_text_from(&st.nodes.lock().unwrap())
    }

    /// 把列表文字写入控件并缓存——仅内容变化才 set_text，避免每秒重置用户选区
    fn set_node_list_if_changed(&self, text: &str) {
        let mut last = self.last_node_text.borrow_mut();
        if *last != text {
            self.node_list.set_text(text);
            *last = text.to_string();
        }
    }

    fn refresh_node_list(&self) {
        let text = self.node_list_text();
        self.set_node_list_if_changed(&text);
        self.node_index_input.set_text("");
    }

    // ---- 节点操作 ----

    fn on_node_switch(&self) {
        let idx: usize = match self.node_index_input.text().trim().parse() {
            Ok(i) => i, Err(_) => { self.lbl_nodes_msg.set_text("提示：先在「编号」框输入数字（如 0），再点切换"); return; }
        };
        let st = STATE.get().unwrap();
        match st.switch_node(idx) {
            Ok(m) => { self.lbl_nodes_msg.set_text(&m); log_line(&m); self.refresh_node_list(); }
            Err(e) => { self.lbl_nodes_msg.set_text(&format!("切换失败：{}", e)); }
        }
    }

    fn on_node_delete(&self) {
        let idx: usize = match self.node_index_input.text().trim().parse() {
            Ok(i) => i, Err(_) => { self.lbl_nodes_msg.set_text("提示：先在「编号」框输入数字（如 0），再点删除"); return; }
        };
        let st = STATE.get().unwrap();
        match st.delete_node(idx) {
            Ok(m) => { self.lbl_nodes_msg.set_text(&m); log_line(&m); self.refresh_node_list(); }
            Err(e) => { self.lbl_nodes_msg.set_text(&format!("删除失败：{}", e)); }
        }
    }

    fn on_latency_test(&self) {
        let st = STATE.get().unwrap();
        let running = st.is_running();
        // 测的是「直连节点服务器」的 RTT，与代理是否启动无关，故允许未启动时比线
        let scope = if running { "（直连节点）" } else { "（直连节点·未启动）" };
        // 取当前节点名，便于比多条线路时区分
        let name = {
            let nodes = st.nodes.lock().unwrap();
            nodes.current_node().map(|n| n.name.clone()).unwrap_or_default()
        };
        log_line(&format!("测延迟 {} {}…", name, scope));
        match st.test_current_latency() {
            Some(ms) => {
                st.latency.store(ms, Ordering::Relaxed);
                if let Some(n) = st.nodes.lock().unwrap().current_node_mut() { n.latency = ms; }
                let disp = if ms == 0 { "<1 ms".to_string() } else { format!("{} ms", ms) };
                log_line(&format!("「{}」延迟：{} {}", name, disp, scope));
            }
            None => {
                st.latency.store(u64::MAX, Ordering::Relaxed);
                if let Some(n) = st.nodes.lock().unwrap().current_node_mut() { n.latency = u64::MAX; }
                log_line(&format!("「{}」延迟测试失败（节点不可达）", name));
            }
        }
    }

    /// 一键测全部节点延迟（后台线程，不卡界面；未启动亦可比线）
    fn on_latency_test_all(&self) {
        let st = STATE.get().unwrap().clone(); // Arc<AppState>，可移入线程
        let n = st.node_count();
        if n == 0 {
            log_line("（暂无节点，无法测速）");
            return;
        }
        log_line(&format!("⚡ 开始测速：共 {} 个节点（未启动亦可比线）…", n));
        std::thread::spawn(move || {
            let mut results: Vec<(usize, Option<u64>)> = Vec::with_capacity(n);
            for i in 0..n {
                let lat = st.test_node_latency(i);
                st.set_node_latency(i, lat.unwrap_or(u64::MAX));
                let name = st.nodes.lock().unwrap().nodes.get(i).map(|x| x.name.clone()).unwrap_or_default();
                let disp = match lat {
                    Some(0) => "<1 ms".to_string(),
                    Some(ms) => format!("{} ms", ms),
                    None => "不可达".to_string(),
                };
                log_line(&format!("「{}」延迟：{}", name, disp));
                results.push((i, lat));
            }
            // 汇总：可达的按延迟升序
            let mut ok: Vec<(usize, u64)> = results.into_iter().filter_map(|(i, l)| l.map(|ms| (i, ms))).collect();
            ok.sort_by_key(|&(_, ms)| ms);
            if ok.is_empty() {
                log_line("测速完成：所有节点均不可达");
            } else {
                let mut s = String::from("测速完成（由快到慢）：");
                for (idx, ms) in ok.iter().take(10) {
                    let nm = st.nodes.lock().unwrap().nodes.get(*idx).map(|x| x.name.clone()).unwrap_or_default();
                    s.push_str(&format!(" {}:{}ms", nm, ms));
                }
                if ok.len() > 10 { s.push_str(" …"); }
                log_line(&s);
            }
        });
    }

    // ---- 路由方式（二选一）：虚拟网卡 / 系统代理 ----

    /// 按当前路由方式重写 config.json 并应用代理状态
    /// tun=true → 虚拟网卡模式（接管所有应用；系统代理多余则关闭）
    /// tun=false → 系统代理模式（开启 Windows 系统代理，仅认代理软件走）
    fn apply_mode(&self, tun: bool) {
        let st = STATE.get().unwrap();
        // 1) 有节点才重写 config.json（set_use_tun 会按最新 use_tun / route_mode 生成，运行中自动重启）
        let has_node = st.nodes.lock().unwrap().current_node().is_some();
        if has_node {
            let _ = st.set_use_tun(tun);
        }
        // 2) 应用代理状态
        if tun {
            if st.is_running() { let _ = st.set_system_proxy(false); }
            log_line("已应用：虚拟网卡（接管所有应用）模式");
        } else {
            if !st.is_running() {
                log_line("⚠ 已设为系统代理模式，请先「启动」sing-box，代理才会生效");
            } else {
                match st.set_system_proxy(true) {
                    Ok(_) => log_line(&format!("系统代理已开启（127.0.0.1:{}）", st.get_proxy_port())),
                    Err(e) => log_line(&format!("系统代理开启失败：{}", e)),
                }
            }
            log_line("已应用：系统代理 模式");
        }
    }

    /// 虚拟网卡复选框变化（与系统代理互斥，至少保留一个）
    fn on_tun_changed(&self) {
        let checked = self.chk_tun.check_state() == nwg::CheckBoxState::Checked;
        if checked {
            self.chk_proxy.set_check_state(nwg::CheckBoxState::Unchecked);
            self.apply_mode(true);
        } else {
            // 取消虚拟网卡 → 强制切到系统代理（二选一）
            self.chk_proxy.set_check_state(nwg::CheckBoxState::Checked);
            self.apply_mode(false);
        }
    }

    /// 系统代理复选框变化（与虚拟网卡互斥，至少保留一个）
    fn on_proxy_changed(&self) {
        let checked = self.chk_proxy.check_state() == nwg::CheckBoxState::Checked;
        if checked {
            self.chk_tun.set_check_state(nwg::CheckBoxState::Unchecked);
            self.apply_mode(false);
        } else {
            self.chk_tun.set_check_state(nwg::CheckBoxState::Checked);
            self.apply_mode(true);
        }
    }

    /// 唯一操作按钮：链接框有内容则先导入（导入时已按当前 use_tun/route_mode 生成配置），
    /// 再统一重新生成配置并应用代理方式（虚拟网卡 / 系统代理 / 全局路由 全部勾选一并生效）
    fn on_apply(&self) {
        let st = STATE.get().unwrap();
        let use_tun = self.chk_tun.check_state() == nwg::CheckBoxState::Checked;
        let text = self.link_input.text();
        let links: Vec<String> = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        // 1) 有链接则先导入
        let mut ok = 0usize;
        if !links.is_empty() {
            for (i, link) in links.iter().enumerate() {
                match st.add_node(link) {
                    Ok(_) => { ok += 1; if i == 0 { log_line("已生成 config.json"); } }
                    Err(e) => { if i == 0 { self.lbl_import_msg.set_text(&format!("导入失败：{}", e)); return; } }
                }
            }
            self.link_input.set_text("");
            log_line(&format!("批量导入 {} 个节点", ok));
        }
        // 2) 统一应用（重写配置 + 应用代理方式；route_mode 也一并生效）
        self.apply_mode(use_tun);
        // 3) 提示（用实际成功导入数 ok，避免中途失败时多报）
        let mut m = String::new();
        if !links.is_empty() { m.push_str(&format!("已导入 {} 个节点，", ok)); }
        m.push_str("已应用配置");
        if !use_tun { m.push_str("（系统代理模式）"); }
        self.lbl_import_msg.set_text(&m);
        self.refresh_node_list();
    }

    /// 启动成功后按当前路由方式自动应用代理（减少手动步骤）
    fn apply_proxy_after_start(&self) {
        let st = STATE.get().unwrap();
        let tun = st.nodes.lock().unwrap().use_tun();
        if tun {
            // 虚拟网卡接管所有流量，系统代理多余，确保关闭
            let _ = st.set_system_proxy(false);
        } else {
            match st.set_system_proxy(true) {
                Ok(_) => log_line(&format!("已自动开启系统代理（127.0.0.1:{}）", st.get_proxy_port())),
                Err(e) => log_line(&format!("系统代理开启失败：{}", e)),
            }
        }
    }

    // ---- 导入（已合并到「应用配置」按钮）----

    /// 启动 sing-box（含端口占用预检 + 冲突弹窗换端口）。控制台按钮与托盘菜单共用。
    fn on_start(&self) {
        let st = STATE.get().unwrap();
        // 端口占用预检：同时检查 mixed 入站端口与 clash_api 控制端口。
        let port = st.get_proxy_port();
        let api = st.api_port();
        let conflict = if AppState::port_in_use(port) {
            Some(format!("{}（代理）", port))
        } else if AppState::port_in_use(api) {
            Some(format!("{}（API）", api))
        } else {
            None
        };
        if let Some(what) = conflict {
            // 若默认 clash API 端口 9090 也被占用，通常是另一份 sing-box 已在运行，
            // 此时无需重复启动，直接提示用户即可（不再提供换端口选项）。
            let default_api = 2080u16 + AppState::API_PORT_OFFSET; // 9090
            if AppState::port_in_use(default_api) {
                let content = "检测到 9090 端口已被其他程序占用，很可能已有 sing-box 实例正在运行。\n\n无需重复启动；如希望由本程序接管，请先停止已有的 sing-box 实例，再点击启动。".to_string();
                let text_w: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();
                let cap_w: Vec<u16> = "已有 sing-box 在运行".encode_utf16().chain(std::iter::once(0)).collect();
                unsafe {
                    MessageBoxW(std::ptr::null_mut(), text_w.as_ptr(), cap_w.as_ptr(), MB_OK | MB_ICONINFORMATION);
                }
                log_line("⚠ 检测到 9090 被占用，可能已有 sing-box 在运行，未启动");
            } else {
                let new_port = AppState::next_free_pair(port);
                let new_api = new_port.saturating_add(AppState::API_PORT_OFFSET);
                let content = format!(
                    "端口 {} 已被其他程序占用，\n无法在该端口启动 sing-box。\n\n是否改用 代理端口 {} / API 端口 {} 启动？\n\n（建议点「是」改端口；若想保留原端口，请先关闭占用程序后重试）",
                    what, new_port, new_api
                );
                let text_w: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();
                let cap_w: Vec<u16> = "端口被占用".encode_utf16().chain(std::iter::once(0)).collect();
                let ret = unsafe {
                    MessageBoxW(std::ptr::null_mut(), text_w.as_ptr(), cap_w.as_ptr(), MB_YESNO | MB_ICONWARNING)
                };
                if ret == IDYES {
                    st.set_proxy_port(new_port);
                    match st.regenerate_config() {
                        Ok(_) => {
                            log_line(&format!("端口冲突，已改用 代理 {} / API {}", new_port, new_api));
                            match st.start_proxy() {
                                Ok(_) => { log_line("已启动 sing-box"); self.apply_proxy_after_start(); }
                                Err(e) => log_line(&format!("启动失败：{}", e)),
                            }
                        }
                        Err(e) => log_line(&format!("⚠ 重新生成配置失败：{}", e)),
                    }
                } else {
                    log_line(&format!("端口 {} 被占用，已取消启动。请先关闭占用该端口的程序（如 box 服务）", what));
                }
            }
        } else {
            match st.start_proxy() {
                Ok(_) => { log_line("已启动 sing-box"); self.apply_proxy_after_start(); }
                Err(e) => log_line(&format!("启动失败：{}", e)),
            }
        }
    }

    /// 停止 sing-box（并同步关闭系统代理，避免死端口）。控制台按钮与托盘菜单共用。
    fn on_stop(&self) {
        let st = STATE.get().unwrap();
        if st.system_proxy.load(Ordering::Relaxed) {
            let _ = st.set_system_proxy(false);
            st.stop_proxy();
            log_line("已停止 sing-box，并同步关闭系统代理");
        } else {
            st.stop_proxy();
            log_line("已停止 sing-box");
        }
    }

    // ---- 定时刷新 ----

    fn on_tick(&self) {
        let st = match STATE.get() { Some(s) => s, None => return };
        let running = st.is_running();
        let (up, down, ut, dt, cn) = {
            let s = st.status.lock().unwrap();
            (s.up_speed, s.down_speed, s.up_total, s.down_total, s.conns)
        };

        // 一次性锁定 nodes：取 tooltip 所需 + 构造列表文字（合并原来的 3 次加锁为 1 次）
        let (node_hint, mode_hint, list_text) = {
            let nodes = st.nodes.lock().unwrap();
            let nh = nodes.current_node().map(|n| format!(" [{}]", n.name)).unwrap_or_default();
            let mh = if nodes.use_tun() { " 模式:虚拟网卡" } else { " 模式:系统代理" };
            (nh, mh, Self::node_list_text_from(&nodes))
        };
        let proxy_hint = if st.system_proxy.load(Ordering::Relaxed) { " 代理:开" } else { " 代理:关" };
        self.tray.set_tip(&format!(
            "Sing-box {}{}{}{}\n▲ {}  ▼ {}",
            if running { "● 运行中" } else { "○ 已停止" }, node_hint, mode_hint, proxy_hint, fmt_speed(up), fmt_speed(down)
        ));
        self.tray.set_icon(if running { &self.icon_green } else { &self.icon_red });

        if !self.console.visible() { return; }

        // 状态区
        self.lbl_running.set_text(if running { "● 运行中" } else { "○ 已停止" });
        self.lbl_up.set_text(&format!("▲ 上行  {}", fmt_speed(up)));
        self.lbl_down.set_text(&format!("▼ 下行  {}", fmt_speed(down)));
        self.lbl_up_total.set_text(&format!("累计 上行 {}", fmt_bytes(ut)));
        self.lbl_down_total.set_text(&format!("累计 下行 {}", fmt_bytes(dt)));
        self.lbl_conns.set_text(&format!("≡ {} 连接", cn));

        let lat = match st.latency.load(Ordering::Relaxed) {
            0 => "未测试".to_string(),
            u64::MAX => "不可达".to_string(),
            ms => format!("{} ms", ms),
        };
        self.lbl_latency.set_text(&format!("⏱ {}", lat));
        self.lbl_proxy.set_text(&format!(
            "🌐 代理：{}  ·  📌 自启：{}",
            if st.system_proxy.load(Ordering::Relaxed) { "开" } else { "关" },
            if st.autostart.load(Ordering::Relaxed) { "开" } else { "关" },
        ));

        // 日志增量刷新
        let seq = st.log_seq.load(Ordering::Relaxed);
        if seq != self.last_log_seq.get() {
            self.last_log_seq.set(seq);
            let logs = st.logs.lock().unwrap();
            let text = logs.join("\r\n"); drop(logs);
            self.log_box.set_text(&text);
        }

        // 节点列表仅在内容变化时刷新（避免每秒 set_text 重置用户选区）
        self.set_node_list_if_changed(&list_text);
    }

    fn on_console_close(&self) { self.console.set_visible(false); }
}

mod app_ui {
    use super::*;
    use nwg::{Event as E, TextBoxFlags, WindowFlags};
    use std::cell::RefCell;

    pub struct AppUi { inner: Rc<App>, default_handlers: RefCell<Vec<nwg::EventHandler>> }

    impl nwg::NativeUi<AppUi> for App {
        fn build_ui(mut d: App) -> Result<AppUi, nwg::NwgError> {
            // 图标
            nwg::Icon::builder().source_bin(Some(include_bytes!("green.png"))).build(&mut d.icon_green)?;
            nwg::Icon::builder().source_bin(Some(include_bytes!("red.png"))).build(&mut d.icon_red)?;
            nwg::MessageWindow::builder().build(&mut d.window)?;

            // 托盘
            nwg::TrayNotification::builder().parent(&d.window).icon(Some(&d.icon_red)).tip(Some("Sing-box · 控制台")).build(&mut d.tray)?;

            // 托盘菜单
            nwg::Menu::builder().popup(true).parent(&d.window).build(&mut d.tray_menu)?;
            nwg::MenuItem::builder().parent(&d.tray_menu).text("控制台").build(&mut d.m_console)?;
            nwg::MenuItem::builder().parent(&d.tray_menu).text("开机自启").build(&mut d.m_autostart)?;
            nwg::MenuSeparator::builder().parent(&d.tray_menu).build(&mut d.m_sep1)?;
            nwg::MenuItem::builder().parent(&d.tray_menu).text("启动").build(&mut d.m_start)?;
            nwg::MenuItem::builder().parent(&d.tray_menu).text("停止").build(&mut d.m_stop)?;
            nwg::MenuSeparator::builder().parent(&d.tray_menu).build(&mut d.m_sep2)?;
            nwg::MenuItem::builder().parent(&d.tray_menu).text("以管理员身份重启").build(&mut d.m_admin)?;
            nwg::MenuItem::builder().parent(&d.tray_menu).text("退出").build(&mut d.m_exit)?;

            // 统一 UI 字体：黑体 13pt，解决中文显示过小/模糊
            nwg::Font::builder()
                .family("SimHei")
                .size(13)
                .build(&mut d.font)?;

            // 路由模式初始勾选状态（STATE 已在 build_ui 前初始化）
            let route_mode_initial = STATE
                .get()
                .map(|s| {
                    let rm = s.nodes.lock().unwrap().route_mode().to_string();
                    if rm == "global" { nwg::CheckBoxState::Checked } else { nwg::CheckBoxState::Unchecked }
                })
                .unwrap_or(nwg::CheckBoxState::Unchecked);

            // ====== 控制台窗口（单窗口整合所有功能）======
            nwg::Window::builder().flags(WindowFlags::MAIN_WINDOW)
                .title("Sing-box 控制台").icon(Some(&d.icon_green))
                .size((510, 768)).position((220, 110))
                .build(&mut d.console)?;

            // -- 状态区 (y:10~132) --
            nwg::Label::builder().parent(&d.console).text("Sing-box · 控制台").position((10,8)).size((488,24)).font(Some(&d.font)).build(&mut d.lbl_title)?;
            nwg::Label::builder().parent(&d.console).text("○ 已停止").position((10,36)).size((230,22)).font(Some(&d.font)).build(&mut d.lbl_running)?;
            nwg::Label::builder().parent(&d.console).text("⏱ 未测试").position((260,36)).size((230,22)).font(Some(&d.font)).build(&mut d.lbl_latency)?;
            nwg::Label::builder().parent(&d.console).text("▲ 上行  0 B/s").position((10,60)).size((242,22)).font(Some(&d.font)).build(&mut d.lbl_up)?;
            nwg::Label::builder().parent(&d.console).text("▼ 下行  0 B/s").position((260,60)).size((232,22)).font(Some(&d.font)).build(&mut d.lbl_down)?;
            nwg::Label::builder().parent(&d.console).text("累计 上行 0 B").position((10,84)).size((242,22)).font(Some(&d.font)).build(&mut d.lbl_up_total)?;
            nwg::Label::builder().parent(&d.console).text("累计 下行 0 B").position((260,84)).size((232,22)).font(Some(&d.font)).build(&mut d.lbl_down_total)?;
            nwg::Label::builder().parent(&d.console).text("≡ 0 连接").position((10,108)).size((230,22)).font(Some(&d.font)).build(&mut d.lbl_conns)?;
            nwg::Label::builder().parent(&d.console).text("🌐 代理：关  ·  📌 自启：关").position((260,108)).size((232,22)).font(Some(&d.font)).build(&mut d.lbl_proxy)?;

            // -- 节点管理区 (y:140~318) --
            nwg::Label::builder().parent(&d.console).text("━━━ 节点管理 ━━━").position((10,140)).size((488,20)).font(Some(&d.font)).build(&mut d.lbl_sep_nodes)?;
            nwg::TextBox::builder().parent(&d.console).text("").position((10,162)).size((488,100))
                .flags(TextBoxFlags::VISIBLE|TextBoxFlags::TAB_STOP|TextBoxFlags::VSCROLL|TextBoxFlags::AUTOVSCROLL)
                .readonly(true).font(Some(&d.font)).build(&mut d.node_list)?;
            nwg::Label::builder().parent(&d.console).text("编号：").position((10,274)).size((42,22)).font(Some(&d.font)).build(&mut d.lbl_node_idx)?;
            nwg::TextBox::builder().parent(&d.console).text("").position((54,272)).size((44,24))
                .flags(TextBoxFlags::VISIBLE|TextBoxFlags::TAB_STOP|TextBoxFlags::AUTOHSCROLL)
                .font(Some(&d.font)).build(&mut d.node_index_input)?;
            nwg::Button::builder().parent(&d.console).text("↻ 切换").position((106,272)).size((62,24)).font(Some(&d.font)).build(&mut d.btn_node_switch)?;
            nwg::Button::builder().parent(&d.console).text("✕ 删除").position((174,272)).size((62,24)).font(Some(&d.font)).build(&mut d.btn_node_delete)?;
            nwg::Button::builder().parent(&d.console).text("◷ 测延迟").position((242,272)).size((62,24)).font(Some(&d.font)).build(&mut d.btn_latency)?;
            nwg::Button::builder().parent(&d.console).text("⚡测全部").position((310,272)).size((62,24)).font(Some(&d.font)).build(&mut d.btn_latency_all)?;
            nwg::Label::builder().parent(&d.console).text("提示：在「编号」框输入节点序号（从 0 开始）后点切换/删除").position((10,308)).size((488,22)).font(Some(&d.font)).build(&mut d.lbl_nodes_msg)?;

            // -- 导入区：链接框多行；路由方式二选一（虚拟网卡 / 系统代理）--
            nwg::Label::builder().parent(&d.console).text("━━━ 导入链接（支持多行批量）━━━").position((10,338)).size((488,20)).font(Some(&d.font)).build(&mut d.lbl_sep_import)?;
            nwg::TextBox::builder().parent(&d.console).text("").position((10,360)).size((488,52))
                .flags(TextBoxFlags::VISIBLE|TextBoxFlags::TAB_STOP|TextBoxFlags::VSCROLL|TextBoxFlags::AUTOVSCROLL|TextBoxFlags::AUTOHSCROLL)
                .font(Some(&d.font)).build(&mut d.link_input)?;
            // 路由方式：二选一（互斥）。虚拟网卡=接管所有应用；系统代理=仅浏览器等认代理软件
            nwg::CheckBox::builder().parent(&d.console).text("虚拟网卡（接管所有应用）")
                .position((10,420)).size((245,22)).flags(nwg::CheckBoxFlags::VISIBLE)
                .check_state(nwg::CheckBoxState::Checked).font(Some(&d.font)).build(&mut d.chk_tun)?;
            nwg::CheckBox::builder().parent(&d.console).text("系统代理（仅浏览器等软件）")
                .position((260,420)).size((240,22)).flags(nwg::CheckBoxFlags::VISIBLE)
                .check_state(nwg::CheckBoxState::Unchecked).font(Some(&d.font)).build(&mut d.chk_proxy)?;
            // 路由规则：勾选即绕过国内直连，所有流量走代理（即时重写配置，运行中自动重启生效）
            nwg::CheckBox::builder().parent(&d.console).text("所有网站都走代理（全局模式）")
                .position((10,452)).size((488,22)).flags(nwg::CheckBoxFlags::VISIBLE)
                .check_state(route_mode_initial).font(Some(&d.font)).build(&mut d.chk_route)?;
            // 操作按钮行：启动 / 停止 / 应用配置（三按钮等宽排列）
            nwg::Button::builder().parent(&d.console).text("▶ 启动").position((10,484)).size((156,30)).font(Some(&d.font)).build(&mut d.btn_start)?;
            nwg::Button::builder().parent(&d.console).text("■ 停止").position((171,484)).size((156,30)).font(Some(&d.font)).build(&mut d.btn_stop)?;
            nwg::Button::builder().parent(&d.console).text("✓ 应用配置").position((332,484)).size((156,30)).font(Some(&d.font)).build(&mut d.btn_apply)?;
            nwg::Label::builder().parent(&d.console).text("").position((10,520)).size((488,18)).font(Some(&d.font)).build(&mut d.lbl_import_msg)?;

            // -- 日志区：加大日志框，明确与导入框分离 --
            nwg::Label::builder().parent(&d.console).text("━━━ 运行日志 ━━━").position((10,548)).size((488,20)).font(Some(&d.font)).build(&mut d.lbl_sep_log)?;
            nwg::TextBox::builder().parent(&d.console).text("").position((10,570)).size((488,150))
                .flags(TextBoxFlags::VISIBLE|TextBoxFlags::TAB_STOP|TextBoxFlags::VSCROLL|TextBoxFlags::AUTOVSCROLL)
                .readonly(true).font(Some(&d.font)).build(&mut d.log_box)?;
            nwg::Label::builder().parent(&d.console).text("Made by Weyes").position((10,730)).size((300,22)).font(Some(&d.font)).build(&mut d.lbl_made)?;
            nwg::Button::builder().parent(&d.console).text("关闭").position((412,728)).size((80,28)).font(Some(&d.font)).build(&mut d.btn_console_close)?;

            // 定时器
            nwg::Timer::builder().parent(&d.window).interval(1000).build(&mut d.timer)?;

            let ui = AppUi { inner: Rc::new(d), default_handlers: Default::default() };

            for wh in [&ui.inner.window.handle, &ui.inner.console.handle] {
                let evt_ui = Rc::downgrade(&ui.inner);
                let handler = move |evt, edata, handle| {
                    if let Some(a) = evt_ui.upgrade() {
                        match evt {
                            E::OnContextMenu if handle == a.tray.handle => App::show_menu(&a),
                            E::OnMenuItemSelected => App::on_menu(&a, &handle),
                            E::OnButtonClick => {
                                if handle == a.btn_node_switch.handle { App::on_node_switch(&a); }
                                else if handle == a.btn_node_delete.handle { App::on_node_delete(&a); }
                                else if handle == a.btn_latency.handle { App::on_latency_test(&a); }
                                else if handle == a.btn_latency_all.handle { App::on_latency_test_all(&a); }
                                else if handle == a.btn_apply.handle { App::on_apply(&a); }
                                else if handle == a.btn_start.handle { App::on_start(&a); }
                                else if handle == a.btn_stop.handle { App::on_stop(&a); }
                                else if handle == a.btn_console_close.handle { App::on_console_close(&a); }
                                else if handle == a.chk_tun.handle { App::on_tun_changed(&a); }
                                else if handle == a.chk_proxy.handle { App::on_proxy_changed(&a); }
                                else if handle == a.chk_route.handle { App::on_route_changed(&a); }
                            }
                            E::OnWindowClose => {
                                if let nwg::EventData::OnWindowClose(cd) = edata { cd.close(false); }
                                if handle == a.console.handle { App::on_console_close(&a); }
                            }
                            E::OnTimerTick if handle == a.timer.handle => App::on_tick(&a),
                            _ => {}
                        }
                    }
                };
                ui.default_handlers.borrow_mut().push(nwg::full_bind_event_handler(wh, handler));
            }
            Ok(ui)
        }
    }

    impl Drop for AppUi {
        fn drop(&mut self) {
            for h in self.default_handlers.borrow_mut().drain(0..) { nwg::unbind_event_handler(&h); }
        }
    }
    impl Deref for AppUi {
        type Target = App; fn deref(&self) -> &App { &self.inner }
    }
}

fn main() {
    // ===== 单实例互斥：防止多次双击产生多个托盘图标 =====
    // 本进程持有互斥体直到退出（OS 自动释放）；若已存在实例则激活其窗口并退出。
    {
        let name: Vec<u16> = SINGLE_INSTANCE_MUTEX
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let h = unsafe { CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr()) };
        let already = h.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already {
            // 已运行：尝试激活已有窗口，让用户双击时原窗口弹出而非无反应
            let title: Vec<u16> = "Sing-box 控制台"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
            if !hwnd.is_null() {
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
            }
            if !h.is_null() {
                unsafe { CloseHandle(h); }
            }
            std::process::exit(0);
        }
        // h 有效且非已存在：本进程持有互斥体，正常继续（不 CloseHandle，退出时 OS 释放）
    }

    let data_dir = match std::env::current_exe() {
        Ok(exe) => exe.parent().map(|p| p.join("data")).unwrap_or_else(|| PathBuf::from("data")),
        Err(_) => PathBuf::from("data"),
    };

    let state = Arc::new(AppState::new(data_dir));
    STATE.set(state.clone()).ok();

    // 后台轮询：每秒状态 + 每30秒持久化
    thread::spawn(move || {
        let mut fc: u32 = 0;
        loop {
            thread::sleep(Duration::from_millis(1000));
            let Some(st) = STATE.get() else { continue };
            let running = st.is_running();
            if !running {
                let mut s = st.status.lock().unwrap();
                s.up_speed = 0; s.down_speed = 0; s.conns = 0;
                continue;
            }
            let (traffic, conns) = (status::fetch_traffic(st.api_port()), status::fetch_connections(st.api_port()));
            let mut s = st.status.lock().unwrap();
            if let Some((up, down)) = traffic { s.up_speed = up; s.down_speed = down; }
            if let Some((ut, dt, c)) = conns { s.up_total = ut; s.down_total = dt; s.conns = c; st.mark_traffic_dirty(); }
            drop(s);
            fc += 1;
            if fc >= 30 { fc = 0; st.flush_traffic_if_dirty(); }
        }
    });

    nwg::init().expect("nwg init");
    let app = App::build_ui(App::default()).expect("build ui");
    // 打开软件直接显示控制台（不再隐藏到托盘）
    app.refresh_all();
    app.console.set_visible(true);
    app.console.set_focus();

    let running = state.is_running();
    app.tray.set_icon(if running { &app.icon_green } else { &app.icon_red });
    app.tray.set_tip("Sing-box · 控制台");

    if !state.binary_exists() { log_line("未找到 data/sing-box.exe"); }
    if state.nodes.lock().unwrap().nodes.is_empty() { log_line("尚未导入节点，请在控制台粘贴链接并点「应用配置」"); }

    app.timer.start();
    nwg::dispatch_thread_events();
}
