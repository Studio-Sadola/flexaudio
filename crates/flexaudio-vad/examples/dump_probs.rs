//! Dump raw Silero probabilities from a 16 kHz mono signed-16-bit PCM WAV.
//! Usage: cargo run --release -p flexaudio-vad --example dump_probs -- [--whisper] <in.wav> <out.json>

use flexaudio_vad::{Vad, VadConfig, WhisperVad, WhisperVadOptions, WhisperVadParams};
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;

const FRAME_SAMPLES: usize = 512;

fn invalid_wav(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_samples(bytes: &[u8]) -> io::Result<Vec<f32>> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid_wav("expected a little-endian RIFF/WAVE file"));
    }
    let riff_size = usize::try_from(u32_le(&bytes[4..8]))
        .map_err(|_| invalid_wav("RIFF size exceeds this platform's limits"))?;
    if riff_size.checked_add(8) != Some(bytes.len()) {
        return Err(invalid_wav("RIFF size does not match the file length"));
    }

    let mut format_seen = false;
    let mut data = None;
    let mut offset = 12;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset.saturating_add(8))
            .ok_or_else(|| invalid_wav("truncated WAV chunk header"))?;
        let size = usize::try_from(u32_le(&header[4..8]))
            .map_err(|_| invalid_wav("WAV chunk size exceeds this platform's limits"))?;
        let start = offset + 8;
        let end = start
            .checked_add(size)
            .ok_or_else(|| invalid_wav("WAV chunk size overflow"))?;
        let chunk = bytes
            .get(start..end)
            .ok_or_else(|| invalid_wav("truncated WAV chunk payload"))?;
        match &header[..4] {
            b"fmt " => {
                if format_seen {
                    return Err(invalid_wav("duplicate WAV fmt chunk"));
                }
                // Accept classic PCM fmt, including its optional zero-length extension.
                if !(chunk.len() == 16 || (chunk.len() == 18 && u16_le(&chunk[16..]) == 0)) {
                    return Err(invalid_wav(
                        "expected a 16-byte PCM fmt chunk (or 18 bytes with no extension)",
                    ));
                }
                if u16_le(chunk) != 1
                    || u16_le(&chunk[2..]) != 1
                    || u32_le(&chunk[4..]) != 16_000
                    || u16_le(&chunk[14..]) != 16
                {
                    return Err(invalid_wav(
                        "expected 16 kHz mono signed-16-bit PCM (format tag 1)",
                    ));
                }
                if u32_le(&chunk[8..]) != 32_000 || u16_le(&chunk[12..]) != 2 {
                    return Err(invalid_wav(
                        "invalid PCM byte rate or block alignment (expected 32000 and 2)",
                    ));
                }
                format_seen = true;
            }
            b"data" => {
                if data.replace(chunk).is_some() {
                    return Err(invalid_wav("duplicate WAV data chunk"));
                }
                if size % 2 != 0 {
                    return Err(invalid_wav("PCM data contains an incomplete 16-bit sample"));
                }
            }
            _ => {}
        }
        offset = end
            .checked_add(size % 2)
            .filter(|&next| next <= bytes.len())
            .ok_or_else(|| invalid_wav("missing WAV chunk padding byte"))?;
    }
    if !format_seen {
        return Err(invalid_wav("missing WAV fmt chunk"));
    }
    let data = data.ok_or_else(|| invalid_wav("missing WAV data chunk"))?;
    Ok(data
        .chunks_exact(2)
        .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0)
        .collect())
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let whisper = args.first().is_some_and(|arg| arg == "--whisper");
    let paths = if whisper { &args[1..] } else { &args[..] };
    if paths.len() != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: cargo run --release -p flexaudio-vad --example dump_probs -- [--whisper] <in.wav> <out.json>",
        )
        .into());
    }
    let samples = read_samples(&fs::read(&paths[0])?)?;
    // Segmentation settings affect events only; these defaults leave raw probabilities unchanged.
    let mut probs = Vec::with_capacity(samples.len().div_ceil(FRAME_SAMPLES));
    if whisper {
        let mut vad = WhisperVad::new(WhisperVadParams::default(), WhisperVadOptions::default())?;
        for chunk in samples.chunks(FRAME_SAMPLES) {
            vad.process(chunk)?;
            probs.extend_from_slice(vad.last_frame_probabilities().values);
        }
        // Whisper EOF infers one zero-padded partial frame: ceil(n_samples / 512).
        vad.finish()?;
        probs.extend_from_slice(vad.last_frame_probabilities().values);
    } else {
        let mut vad = Vad::new(VadConfig {
            sample_rate: 16_000,
            ..VadConfig::default()
        })?;
        for chunk in samples.chunks(FRAME_SAMPLES) {
            vad.process(chunk)?;
            probs.extend_from_slice(vad.last_frame_probabilities());
        }
        // Legacy flush discards the trailing partial frame without inference and clears
        // latest probabilities. Collect before flush: floor(n_samples / 512).
        vad.flush()?;
    }
    if probs.iter().any(|prob| !prob.is_finite()) {
        return Err(invalid_wav("VAD returned a non-finite probability; cannot write JSON").into());
    }

    let mut output = BufWriter::new(File::create(&paths[1])?);
    write!(
        output,
        "{{\"model\":\"flexaudio-silero-v6\",\"n_samples\":{},\"frame_samples\":512,\"probs\":[",
        samples.len()
    )?;
    for (index, prob) in probs.iter().enumerate() {
        if index != 0 {
            write!(output, ",")?;
        }
        write!(output, "{prob}")?;
    }
    writeln!(output, "]}}")?;
    output.flush()?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dump_probs: {error}");
            ExitCode::FAILURE
        }
    }
}
