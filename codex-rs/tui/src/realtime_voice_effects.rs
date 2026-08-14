//! Client-side output effects for GPT-Live voice sessions.
//!
//! Presets are deliberately kept outside config.toml: they are local audio assets that can be
//! edited and copied between machines without changing the rest of a user's Codex configuration.
//! The processor runs after the remote Opus stream is decoded and before samples reach the speaker.

use anyhow::Context;
use anyhow::Result;
#[cfg(windows)]
use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::realtime_voice::SAMPLE_RATE;

const ACTIVE_PRESET_FILE: &str = "active.json";
const PRESET_DIRECTORY: &str = "voice-presets";
const PRESET_VERSION: u32 = 1;
const MAX_EQ_BANDS: usize = 8;
const BUILTIN_PRESET_NAMES: [&str; 1] = ["jarvis"];

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EqFilterKind {
    LowShelf,
    Peaking,
    HighShelf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct EqBand {
    pub(crate) kind: EqFilterKind,
    pub(crate) frequency_hz: f32,
    pub(crate) gain_db: f32,
    pub(crate) q: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct CompressorSettings {
    pub(crate) threshold_db: f32,
    pub(crate) ratio: f32,
    pub(crate) attack_ms: f32,
    pub(crate) release_ms: f32,
    pub(crate) makeup_gain_db: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct VoiceEffectPreset {
    #[serde(default = "default_preset_version")]
    pub(crate) version: u32,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) bands: Vec<EqBand>,
    #[serde(default)]
    pub(crate) compressor: Option<CompressorSettings>,
    #[serde(default)]
    pub(crate) output_gain_db: f32,
    #[serde(default)]
    pub(crate) pitch_shift_semitones: f32,
    #[serde(default)]
    pub(crate) formant_shift_semitones: f32,
    #[serde(default)]
    pub(crate) saturation: f32,
    #[serde(default)]
    pub(crate) ring_mod_frequency_hz: f32,
    #[serde(default)]
    pub(crate) ring_mod_mix: f32,
    #[serde(default = "default_bitcrush_bits")]
    pub(crate) bitcrush_bits: u8,
    #[serde(default)]
    pub(crate) reverb_mix: f32,
}

#[derive(Debug, Deserialize, Serialize)]
struct ActivePresetState {
    name: String,
}

impl VoiceEffectPreset {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != PRESET_VERSION {
            bail!(
                "voice effect preset {} uses unsupported version {}; expected {}",
                self.name,
                self.version,
                PRESET_VERSION
            );
        }
        validate_preset_name(&self.name)?;
        if self.bands.len() > MAX_EQ_BANDS {
            bail!(
                "voice effect preset {} has too many EQ bands; maximum is {}",
                self.name,
                MAX_EQ_BANDS
            );
        }
        if !self.output_gain_db.is_finite() || !(-24.0..=12.0).contains(&self.output_gain_db) {
            bail!(
                "voice effect preset {} has output_gain_db outside -24..12 dB",
                self.name
            );
        }
        if !self.pitch_shift_semitones.is_finite()
            || !(-12.0..=12.0).contains(&self.pitch_shift_semitones)
        {
            bail!(
                "voice effect preset {} has pitch_shift_semitones outside -12..12",
                self.name
            );
        }
        if !self.formant_shift_semitones.is_finite()
            || !(-6.0..=6.0).contains(&self.formant_shift_semitones)
        {
            bail!(
                "voice effect preset {} has formant_shift_semitones outside -6..6",
                self.name
            );
        }
        if !self.saturation.is_finite() || !(0.0..=1.0).contains(&self.saturation) {
            bail!(
                "voice effect preset {} has saturation outside 0..1",
                self.name
            );
        }
        if !self.ring_mod_frequency_hz.is_finite()
            || !(0.0..=2_000.0).contains(&self.ring_mod_frequency_hz)
        {
            bail!(
                "voice effect preset {} has ring_mod_frequency_hz outside 0..2000",
                self.name
            );
        }
        if !self.ring_mod_mix.is_finite() || !(0.0..=1.0).contains(&self.ring_mod_mix) {
            bail!(
                "voice effect preset {} has ring_mod_mix outside 0..1",
                self.name
            );
        }
        if !(4..=16).contains(&self.bitcrush_bits) {
            bail!(
                "voice effect preset {} has bitcrush_bits outside 4..16",
                self.name
            );
        }
        if !self.reverb_mix.is_finite() || !(0.0..=1.0).contains(&self.reverb_mix) {
            bail!(
                "voice effect preset {} has reverb_mix outside 0..1",
                self.name
            );
        }
        for (index, band) in self.bands.iter().enumerate() {
            if !band.frequency_hz.is_finite()
                || !(20.0..=(SAMPLE_RATE as f32 * 0.45)).contains(&band.frequency_hz)
            {
                bail!(
                    "voice effect preset {} band {} has frequency_hz outside 20..{}",
                    self.name,
                    index,
                    SAMPLE_RATE as f32 * 0.45
                );
            }
            if !band.gain_db.is_finite() || !(-24.0..=24.0).contains(&band.gain_db) {
                bail!(
                    "voice effect preset {} band {} has gain_db outside -24..24 dB",
                    self.name,
                    index
                );
            }
            if !band.q.is_finite() || !(0.1..=10.0).contains(&band.q) {
                bail!(
                    "voice effect preset {} band {} has q outside 0.1..10",
                    self.name,
                    index
                );
            }
            if matches!(&band.kind, EqFilterKind::LowShelf | EqFilterKind::HighShelf) {
                let amplitude = 10.0_f32.powf(band.gain_db / 40.0);
                let radicand = (amplitude + 1.0 / amplitude) * (1.0 / band.q - 1.0) + 2.0;
                if !radicand.is_finite() || radicand < 0.0 {
                    bail!(
                        "voice effect preset {} band {} has an invalid shelf gain/q combination",
                        self.name,
                        index
                    );
                }
            }
        }
        if let Some(compressor) = &self.compressor {
            if !compressor.threshold_db.is_finite()
                || !(-60.0..=0.0).contains(&compressor.threshold_db)
            {
                bail!(
                    "voice effect preset {} has compressor threshold_db outside -60..0 dB",
                    self.name
                );
            }
            if !compressor.ratio.is_finite() || !(1.0..=20.0).contains(&compressor.ratio) {
                bail!(
                    "voice effect preset {} has compressor ratio outside 1..20",
                    self.name
                );
            }
            if !compressor.attack_ms.is_finite() || !(0.1..=1_000.0).contains(&compressor.attack_ms)
            {
                bail!(
                    "voice effect preset {} has compressor attack_ms outside 0.1..1000",
                    self.name
                );
            }
            if !compressor.release_ms.is_finite()
                || !(1.0..=5_000.0).contains(&compressor.release_ms)
            {
                bail!(
                    "voice effect preset {} has compressor release_ms outside 1..5000",
                    self.name
                );
            }
            if !compressor.makeup_gain_db.is_finite()
                || !(-24.0..=24.0).contains(&compressor.makeup_gain_db)
            {
                bail!(
                    "voice effect preset {} has compressor makeup_gain_db outside -24..24 dB",
                    self.name
                );
            }
        }
        Ok(())
    }
}

fn default_preset_version() -> u32 {
    PRESET_VERSION
}

fn default_bitcrush_bits() -> u8 {
    16
}

fn validate_preset_name(name: &str) -> Result<String> {
    let normalized = name.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 64
        || normalized == "off"
        || !normalized.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        bail!(
            "voice effect preset name {name} must be 1-64 characters of letters, numbers, -, or _"
        );
    }
    Ok(normalized)
}

