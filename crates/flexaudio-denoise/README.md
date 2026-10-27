# flexaudio-denoise

**Offline noise suppression for Rust**, powered by [RNNoise] via the pure-Rust
[nnnoiseless] port. The model weights are embedded in the library, so denoising
runs **fully offline** -- no model file to ship and no network access at runtime.
It targets stationary microphone noise such as fans, air conditioning, and
keyboard rumble.

This crate is independent of `flexaudio-core`: it consumes plain interleaved
`&[f32]` samples (+/-1.0 normalized, 48 kHz), so you can pair it with any audio
source. The i16-range scaling that nnnoiseless expects is handled internally.

## Latency & carry-over semantics

RNNoise processes fixed 480-sample (10 ms) frames, so `process` slices its
input into frames internally and carries the remainder over to the next call.
The output is the input delayed by exactly **480 samples per channel**,
independent of call granularity:

- `process` fills the buffer it receives in place, same length. The first
  480 samples/ch of the stream are the delay padding (silence).
- `flush` returns the final 480 samples/ch and closes the stream, leaving the
  denoiser reset for a new stream. Total output = total input + 480/ch.

## Example

```rust
use flexaudio_denoise::{Denoiser, FRAME_SIZE};

let mut dn = Denoiser::new(1).unwrap(); // 1 = mono, 2 = stereo interleaved
let mut chunk = vec![0.0f32; 1000];     // +/-1.0 normalized, 48 kHz
dn.process(&mut chunk).unwrap();        // in place, delayed by 480 samples
let tail = dn.flush();                  // the remaining 480 samples/ch
assert_eq!(tail.len(), FRAME_SIZE);
```

## With flexaudio

`flexaudio` defaults to 48 kHz stereo interleaved f32 chunks; its output rate is
configurable, but `Denoiser` requires 48 kHz. With a stream configured for
48 kHz stereo, denoise the microphone stream before encoding or transcription:

```rust,ignore
let mut dn = flexaudio_denoise::Denoiser::new(2).unwrap();
while let Some(mut chunk) = stream.poll_chunk() {
    dn.process(&mut chunk.data).unwrap();
    sink.write(&chunk.data);
}
sink.write(&dn.flush());
```

## License & third-party notices

[MIT](LICENSE) (c) 2026 tubome / Studio Sadola.

This crate depends on [nnnoiseless] (**BSD-3-Clause**), a pure-Rust port of
Xiph's [RNNoise] (also BSD-3-Clause), and embeds its RNNoise model weights in
every binary. Ship the nnnoiseless/RNNoise BSD-3-Clause notice alongside
binaries that include this crate.

[RNNoise]: https://github.com/xiph/rnnoise
[nnnoiseless]: https://github.com/jneem/nnnoiseless
