//! 把用户提供的图标嵌入 Windows EXE 资源，窗口和托盘共用同一图案。
/// 编译用户图标为 Windows 资源，失败时中止构建。
fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/app.ico")
            .set("ProductName", "Agent Router")
            .set("FileDescription", "Agent Router")
            .compile()
            .expect("compile Windows icon resource");
    }
}
