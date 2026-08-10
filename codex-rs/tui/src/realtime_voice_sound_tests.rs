use super::RealtimeAcknowledgementSound;
use super::load_acknowledgement_sound;
use crate::realtime_voice::SAMPLE_RATE;
use std::io::Write;

#[test]
fn built_in_acknowledgement_sound_is_stereo() {
    let samples = load_acknowledgement_sound(&RealtimeAcknowledgementSound::BuiltIn)
        .expect("built-in sound should load")
        .expect("built-in sound should be enabled");

    assert_eq!(samples.len(), SAMPLE_RATE as usize * 140 / 1_000 * 2);
    assert!(samples.iter().any(|sample| *sample != 0));
}

#[test]
fn wav_acknowledgement_sound_is_resampled_to_the_voice_rate() {
    let mut file = tempfile::NamedTempFile::new().expect("temporary WAV file");
    let pcm = [0i16, 16_384, -16_384, 0];
    let data_len = pcm.len() * std::mem::size_of::<i16>();
    let riff_len = 36 + data_len;
    file.write_all(b"RIFF").expect("RIFF header");
    file.write_all(&(riff_len as u32).to_le_bytes())
        .expect("RIFF length");
    file.write_all(b"WAVEfmt ").expect("format header");
    file.write_all(&16u32.to_le_bytes()).expect("format length");
    file.write_all(&1u16.to_le_bytes()).expect("PCM format");
    file.write_all(&1u16.to_le_bytes())
        .expect("mono channel count");
    file.write_all(&8_000u32.to_le_bytes())
        .expect("source rate");
    file.write_all(&16_000u32.to_le_bytes()).expect("byte rate");
    file.write_all(&2u16.to_le_bytes())
        .expect("block alignment");
    file.write_all(&16u16.to_le_bytes()).expect("sample depth");
    file.write_all(b"data").expect("data header");
    file.write_all(&(data_len as u32).to_le_bytes())
        .expect("data length");
    for sample in pcm {
        file.write_all(&sample.to_le_bytes()).expect("PCM sample");
    }
    file.flush().expect("flush WAV file");

    let samples = load_acknowledgement_sound(&RealtimeAcknowledgementSound::File(
        file.path().to_path_buf(),
    ))
    .expect("WAV file should load")
    .expect("WAV sound should be enabled");

    assert_eq!(samples.len(), pcm.len() * SAMPLE_RATE as usize / 8_000 * 2);
    assert!(samples.iter().any(|sample| *sample > 0));
    assert!(samples.iter().any(|sample| *sample < 0));
}
