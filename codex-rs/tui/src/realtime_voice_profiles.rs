//! Client-side profiles that pair a GPT-Live voice with an output effect preset.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::realtime_voice_effects::VoiceEffectPreset;
use crate::realtime_voice_effects::activate_preset;
use crate::realtime_voice_effects::active_preset_name;
use crate::realtime_voice_effects::deactivate_preset;
use crate::realtime_voice_effects::write_json_atomic;

const PROFILE_DIRECTORY: &str = "voice-presets/profiles";
const ACTIVE_PROFILE_FILE: &str = "active.json";
const PROFILE_VERSION: u32 = 1;
const BUILTIN_PROFILE_NAMES: [&str; 1] = ["jarvis"];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct VoiceProfile {
    #[serde(default = "default_profile_version")]
    pub(crate) version: u32,
    pub(crate) name: String,
    pub(crate) voice: RealtimeVoice,
    pub(crate) effect: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ActiveProfileState {
    name: String,
}

impl VoiceProfile {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != PROFILE_VERSION {
            bail!(
                "voice profile {} uses unsupported version {}; expected {}",
                self.name,
                self.version,
                PROFILE_VERSION
            );
        }
        validate_profile_name(&self.name)?;
        validate_profile_name(&self.effect)?;
        if !RealtimeVoicesList::builtin().v1.contains(&self.voice) {
            bail!(
                "voice profile {} uses GPT-Live voice `{}` which is not available on the V3 client path",
                self.name,
                self.voice.wire_name()
            );
        }
        Ok(())
    }
}

pub(crate) fn profile_file_path(codex_home: &Path, name: &str) -> Result<PathBuf> {
    let normalized = validate_profile_name(name)?;
    Ok(codex_home
        .join(PROFILE_DIRECTORY)
        .join(format!("{normalized}.json")))
}

pub(crate) fn active_profile_name(codex_home: &Path) -> Result<Option<String>> {
    let path = active_profile_path(codex_home);
    if !path.exists() {
        return Ok(None);
    }
    let state: ActiveProfileState = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    if state.name == "off" {
        return Ok(Some(state.name));
    }
    Ok(Some(validate_profile_name(&state.name)?))
}

pub(crate) fn load_active_profile(codex_home: &Path) -> Result<Option<VoiceProfile>> {
    let Some(name) = active_profile_name(codex_home)? else {
        return Ok(None);
    };
    if name == "off" {
        return Ok(None);
    }
    load_named_profile(codex_home, &name).map(Some)
}

pub(crate) fn activate_profile(codex_home: &Path, name: &str) -> Result<VoiceProfile> {
    let normalized = validate_profile_name(name)?;
    let path = existing_profile_path(codex_home, &normalized)?
        .unwrap_or(profile_file_path(codex_home, &normalized)?);
    let profile = if path.exists() {
        read_profile(&path, &normalized)?
    } else if normalized == "jarvis" {
        let profile = jarvis_profile();
        save_profile(codex_home, &profile)?;
        profile
    } else {
        bail!(
            "voice profile {normalized} was not found at {}; use /voice profile list",
            path.display()
        );
    };
    let previous_effect = active_preset_name(codex_home)?;
    activate_preset(codex_home, &profile.effect)?;
    if let Err(err) = write_active_profile(codex_home, &normalized) {
        if let Err(rollback_err) = restore_active_preset(codex_home, previous_effect.as_deref()) {
            bail!(
                "activating voice profile {normalized} failed: {err:#}; restoring the previous effect also failed: {rollback_err:#}"
            );
        }
        return Err(err)
            .context("restoring the previous active effect after profile activation failed");
    }
    Ok(profile)
}

pub(crate) fn activate_preset_and_deactivate_profile(
    codex_home: &Path,
    name: &str,
) -> Result<VoiceEffectPreset> {
    let previous_profile = active_profile_name(codex_home)?;
    let previous_effect = active_preset_name(codex_home)?;
    deactivate_profile(codex_home).context("clearing the active voice profile")?;
    match activate_preset(codex_home, name) {
        Ok(preset) => Ok(preset),
        Err(err) => {
            let profile_restore = restore_active_profile(codex_home, previous_profile.as_deref());
            let effect_restore = restore_active_preset(codex_home, previous_effect.as_deref());
            match (profile_restore, effect_restore) {
                (Ok(()), Ok(())) => {
                    Err(err).context("restoring the previous voice profile and effect")
                }
                (Err(profile_err), Ok(())) => Err(err).context(format!(
                    "restoring the previous voice profile failed: {profile_err:#}"
                )),
                (Ok(()), Err(effect_err)) => Err(err).context(format!(
                    "restoring the previous voice effect failed: {effect_err:#}"
                )),
                (Err(profile_err), Err(effect_err)) => Err(err).context(format!(
                    "restoring the previous voice profile failed: {profile_err:#}; restoring the previous voice effect failed: {effect_err:#}"
                )),
            }
        }
    }
}

