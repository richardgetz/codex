use super::*;
use pretty_assertions::assert_eq;

#[test]
fn input_converters_center_unsigned_audio() {
    assert_eq!(u8_to_i16(128), 0);
    assert_eq!(u16_to_i16(32_768), 0);
    assert_eq!(u32_to_i16(2_147_483_648), 0);
    assert_eq!(u64_to_i16(9_223_372_036_854_775_808), 0);
}

#[test]
fn output_queue_fills_missing_right_channel_with_left() {
    let mut queue = VecDeque::from([123]);
    assert_eq!(pop_stereo_sample(&mut queue), (123, 123));
}

#[test]
fn muted_remote_audio_preserves_local_acknowledgement_sound() {
    let mut output_queue = VecDeque::from([123]);
    let mut acknowledgement_queue = VecDeque::from([456]);

    assert_eq!(
        pop_output_sample(&mut output_queue, &mut acknowledgement_queue, true),
        (456, 456)
    );
    assert!(output_queue.is_empty());
    assert_eq!(
        pop_output_sample(&mut output_queue, &mut acknowledgement_queue, true),
        (0, 0)
    );
    assert!(output_queue.is_empty());
    output_queue.push_back(789);
    assert_eq!(
        pop_output_sample(&mut output_queue, &mut acknowledgement_queue, false),
        (789, 789)
    );
}

#[test]
fn input_buffer_keeps_preroll_before_first_signal() {
    let mut frames = VecDeque::from_iter((0..10).map(|_| vec![0i16]));
    frames[7][0] = INPUT_SIGNAL_THRESHOLD;

    release_input_buffer(&mut frames);

    let mut expected = VecDeque::from_iter((0..8).map(|_| vec![0i16]));
    expected[5][0] = INPUT_SIGNAL_THRESHOLD;
    assert_eq!(frames, expected);
}

#[test]
fn input_buffer_discards_silence_until_connection_release() {
    let mut frames = VecDeque::from_iter((0..10).map(|_| vec![0i16]));

    release_input_buffer(&mut frames);

    assert!(frames.is_empty());
}
