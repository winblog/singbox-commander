# Sing-box Commander

> 基于 **Rust + native-windows-gui** 的 Windows 代理管理图形界面。粘贴节点链接，自动解析并动态生成 Sing-box 配置，一键接管全局流量。

## 功能特性

- 🖥 **轻量原生 GUI**：纯 Rust 编译，无运行时依赖，单文件 exe（约 624 KB）
- 🔗 **节点解析**：粘贴 `vless` / `vmess` 等订阅链接，自动解析并管理节点列表
- 🔀 **两种接管模式**（互斥）
  - **虚拟网卡（TUN）**：接管系统全部应用的流量（推荐）
  - **系统代理**：仅浏览器等识别系统代理的软件生效
- 🌐 **全局模式**：勾选「所有网站都走代理」，路由规则切换为 `global`
- 📡 **延迟测速**：单节点 / 全部节点一键测速
- 🚀 **启动 / 停止**：一键拉起或停止后台 `sing-box` 子进程
- 📂 **开机自启**：可选随系统启动（注册表实现）
- 🔧 **托盘常驻**：最小化到系统托盘，右键菜单含启动 / 停止 / 应用配置 / 以管理员重启 / 开机自启
- ⚙️ **动态配置**：修改设置后若已在运行则即时重载；未运行时仅写盘，下次启动生效

## 系统要求

- Windows 7 及以上（x86_64）
- 需自行准备 **Sing-box 1.11+** 的 `sing-box.exe`，放置于程序 `data/` 目录（程序会作为子进程拉起它）

## 构建

```bash
# 需要 Rust MSVC 工具链 (x86_64-pc-windows-msvc)
cargo build --release
# 产物：target/release/SingBoxGUI.exe
```

已开启 `[profile.release]` 体积优化：`strip = true`、`lto = true`、`opt-level = "z"`、`panic = "abort"`、`codegen-units = 1`。

## 使用步骤

1. 将 `sing-box.exe`（1.11+）放到程序 `data/` 目录
2. 打开程序，控制台默认显示
3. 在节点框粘贴节点链接，点击 **✓ 应用配置**（生成 `data/config.json`）
4. 选择接管方式：**虚拟网卡（TUN）** 或 **系统代理**
5. 点击 **▶ 启动** 拉起 sing-box；**■ 停止** 关闭
6. 右键托盘图标可管理启动 / 停止 / 自启等

> 运行中修改设置会即时重载；未启动时修改仅写入配置，下次启动生效。

## 目录结构

```
singbox-native/
├── src/
│   ├── main.rs        # 主入口：UI、事件分发、托盘、右键菜单
│   ├── singbox.rs     # 进程管理、链接解析、系统代理/自启注册表、端口分配
│   ├── nodes.rs       # 节点列表加载 / 保存
│   ├── config_gen.rs  # 生成 sing-box 配置（mixed 入站 + TUN + clash_api）
│   └── status.rs      # AppState 共享状态（原子端口、运行标志、互斥路由等）
├── Cargo.toml
└── .gitignore
```

## 隐私说明

- 节点链接、配置均**仅存储于本机**（`data/nodes.json`、`data/config.json`），程序不联网上报任何数据。
- 本项目**不包含** `data/`、`发布/`、编译产物（`target/`）与二进制可执行文件，详见 `.gitignore`。

## 许可证

本项目以 **Apache License 2.0** 发布。详见仓库根目录的 [`LICENSE`](./LICENSE) 文件。

版权所有 © 2026 Weyes。
