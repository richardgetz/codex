use super::*;
use pretty_assertions::assert_eq;

#[test]
fn input_converters_center_unsigned_audio() {
    assert_eq!(u8_to_i16(128), 0);
    assert_eq!(u16_to_i16(32_768), 0);
    assert_eq!(u24_to_i16(U24::from(8_388_608)), 0);
    assert_eq!(u32_to_i16(2_147_483_648), 0);
    assert_eq!(u64_to_i16(9_223_372_036_854_775_808), 0);
}

#[test]
fn input_converters_handle_24_bit_pcm() {
    assert_eq!(i24_to_i16(I24::from(-8_388_608)), i16::MIN);
    assert_eq!(i24_to_i16(I24::from(8_388_607)), i16::MAX);
    assert_eq!(u24_to_i16(U24::from(0)), i16::MIN);
    assert_eq!(u24_to_i16(U24::from(16_777_215)), i16::MAX);
    assert_eq!(i16_to_i24(i16::MIN).inner(), -8_388_608);
    assert_eq!(i16_to_i24(i16::MAX).inner(), 8_388_352);
    assert_eq!(i16_to_u24(i16::MIN).inner(), 0);
    assert_eq!(i16_to_u24(i16::MAX).inner(), 16_776_960);
}

#[test]
fn input_config_prefers_48_khz_when_available() {
    let ranges = vec![input_config_range(16_000), input_config_range(SAMPLE_RATE)];

    let config =
        select_supported_audio_config(&ranges, INPUT_SAMPLE_RATES, SampleRatePolicy::AllowFallback)
            .expect("a supported input config should be selected");

    assert_eq!(config.sample_rate(), SAMPLE_RATE);
}

#[test]
fn input_config_falls_back_to_a_common_device_rate() {
    let ranges = vec![input_config_range_between(96_000, 192_000)];

    let config =
        select_supported_audio_config(&ranges, INPUT_SAMPLE_RATES, SampleRatePolicy::AllowFallback)
            .expect("a supported input config should be selected");

    assert_eq!(config.sample_rate(), 96_000);
}

#[test]
fn input_config_accepts_24_bit_pcm_formats() {
    assert!(supported_sample_format(SampleFormat::I24));
    assert!(supported_sample_format(SampleFormat::U24));
}

#[test]
fn output_config_requires_exactly_48_khz() {
    let ranges = vec![input_config_range(44_100)];

    assert!(
        select_supported_audio_config(&ranges, &[SAMPLE_RATE], SampleRatePolicy::Exact).is_none()
    );
}

#[test]
fn input_resampler_converts_24_khz_to_48_khz() {
    let mut resampler = InputResampler::new(24_000, SAMPLE_RATE);
    let mut output = Vec::new();

    for sample in [0, 1_000, 2_000] {
        resampler.push(sample);
        while let Some(sample) = resampler.pop_output() {
            output.push(sample);
        }
    }

    assert_eq!(output, [0, 500, 1_000, 1_500]);
}

#[test]
fn input_resampler_preserves_phase_when_downsampling() {
    let mut resampler = InputResampler::new(96_000, SAMPLE_RATE);
    let mut output = Vec::new();

    for _ in 0..10 {
        resampler.push(1_000);
        while let Some(sample) = resampler.pop_output() {
            output.push(sample);
        }
    }

    assert_eq!(output, [1_000, 1_000, 1_000, 1_000, 1_000]);
}

#[test]
fn input_resampler_preserves_phase_for_44_1_khz_input() {
    let mut resampler = InputResampler::new(44_100, SAMPLE_RATE);
    let mut output_count = 0;

    for _ in 0..4_410 {
        resampler.push(1_000);
        while resampler.pop_output().is_some() {
            output_count += 1;
        }
    }

    assert_eq!(output_count, 4_799);
}

#[test]
fn input_resampler_reduces_above_nyquist_tones_when_downsampling() {
    let mut resampler = InputResampler::new(96_000, SAMPLE_RATE);
    let mut output = Vec::new();

    for sample_index in 0..2_048 {
        let phase = 2.0 * std::f64::consts::PI * 36_000.0 * sample_index as f64 / 96_000.0;
        resampler.push((phase.sin() * f64::from(i16::MAX)) as i16);
        while let Some(sample) = resampler.pop_output() {
            output.push(sample);
        }
    }

    let tail = &output[128..];
    let rms = (tail
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / tail.len() as f64)
        .sqrt();
    assert!(rms < 1_000.0, "unexpected aliased RMS: {rms}");
}

#[test]
fn input_accumulator_keeps_fixed_frame_size_across_callbacks() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(1);
    let mut accumulator =
        InputFrameAccumulator::new(1, 24_000, input_tx, Arc::new(AtomicBool::new(false)));
    let samples = vec![0i16; 481];

    accumulator.push(&samples[..240], &|sample| sample);
    accumulator.push(&samples[240..], &|sample| sample);

    assert_eq!(
        input_rx
            .try_recv()
            .expect("one frame should be ready")
            .len(),
        FRAME_SAMPLES
    );
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn output_queue_fills_missing_right_channel_with_left() {
    let mut queue = VecDeque::from([123]);
    assert_eq!(pop_stereo_sample(&mut queue), (123, 123));
}

#[test]
fn muted_remote_audio_is_discarded_until_handoff_completion() {
    let output_queue = Arc::new(Mutex::new(VecDeque::from([1, 2])));
    let output_muted = Arc::new(AtomicBool::new(true));

    append_remote_audio(&output_queue, &output_muted, &[3, 4]);

    assert_eq!(
        *output_queue
            .lock()
            .expect("output queue should be available"),
        [1, 2]
    );
    output_muted.store(false, Ordering::Relaxed);
    append_remote_audio(&output_queue, &output_muted, &[3, 4]);
    assert_eq!(
        *output_queue
            .lock()
            .expect("output queue should be available"),
        [1, 2, 3, 4]
    );
}

#[test]
fn input_buffer_keeps_preroll_before_first_signal() {
    let mut frames = VecDeque::from_iter((0..10).map(|_| [0i16; FRAME_SAMPLES]));
    frames[7][0] = INPUT_SIGNAL_THRESHOLD;

    release_input_buffer(&mut frames);

    let mut expected = VecDeque::from_iter((0..8).map(|_| [0i16; FRAME_SAMPLES]));
    expected[5][0] = INPUT_SIGNAL_THRESHOLD;
    assert_eq!(frames, expected);
}

#[test]
fn input_buffer_discards_silence_until_connection_release() {
    let mut frames = VecDeque::from_iter((0..10).map(|_| [0i16; FRAME_SAMPLES]));

    release_input_buffer(&mut frames);

    assert!(frames.is_empty());
}

fn input_config_range(sample_rate: u32) -> cpal::SupportedStreamConfigRange {
    input_config_range_between(sample_rate, sample_rate)
}

fn input_config_range_between(
    min_sample_rate: u32,
    max_sample_rate: u32,
) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(
        1,
        min_sample_rate,
        max_sample_rate,
        cpal::SupportedBufferSize::Unknown,
        SampleFormat::F32,
    )
}
