# Third-party notices

## Optional Windows packet recovery: WinDivert

Windows archives contain the unmodified WinDivert 2.2.2-A x64 DLL and signed
driver from https://github.com/basil00/WinDivert/releases/tag/v2.2.2, used only
by the explicitly started administrator helper. This component is distributed
under **GNU LGPL version 3**, selected from upstream's LGPLv3/GPLv2 dual license.
The complete terms, including the incorporated GPLv3 text, are shipped in
`windivert/LICENSE`. Matching upstream source and build scripts are included as
`windivert/WinDivert-2.2.2-source.zip`; SHA-256 provenance and instructions for
rebuilding fastcord with a modified ABI-compatible library are in that folder's
`provenance.json` and `README.md`. fastcord's application source remains MIT.

fastcord itself is GPL-3.0-only (see `LICENSE`). Release archives carry this file as
`THIRD_PARTY_NOTICES.md`, the generated license texts of every Rust dependency as
`THIRD_PARTY_CRATES.md`, and the emoji notices under `assets/emoji/`. This file covers the
bundled native codecs and models: libopus (voice), the microphone noise-suppression engines of
`fastcord-audio` (SPEC section 7.4, ADR 0004), the bundled AV1 decoder of `fastcord-video`
(ADR 0001), the bundled OpenH264 H.264 codec and the runtime-linked libva (ADR 0001), the camera
background model (ADR 0010), and the davey/OpenMLS end-to-end encryption crates.

## zapret2 UDP recovery data and profile

The optional Windows packet helper embeds the unmodified 1200-byte
`quic_initial_www_google_com.bin` from zapret2 v1.0.6, commit
`221d7dff24bfec0cd5ac0a5070b8acef2008af3d`, copyright (c) 2016-2026 bol-van,
distributed under the MIT license. Its complete notice, hashes and provenance
are included in the archive's `zapret2/` directory. The profiles follow
upstream's six-repeat UDP fake and original-preserving IPv4 fragmentation
strategies. The standalone winws2/Cygwin/Lua runtime is not shipped; the helper
uses the WinDivert distribution described above.

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

## byedpi (blocking bypass)

byedpi 0.17.3, https://github.com/hufrea/byedpi, is statically compiled from the
release archive `crates/fastcord-bypass/native/byedpi-0.17.3.tar.gz`; the build
verifies its SHA-256
`0a9cb8585554c68c3e2be88c33c9bf6f99f8e8c7f54b362285adab99e262566c` and applies the
small patches recorded in `native/byedpi-source.json` to its extracted copy. Its
`LICENSE` is MIT:

```text
MIT License

Copyright (c) 2024 hufrea

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

## Symphonia AAC decoder (MPL-2.0)

Inline playback of AAC-LC audio in MP4 and WebM attachments (ADR 0008) uses `symphonia-codec-aac`
0.6.1, and soundboard sounds (ADR 0011) use `symphonia-bundle-mp3` 0.6.1 (MP3 only), each with `symphonia-core`, `symphonia-common` and `symphonia-metadata` (Copyright (c) 2019-2026
The Project Symphonia Developers, https://github.com/pdeljanov/Symphonia), licensed under the
Mozilla Public License 2.0. The crates are used unmodified from crates.io; their source is
available from crates.io and the repository above, and the license text is in
`THIRD_PARTY_CRATES.md`. The AAC decoder source originates from the NihAV project and was
relicensed for Symphonia with its author's permission.

## audio_thread_priority (MPL-2.0)

Real-time scheduling of audio threads (ADR 0009; used directly by `fastcord-platform` and through
cpal's `realtime` feature) uses `audio_thread_priority` 0.35.1 (Copyright (c) Mozilla and
contributors, https://github.com/mozilla/audio_thread_priority), licensed under the Mozilla Public
License 2.0. The crate is used unmodified from crates.io; its source is available from crates.io
and the repository above, and the license text is in `THIRD_PARTY_CRATES.md`.

## MediaPipe selfie segmenter (Apache-2.0)

Camera background blur and preset backgrounds (issue #85, ADR 0010) segment the person with Google's
MediaPipe selfie segmentation model (`selfie_segmenter.tflite`, 256x256, 249,537 bytes, SHA-256
`191ac9529ae506ee0beefa6b2c945a172dab9d07d1e802a290a4e4038226658b`, downloaded from
https://storage.googleapis.com/mediapipe-models/image_segmenter/selfie_segmenter/float16/latest/selfie_segmenter.tflite
on 2026-10-20). Its model card (Tingbo Hou, Siargey Pisarchyk and Karthik Raveendran, Google, 2021,
https://storage.googleapis.com/mediapipe-assets/Model%20Card%20MediaPipe%20Selfie%20Segmentation.pdf)
states "Licensed under Apache License, Version 2.0", and the MediaPipe repository
(https://github.com/google-ai-edge/mediapipe) is Apache-2.0. fastcord ships a modified form of the
file: `crates/fastcord-video/native/selfie/convert_selfie_segmenter.py` translates it operator by
operator into `selfie_segmenter.onnx` (437,436 bytes, SHA-256
`71a51a5643d0da49cab74e469c43539281ba2b373bb62dc08bb176f4273bfc35`) so that tract, which already
runs DeepFilterNet, can execute it. The weights are unchanged (float16 widened to float32); only the
graph encoding and tensor layout (NHWC to NCHW) differ. The original, the converted file, the
converter and `selfie-model.json` (URL, hashes, tool versions) are in that directory. The model is
the only third-party artwork or data in the feature: the four preset backgrounds in `assets/camera/`
are generated by `assets/camera/generate_presets.py` from formulas, are original work released with
fastcord under its license, and contain no third-party images.

```text
                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright [yyyy] [name of copyright owner]

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
```
