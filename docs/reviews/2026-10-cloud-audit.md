# Cloud audit, 2026-10-12: build, security, spec conformance

Commit audited: `a51a272` (`feat: add AV1 receive (M26)`), the head of `main` on GitHub.
Scope: an offline review only. No Discord token was used and no source file was modified.
Line numbers refer to `a51a272`.

## 1. Build check (Linux, x86_64, toolchain 1.99.0)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` (with `RUSTFLAGS=-D warnings`, as in CI) | pass |
| `cargo test --workspace --all-targets --locked` | pass: 944 passed, 0 failed, 14 ignored (8 test binaries) |

GitHub CI on the same commit (run 38216355490): `fmt` and `check (ubuntu-24.04)` pass. **`check (macos-15)` and `macos-intel-codec` fail** (finding B1). `check (windows-2025)` passes clippy and tests.

## 2. Findings

Severity levels: **blocker** breaks a build or release, **major** is a real security or correctness defect, **minor** is a spec or documentation defect with limited impact.

### B1 (blocker): dav1d fails to compile on macOS, so `main` is red on both macOS targets

- **Where:** `crates/fastcord-video/build.rs:197` (with `native_compiler`, `build.rs:118-144`).
- **Evidence:** CI jobs `check (macos-15)` (job 114701902551) and `macos-intel-codec` (job 114701902409) fail in `cc-rs` while compiling `dav1d/src/cpu.c`:
  `MacOSX.sdk/usr/include/sys/ucred.h:101:2: error: unknown type name 'u_int'` (plus seven more `u_int`/`u_char`/`u_short` errors from `sys/proc.h`).
  `configure_dav1d` writes `#define _POSIX_C_SOURCE 200809L` into `config.h` for every non-Windows target, and the code is built with `-std=c11`. On Darwin, a strict `_POSIX_C_SOURCE` hides the BSD typedefs that `<sys/sysctl.h>` needs. Upstream dav1d's meson build defines `_DARWIN_C_SOURCE` on Darwin for this reason. Linux passes because `build.rs:140` adds `_GNU_SOURCE`, but there is no macOS equivalent.
  The M26 commit message says macOS builds of the native code "are covered only by CI", and that CI is the job failing here. The release workflow builds `aarch64-apple-darwin` and `x86_64-apple-darwin` from the same `build.rs`, so a tagged release would fail too.
- **Fix:** in `configure_dav1d`, also emit `#define _DARWIN_C_SOURCE 1` when `target_os == "macos"`. Alternatively, call `build.define("_DARWIN_C_SOURCE", None)` in `native_compiler` for macOS, mirroring the Linux `_GNU_SOURCE` branch. Re-run both macOS CI jobs to confirm.

### B2 (blocker): the release workflow does not install the ALSA headers, so the Linux release build fails

- **Where:** `.github/workflows/release.yml:31-51` (the `build` job), compared with `.github/workflows/ci.yml:42-44`.
- **Evidence:** since M19, `fastcord-audio` depends on `cpal`, which links ALSA through `alsa-sys`. That crate's build script needs `alsa.pc` (`libasound2-dev`). `ci.yml` installs the package explicitly before clippy and test on Linux, but `release.yml` runs `cargo build --release --locked -p fastcord-app --target x86_64-unknown-linux-gnu` on `ubuntu-24.04` without that step. The only release runs are from 2026-10-07: a dry run that failed, and the `v0.1.0-alpha.1` tag at the empty-window commit. Neither ran after `cpal` was added, so this path has never run since the dependency arrived. With `fail-fast: true`, one failed target cancels the whole release.
- **Fix:** add the same step to the release `build` job: `if: runner.os == 'Linux'`, then `sudo apt-get update && sudo apt-get install -y --no-install-recommends libasound2-dev`. After that, run the documented dry run (`gh workflow run release.yml`) once B1 is fixed, and confirm that all four targets package.

### M1 (major, security): the "never open programs" guard is a short denylist with real gaps

