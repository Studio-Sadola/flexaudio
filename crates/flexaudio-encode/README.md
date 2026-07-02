# flexaudio-encode

**Streaming FLAC recording sink for Rust.** Feed interleaved `f32` audio chunks
as they arrive and they are compressed losslessly to a `.flac` file on the fly —
a 3-hour meeting recording that would be ~2 GB as WAV lands at roughly a third
to a half of that, bit-exact at 16-bit. Encoding is pure Rust ([flacenc]), runs
**fully offline**, and memory usage stays constant no matter how long the
recording gets.

This crate is independent of `flexaudio-core`: it consumes a plain `&[f32]`
sample stream, so you can pair it with any audio source.

## Example

```rust
use flexaudio_encode::FlacWriter;

let mut writer = FlacWriter::create("meeting.flac", 48_000, 2)?;
for chunk in some_audio_chunks() {
    writer.write_chunk(chunk)?; // interleaved f32, length = frames * channels
}
writer.finalize()?; // flushes the tail and fixes up the STREAMINFO header
```

## With flexaudio

`AudioChunk.data` is already interleaved `f32` in the output format, so it can
be passed straight through:

```rust
use flexaudio::{open, SourceKind, StreamConfig};
use flexaudio_encode::FlacWriter;

let config = StreamConfig { kind: SourceKind::Mic, ..Default::default() };
let mut stream = open(config)?;
// flexaudio's default output format is 48 kHz / stereo.
let mut writer = FlacWriter::create("recording.flac", 48_000, 2)?;

stream.start()?;
while !done() {
    while let Some(chunk) = stream.poll_chunk() {
        writer.write_chunk(&chunk.data)?;
    }
}
stream.stop();
writer.finalize()?;
```

## Notes

- Samples are quantized from `f32` to 16-bit (simple rounding with clamping, no
  dither). 1–2 channels, sample rates up to 96 kHz.
- `finalize()` is recommended: it reports I/O errors and rewrites the header
  with the final sample count and MD5. Dropping the writer without finalizing
  closes the file on a best-effort basis (errors are swallowed).
- Encoding runs synchronously on the calling thread.

## Install

```sh
cargo add flexaudio-encode
```

## License & third-party notices

[MIT](LICENSE) © 2026 tubome / Studio Sadola.

This crate links [flacenc] (**Apache-2.0**), a pure-Rust FLAC encoder; binaries
embedding this crate include flacenc code under Apache-2.0 terms. Tests
additionally use the [claxon] decoder (Apache-2.0, dev-dependency only, not
distributed).

[flacenc]: https://github.com/yotarok/flacenc-rs
[claxon]: https://github.com/ruuda/claxon
