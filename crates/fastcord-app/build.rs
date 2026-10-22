//! Embed the application icon in Windows executables without another crate.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icons/app/fastcord.ico");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets manifest dir"))
        .join("../../assets/icons/app/fastcord.ico")
        .canonicalize()
        .expect("application icon exists");
    let source = out.join("fastcord.rc");
    // RC quoted paths accept forward slashes, including extended Windows paths.
    let icon = icon
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    std::fs::write(&source, format!("1 ICON \"{icon}\"\n")).expect("write icon resource source");
    let resource = out.join("fastcord.res");
    let status = Command::new(resource_compiler())
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&source)
        .status()
        .expect("run Windows SDK rc.exe (or set RC to a compatible resource compiler)");
    assert!(status.success(), "Windows application icon resource failed");
    println!("cargo:rustc-link-arg-bin=fastcord={}", resource.display());
}

fn resource_compiler() -> PathBuf {
    if let Some(path) = env::var_os("RC") {
        return path.into();
    }
    // A VS developer shell already exposes rc.exe; ordinary rustup shells may not.
    if let Some(paths) = env::var_os("PATH") {
        for directory in env::split_paths(&paths) {
            let candidate = directory.join("rc.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    let root = env::var_os("WindowsSdkDir")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                env::var_os("ProgramFiles(x86)").unwrap_or_else(|| "C:/Program Files (x86)".into()),
            )
            .join("Windows Kits/10")
        });
    let mut versions: Vec<_> = std::fs::read_dir(root.join("bin"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    versions.sort();
    // rc runs on the build host, even when targeting Windows ARM64.
    let host_arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    versions
        .into_iter()
        .rev()
        .map(|version| version.join(host_arch).join("rc.exe"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("rc.exe"))
}
