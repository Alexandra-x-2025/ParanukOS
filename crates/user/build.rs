//! 把用户态服务的链接脚本交给链接器（与内核 crate 同样的做法）。
fn main() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("linker.ld");
    println!("cargo:rustc-link-arg=-T{}", script.display());
    println!("cargo:rerun-if-changed=linker.ld");
}
