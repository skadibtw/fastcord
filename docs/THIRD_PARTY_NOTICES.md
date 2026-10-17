# Third-party notices

fastcord itself is GPL-3.0-only (see `LICENSE`). Release archives carry this file as
`THIRD_PARTY_NOTICES.md`, the generated license texts of every Rust dependency as
`THIRD_PARTY_CRATES.md`, and the emoji notices under `assets/emoji/`. This file covers the
bundled native codecs and models: libopus (voice), the microphone noise-suppression engines of
`fastcord-audio` (SPEC section 7.4, ADR 0004), the bundled AV1 decoder of `fastcord-video`
(ADR 0001), the bundled OpenH264 H.264 codec and the runtime-linked libva (ADR 0001), and the
davey/OpenMLS end-to-end encryption crates.

## nnnoiseless and RNNoise (Light mode)

nnnoiseless 0.5.2, https://github.com/jneem/nnnoiseless, is a Rust port of Xiph.Org's RNNoise,
https://gitlab.xiph.org/xiph/rnnoise, including RNNoise's trained network weights. It is
licensed under BSD-3-Clause:

```text
Copyright (c) 2020, Joe Neeman
Copyright (c) 2017, Mozilla
Copyright (c) 2007-2017, Jean-Marc Valin
Copyright (c) 2005-2017, Xiph.Org Foundation
Copyright (c) 2003-2004, Mark Borgerding

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of the Xiph.Org Foundation nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE FOUNDATION
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## DeepFilterNet 3 (High quality mode)

The `deep_filter` crate 0.5.6 (library `df`), https://github.com/Rikorose/DeepFilterNet, built
from the pinned commit `978576aa8400552a4ce9730838c635aa30db5e61`, and the DeepFilterNet3 ONNX
model that crate bundles (`models/DeepFilterNet3_onnx.tar.gz`, about 8 MB, stored compressed in the
executable). The DeepFilterNet repository states that its code is dual-licensed under either
the MIT license or the Apache License 2.0, at the user's option; fastcord uses it under the MIT
license:

```text
The MIT License (MIT)
Copyright (c) 2021 Hendrik Schröter

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

The model is described in H. Schröter, T. Rosenkranz, A. N. Escalante-B. and A. Maier,
"DeepFilterNet: Perceptually Motivated Real-Time Speech Enhancement", INTERSPEECH 2023.

## tract

DeepFilterNet runs its ONNX graphs on tract 0.19.16 (`tract-core`, `tract-onnx`, `tract-hir`,
`tract-pulse` and their companion crates), https://github.com/snipsco/tract, by Mathieu
Poumeyrol and contributors. tract is dual-licensed under either the MIT license or the Apache
License 2.0; fastcord uses it under the MIT license:

```text
Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## Other Rust crates

Every other Rust crate that release binaries can contain (the complete normal and build
dependency set of the Windows, Linux and macOS release targets pinned by `Cargo.lock`) is
listed, with its license text, in `THIRD_PARTY_CRATES.md`, which release archives carry next to
this file. That file is generated by `scripts/gen_third_party_licenses.py`; regenerate it
whenever `Cargo.lock` changes.

## libopus (Opus audio codec)

Opus 1.5 series, https://opus-codec.org/, https://gitlab.xiph.org/xiph/opus, is statically linked
into every release binary for voice. It is compiled from the source bundled by
`libopus_sys` 0.3.3 (crates.io checksum
`b81c32f233fb2507347a93f97b9919493be9db7ec2249ce2c3ed0c89ad8edf60` in `Cargo.lock`), the
`bundled` feature of `opus2` 0.4.0, https://github.com/cijiugechu/libopus_sys. The source's
`opus/COPYING` is BSD-3-Clause, reproduced here in full:

```text
Copyright 2001-2023 Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo, Mozilla, Amazon

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Opus is subject to the royalty-free patent licenses which are
specified at:

Xiph.Org Foundation:
https://datatracker.ietf.org/ipr/1524/

Microsoft Corporation:
https://datatracker.ietf.org/ipr/1914/