- **Where:** `crates/fastcord-platform/src/lib.rs:47-83` (`open_local_file`, `PROGRAM_EXTENSIONS`), called from `crates/fastcord-app/src/attachment_ui.rs:405-418`.
- **Evidence:** **Open** hands a file from the attachment cache to `explorer.exe`, `open` or `xdg-open`. Both the attachment and its file name come from another Discord user, and the extension survives `safe_extension` (`attachments.rs:965`, any 1-12 alphanumeric characters). AGENTS.md and the UI text promise that programs and scripts are never launched from here. The list does block `dmg` ("install/mount"), but these handlers that run code or mount images are missing:
  - **Windows:** `iso`, `img`, `vhd`, `vhdx` (mounting; the same class as the blocked `dmg`), `chm` (compiled HTML Help runs scripts), `application` and `appref-ms` (ClickOnce), `appx`, `appxbundle`, `msix`, `msixbundle`, `appinstaller`, `py` and `pyw` (run directly when the Python launcher is installed), `settingcontent-ms`, `xll`, `diagcab`, `mst`, `library-ms`, `search-ms`.
  - **macOS:** `terminal` and `tool` (run shell commands in Terminal.app), `workflow` (Automator), `fileloc` and `inetloc` (launch targets), `mpkg`.

  Files written by fastcord also carry no Mark-of-the-Web (`Zone.Identifier`) on Windows. SmartScreen and Office Protected View therefore do not apply, which leaves this list as the only safeguard.
- **Fix:** invert the policy for **Open** to an allowlist of passive types that fastcord already understands (the image, audio, video, `pdf` and `txt` set from `mime_extension` in `attachment_ui.rs:505`). Everything else goes through **Download** only. If a denylist is kept, add at least the extensions above. On Windows, also write a `Zone.Identifier` stream (`ZoneId=3`) on cached files before opening them.

### M2 (major, licensing): statically bundled libopus has no binary-redistribution notice

- **Where:** `docs/THIRD_PARTY_NOTICES.md:1-6` (scope statement) and `scripts/package.sh:14-18`. The dependency is `Cargo.toml:40`, which enables `opus2` with `backend-libopus`, `static` and `bundled`.
- **Evidence:** every release binary links libopus statically. libopus is BSD-3-Clause, and clause 2 requires binary redistributions to reproduce its copyright notice and conditions in the accompanying documentation. `THIRD_PARTY_NOTICES.md` says it covers only the noise-suppression engines and dav1d, and the archive contains no other notice. This is the same class of obligation the file already meets for dav1d (BSD-2). SPEC §13.2 and the §15 "Codec/asset licensing" row require a notice audit before publication.
- **Fix:** add a libopus section with the `COPYING` text from the bundled source that `opus2-sys` vendors at the version pinned by `Cargo.lock`. Better still, generate a full Rust-crate notice bundle (for example with `cargo about`) into the archive. That also covers the MIT notice requirements of `davey`, OpenMLS and the rest of the dependency tree.

### m1 (minor, spec conformance): attachments over 64 MiB cannot be downloaded or opened at all

- **Where:** `crates/fastcord-app/src/attachments.rs:36-38` (`MAX_FILE_BYTES = DISK_CACHE_LIMIT / 4`) and `attachment_ui.rs:386-404`.
- **Evidence:** **Download** first copies the whole file into the 256 MiB attachment cache and then copies it to the chosen destination. Anything over 64 MiB fails with "larger than the in-app download limit". Discord allows uploads well above that (boosted servers, Nitro). SPEC §2.1 and the M14 acceptance criterion say that unsupported files remain accessible through an explicit open or download action, and §5.3 bounds the *cache*, not user-requested saves.
- **Fix:** stream **Download** straight to the destination the user picked, through the same credential-free redirect-validated client, without charging the cache. Keep the 64 MiB cap only for **Open** and previews, which do go through the cache.

### m2 (minor, docs): SPEC still describes an empty repository

- **Where:** `docs/SPEC.md:25` (§2.2: "The repository inspection found only `LICENSE` … Every path in this specification other than `LICENSE` describes a proposed implementation"), `docs/SPEC.md:655` (§17: "Implementation does not exist yet"), `docs/SPEC.md:585` (§16: "none have been run for this specification").
- **Evidence:** M1-M27 (including 5a and 23a) and M31 are implemented, and their offline results and pending live checks are recorded in `docs/TESTING.md`. SPEC is the declared source of truth (AGENTS.md), so these sentences now contradict the repository.
- **Fix:** reword them to say that the evidence status described the state when the spec was written, and point to `docs/TESTING.md` and `docs/PROTOCOL.md` for what is now implemented and verified.

### m3 (minor, docs): SPEC §3 and §13.2 list a `native/` root and a `native-sources` artifact that do not exist

