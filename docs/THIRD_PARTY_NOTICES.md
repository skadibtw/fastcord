# Third-party notices

fastcord itself is GPL-3.0-only (see `LICENSE`). Release archives carry this file as
`THIRD_PARTY_NOTICES.md` and the emoji notices under `assets/emoji/`. This file covers the
microphone noise-suppression engines of `fastcord-audio` (SPEC section 7.4, ADR 0004)
and the bundled AV1 decoder of `fastcord-video` (ADR 0001).

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

## Other crates of these engines

The remaining Rust crates these engines depend on (for example `rustfft`, `realfft`, `ndarray`,
`rubato`, `prost`, `flate2`, `tar`) are published under permissive licenses (MIT, Apache-2.0,
BSD, Zlib, Unlicense, CC0-1.0 or a choice among them); each crate's own license text ships in its
source at the version pinned by `Cargo.lock`. This file does not reproduce them.

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
