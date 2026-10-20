# ADR 0011: MP3 decoding for soundboard sounds

Status: accepted (2026-10-20)

## Context
Issue #87 plays a server's soundboard sounds locally (preview, and the sounds other participants play). Discord stores them as short MP3 or Ogg files (about 5.2 s, 512 KB) on the public CDN. The repository decodes Ogg Opus (`ogg`) and AAC-LC (`aac`, ADR 0008) but not MP3. Options:

- **`symphonia-bundle-mp3` 0.6.1 (MPL-2.0, pure Rust):** the same project, version and licence family as the AAC decoder already adopted in ADR 0008, with the `symphonia-core` already built. Only the `mp3` feature is enabled (no MP1/MP2) and only the reader and decoder crates are used, not the `symphonia` meta crate. New lock entries: `symphonia-bundle-mp3` only (`lazy_static` and `log` were already present).
- **An OS decoder (Media Foundation, AudioToolbox):** one new `unsafe` bridge per OS and none for Linux.
- **FFmpeg or libmpg123:** excluded by the repository rules (no FFmpeg) or a new C dependency.

## Decision
Use `symphonia-bundle-mp3` on every platform. `fastcord-audio::soundboard::decode` turns a downloaded file into a compact `Clip` of 20 ms Opus packets, a `PacketSource` for the existing `FilePlayer` and its output-only engine (no new engine input, mixer or callback code):

- Ogg Opus passes through unchanged. MP3 (after skipping a leading ID3v2 tag) is decoded frame by frame, mono is duplicated to stereo, any rate is converted to 48 kHz by the engine's rubato converter and the audio is encoded with libopus (stereo, 96 kbit/s). The converter's tail is flushed with silence and the last packet is padded.
- Bounds: input at most 1 MiB, output at most 6 s (300 packets, about 100 KiB stored compactly); a longer clip is refused rather than cut. A damaged frame is skipped (more than 50 means the file is damaged); decoding runs under `catch_unwind` because the file is untrusted input. All of it runs on a blocking worker before the player starts, never on the UI thread or in an audio callback.
- Ogg Vorbis (and WAV, FLAC and every other format) is `Unsupported`: it needs `symphonia-codec-vorbis` plus an Ogg demuxer for a format that was not shown to be used. The panel says so for that sound instead of failing silently; revisit if live captures show Vorbis sounds.

## Consequences
- `Cargo.lock` gains `symphonia-bundle-mp3`; `docs/THIRD_PARTY_CRATES.md` is regenerated and `docs/THIRD_PARTY_NOTICES.md` names the crate beside the AAC decoder (MPL-2.0 source availability).
- Playback needs no new unsafe code. A second lossy generation (MP3 to Opus at 96 kbit/s) is acceptable for a one-shot effect.
- The MP3 test fixture is synthetic: generated silent MPEG-1 Layer III frames and an ID3 header in the test itself. It proves framing, rate conversion, bounds and the packet count, not the sound of real music; real sounds are a manual check (docs/TESTING.md, #87).
