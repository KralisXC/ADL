//! 把程序图标与版本信息嵌进 exe（只在 Windows 上有活干）。
//!
//! 没有这一步时，exe 是"无图标"的：资源管理器显示白纸图标、任务栏和
//! Alt+Tab 都是默认方块；窗口的图标我们还能用 `ViewportBuilder::with_icon`
//! 现设，但文件本身的图标只能靠 PE 资源段 —— 那就必须在编译期过一遍 rc。
//!
//! 用法是 `embed_resource::compile(rc, collection)`：它自己会去注册表 /
//! vswhere 里找 `rc.exe`（本机在 `D:\Windows Kits\10\bin\10.0.26100.0\x64`）。
//! 找不到时它返回 `CompilationResult::NotAttempted` 而不是 panic，这里只打一行
//! 提示、继续编 —— CI 上缺 Windows SDK 只是"没图标"，不该让整个构建失败。

fn main() {
    #[cfg(windows)]
    {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let rc = manifest_dir.join("assets").join("adl.rc");
        // 图标换了要让 cargo 重跑本脚本，否则 exe 里的资源永远是旧的。
        println!("cargo:rerun-if-changed={}", rc.display());
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join("assets").join("adl.ico").display()
        );

        // `manifest_optional()`：NotWindows / Ok / NotAttempted 都算成功，
        // 只有真的调用 rc.exe 并失败才算错。而我们连错也不 panic ——
        // 没图标的 exe 仍然能用，编不出 exe 才是事故。
        if let Err(e) = embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
            println!("cargo:warning=嵌入图标资源失败（exe 将没有图标）：{e:?}");
        }
    }
}
