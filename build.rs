fn main() {
    // 让链接器包含 esp-hal 生成的链接脚本（定义 _bss_start / GPIO 等符号）。
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    println!("cargo:rerun-if-changed=build.rs");
}
