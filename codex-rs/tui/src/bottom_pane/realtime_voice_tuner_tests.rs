use super::*;
use insta::assert_snapshot;
use tokio::sync::mpsc::unbounded_channel;

fn render_lines(view: &RealtimeVoiceTuner, width: u16) -> String {
    let area = Rect::new(0, 0, width, view.desired_height(width));
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|col| buf[(col, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn renders_voice_tuner_controls() {
    let (tx, _rx) = unbounded_channel();
    let view = RealtimeVoiceTuner::new(
        VoiceEffectPreset {
            version: 1,
            name: "jarvis".to_string(),
            bands: Vec::new(),
            compressor: None,
            output_gain_db: 1.0,
            pitch_shift_semitones: -2.0,
            formant_shift_semitones: -1.0,
            saturation: 0.2,
            ring_mod_frequency_hz: 30.0,
            ring_mod_mix: 0.1,
            bitcrush_bits: 16,
            reverb_mix: 0.15,
        },
        AppEventSender::new(tx),
    );
    assert_snapshot!(render_lines(&view, 80));
}