fn active_preset_path(codex_home: &Path) -> PathBuf {
    codex_home.join(PRESET_DIRECTORY).join(ACTIVE_PRESET_FILE)
}

pub(crate) fn preset_file_path(codex_home: &Path, name: &str) -> Result<PathBuf> {
    let normalized = validate_preset_name(name)?;
    Ok(codex_home
        .join(PRESET_DIRECTORY)
        .join(format!("{normalized}.json")))
}

pub(crate) fn active_preset_name(codex_home: &Path) -> Result<Option<String>> {
    let path = active_preset_path(codex_home);
    if !path.exists() {
        return Ok(None);
    }
    let state: ActivePresetState = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    if state.name == "off" {
        return Ok(Some(state.name));
    }
    Ok(Some(validate_preset_name(&state.name)?))
}

pub(crate) fn load_active_preset(codex_home: &Path) -> Result<Option<VoiceEffectPreset>> {
    let Some(name) = active_preset_name(codex_home)? else {
        return Ok(None);
    };
    if name == "off" {
        return Ok(None);
    }
    load_named_preset(codex_home, &name).map(Some)
}

pub(crate) fn activate_preset(codex_home: &Path, name: &str) -> Result<VoiceEffectPreset> {
    let normalized = validate_preset_name(name)?;
    let path = existing_preset_path(codex_home, &normalized)?
        .unwrap_or(preset_file_path(codex_home, &normalized)?);
    let preset = if path.exists() {
        read_preset(&path, &normalized)?
    } else if normalized == "jarvis" {
        let preset = jarvis_preset();
        save_preset(codex_home, &preset)?;
        preset
    } else {
        bail!(
            "voice effect preset {normalized} was not found at {}; use /voice effect list",
            path.display()
        );
    };
    preset.validate()?;
    write_active_preset(codex_home, &normalized)?;
    Ok(preset)
}

