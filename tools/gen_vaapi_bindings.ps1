# Regenerates crates/fastcord-video/src/vaapi/bindings.rs offline.
#
# Prerequisites (one-time):
#   * bindgen CLI 0.73.2 on PATH (or set to $Bindgen)
#   * libva 2.24.1 headers extracted from native/libva-2.24.1.tar.bz2
#   * a libclang build; on Windows also set LIBCLANG_PATH to its bin/ directory
#
# Run from the repository root:
#   powershell -File tools/gen_vaapi_bindings.ps1 <path-to-extracted-libva-root>
param(
    [Parameter(Mandatory = $true)][string]$LibvaRoot,
    [string]$Bindgen = "C:\Users\user\.cargo\bin\bindgen.exe",
    [string]$ClangInclude = "C:\Users\user\llvm\clang+llvm-23.1.3-x86_64-pc-windows-msvc\lib\clang\23\include"
)

if (-not (Test-Path $Bindgen)) { throw "bindgen not found: $Bindgen" }
if (-not (Test-Path "$LibvaRoot\va\va.h")) { throw "libva headers not found under: $LibvaRoot" }

$out = "crates\fastcord-video\src\vaapi\bindings.rs"
New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null

# Case-sensitive substring blocklists. Inline regex flags such as "(?i)" are
# silently ignored here whenever the function allowlist is in effect, so every
# spelling of each excluded codec family is listed explicitly. DRM entries are
# intentionally kept: fastcord needs PRIME export/import on Linux.
$blocked = @(".*HEVC.*", ".*Hevc.*", ".*VP8.*", ".*VP9.*", ".*AV1.*",
    ".*JPEG.*", ".*Jpeg.*", ".*MPEG.*", ".*Mpeg.*", ".*FEI.*", ".*Fei.*",
    ".*VVC.*", ".*Vvc.*", ".*Proc.*", ".*Subpic.*", ".*Cipher.*", ".*Motion.*",
    ".*Vpp.*", ".*VPP.*", ".*Trace.*", ".*TPI.*", ".*Protected.*", ".*GLX.*",
    ".*X11.*", ".*Wayland.*", ".*HDR.*")

$blockArgs = @()
foreach ($pattern in $blocked) {
    $blockArgs += "--blocklist-type"
    $blockArgs += $pattern
}

& $Bindgen "tools\vaapi_bindings_wrapper.h" `
    @blockArgs `
    --allowlist-function "va[A-Z]\w+" `
    --allowlist-type "VA.*" `
    --allowlist-var "VA[A-Z0-9_]+" `
    -o $out `
    -- -target x86_64-unknown-linux-gnu "-I$($LibvaRoot.Replace('\', '/'))" `
    "-isystem$($ClangInclude.Replace('\', '/'))"
if ($LASTEXITCODE -ne 0) { throw "bindgen failed" }

# Prepend the module header (LF line endings; keep the bindgen banner first).
$body = [System.IO.File]::ReadAllText((Resolve-Path $out))
$header = [string]::Join("`n", @(
    "//! VA-API (libva) bindings, generated offline with bindgen 0.73.2.",
    "//! Hand-written VA-API glue lives in the sibling modules; this vendored",
    "//! file must not be edited by hand.",
    "",
    "// Provenance and the pinned source archive SHA-256 live in",
    "// crates/fastcord-video/native/libva-source.json; the archive is committed as",
    "// crates/fastcord-video/native/libva-2.24.1.tar.bz2 (libva 2.24.1, MIT/X11).",
    "// Regenerate exactly as shipped:",
    "//   1. tar -xjf crates/fastcord-video/native/libva-2.24.1.tar.bz2 (keep <dir>)",
    "//   2. powershell -File tools/gen_vaapi_bindings.ps1 -LibvaRoot <dir>",
    "//   3. cargo fmt --all",
    "// The wrapper header, the allowlists and the blocklists are in the script.",
    "",
    "#![allow(dead_code, non_camel_case_types, non_snake_case, non_upper_case_globals, unused_imports)]",
    "#![allow(clippy::all)]",
    ""
)) + "`n"
[System.IO.File]::WriteAllText((Resolve-Path $out), $header + $body)

Write-Output "blocklist args: $($blockArgs.Count)"
Write-Output "wrote $out ($((Get-Item $out).Length) bytes, $((Get-Content $out | Measure-Object -Line).Lines) lines)"
