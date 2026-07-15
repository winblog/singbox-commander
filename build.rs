// 构建脚本：把 embed.rc 编译进 exe，使托盘图标成为内嵌资源（单文件分发）。
// embed-resource 在 MSVC 目标下会调用 rc.exe（需 VS 构建环境，与 `cargo build` 一致）。
// 注意：embed-resource 2.x 起 compile() 必须带第二个参数（NONE = 无额外宏/包含目录），
// 否则与 1.x 的一参写法不兼容会编译报错。
fn main() {
    embed_resource::compile("embed.rc", embed_resource::NONE);
}