pub(crate) fn deactivate_profile(codex_home: &Path) -> Result<()> {
    write_active_profile(codex_home, "off")
}

pub(crate) fn deactivate_profile_and_preset(codex_home: &Path) -> Result<()> {
    let previous_profile = active_profile_name(codex_home)?;
    let previous_effect = active_preset_name(codex_home)?;
    deactivate_profile(codex_home)?;
    if let Err(err) = deactivate_preset(codex_home) {
        if let Err(rollback_err) = restore_active_profile(codex_home, previous_profile.as_deref()) {
            bail!(
                "disabling the voice profile and effect failed: {err:#}; restoring the previous profile also failed: {rollback_err:#}"
            );
        }
        restore_active_preset(codex_home, previous_effect.as_deref())?;
        return Err(err)
            .context("restoring the previous active profile and effect after disabling failed");
    }
    Ok(())
}

pub(crate) fn list_profile_names(codex_home: &Path) -> Result<Vec<String>> {
    let directory = codex_home.join(PROFILE_DIRECTORY);
    let mut names = BUILTIN_PROFILE_NAMES
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
                || path.file_name().and_then(|name| name.to_str()) == Some(ACTIVE_PROFILE_FILE)
            {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && validate_profile_name(stem).is_ok()
            {
                names.insert(stem.to_ascii_lowercase());
            }
        }
    }
    Ok(names.into_iter().collect())
}

pub(crate) fn save_profile(codex_home: &Path, profile: &VoiceProfile) -> Result<()> {
    profile.validate()?;
    crate::realtime_voice_effects::load_named_preset(codex_home, &profile.effect)?;
    let path = profile_file_path(codex_home, &profile.name)?;
    write_json_atomic(&path, profile)
}

pub(crate) fn load_named_profile(codex_home: &Path, name: &str) -> Result<VoiceProfile> {
    let normalized = validate_profile_name(name)?;
    if let Some(path) = existing_profile_path(codex_home, &normalized)? {
        let profile = read_profile(&path, &normalized)?;
        crate::realtime_voice_effects::load_named_preset(codex_home, &profile.effect)?;
        return Ok(profile);
    }
    let path = profile_file_path(codex_home, &normalized)?;
    if normalized == "jarvis" {
        let profile = jarvis_profile();
        crate::realtime_voice_effects::load_named_preset(codex_home, &profile.effect)?;
        return Ok(profile);
    }
    bail!(
        "voice profile {normalized} was not found at {}",
        path.display()
    );
}

fn read_profile(path: &Path, expected_name: &str) -> Result<VoiceProfile> {
    let profile: VoiceProfile = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    profile
        .validate()
        .with_context(|| format!("{}", path.display()))?;
    if profile.name != expected_name {
        bail!(
            "voice profile name `{}` does not match the requested name `{expected_name}`",
            profile.name
        );
    }
    Ok(profile)
}

fn existing_profile_path(codex_home: &Path, name: &str) -> Result<Option<PathBuf>> {
    let directory = codex_home.join(PROFILE_DIRECTORY);
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
            || path.file_name().and_then(|file_name| file_name.to_str())
                == Some(ACTIVE_PROFILE_FILE)
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

fn active_profile_path(codex_home: &Path) -> PathBuf {
    codex_home.join(PROFILE_DIRECTORY).join(ACTIVE_PROFILE_FILE)
}

fn write_active_profile(codex_home: &Path, name: &str) -> Result<()> {
    let normalized = if name == "off" {
        name.to_string()
    } else {
        validate_profile_name(name)?
    };
    write_json_atomic(
        &active_profile_path(codex_home),
        &ActiveProfileState { name: normalized },
    )
}

fn restore_active_profile(codex_home: &Path, name: Option<&str>) -> Result<()> {
    match name {
        None | Some("off") => deactivate_profile(codex_home),
        Some(name) => write_active_profile(codex_home, name),
    }
}

fn restore_active_preset(codex_home: &Path, name: Option<&str>) -> Result<()> {
    match name {
        None | Some("off") => deactivate_preset(codex_home),
        Some(name) => activate_preset(codex_home, name).map(|_| ()),
    }
}

fn jarvis_profile() -> VoiceProfile {
    VoiceProfile {
        version: PROFILE_VERSION,
        name: "jarvis".to_string(),
        voice: RealtimeVoice::Arbor,
        effect: "jarvis".to_string(),
    }
}

fn default_profile_version() -> u32 {
    PROFILE_VERSION
}

fn validate_profile_name(name: &str) -> Result<String> {
    let normalized = name.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 64
        || normalized == "off"
        || !normalized.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        bail!("voice profile name {name} must be 1-64 characters of letters, numbers, -, or _");
    }
    Ok(normalized)
}

#[cfg(test)]
#[path = "realtime_voice_profiles_tests.rs"]
mod tests;
