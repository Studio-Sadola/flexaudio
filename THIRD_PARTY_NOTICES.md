<!-- THIRD_PARTY_NOTICES.md — generated/curated for flexaudio v0.3.0 distribution. -->
<!-- See README and individual upstream repositories for canonical license text. -->

# Third-Party Notices

flexaudio (Copyright (c) 2026 tubome / Studio Sadola, MIT) bundles, statically links,
or dynamically links against third-party software. This document collects the
notices and license terms that must accompany binary and source distributions of
flexaudio and its component crates.

The list below is generated from `cargo metadata` for the workspace. Optional /
platform-specific dependencies (Windows `windows-*`, macOS `objc2-*`, Linux
`pipewire`/`libspa`, etc.) are only pulled in — and only need to be
reproduced — on the platforms / feature sets that use them.

---

## 1. Bundled assets (shipped inside the binary)

### Silero VAD model — `silero_vad_openvino_16k.onnx`

`flexaudio-vad` embeds the Silero VAD ONNX model
(`crates/flexaudio-vad/assets/silero_vad_openvino_16k.onnx`) into the compiled artifact via
`include_bytes!`. The model weights are therefore present in every binary that
links `flexaudio-vad`, and this notice MUST be reproduced in distributions.

  - Project: Silero VAD — https://github.com/snakers4/silero-vad
  - Copyright (c) 2020-present Silero Team
  - License: MIT

```
MIT License

Copyright (c) 2020-present Silero Team

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

> NOTE: Confirmed against upstream. The Silero VAD `LICENSE` on the `master`
> branch (https://github.com/snakers4/silero-vad/blob/master/LICENSE) reads
> "Copyright (c) 2020-present Silero Team" under the MIT License; the block above
> is reproduced verbatim from it. The vendored weights are
> `src/silero_vad/data/silero_vad_openvino_16k.onnx` from commit
> `1a26f187f9dbc77d9dcaee0bfefafcc092ef7970`
> (https://github.com/snakers4/silero-vad/blob/1a26f187f9dbc77d9dcaee0bfefafcc092ef7970/src/silero_vad/data/silero_vad_openvino_16k.onnx),
> sha256 `7776b81ad1b0350c15d7f1555943b9232eb53e9ca5d989c6d0cea9ebc8664d87`.
> 16 kHz dedicated graph (no `If` nodes, no `sr` input). Embedded as
> `crates/flexaudio-vad/assets/silero_vad_openvino_16k.onnx`.

---

## 2. Inference engine (compiled Rust)

### tract (`tract-onnx` 0.23.7)

`flexaudio-vad` runs the Silero VAD model through the pure-Rust `tract-onnx`
crate. No Microsoft ONNX Runtime binary is downloaded or linked; there is no
`download-binaries` HTTP fetch at build time.

  - Project: tract — https://github.com/snipsco/tract
  - Crate: `tract-onnx` 0.23.7 (and its `tract-*` workspace crates at the same version)
  - License: **MIT OR Apache-2.0** (from `cargo metadata` for `tract-onnx` 0.23.7;
    `license` field and the crate's `LICENSE` file)

```
Licensed under either of
 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
