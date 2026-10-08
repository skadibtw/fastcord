fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("src/videotoolbox.c")
        .warnings(true)
        .compile("fastcord_videotoolbox");
    for framework in ["CoreFoundation", "CoreMedia", "CoreVideo", "VideoToolbox"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rerun-if-changed=src/videotoolbox.c");
}