Broadcom Corporation:
https://datatracker.ietf.org/ipr/1526/
```

The Rust bindings `libopus_sys` (MIT, Copyright (c) 2024 nemurubaka) and `opus2` (MIT OR
Apache-2.0) are listed with their licenses in `THIRD_PARTY_CRATES.md`.

## davey and OpenMLS (DAVE end-to-end encryption)

Voice end-to-end encryption (the DAVE protocol) uses `davey` 0.1.4,
https://github.com/Snazzah/davey, which builds on OpenMLS 0.8.1 (`openmls`,
`openmls_basic_credential`, `openmls_memory_storage`, `openmls_rust_crypto`,
`openmls_traits`), https://github.com/openmls/openmls. Neither crate publishes a license file in
its crates.io package; both are MIT-licensed (`license = "MIT"`) and their repositories carry
the following notices. The cryptographic and HPKE crates they depend on (`hpke-rs*`, MPL-2.0;
`libcrux-*`, Apache-2.0; RustCrypto crates) are listed in `THIRD_PARTY_CRATES.md`.

davey, https://github.com/Snazzah/davey/blob/main/LICENSE:

```text
MIT License

Copyright (c) 2025-present Snazzah

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

OpenMLS, https://github.com/openmls/openmls/blob/main/LICENSE:

```text
Copyright (c) 2020 OpenMLS Authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## OpenH264 (software H.264 codec)

OpenH264, https://github.com/cisco/openh264, is compiled from the unmodified upstream source
at commit `4a2615fac570c6ca1ed4f157b9fdab9466edfd80` that the `openh264` 0.9.8 Rust wrapper
(https://github.com/ralfbiedert/openh264-rs, vendored by `openh264-sys2`, whose `upstream/VERSION`
and `upstream/LICENSE` record this pin) bundles, into every release binary for `fastcord-video`
(SPEC section 8.2, ADR 0001; SPEC milestone 30). The wrapper crate's license is listed in
`THIRD_PARTY_CRATES.md`. The bundled source is BSD-2-Clause-style:

```text
Copyright (c) 2013, Cisco Systems
All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice, this
  list of conditions and the following disclaimer in the documentation and/or
  other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

H.264/AVC: making an H.264 codec implementation available for distribution or
using it in some deployment scenarios may require a royalty-bearing license from
MPEG LA or its successors for uses outside the scope of the BSD license above.
Cisco operates a pre-compiled OpenH264 binary distribution under its own H.264
licensing arrangements; those arrangements apply to Cisco's binaries and do not
extend automatically to fastcord's own builds from source. Upstream
documentation: https://www.openh264.org/BINARY_LICENSE.txt.

## libva (VA-API Linux runtime)

libva 2.24.1, https://github.com/intel/libva, is dynamically linked into Linux
release binaries (`cargo:rustc-link-lib=dylib=va` and `dylib=va-drm` emitted by
the `fastcord-video` build script), so a Linux user needs the distribution's
VA-API runtime packages installed (typically `libva2` and `libva-drm2` on
Debian/Ubuntu, or the `libva`/`libva-drm` packages on Fedora) plus a working
VA-API driver. The complete source archive is committed for the provenance of
`fastcord-video`'s generated VA-API bindings as
`crates/fastcord-video/native/libva-2.24.1.tar.bz2` (SHA-256
`eec6050b52876f229bd35e9df17cd31a06785e18e6f7990c445b584628483d67`, provenance
in `native/libva-source.json`); no libva object code is compiled from it. The
archive's `COPYING` is the MIT license, reproduced here:

```text
Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the "Software"),
to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense,
and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice (including the
next paragraph) shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NON-INFRINGEMENT. IN NO EVENT SHALL
THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## dav1d (AV1 receive decoding)

dav1d 1.5.4, https://code.videolan.org/videolan/dav1d, is statically compiled from
the unmodified source at commit `54706fc6bc0cdecab7e9593974a4039cc038fca7`. The
repository contains `crates/fastcord-video/native/dav1d-1.5.4.tar.gz`; the build
verifies its SHA-256
`9edb11a2108b375cc58370354e705feebc93430bb780130363815c1e1ac0c250` before compiling.
The source/build provenance is recorded in `native/dav1d-source.json` alongside it.
No dav1d DLL/shared library is required in release archives. The source's `COPYING`
is BSD-2-Clause:

```text
Copyright © 2018-2025, VideoLAN and dav1d authors
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

The AV1 test fixtures use original synthetic raw pixels encoded losslessly with
independent libaom `aomenc` 3.15.2, https://aomedia.googlesource.com/aom/. The
encoder and its libraries are fixture-generation tools only and are not linked
or redistributed in fastcord packages; `fixtures/video/gen_av1_fixture.py` and
`av1-fixture.json` record their pinned provenance and regeneration command.
