//! 把内核链接脚本的**绝对路径**传给链接器。
//!
//! 为什么不写在 `.cargo/config.toml` 的 `rustflags` 里：那里的相对路径会相对于
//! 调用 cargo 时的当前目录解析，换个目录构建就会失败；而 build script 里能拿到
//! `CARGO_MANIFEST_DIR`，因此路径总是正确的。

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
}
