use super::*;

#[test]
fn pitch_shifter_keeps_both_directions_live_without_unbounded_backlog() {
    for semitones in [-6.0, 6.0] {
        let mut shifter = StereoPitchShifter::new(semitones);
        let mut produced_signal = false;
        for index in 0..SAMPLE_RATE as usize * 3 {
            let (left, right) = shifter.process(0.25, 0.25);
            if index > shifter.latency + PITCH_GRAIN_SIZE && left.abs() > 0.01 {
                produced_signal = true;
                assert_eq!(left, right);
            }
        }

        assert!(produced_signal);
        assert!(shifter.buffer.len() < PITCH_GRAIN_SIZE * 10);
    }
}