pub(crate) fn deactivate_preset(codex_home: &Path) -> Result<()> {
    write_active_preset(codex_home, "off")
}

pub(crate) fn list_preset_names(codex_home: &Path) -> Result<Vec<String>> {
    let directory = codex_home.join(PRESET_DIRECTORY);
    let mut names = BUILTIN_PRESET_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    if directory.exists() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("listing {}", directory.display()))?
        {
            let path = entry
                .with_context(|| format!("reading an entry in {}", directory.display()))?
                .path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json")
                || path.file_name().and_then(|file_name| file_name.to_str())
                    == Some(ACTIVE_PRESET_FILE)
            {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && validate_preset_name(stem).is_ok()
            {
                names.insert(stem.to_ascii_lowercase());
            }
        }
    }
    Ok(names.into_iter().collect())
}

pub(crate) fn save_preset(codex_home: &Path, preset: &VoiceEffectPreset) -> Result<()> {
    preset.validate()?;
    let path = preset_file_path(codex_home, &preset.name)?;
    write_json_atomic(&path, preset)
}

pub(crate) fn load_named_preset(codex_home: &Path, name: &str) -> Result<VoiceEffectPreset> {
    let normalized = validate_preset_name(name)?;
    if let Some(path) = existing_preset_path(codex_home, &normalized)? {
        return read_preset(&path, &normalized);
    }
    if normalized == "jarvis" {
        return Ok(jarvis_preset());
    }
    let path = preset_file_path(codex_home, &normalized)?;
    bail!(
        "active voice effect preset {normalized} was not found at {}",
        path.display()
    );
}

fn read_preset(path: &Path, expected_name: &str) -> Result<VoiceEffectPreset> {
    let preset: VoiceEffectPreset = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    preset
        .validate()
        .with_context(|| format!("{}", path.display()))?;
    if preset.name != expected_name {
        bail!(
            "voice effect preset name `{}` does not match the requested name `{expected_name}`",
            preset.name
        );
    }
    Ok(preset)
}

