# macOS packet recovery decision (#227)

Decision date: 2026-11-01. Keep portable application-owned TLS/no-SNI and fresh UDP-port recovery. Return Unsupported for native packet manipulation until a signed prototype demonstrates fastcord-only scope and direct physical egress. A successful compile or a virtual tunnel is insufficient evidence. Runtime capability reporting and the refusal regression are supplied by #226.

| Operation | Current implementation | Feasibility evidence / permission |
| --- | --- | --- |
| Application-owned TLS record/write transformations and no-SNI policy | Shared Rust transport | No new privilege; native Intel/Apple Silicon route validation pending |
| Ordinary canonical UDP discovery and fresh ports | Shared Rust transport | No raw interception; live efficacy separately pending |
| TCP fake/disorder/overlap at physical egress | Unsupported | No evaluated native physical-egress API/prototype |
| Canonical UDP fake/IPv4 fragmentation at physical egress | Unsupported | No evaluated native physical-egress API/prototype; IPv6 is separate |
| App Proxy flow interception | Candidate API, not implemented | Apple documents per-application flow rules and the Network Extension entitlement |
| Packet Tunnel virtual-interface packets | Candidate API, not implemented | Apple documents virtual-interface routing and injection plus the Network Extension entitlement; this is not proof of direct physical-egress injection |
| User-configured remote proxy | Separate existing transport | Requires a reachable user-selected proxy; does not satisfy direct-bypass acceptance |

Apple's [NEAppProxyProvider documentation](https://developer.apple.com/documentation/networkextension/neappproxyprovider) describes transparent flow proxies selected by application rules and requires the Network Extension entitlement. It does not document the required packet sequence/fake manipulation. [NEPacketTunnelProvider](https://developer.apple.com/documentation/networkextension/nepackettunnelprovider) works through a virtual interface; its packetFlow injects through that interface. Inference: neither description by itself proves the required physical-egress, no-relay behavior. A prototype must prove that separately. Both sources were retrieved on the decision date.

The [zapret2 upstream platform statement](https://github.com/bol-van/zapret2) explicitly excludes macOS. Do not assume an older stream proxy supplies a UDP packet backend. No upstream code/fixture was adopted in this decision.

Exact prerequisites: macOS Intel and Apple Silicon test hosts; developer signing/provisioning and the applicable Network Extension entitlement; documented installation/user-approval/distribution path; an application-scoped prototype with secret-free bounded IPC and original-traffic preservation; physical-egress captures proving direct transmission and exclusion of unrelated applications; teardown/crash tests and filtered-route voice acceptance. None is available in this Linux run. No PF/routing edits, SIP change, TLS-verification relaxation or root account worker is proposed.
