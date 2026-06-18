fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set");
    let plist = format!("{manifest_dir}/Info.plist");

    println!("cargo:rerun-if-changed=Info.plist");
    println!(
        "cargo:rustc-link-arg-bin=wizpr-ring-desktop=-Wl,-sectcreate,__TEXT,__info_plist,{plist}"
    );
}
