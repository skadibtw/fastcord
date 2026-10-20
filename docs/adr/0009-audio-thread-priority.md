# ADR 0009: Real-time scheduling through `audio_thread_priority`

Status: accepted (2026-10-20)

## Context
Issue #114 reports poor playback quality. The WASAPI callback threads created by cpal ran at normal priority, and the engine's owner thread waited with `recv_timeout(5 ms)`, which on Windows lasts about 16 ms and, under CPU load, over 60 ms, longer than the 40 ms playback target (`docs/TESTING.md`, #114). Each platform has its own way to ask for audio scheduling (MMCSS, Mach time-constraint policy, `SCHED_RR`/rtkit), and cpal already has the code for the callback threads.

## Decision
- Enable cpal's `realtime` feature, which uses `audio_thread_priority` for the device callback threads, and use the same crate (0.35.1, default features off, so no D-Bus) in `fastcord-platform::realtime` for the owner thread and the denoise worker. The crate keeps the unsafe FFI; fastcord adds none. It is cpal's own choice (and Firefox's), so the callback threads and ours follow one policy.
- Elevation is best effort: a refusal is counted (`EngineStats::realtime_threads`, `callback_realtime_denied`) and shown in the audio dialog, never an error.
- The owner tick uses `thread::sleep` with fixed deadlines instead of a global timer-resolution change.
- Linux without D-Bus/rtkit gets `SCHED_RR` only where `RLIMIT_RTPRIO` allows it; otherwise threads stay at normal priority. Adding `with_dbus` (rtkit) was rejected for now: it pulls `dbus`/libdbus into every Linux build.

## Consequences
- `Cargo.lock` gains `audio_thread_priority` 0.35.1 (MPL-2.0, file-level copyleft, already accepted for Symphonia in ADR 0008) and `mach2` 0.4.3 (macOS only; another version is already in the tree); `docs/THIRD_PARTY_CRATES.md` is regenerated.
- A real-time thread that never sleeps can starve the machine; the owner sleeps every 5 ms and the denoise worker blocks on its queue.
