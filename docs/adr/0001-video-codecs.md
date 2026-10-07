# ADR 0001: OS-native video codecs plus dav1d, no FFmpeg

Status: accepted (2026-10-07)

## Context
Screen share must send H.264 and receive whatever official clients send (H.264 by default, AV1 from recent GPUs; VP8/VP9 unconfirmed, SPEC U4). Primary goals are low CPU and RAM. The original spec draft used `openh264` plus FFmpeg via `ffmpeg-next`.

## Decision
- **Windows:** Media Foundation for H.264 encode/decode (hardware MFT preferred, Microsoft software MFT fallback). Windows.Graphics.Capture D3D11 textures go straight to the hardware encoder on a shared D3D11 device.
- **macOS:** VideoToolbox for H.264 encode/decode.
- **Linux:** VA-API for H.264; `openh264` software fallback.
- **AV1 decode, all OSes:** `dav1d`; OS hardware AV1 where exposed.
- **VP8/VP9:** `libvpx`, added only if live tests show Discord senders emit them.
- No FFmpeg, no codec subprocesses.

## Consequences
- Pros: zero-copy GPU path on Windows (capture → encode never touches system RAM); no FFmpeg MSVC build or 20–40 MB of DLLs; H.264 patent licensing covered by the OS on Windows/macOS; small, maintained dependencies (`dav1d` is BSD-2).
- Cons: three H.264 backends behind one `video` codec trait (matches the per-OS capture backends we need anyway); Linux `openh264` built from source lacks Cisco's binary patent coverage (document or load Cisco's binary at runtime).
- Video/audio **attachments** are not played inline in MVP; they open in the OS default player.

## Rejected
- **FFmpeg (`ffmpeg-next`)**: crate in maintenance mode, heavy native build, LGPL linking constraints, and zero-copy still needs per-OS interop.
- **Hybrid (native on Windows/macOS, FFmpeg on Linux)**: keeps the full FFmpeg build/packaging cost for one platform.
- **GStreamer**: larger runtime than FFmpeg.
