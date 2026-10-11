# ADR 0004: Two noise-suppression modes (nnnoiseless + DeepFilterNet)

Status: accepted (2026-10-07)

## Context
Krisp is proprietary. Open alternatives: `nnnoiseless` (RNNoise port: ~1–2% of a core, ~10 ms latency, tiny model; weak on keyboard clicks and background voices) and DeepFilterNet 3 (close to Krisp on non-stationary noise; estimated 5–15% of a core, +10–20 MB RAM, ~20–40 ms extra latency). Measured on the reference PC in milestones 23 and 23a (`docs/TESTING.md`): Light 0.31% of a core and 10 ms; High quality 4.5–4.8% of a core at real time, about +13 MB private memory (32–35 MB while building) and 30 ms plus about 1 ms of worker round trip.

## Decision
A microphone setting with **Off / Light (default, nnnoiseless) / High quality (DeepFilterNet)**. Both behind one `Denoiser` trait in `fastcord-audio`. DeepFilterNet loads only when selected and is dropped when the user switches away.

## Consequences
- Default users keep the low-resource profile; users who need Krisp-like quality opt in.
- Two engines to maintain and test (milestones 23 and 23a).
- High quality latency must fit the §12.2 local-latency budget and is disclosed in the UI.
- DeepFilterNet's tract runtime contains non-`Send` state and its graph takes about 0.1 s (release) to 1.4 s (debug) to build. One dedicated denoise thread therefore builds, runs and drops the runtime; the audio owner thread only exchanges 480-sample blocks with it over bounded queues (80 ms cap) and never waits, so playback and capture are never stalled, `Denoiser` has no `Send` requirement, and callback threads still only touch preallocated PCM rings. The previous engine keeps running until the new one has produced audio; a slower engine takes over by repeating at most its extra delay and a faster one resumes exactly where the slower stopped, so a mode switch leaves no gap. A worker that fails, panics or falls 80 ms behind hands the stream back to Light with a visible error. At most one runtime exists at a time; dropping it frees its memory on its own thread.

## Rejected
- **nnnoiseless only:** quality noticeably below Krisp for the user's stated need.
- **DeepFilterNet only / default:** CPU and RAM cost on every call for every user.
- **Measure-then-pick-one:** the user prefers giving the choice to the end user.
