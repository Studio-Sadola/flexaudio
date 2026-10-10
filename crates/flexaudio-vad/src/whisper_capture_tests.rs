use super::*;

fn convert(stereo: &[f32], chunk_frames: usize) -> Vec<f32> {
    let mut converter = CaptureConverter::new().unwrap();
    let mut out = Vec::new();
    for chunk in stereo.chunks(chunk_frames * 2) {
        out.extend(converter.push(chunk).unwrap());
    }
    out.extend(converter.drain().unwrap());
    assert!(converter.drain().unwrap().is_empty());
    out
}

fn peak(samples: &[f32]) -> usize {
    samples
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
        .unwrap()
        .0
}

#[test]
fn phase_aligned_startup_has_320_raw_and_299_retained_outputs() {
    let mut raw = PcmConverter::new_capture_48k().unwrap();
    let mut output = Vec::new();
    raw.convert(&[0.0; 1920], &mut output).unwrap();
    assert_eq!(output.len(), 320);
    let mut converter = CaptureConverter::new().unwrap();
    assert_eq!(converter.push(&[0.0; 1920]).unwrap().len(), 299);
    assert_eq!(converter.drain().unwrap().len(), 21);
    // The legacy, unaligned convention remains unchanged.
    let mut legacy = PcmConverter::new(
        crate::PcmFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        16_000,
    )
    .unwrap();
    output.clear();
    legacy.convert(&[0.0; 1920], &mut output).unwrap();
    assert_eq!(output.len(), 319);
}

#[test]
fn click_energy_maps_to_capture_clock_within_one_16k_sample() {
    for click in [4800_usize, 4801, 4802, 5758, 5759, 5760] {
        let mut stereo = vec![0.0; 12003 * 2];
        stereo[click * 2] = 1.0;
        stereo[click * 2 + 1] = 1.0;
        let mut raw = Vec::new();
        PcmConverter::new_capture_48k()
            .unwrap()
            .convert(&stereo, &mut raw)
            .unwrap();
        let aligned = convert(&stereo, 1776);
        assert_eq!(peak(&raw), peak(&aligned) + STARTUP_TRIM);
        let capture_origin = (1_u64 << 54) + 13;
        let mapped = capture_origin + u64::try_from(peak(&aligned)).unwrap() * 3;
        let actual = capture_origin + u64::try_from(click).unwrap();
        assert!(
            mapped.abs_diff(actual) <= 3,
            "click={click}, peak={}",
            peak(&aligned)
        );
    }
}

#[test]
fn delay_is_compensated_and_eof_energy_recovered_instead_of_truncated() {
    for n in [1_usize, 2, 3, 4, 959, 960, 961, 1900, 1920, 1921, 12003] {
        let mut stereo = vec![0.0; n * 2];
        stereo[..2].fill(1.0);
        let start = convert(&stereo, 480);
        assert_eq!(start.len(), n.div_ceil(3));
        assert!(start[0] > 0.25, "startup click lost for N={n}");
        stereo.fill(0.0);
        stereo[(n - 1) * 2..].fill(1.0);
        let tail = convert(&stereo, 1776);
        assert_eq!(tail.len(), n.div_ceil(3));
        assert!(peak(&tail).abs_diff((n - 1) / 3) <= 1);
        assert!(tail.last().unwrap().abs() > 0.1, "EOF click lost for N={n}");
    }
}

#[test]
fn aligned_ramp_and_stereo_average_are_partition_invariant() {
    let n = 12003;
    let stereo: Vec<f32> = (0..n)
        .flat_map(|i| {
            let sample = i as f32 * 0.00001;
            [sample + 0.1, sample - 0.1]
        })
        .collect();
    let whole = convert(&stereo, n);
    for frames in [1, 480, 960, 1776, 17, 311] {
        assert_eq!(convert(&stereo, frames), whole);
    }
    let constant = convert(&vec![0.5; n * 2], n);
    for (j, value) in whole.iter().enumerate().take(3900).skip(100) {
        // Independent source ramp, not a second resampler or a fitted waveform offset.
        // Normalize rubato's f32 kernel DC gain separately from its sampling phase.
        let expected = (j * 3) as f32 * 0.00001;
        let normalized = value * 0.5 / constant[j];
        assert!(
            (normalized - expected).abs() < 0.0000001,
            "j={j}: {normalized} vs {expected}"
        );
    }
    let inverted: Vec<_> = (0..n)
        .flat_map(|i| {
            let value = (i as f32 * 0.2).sin();
            [value, -value]
        })
        .collect();
    assert!(convert(&inverted, 480).iter().all(|&value| value == 0.0));
}
