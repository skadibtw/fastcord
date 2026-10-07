# Performance measurements

## Milestone 8 — native renderer and navigation (2026-10-07)

The renderer is explicitly configured to **DX12 on Windows, Metal on macOS, and Vulkan on Linux**, through iced/wgpu APIs, without unsafe code or environment mutation (ADR 0002). The built-in tiny-skia fallback is attempted only when the native API has no supported adapter; surface/device failures remain errors. Both `WGPU_BACKEND=gl` and `ICED_BACKEND=tiny-skia` were set for the Windows launch below, and could not override the native compositor configuration. No OpenGL or automatic GPU selection is used.

### Reference system and scope

- Source: the M8 feature tree in this commit, parent `a84109d`; release profile, toolchain 1.99.0, no debugger or verbose logging.
- Cargo.lock SHA-256: `e73f718330470a6195d29ccbd791530fa419bb8c0a3263fe1320912aae361a7e`.
- Windows 11 Pro, version 10.0.26300, x64; AMD Ryzen 7 7700; AMD Radeon RX 9070 XT, driver 32.0.32015.2008.
- Display: 2560 × 1440, window DPI 96 (100% scale); default 1080 × 640 client window (1096 × 679 including native frame). Balanced power plan (`381b4222-f694-41f0-9685-ff5bb260df2e`).
- **Unauthenticated login-screen surrogate**, zero guilds/channels/messages/users loaded, no audio/video/capture workers, no QR animation, no input to the measured process. No live alt-account session was available. A separate offline navigation fixture window was briefly exercised during the run; counters below are filtered to the measured release process's PID, not summed across both processes.
- Sampling: 60 samples about ten seconds apart, 606.13 seconds total, following launch and warmup. Process `WorkingSet64` and `PrivateMemorySize64`; dedicated/shared GPU allocations from `Win32_PerfRawData_GPUPerformanceCounters_GPUProcessMemory` filtered by `pid_<PID>_`. GPU allocation counters are reported separately and are not subtracted from, or blindly added to, the process working set.

### Results (decimal MB, 1 MB = 1,000,000 bytes)

| Metric | Minimum | Mean | Maximum | Final |
|---|---:|---:|---:|---:|
| Process working set | 77.529 | 78.147 | 81.756 | 77.976 |
| Private committed memory | 84.169 | 84.951 | 92.037 | 84.361 |
| Dedicated GPU allocation | 62.546 | 62.546 | 62.546 | 62.546 |
| Shared GPU allocation | 16.404 | 16.684 | 20.599 | 16.404 |

At eight seconds after launch, working set was 81.814 MB and private committed memory was 92.238 MB; these are individual startup observations, **not** a sampled peak. During the measured interval the process CPU-time counter advanced by 0 seconds (0% of one logical core at the counter's resolution), consistent with event-driven redraw rather than a periodic UI tick; this unauthenticated screen has no Gateway heartbeat and is not an authenticated CPU acceptance result. Both the real release app and the offline fixture window rendered on the native desktop and closed normally.

**Versus the ADR 0002 empty-window DX12 baseline of 77 MB:** mean working set is **+1.147 MB**, and final working set is **+0.976 MB**. The earlier baseline had a different empty-window workload and did not record a DX12 private-memory/GPU value, so no private/GPU delta or precise like-for-like reduction is claimed. The 45 MiB runtime/idle-renderer planning allowance is still exceeded (78.147 MB is about 74.53 MiB). Reducing renderer overhead or rebalancing the envelope remains required in milestone 38.

### Reproducing the measurement

1. Build `cargo build --release -p fastcord-app --locked` from the recorded lockfile. Launch the executable directly, without a debugger or verbose logging. Optional override-resistance check: set `WGPU_BACKEND=gl` and `ICED_BACKEND=tiny-skia` only in the launching shell/process environment.
2. Record OS/CPU/GPU/driver, resolution, DPI, power plan, source commit and lock checksum. Label whether the screen is unauthenticated, a deterministic fixture, or the real authorized alt account. Do not mix those scenarios.
3. After warmup, retain the app's PID, initial `TotalProcessorTime.TotalSeconds`, and a monotonic stopwatch. Every ten seconds for ten minutes, refresh the process and collect `WorkingSet64`, `PrivateMemorySize64`, and GPU counters whose instance name starts `pid_<PID>_`. Keep only this bounded sample set; do not record account content or credentials. Native GPU counters can be collected with PowerShell `Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUProcessMemory`, summing `DedicatedUsage`/`SharedUsage` for matching instances.
4. Report min/mean/max/final in explicit units. CPU is `100 * process_CPU_seconds_delta / elapsed_seconds`, without dividing by system core count. Separate startup peak, warmed visible idle, minimized idle, and active animation/call/share scenarios. Release builds do not ship a sampling profiler.

### Pending live/native measurements

The SPEC §12 authenticated idle scenario (selected text channel, real Gateway heartbeats and account state) is **not verified**. Repeat with the authorized alt, including the large-state scenario (100 guilds, 2,000 channels/messages and 500 users), minimized idle, CPU/heartbeat spikes, and separate GPU counters. No <150 MB logged-in product acceptance claim is made here. macOS Metal footprint/RSS and Linux Vulkan RSS/private-memory measurements, interactive scrolling/scale checks, and native no-adapter tiny-skia fallback remain pending; see `docs/TESTING.md`. No token was extracted from browser storage or written to the repository.
