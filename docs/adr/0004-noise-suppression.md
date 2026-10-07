# ADR 0004: Two noise-suppression modes (nnnoiseless + DeepFilterNet)

Status: accepted (2026-10-07)

## Context
Krisp is proprietary. Open alternatives: `nnnoiseless` (RNNoise port: ~1–2% of a core, ~10 ms latency, tiny model; weak on keyboard clicks and background voices) and DeepFilterNet 3 (close to Krisp on non-stationary noise; estimated 5–15% of a core, +10–20 MB RAM, ~20–40 ms extra latency). CPU/RAM figures are estimates until measured on the reference PC.

## Decision
A microphone setting with **Off / Light (default, nnnoiseless) / High quality (DeepFilterNet)**. Both behind one `Denoiser` trait in `fastcord-audio`. DeepFilterNet loads only when selected and is dropped when the user switches away.

## Consequences
- Default users keep the low-resource profile; users who need Krisp-like quality opt in.
- Two engines to maintain and test (milestones 23 and 23a).
- High quality latency must fit the §12.2 local-latency budget and is disclosed in the UI.

## Rejected
- **nnnoiseless only:** quality noticeably below Krisp for the user's stated need.
- **DeepFilterNet only / default:** CPU and RAM cost on every call for every user.
- **Measure-then-pick-one:** the user prefers giving the choice to the end user.
