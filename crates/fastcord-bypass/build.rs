use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

const ARCHIVE: &str = "native/byedpi-0.17.3.tar.gz";
const SHA256: &str = "0a9cb8585554c68c3e2be88c33c9bf6f99f8e8c7f54b362285adab99e262566c";
const ROOT: &str = "byedpi-0.17.3";
// Upstream Makefile `SRC`; win_service.c is replaced by src/byedpi.c.
const SOURCES: &[&str] = &[
    "packets", "main", "conev", "proxy", "desync", "mpool", "extend",
];
// Each replacement must match exactly once, so an upstream change fails the
// build instead of compiling silently. The engine stops when Rust sets the
// flag and then connects to the listener to wake the event loop: upstream's
// SIGINT `shutdown` does not wake WSAPoll on Windows.
const PATCHES: &[(&str, &str, &str)] = &[(
    "proxy.c",
    "static int on_accept(struct poolhd *pool, struct eval *val, int et)\n{\n",
    "extern volatile int fastcord_stop_requested;\n\n\
     static int on_accept(struct poolhd *pool, struct eval *val, int et)\n{\n\
     \x20   if (fastcord_stop_requested) {\n\
     \x20       pool->brk = 1;\n\
     \x20       return -1;\n\
     \x20   }\n",
)];
// MSVC has no C99 variable-length arrays.
const MSVC_PATCHES: &[(&str, &str, &str)] = &[
    (
        "main.c",
        "    char opt[optc + 1];\n",
        "    char *opt = _alloca(optc + 1);\n",
    ),
    (
        "main.c",
        "        char ip[e - s + 1];\n",
        "        char *ip = _alloca(e - s + 1);\n",
    ),
    (
        "proxy.c",
        "    char host[len + 1];\n",
        "    char *host = _alloca(len + 1);\n",
    ),
    // Only ever expanded with at most 16 bytes.
    (
        "error.h",
        "    char HEX_STR[s * 2 + 1]; \\\n",
        "    char HEX_STR[16 * 2 + 1]; \\\n",
    ),
];

fn main() {
    for path in [
        ARCHIVE,
        "native/byedpi-source.json",
        "native/compat",
        "src/byedpi.c",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let archive = fs::read(ARCHIVE).expect("bundled byedpi source archive");
    assert_eq!(
        format!("{:x}", Sha256::digest(&archive)),
        SHA256,
        "bundled byedpi source hash differs from the pinned recipe"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"));
    let source = out.join(ROOT);
    // Re-unpack every time so a patched or interrupted extraction is never reused.
    let _ = fs::remove_dir_all(&source);
    tar::Archive::new(GzDecoder::new(archive.as_slice()))
        .unpack(&out)
        .expect("unpacking pinned byedpi source");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let msvc_patches = if msvc { MSVC_PATCHES } else { &[] };
    for (file, from, to) in PATCHES.iter().chain(msvc_patches) {
        let path = source.join(file);
        let text = fs::read_to_string(&path).expect("byedpi source file");
        assert_eq!(text.matches(from).count(), 1, "byedpi patch site in {file}");
        fs::write(&path, text.replace(from, to)).expect("patched byedpi source");
    }

    let mut upstream = compiler(&source, msvc);
    upstream
        .define("main", "byedpi_main")
        .define("signal", "byedpi_signal");
    for name in SOURCES {
        upstream.file(source.join(format!("{name}.c")));
    }
    if msvc {
        upstream.file("native/compat/getopt.c");
    }
    upstream.compile("byedpi");
    compiler(&source, msvc)
        .file("src/byedpi.c")
        .compile("fastcord_byedpi");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // TransmitFile, used by byedpi's fake packets.
        println!("cargo:rustc-link-lib=mswsock");
    }
}

fn compiler(source: &Path, msvc: bool) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .include(source)
        .define("_DEFAULT_SOURCE", None)
        .opt_level(2)
        .warnings(false)
        .cargo_warnings(false);
    if msvc {
        let compat = Path::new(&env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
            .join("native/compat");
        build
            .include(&compat)
            .flag(format!("/FI{}", compat.join("msvc.h").display()))
            .define("_CRT_SECURE_NO_WARNINGS", None)
            .define("_WINSOCK_DEPRECATED_NO_WARNINGS", None);
    }
    build
}