at your option.
```

The full Apache-2.0 and MIT texts ship in the `tract-onnx` crate source
(`LICENSE-APACHE`, `LICENSE-MIT`). Reproduce them with binary distributions
that include `flexaudio-vad`.

Related `tract-*` crates resolved at 0.23.7 (all MIT OR Apache-2.0 per
`cargo metadata`): `tract-core`, `tract-data`, `tract-extra`, `tract-hir`,
`tract-linalg`, `tract-nnef`, `tract-onnx-opl`, `tract-pulse`, `tract-pulse-opl`,
`tract-transformers`.

---

## 3. Dynamically linked native libraries (Linux, runtime)

### PipeWire / SPA (libpipewire-0.3 / libspa-0.2)

On **Linux**, `flexaudio-os-linux` captures system / per-process audio through
PipeWire, linking against the system's `libpipewire-0.3` and `libspa-0.2`
shared libraries at runtime via the `pipewire` / `libspa` Rust bindings. These
**system libraries are NOT bundled** by flexaudio — they are provided by the end
user's Linux distribution.

  - Project: PipeWire — https://gitlab.freedesktop.org/pipewire/pipewire
  - License (libpipewire / libspa runtime libraries): **MIT** for most of the
    tree; some SPA plugins are LGPL-2.1+. flexaudio links the MIT-licensed
    `libpipewire-0.3` / `libspa-0.2` client libraries dynamically.
  - The Rust binding crates `pipewire`, `pipewire-sys`, `libspa`, `libspa-sys`
    (v0.10.x) are MIT.

> NOTE: Because PipeWire is linked **dynamically** and supplied by the OS, no
> PipeWire object code is redistributed in flexaudio binaries. If you choose to
> bundle PipeWire shared objects yourself (e.g. a self-contained Linux package),
> reproduce the applicable PipeWire MIT/LGPL notices for the exact `.so` files
> you ship, and honor LGPL-2.1+ relinking obligations for any LGPL components.

---

## 4. Permissive Rust dependencies (compiled in)

All remaining workspace dependencies are permissively licensed (MIT / Apache-2.0
/ BSD / ISC / Zlib / Unlicense and dual-license combinations thereof). Their
copyright notices and license texts are available in each crate's source on
crates.io / the respective repositories. The table is the deduplicated transitive
dependency set resolved with `--all-features` (so it is a superset of any single
platform / feature build).

| Crate | SPDX License |
|-------|--------------|
| adler2 | Apache-2.0 OR MIT |
| anyhow | MIT OR Apache-2.0 |
| bit-set | Apache-2.0 OR MIT |
| bit-vec | Apache-2.0 OR MIT |
| flate2 | MIT OR Apache-2.0 |
| prost | Apache-2.0 |
| rayon | MIT OR Apache-2.0 |
| safetensors | Apache-2.0 |
| serde_json | MIT OR Apache-2.0 |
| tar | MIT OR Apache-2.0 |
| tempfile | MIT OR Apache-2.0 |
| tract-core | MIT OR Apache-2.0 |
| tract-data | MIT OR Apache-2.0 |
| tract-extra | MIT OR Apache-2.0 |
| tract-hir | MIT OR Apache-2.0 |
| tract-linalg | MIT OR Apache-2.0 |
| tract-nnef | MIT OR Apache-2.0 |
| tract-onnx | MIT OR Apache-2.0 |
| tract-onnx-opl | MIT OR Apache-2.0 |
| tract-pulse | MIT OR Apache-2.0 |
| tract-pulse-opl | MIT OR Apache-2.0 |
| tract-transformers | MIT OR Apache-2.0 |
| zlib-rs | MIT OR Apache-2.0 |
| aho-corasick | Unlicense OR MIT |
| alsa | Apache-2.0 OR MIT |
| alsa-sys | MIT |
| annotate-snippets | MIT OR Apache-2.0 |
| anstream | MIT OR Apache-2.0 |
| anstyle | MIT OR Apache-2.0 |
| anstyle-parse | MIT OR Apache-2.0 |
| anstyle-query | MIT OR Apache-2.0 |
| anstyle-wincon | MIT OR Apache-2.0 |
| audio-core | MIT OR Apache-2.0 |
| audioadapter | MIT OR Apache-2.0 |
| audioadapter-buffers | MIT OR Apache-2.0 |
| audioadapter-sample | MIT OR Apache-2.0 |
| autocfg | Apache-2.0 OR MIT |
| bindgen | BSD-3-Clause |
| bitflags | MIT OR Apache-2.0 |
| block2 | MIT |
| bumpalo | MIT OR Apache-2.0 |
| byteorder | Unlicense OR MIT |
| bytes | MIT |
| cc | MIT OR Apache-2.0 |
| cesu8 | Apache-2.0 OR MIT |
| cexpr | Apache-2.0 OR MIT |
| cfg-expr | MIT OR Apache-2.0 |
| cfg-if | MIT OR Apache-2.0 |
| cfg_aliases | MIT |
| clang-sys | Apache-2.0 |
| clap | MIT OR Apache-2.0 |
| clap_builder | MIT OR Apache-2.0 |
| clap_derive | MIT OR Apache-2.0 |
| clap_lex | MIT OR Apache-2.0 |
| colorchoice | MIT OR Apache-2.0 |
| combine | MIT |
| convert_case | MIT |
| cookie-factory | MIT |
| coreaudio-rs | MIT OR Apache-2.0 |
| cpal | Apache-2.0 |
| crossbeam-utils | MIT OR Apache-2.0 |
| ctor | Apache-2.0 OR MIT |
| ctrlc | MIT OR Apache-2.0 |
| dasp_sample | MIT OR Apache-2.0 |
| dispatch2 | Zlib OR Apache-2.0 OR MIT |
| either | MIT OR Apache-2.0 |
| equivalent | Apache-2.0 OR MIT |
| errno | MIT OR Apache-2.0 |
| find-msvc-tools | MIT OR Apache-2.0 |
| futures-core | MIT OR Apache-2.0 |
| futures-task | MIT OR Apache-2.0 |
| futures-util | MIT OR Apache-2.0 |
| getrandom | MIT OR Apache-2.0 |
| glob | MIT OR Apache-2.0 |
| hashbrown | MIT OR Apache-2.0 |
| heck | MIT OR Apache-2.0 |
| hound | Apache-2.0 |
| indexmap | Apache-2.0 OR MIT |
| is_terminal_polyfill | MIT OR Apache-2.0 |
| itertools | MIT OR Apache-2.0 |
| itoa | MIT OR Apache-2.0 |
| jni | MIT OR Apache-2.0 |
| jni-sys | MIT OR Apache-2.0 |
| jni-sys-macros | MIT OR Apache-2.0 |
| js-sys | MIT OR Apache-2.0 |
| libc | MIT OR Apache-2.0 |
| libloading | ISC |
| libspa | MIT |
| libspa-sys | MIT |
| linux-raw-sys | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| log | MIT OR Apache-2.0 |
| mach2 | BSD-2-Clause OR MIT OR Apache-2.0 |
| matrixmultiply | MIT OR Apache-2.0 |
| memchr | Unlicense OR MIT |
| minimal-lexical | MIT OR Apache-2.0 |
| napi | MIT |
| napi-build | MIT |
| napi-derive | MIT |
| napi-derive-backend | MIT |
| napi-sys | MIT |
| ndarray | MIT OR Apache-2.0 |
| ndk | MIT OR Apache-2.0 |
| ndk-context | MIT OR Apache-2.0 |
| ndk-sys | MIT OR Apache-2.0 |
| nix | MIT |
| nom | MIT |
| num-complex | MIT OR Apache-2.0 |
| num-derive | MIT OR Apache-2.0 |
| num-integer | MIT OR Apache-2.0 |
| num-traits | MIT OR Apache-2.0 |
| num_enum | BSD-3-Clause OR MIT OR Apache-2.0 |
| num_enum_derive | BSD-3-Clause OR MIT OR Apache-2.0 |
| objc2 | MIT |
| objc2-audio-toolbox | Zlib OR Apache-2.0 OR MIT |
| objc2-core-audio | Zlib OR Apache-2.0 OR MIT |
| objc2-core-audio-types | Zlib OR Apache-2.0 OR MIT |
| objc2-core-foundation | Zlib OR Apache-2.0 OR MIT |
| objc2-encode | MIT |
| objc2-foundation | MIT |
| once_cell | MIT OR Apache-2.0 |
| once_cell_polyfill | MIT OR Apache-2.0 |
| pin-project-lite | Apache-2.0 OR MIT |
| pipewire | MIT |
| pipewire-sys | MIT |
| pkg-config | MIT OR Apache-2.0 |
| portable-atomic | Apache-2.0 OR MIT |
| portable-atomic-util | Apache-2.0 OR MIT |
| primal-check | MIT OR Apache-2.0 |
| proc-macro-crate | MIT OR Apache-2.0 |
| proc-macro2 | MIT OR Apache-2.0 |
| quote | MIT OR Apache-2.0 |
| rawpointer | MIT OR Apache-2.0 |
| realfft | MIT |
| regex | MIT OR Apache-2.0 |
| regex-automata | MIT OR Apache-2.0 |
| regex-syntax | MIT OR Apache-2.0 |
| ringbuf | MIT OR Apache-2.0 |
| rtrb | MIT OR Apache-2.0 |
| rubato | MIT |
| rustc-hash | Apache-2.0 OR MIT |
| rustfft | MIT OR Apache-2.0 |
| rustix | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| rustversion | MIT OR Apache-2.0 |
| same-file | Unlicense OR MIT |
| semver | MIT OR Apache-2.0 |
| serde_core | MIT OR Apache-2.0 |
| serde_derive | MIT OR Apache-2.0 |
| serde_spanned | MIT OR Apache-2.0 |
| shlex | MIT OR Apache-2.0 |
| slab | MIT |
| smallvec | MIT OR Apache-2.0 |
| strength_reduce | MIT OR Apache-2.0 |
| strsim | MIT |
| syn | MIT OR Apache-2.0 |
| system-deps | MIT OR Apache-2.0 |
| target-lexicon | Apache-2.0 WITH LLVM-exception |
| thiserror | MIT OR Apache-2.0 |
| thiserror-impl | MIT OR Apache-2.0 |
| tokio | MIT |
| toml | MIT OR Apache-2.0 |
| toml_datetime | MIT OR Apache-2.0 |
| toml_edit | MIT OR Apache-2.0 |
| toml_parser | MIT OR Apache-2.0 |
| toml_writer | MIT OR Apache-2.0 |
| transpose | MIT OR Apache-2.0 |
| unicode-ident | (MIT OR Apache-2.0) AND Unicode-3.0 |
| unicode-segmentation | MIT OR Apache-2.0 |
| unicode-width | MIT OR Apache-2.0 |
| utf8parse | Apache-2.0 OR MIT |
| version-compare | MIT |
| visibility | Zlib OR MIT OR Apache-2.0 |
| walkdir | Unlicense OR MIT |
| wasm-bindgen | MIT OR Apache-2.0 |
| wasm-bindgen-futures | MIT OR Apache-2.0 |
| wasm-bindgen-macro | MIT OR Apache-2.0 |
| wasm-bindgen-macro-support | MIT OR Apache-2.0 |
| wasm-bindgen-shared | MIT OR Apache-2.0 |
| web-sys | MIT OR Apache-2.0 |
| winapi-util | Unlicense OR MIT |
| windowfunctions | MIT |
| windows | MIT OR Apache-2.0 |
| windows-core | MIT OR Apache-2.0 |
| windows-implement | MIT OR Apache-2.0 |
| windows-interface | MIT OR Apache-2.0 |
| windows-link | MIT OR Apache-2.0 |
| windows-result | MIT OR Apache-2.0 |
| windows-sys | MIT OR Apache-2.0 |
| windows-targets | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | MIT OR Apache-2.0 |
| windows_aarch64_msvc | MIT OR Apache-2.0 |
| windows_i686_gnu | MIT OR Apache-2.0 |
| windows_i686_gnullvm | MIT OR Apache-2.0 |
| windows_i686_msvc | MIT OR Apache-2.0 |
| windows_x86_64_gnu | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | MIT OR Apache-2.0 |
| windows_x86_64_msvc | MIT OR Apache-2.0 |
| winnow | MIT |

---

## How this file was produced

The permissive dependency table is the deduplicated output of:

```
cargo metadata --format-version 1 --all-features
```

Regenerate after dependency changes. Sections 1-3 (bundled / statically linked /
dynamically linked native code) are curated by hand because they carry
redistribution obligations that a flat crate list does not capture.
