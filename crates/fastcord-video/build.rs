use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

const DAV1D_ARCHIVE: &str = "native/dav1d-1.5.4.tar.gz";
const DAV1D_SHA256: &str = "9edb11a2108b375cc58370354e705feebc93430bb780130363815c1e1ac0c250";
const DAV1D_ROOT: &str = "dav1d-54706fc6bc0cdecab7e9593974a4039cc038fca7";

// Keep these lists in step with the pinned source's src/meson.build. The
// portable C implementation deliberately does not need an assembler.
const BASE_SOURCES: &[&str] = &[
    "cdf",
    "cpu",
    "ctx",
    "data",
    "decode",
    "dequant_tables",
    "getbits",
    "intra_edge",
    "itx_1d",
    "lf_mask",
    "lib",
    "log",
    "mem",
    "msac",
    "obu",
    "pal",
    "picture",
    "qm",
    "ref",
    "refmvs",
    "scan",
    "tables",
    "thread_task",
    "warpmv",
    "wedge",
];
const BITDEPTH_SOURCES: &[&str] = &[
    "cdef_apply_tmpl",
    "cdef_tmpl",
    "fg_apply_tmpl",
    "filmgrain_tmpl",
    "ipred_prepare_tmpl",
    "ipred_tmpl",
    "itx_tmpl",
    "lf_apply_tmpl",
    "loopfilter_tmpl",
    "looprestoration_tmpl",
    "lr_apply_tmpl",
    "mc_tmpl",
    "recon_tmpl",
];

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS");
    build_dav1d(&target_os);
    if target_os != "macos" {
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

fn build_dav1d(target_os: &str) {
    println!("cargo:rerun-if-changed={DAV1D_ARCHIVE}");
    println!("cargo:rerun-if-changed=native/dav1d-source.json");
    println!("cargo:rerun-if-changed=src/dav1d.c");
    let archive = fs::read(DAV1D_ARCHIVE).expect("bundled dav1d source archive");
    assert_eq!(
        format!("{:x}", Sha256::digest(&archive)),
        DAV1D_SHA256,
        "bundled dav1d source hash differs from the pinned recipe"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo build output directory"));
    // OUT_DIR belongs to this build, not the checkout. Re-unpacking ensures
    // an interrupted/stale extraction is never used as the source of truth.
    tar::Archive::new(GzDecoder::new(archive.as_slice()))
        .unpack(&out)
        .expect("unpacking pinned dav1d source");
    let source = out.join(DAV1D_ROOT);
    configure_dav1d(&out, target_os);
    let mut base = native_compiler(&source, &out, target_os);
    for name in BASE_SOURCES {
        base.file(source.join("src").join(format!("{name}.c")));
    }
    if target_os == "windows" {
        base.file(source.join("src/win32/thread.c"));
    }
    // One archive avoids a cyclic static-link dependency between the common
    // transform tables and the bitdepth-specific implementations.
    base.file("src/dav1d.c");
    for bitdepth in ["8", "16"] {
        let mut build = native_compiler(&source, &out, target_os);
        let bitdepth_out = out.join(format!("bitdepth-{bitdepth}"));
        fs::create_dir_all(&bitdepth_out).expect("dav1d bitdepth object directory");
        build.out_dir(bitdepth_out);
        build.define("BITDEPTH", bitdepth);
        for name in BITDEPTH_SOURCES {
            build.file(source.join("src").join(format!("{name}.c")));
        }
        base.objects(build.compile_intermediates());
    }
    base.compile("fastcord_dav1d");
    if target_os != "windows" {
        println!("cargo:rustc-link-lib=pthread");
    }
}

fn native_compiler(source: &Path, out: &Path, target_os: &str) -> cc::Build {
    let mut build = cc::Build::new();
    build
        .include(out)
        .include(source)
        .include(source.join("include"))
        .include(source.join("include/dav1d"))
        .opt_level(2)
        .warnings(false)
        .define("NDEBUG", None);
    if target_os == "windows" {
        // Upstream's atomics compatibility header works with ordinary MSVC;
        // using it avoids depending on optional experimental C11 atomics.
        if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            build.include(source.join("include/compat/msvc"));
            build.flag("/std:c11");
        } else {
            build.std("c11");
        }
    } else {
        build.std("c11").flag("-pthread");
        if target_os == "linux" {
            build.define("_GNU_SOURCE", None);
        } else if target_os == "macos" {
            // Darwin's sysctl headers need BSD typedefs hidden by strict C11
            // and _POSIX_C_SOURCE; enable the same extensions as upstream.
            build.define("_DARWIN_C_SOURCE", None);
        }
    }
    build
}

fn configure_dav1d(out: &Path, target_os: &str) {
    let windows = target_os == "windows";
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture");
    let endian = env::var("CARGO_CFG_TARGET_ENDIAN").expect("Cargo target endianness");
    let defines = [
        ("CONFIG_8BPC", true),
        ("CONFIG_16BPC", true),
        ("CONFIG_LOG", false),
        ("HAVE_ASM", false),
        ("TRIM_DSP_FUNCTIONS", false),
        ("HAVE_CLOCK_GETTIME", !windows),
        ("HAVE_SIGACTION", !windows),
        ("HAVE_POSIX_MEMALIGN", !windows),
        ("HAVE_MEMALIGN", false),
        ("HAVE_ALIGNED_ALLOC", false),
        ("HAVE_DLSYM", false),
        ("HAVE_SYS_TYPES_H", true),
        ("HAVE_UNISTD_H", !windows),
        ("HAVE_IO_H", windows),
        ("HAVE_PTHREAD_NP_H", false),
        ("HAVE_PTHREAD_GETAFFINITY_NP", false),
        ("HAVE_PTHREAD_SETAFFINITY_NP", false),
        ("HAVE_PTHREAD_SETNAME_NP", false),
        ("HAVE_PTHREAD_SET_NAME_NP", false),
        ("HAVE_GETAUXVAL", false),
        ("HAVE_ELF_AUX_INFO", false),
        ("HAVE_C11_GENERIC", false),
        ("ENDIANNESS_BIG", endian == "big"),
        ("ARCH_X86", matches!(arch.as_str(), "x86" | "x86_64")),
        ("ARCH_X86_64", arch == "x86_64"),
        ("ARCH_X86_32", arch == "x86"),
        ("ARCH_AARCH64", arch == "aarch64"),
        ("ARCH_ARM", arch == "arm"),
        ("ARCH_PPC64LE", arch == "powerpc64" && endian == "little"),
        ("ARCH_RISCV", matches!(arch.as_str(), "riscv32" | "riscv64")),
        ("ARCH_RV32", arch == "riscv32"),
        ("ARCH_RV64", arch == "riscv64"),
        ("ARCH_LOONGARCH", arch == "loongarch64"),
        ("ARCH_LOONGARCH32", false),
        ("ARCH_LOONGARCH64", arch == "loongarch64"),
    ];
    let mut config = String::from("/* Generated for the pinned portable dav1d C build. */\n");
    for (name, enabled) in defines {
        config.push_str(&format!("#define {name} {}\n", u8::from(enabled)));
    }
    if windows {
        config.push_str(
            "#define _WIN32_WINNT 0x0601\n#define UNICODE 1\n#define _UNICODE 1\n\
             #define _CRT_DECLARE_NONSTDC_NAMES 1\n",
        );
    } else {
        config.push_str("#define _POSIX_C_SOURCE 200809L\n");
    }
    fs::write(out.join("config.h"), config).expect("dav1d platform configuration");
    fs::write(
        out.join("vcs_version.h"),
        "#define DAV1D_VERSION \"1.5.4\"\n",
    )
    .expect("dav1d pinned version");
}
