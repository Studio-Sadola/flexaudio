## Non-crate components

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
