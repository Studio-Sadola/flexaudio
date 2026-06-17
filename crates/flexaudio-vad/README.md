# flexaudio-vad

**Offline Voice Activity Detection for Rust**, powered by the [Silero VAD] model
running on [ONNX Runtime]. The model is embedded into the binary via
`include_bytes!`, so detection runs **fully offline** — no model file to ship and
no network access at runtime.

This crate is independent of `flexaudio-core`: it consumes a plain `&[f32]`
sample stream, so you can pair it with any audio source.

## Streaming example

```rust
use flexaudio_vad::{Vad, VadConfig, VadEvent};

let mut vad = Vad::new(VadConfig::default()).unwrap();
for chunk in some_audio_chunks() {
    for ev in vad.process(chunk) {
        match ev {
            VadEvent::SpeechStart { at_sample } => println!("start @ {at_sample}"),
            VadEvent::SpeechEnd { at_sample }   => println!("end @ {at_sample}"),
        }
    }
}
# fn some_audio_chunks() -> Vec<&'static [f32]> { vec![] }
```

## Batch example

```rust
use flexaudio_vad::{get_speech_timestamps, VadConfig};

let samples: Vec<f32> = vec![0.0; 16_000];
let segments = get_speech_timestamps(&samples, &VadConfig::default()).unwrap();
for s in segments {
    println!("{}..{}", s.start_sample, s.end_sample);
}
```

`VadConfig` ships `aggressive()`, `balanced()`, and `conservative()` presets.
Input is expected as 16 kHz mono `f32`.

## Install

```sh
cargo add flexaudio-vad
```

## MSRV

Rust **1.88** (required by the `ort` / ONNX Runtime toolchain).

## License & third-party notices

[MIT](LICENSE) © 2026 tsubome / Aratech.

This crate **redistributes** the Silero VAD model (MIT) and a statically linked
build of Microsoft **ONNX Runtime** (MIT) inside every binary. You MUST ship the
accompanying notices — see [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
The ONNX Runtime `ThirdPartyNotices.txt` for the pinned release still needs to be
attached (legal review); this is flagged in that file.

[Silero VAD]: https://github.com/snakers4/silero-vad
[ONNX Runtime]: https://github.com/microsoft/onnxruntime
