// Writes a 16-bit PCM stereo WAV of a pure sine so the smoke test needs no
// external audio tools. Amplitude is linear full-scale (0.5 = -6 dBFS).
import { writeFileSync } from 'node:fs';

export function writeToneWav(path, { freqHz, seconds, rate = 48000, amplitude = 0.5 }) {
  const channels = 2;
  const frames = Math.round(seconds * rate);
  const dataBytes = frames * channels * 2;
  const buf = Buffer.alloc(44 + dataBytes);
  buf.write('RIFF', 0); buf.writeUInt32LE(36 + dataBytes, 4); buf.write('WAVE', 8);
  buf.write('fmt ', 12); buf.writeUInt32LE(16, 16); buf.writeUInt16LE(1, 20);
  buf.writeUInt16LE(channels, 22); buf.writeUInt32LE(rate, 24);
  buf.writeUInt32LE(rate * channels * 2, 28); buf.writeUInt16LE(channels * 2, 32);
  buf.writeUInt16LE(16, 34); buf.write('data', 36); buf.writeUInt32LE(dataBytes, 40);
  let o = 44;
  for (let i = 0; i < frames; i++) {
    const s = Math.round(Math.sin((2 * Math.PI * freqHz * i) / rate) * amplitude * 32767);
    buf.writeInt16LE(s, o); buf.writeInt16LE(s, o + 2); o += 4;
  }
  writeFileSync(path, buf);
}
