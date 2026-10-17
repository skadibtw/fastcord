# ADR 0001: OS-native video codecs plus dav1d, no FFmpeg

Status: accepted (2026-10-07)

## Context
Screen share must send H.264 and receive whatever official clients send (H.264 by default, AV1 from recent GPUs; VP8/VP9 unconfirmed, SPEC U4). Primary goals are low CPU and RAM. The original spec draft used `openh264` plus FFmpeg via `ffmpeg-next`.

## Decision
- **Windows:** Media Foundation for H.264 encode/decode (hardware MFT preferred, Microsoft software MFT fallback). Windows.Graphics.Capture D3D11 textures go straight to the hardware encoder on a shared D3D11 device.
- **macOS:** VideoToolbox for H.264 encode/decode.
- **Linux:** VA-API for H.264 encode/decode (hardware candidates per DRM render node first), then Cisco's OpenH264 as the single software fallback candidate (SPEC milestone 30). Details below.
- **AV1 decode, all OSes:** `dav1d`; OS hardware AV1 where exposed.
- **VP8/VP9:** `libvpx`, added only if live tests show Discord senders emit them.
- No FFmpeg, no codec subprocesses.

## The Linux H.264 backend (added by milestone 30)

- **OpenH264** (software fallback): the `openh264` 0.9.8 Rust wrapper bundles Cisco's
  `cisco/openh264` source pinned at `4a2615fac570c6ca1ed4f157b9fdab9466edfd80`
  (no runtime loader; no network fetch; the wrapper owns all native objects), so
  the codec itself is compiled into every binary and the identical offline
  round-trip tests run on all three OSes. Only Linux registers the software
  candidates; Windows (Media Foundation) and macOS (VideoToolbox) behavior is
  unchanged. The wrapper exposes no safe mid-session bitrate change, so
  `VideoEncoder::set_bitrate` keeps the contract's default `Unsupported` answer
  and the share pipeline reopens at a fresh keyframe instead.
- **VA-API**: libva 2.24.1 is *dynamically linked* on Linux targets
  (`libva`/`libva-drm` only; `x11`, `wayland`, `glx` are not required). Bindings
  are generated once offline with bindgen 0.73.2 from the committed source
  archive `crates/fastcord-video/native/libva-2.24.1.tar.bz2` (SHA-256 pinned in
  `native/libva-source.json`) and committed as
  `src/vaapi/bindings.rs`; the build needs no bindgen, libclang or pkg-config,
  and regenerating is a documented script run (`tools/gen_vaapi_bindings.ps1`).
  Runtime users need their distribution's `libva2`/`libva-drm2` (or equivalent)
  packages plus a VA-API driver; upstream `libva-sys` crates were rejected
  because they need Meson/Ninja/pkg-config plus bindgen at every build.
- **Devices and profiles**: candidates are the `/dev/dri/renderD*` nodes in
  ascending order, probed for `VAEntrypointEncSlice` with 4:2:0 + CBR rate
  control (encoder) or `VAEntrypointVLD` on the H.264 constrained
  Baseline/Main/High/Baseline profiles (decoder); a device that will not probe
  is skipped, and `/dev/dri` being unreadable is a discovery error, as with the
  other platforms' enumeration step.
- **Selection**: single `select` pass for all OSes — hardware candidates first
  in device order, then OpenH264 last in automatic mode; explicit
  Hardware/Software preferences never fall back.
- Runtime hardware decode failure recovery stays in the transport layer
  (`stream_video`): a decoder that stops answering is replaced and the next
  keyframe rebuilds the stream, exactly as Media Foundation behaves on Windows.

## Consequences
- Pros: zero-copy GPU path on Windows (capture → encode never touches system RAM); no FFmpeg MSVC build or 20–40 MB of DLLs; H.264 patent licensing covered by the OS on Windows/macOS; small, maintained dependencies (`dav1d` is BSD-2).
- Cons: three H.264 backends behind one `video` codec trait (matches the per-OS capture backends we need anyway); Linux OpenH264 built from source lacks Cisco's binary patent coverage, and Linux binaries require the libva runtime packages in addition to a VA-API driver — the licensing note and the package requirements are recorded in `THIRD_PARTY_NOTICES.md`, `docs/TESTING.md`, and the packaging docs, and general H.264 usage licensing remains the user's responsibility.
- Video/audio **attachments** are not played inline in MVP; they open in the OS default player.

## Rejected
- **FFmpeg (`ffmpeg-next`)**: crate in maintenance mode, heavy native build, LGPL linking constraints, and zero-copy still needs per-OS interop.
- **Hybrid (native on Windows/macOS, FFmpeg on Linux)**: keeps the full FFmpeg build/packaging cost for one platform.
- **GStreamer**: larger runtime than FFmpeg.