- **Where:** `docs/SPEC.md:58` (`native/   # pinned native dependency build recipes/manifests`) and `docs/SPEC.md:527` (`fastcord-vX.Y.Z-native-sources.tar.gz`), compared with `.github/workflows/release.yml:76-78`.
- **Evidence:** the only native source lives in `crates/fastcord-video/native/` (the dav1d archive). The release workflow produces `-source.tar.gz` and `SHA256SUMS` but no `native-sources` archive. libopus comes from the `opus2-sys` crate source and is not in either archive.
- **Fix:** either update §3 and §13.2 to the per-crate layout and to "the source archive contains the dav1d tarball", or add the `native-sources` archive (dav1d plus the vendored libopus source) to `release.yml`.

### m4 (minor, docs): stale statements in README.md and AGENTS.md

- `README.md:127-128`: "fetching and viewing attachment images is milestone 14." M14 shipped in `71301b6`, and README has no attachments section that replaces this line.
  **Fix:** describe the shipped behavior (Load preview, larger view, Download/Open, 64 MiB limit) or link to the CHANGELOG entry.
- `AGENTS.md:69`: "Planned: `davey` (DAVE)." `davey =0.1.4` is a dependency (`Cargo.toml:26`) and DAVE has been mandatory since M18 (`fastcord-media::dave`; AGENTS.md:30 already says so).
  **Fix:** move `davey` (pinned 0.1.4, behind the `dave` adapter) into the list of current dependencies.

### m5 (minor, docs): `docs/TESTING.md` has two "Windows Media Foundation codecs (M27)" sections, and one is under "Automated (CI)"

- **Where:** `docs/TESTING.md:11-14` and `docs/TESTING.md:52-56`.
- **Evidence:** the first copy sits under "Automated (CI, every push)" but records a manual hardware run on an RX 9070 XT, which CI cannot do. The second copy, under "Live / manual", records a different invocation (`--include-ignored`, "33 passed") and different observations. A reader cannot tell which is current. The macOS M31 section (`docs/TESTING.md:6-9`) is placed under "Automated" in the same way.
- **Fix:** merge the two M27 sections under "Live / manual". Under "Automated", keep only what CI actually runs (the macOS software round trip and the Intel cross-check).

## 3. Areas reviewed with no finding

These were checked and found consistent with SPEC, AGENTS.md and PROTOCOL.md:

- **Secrets:** `UserToken`, `VoiceToken` and `TransportKey` are zeroizing with redacted `Debug`. The voice `secret_key` is decoded straight into `Zeroizing<[u8; 32]>`. Keyring errors are mapped to categorical errors, and malformed secret bytes are zeroized. No `println!`/`eprintln!` in non-test code prints secrets. The debug-only stream statistics print counters only.
- **CDN client:** a separate client with no default headers, `Policy::none()` redirects followed manually with HTTPS and no-userinfo validation on every hop, at most 5 hops, signed query strings preserved, and one authenticated refresh on 403/404. Disk and decoded caches are byte-bounded and account-scoped, the cache root is created with `0700`, and partial files are reserved and cleaned up.
- **Transport crypto:** one `TransportCipher` per voice connection, so audio, video, RTX and RTCP share one strictly increasing nonce, which fails closed at `u32::MAX`. `open` bounds-checks before splitting, and AEAD failures clear the output.
- **dav1d FFI** (`fastcord-video/src/av1.rs`, `src/dav1d.c`): OBU framing is validated before native entry. The C bridge clamps dimensions and pool bytes, and returns plane lengths that cover exactly the visible rows. The Rust slices built from them (`emit`, `interleave_chroma`) stay inside those lengths for every layout and bit depth checked (4:0:0, 4:2:0, 4:2:2, 4:4:4; 8, 10 and 12 bit; odd sizes). The context is discarded after any error.
- **Bounded queues and idle behavior:** every Tokio `mpsc` channel in non-test code is bounded. The only UI timer is the 20 ms emoji animation tick, present only while a visible animated emoji is active (with a test asserting no timer at idle).
- **Dependency direction:** `model <- discord`, `model <- media`, `media <- audio`, `video` (no internal dependencies) and `app` match SPEC §3 and AGENTS.md, with no cycles.

## 4. Not covered

- Windows-only code (`media_foundation/*`, about 80 `unsafe` sites) and macOS-only code (`videotoolbox.rs`/`.c`) were read only superficially, because they cannot be built on Linux. They deserve a dedicated review on their own platforms.
- No live Discord check was run, by design.
