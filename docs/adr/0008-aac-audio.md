# ADR 0008: AAC-LC audio for inline MP4 playback

Status: accepted (2026-10-20)

## Context
ADR 0007 left H.264 + AAC clips (the usual phone and screen-recorder output) playing without sound. Issue #116 asks for AAC through an OS decoder where available and a documented Linux behaviour. Options:

- **Media Foundation AAC decoder MFT (Windows) and AudioToolbox `AudioConverter` (macOS):** also handle HE-AAC, but each is a new `unsafe` FFI bridge, none exists for Linux, they cannot be unit-tested on the other two OSes, and the test fixture needs a real AAC stream either way. Three behaviours (two decoders plus "silent") for one feature.
- **`symphonia-codec-aac` 0.6.1 (MPL-2.0, pure Rust):** AAC-LC, mono or stereo, 1024-sample frames, no SBR. Small (with `symphonia-core`, `symphonia-common`, `symphonia-metadata`, `lazy_static`, `regex-lite`), no `unsafe` in our code, identical on all three OSes, deterministic offline tests. MPL-2.0 is file-level copyleft and already in the build (`hpke-rs*` through DAVE); fastcord is GPL-3.0, which can include MPL-2.0 code. Only the decoder crate and its core are used, not the `symphonia` meta crate, so none of its container probes or other codecs are compiled in.
- **A hand-written decoder:** rejected; AAC-LC needs Huffman tables, TNS, PNS, intensity/MS stereo and the IMDCT.

## Decision
Use `symphonia-codec-aac` on every platform. `fastcord-audio::aac::AacTranscoder` decodes a frame, converts it to 48 kHz stereo (the engine's existing rubato converter) and re-encodes it as 20 ms Opus packets (libopus, stereo, 128 kbit/s, `Application::Audio`). The packets go through the same `PacketSource` -> `FilePlayer` -> engine remote-speaker path as Opus tracks, so the engine, the audio callbacks, the master clock, pause, seek, volume and the single active player are untouched. A PCM entry point into the engine would have meant a second input type in the jitter buffer and mixer for the sake of one codec; the cost of the re-encode is a small, bounded amount of control-thread CPU while playing and zero when paused or idle.

- The container layer accepts an AAC track only when its AudioSpecificConfig is AAC-LC, 1024-sample frames and one or two channels (`container::aac_lc_info`); MP4 reads it from `esds` (ES/DecoderConfig/DecoderSpecificInfo descriptors, object type 0x40), Matroska from `CodecPrivate`. Everything else (HE-AAC / SBR, multichannel, 960-sample frames, QuickTime sound description v1) stays `AudioSkipped::Aac`: the video plays silently with a note.
- Memory: one AAC frame of PCM plus one Opus frame; the caller pops every ready packet before pushing the next frame. Nothing runs while paused, finished or idle.
- Seek: the control thread resets the decoder, resampler and encoder, decodes the frame before the target to prime the overlap-add (its audio is dropped) and continues from the frame at or after the target.
- A frame that does not decode becomes silence of the same length (the clock keeps its place); a first frame that does not decode at start, or an unconstructible decoder, gives the note "This AAC audio cannot be decoded here". The decoder runs under `catch_unwind` and is rebuilt after a panic, because attachments are untrusted input and this is third-party code.
- Quality: AAC to Opus is a second lossy generation. At 128 kbit/s stereo it is a playback preview, not an archive path (Download keeps the original).

## Consequences
- New third-party code (MPL-2.0): `Cargo.lock` gains `symphonia-codec-aac`, `symphonia-common`, `symphonia-core`, `symphonia-metadata` and `regex-lite`; `docs/THIRD_PARTY_CRATES.md` is regenerated and `THIRD_PARTY_NOTICES.md` carries the MPL source-availability notice.
- HE-AAC (v1/v2, SBR/PS) and multichannel AAC are not decoded on any OS. If real clips show a need, an OS decoder bridge can be added behind `AacTranscoder` for Windows/macOS without changing the callers.
- The fixture `container-h264-aac-lc.mp4` carries genuine AAC-LC frames produced by the Windows Media Foundation AAC encoder (`fixtures/audio/gen_aac_tone.py`); tests decode them and check the 440 Hz / 880 Hz channels by correlation.