fn existing_preset_path(codex_home: &Path, name: &str) -> Result<Option<PathBuf>> {
    let directory = codex_home.join(PRESET_DIRECTORY);
    if !directory.exists() {
        return Ok(None);
    }
    for entry in
        fs::read_dir(&directory).with_context(|| format!("listing {}", directory.display()))?
    {
        let path = entry
            .with_context(|| format!("reading an entry in {}", directory.display()))?
            .path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json")
            || path.file_name().and_then(|file_name| file_name.to_str()) == Some(ACTIVE_PRESET_FILE)
        {
            continue;
        }
        if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn write_active_preset(codex_home: &Path, name: &str) -> Result<()> {
    let normalized = if name == "off" {
        name.to_string()
    } else {
        validate_preset_name(name)?
    };
    let path = active_preset_path(codex_home);
    write_json_atomic(&path, &ActivePresetState { name: normalized })
}

pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let Some(directory) = path.parent() else {
        bail!("cannot persist voice effect state without a parent directory");
    };
    fs::create_dir_all(directory).with_context(|| format!("creating {}", directory.display()))?;
    let mut temporary_path = path.to_path_buf();
    temporary_path.set_extension(format!("{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value).context("serializing voice effect state")?;
    fs::write(&temporary_path, bytes)
        .with_context(|| format!("writing {}", temporary_path.display()))?;
    match fs::rename(&temporary_path, path) {
        Ok(()) => Ok(()),
        Err(initial_error) => {
            #[cfg(windows)]
            {
                if path.exists() {
                    let mut backup_path = path.to_path_buf();
                    backup_path.set_extension(format!("bak-{}", std::process::id()));
                    fs::rename(path, &backup_path).with_context(|| {
                        format!("backing up {} before replacement", path.display())
                    })?;
                    match fs::rename(&temporary_path, path) {
                        Ok(()) => {
                            let _ = fs::remove_file(&backup_path);
                            return Ok(());
                        }
                        Err(replacement_error) => {
                            let restore_error = fs::rename(&backup_path, path).err();
                            let _ = fs::remove_file(&temporary_path);
                            return match restore_error {
                                None => Err(replacement_error)
                                    .with_context(|| format!("replacing {}", path.display())),
                                Some(restore_error) => Err(anyhow!(
                                    "replacing {} failed: {replacement_error}; restoring the previous file failed: {restore_error}",
                                    path.display()
                                )),
                            };
                        }
                    }
                }
            }
            let _ = fs::remove_file(&temporary_path);
            Err(initial_error).with_context(|| format!("replacing {}", path.display()))
        }
    }
}

fn jarvis_preset() -> VoiceEffectPreset {
    VoiceEffectPreset {
        version: PRESET_VERSION,
        name: "jarvis".to_string(),
        bands: vec![
            EqBand {
                kind: EqFilterKind::LowShelf,
                frequency_hz: 110.0,
                gain_db: -3.5,
                q: 0.707,
            },
            EqBand {
                kind: EqFilterKind::Peaking,
                frequency_hz: 260.0,
                gain_db: -2.0,
                q: 0.9,
            },
            EqBand {
                kind: EqFilterKind::Peaking,
                frequency_hz: 1_900.0,
                gain_db: 2.5,
                q: 0.9,
            },
            EqBand {
                kind: EqFilterKind::Peaking,
                frequency_hz: 4_200.0,
                gain_db: 1.5,
                q: 1.1,
            },
            EqBand {
                kind: EqFilterKind::HighShelf,
                frequency_hz: 9_500.0,
                gain_db: -2.0,
                q: 0.707,
            },
        ],
        compressor: Some(CompressorSettings {
            threshold_db: -20.0,
            ratio: 2.5,
            attack_ms: 8.0,
            release_ms: 120.0,
            makeup_gain_db: 1.5,
        }),
        output_gain_db: -1.5,
        pitch_shift_semitones: -1.5,
        formant_shift_semitones: -1.0,
        saturation: 0.08,
        ring_mod_frequency_hz: 0.0,
        ring_mod_mix: 0.0,
        bitcrush_bits: 16,
        reverb_mix: 0.05,
    }
}

#[cfg(test)]
#[path = "realtime_voice_effects_tests.rs"]
mod tests;
