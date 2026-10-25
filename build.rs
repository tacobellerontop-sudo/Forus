//! Embeds the app icon, manifest and version info into forus.exe.

fn main() {
    println!("cargo:rerun-if-changed=assets/forus.ico");
    println!("cargo:rerun-if-changed=assets/forus.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    // Resource ID 1: the app icon, also used for registered file types.
    res.set_icon_with_id("assets/forus.ico", "1")
        .set_manifest_file("assets/forus.manifest")
        .set("FileDescription", "Forus")
        .set("ProductName", "Forus")
        .set("OriginalFilename", "forus.exe");
    if let Err(e) = res.compile() {
        panic!("failed to embed Windows resources: {e}");
    }
}
