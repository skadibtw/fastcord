# ADR 0002: wgpu pinned to the native graphics API

Status: accepted (2026-10-07)

## Context
Empty iced 0.14 window, release build, primary Windows PC, working set: automatic wgpu selection 172 MB (private 286), DX12 77, Vulkan 96, OpenGL 221, tiny-skia (CPU) 21. Screen-share playback needs GPU YUV→RGB shaders (SPEC §8.6); a CPU renderer would convert every video frame on the CPU.

## Decision
Use iced's wgpu renderer with the backend pinned per OS: DX12 (Windows), Metal (macOS), Vulkan (Linux). Never OpenGL, never automatic selection. tiny-skia remains only as iced's built-in fallback when no supported adapter exists.

## Consequences
- Roughly 95 MB saved versus automatic selection on the reference PC.
- The renderer alone (~77 MB) exceeds the 45 MiB allowance in SPEC §12.1; reducing it or rebalancing the budget is tracked from the first UI milestone through milestone 38.
- Pinning must happen without `unsafe` `std::env::set_var` (workspace denies `unsafe_code`); use iced/wgpu configuration APIs.

## Rejected
- **tiny-skia for UI:** 21 MB, but CPU cost on every scroll/animation frame and per-frame CPU RGBA conversion for video.
- **Adaptive switch (tiny-skia ↔ wgpu):** iced cannot swap renderers at runtime without recreating the window.

## Implementation notes (milestone 8)
- `fastcord-app::render` selects a singleton native backend in explicit `iced_wgpu::Settings` passed to `Compositor::request`; iced 0.14's default environment-based selection is not used. A root-widget/overlay bridge keeps iced's existing renderer-specific widgets (including QR) and widget-tree state compatible with the pinned compositor, without wrapping each list row or cloning render data.
- `WGPU_BACKEND` and `ICED_BACKEND` cannot override the API. tiny-skia is attempted only for `NoAdapterFound`; GPU surface/device errors propagate. Headless configuration is pinned as well; no unsafe code, environment mutation, or dependency fork is needed.
- Windows release UI and offline fixture navigation were launched under conflicting backend environment settings. Ten-minute unauthenticated mean working set was 78.147 MB (+1.147 MB versus the 77 MB baseline); private and GPU counters, workload limits, and pending logged-in/native checks are recorded in `docs/PERFORMANCE.md` and `docs/TESTING.md`. The renderer allowance risk remains open.
