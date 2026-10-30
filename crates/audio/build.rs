//! Sets `cfg(desktop)` for the targets the plugin host runs on (Linux,
//! Windows, macOS), so its loaders aren't built for the phones.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(desktop)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(os.as_str(), "linux" | "windows" | "macos") {
        println!("cargo::rustc-cfg=desktop");
    }
}
