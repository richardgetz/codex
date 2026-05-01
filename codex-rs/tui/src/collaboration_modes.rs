use codex_models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;

use crate::model_catalog::ModelCatalog;

fn filtered_presets(
    _model_catalog: &ModelCatalog,
    collaboration_modes_config: CollaborationModesConfig,
) -> Vec<CollaborationModeMask> {
    builtin_collaboration_mode_presets(collaboration_modes_config)
        .into_iter()
        .filter(|mask| mask.mode.is_some_and(ModeKind::is_tui_visible))
        .collect()
}

pub(crate) fn presets_for_tui_with_config(
    model_catalog: &ModelCatalog,
    collaboration_modes_config: CollaborationModesConfig,
) -> Vec<CollaborationModeMask> {
    filtered_presets(model_catalog, collaboration_modes_config)
}

#[cfg(test)]
pub(crate) fn default_mask(model_catalog: &ModelCatalog) -> Option<CollaborationModeMask> {
    default_mask_with_config(model_catalog, CollaborationModesConfig::default())
}

pub(crate) fn default_mask_with_config(
    model_catalog: &ModelCatalog,
    collaboration_modes_config: CollaborationModesConfig,
) -> Option<CollaborationModeMask> {
    let presets = filtered_presets(model_catalog, collaboration_modes_config);
    presets
        .iter()
        .find(|mask| mask.mode == Some(ModeKind::Default))
        .cloned()
        .or_else(|| presets.into_iter().next())
}

#[cfg(test)]
pub(crate) fn mask_for_kind(
    model_catalog: &ModelCatalog,
    kind: ModeKind,
) -> Option<CollaborationModeMask> {
    mask_for_kind_with_config(model_catalog, kind, CollaborationModesConfig::default())
}

pub(crate) fn mask_for_kind_with_config(
    model_catalog: &ModelCatalog,
    kind: ModeKind,
    collaboration_modes_config: CollaborationModesConfig,
) -> Option<CollaborationModeMask> {
    if !kind.is_tui_visible() {
        return None;
    }
    filtered_presets(model_catalog, collaboration_modes_config)
        .into_iter()
        .find(|mask| mask.mode == Some(kind))
}

pub(crate) fn next_mask_with_config(
    model_catalog: &ModelCatalog,
    current: Option<&CollaborationModeMask>,
    collaboration_modes_config: CollaborationModesConfig,
) -> Option<CollaborationModeMask> {
    let presets = filtered_presets(model_catalog, collaboration_modes_config);
    if presets.is_empty() {
        return None;
    }
    let current_kind = current.and_then(|mask| mask.mode);
    let next_index = presets
        .iter()
        .position(|mask| mask.mode == current_kind)
        .map_or(0, |idx| (idx + 1) % presets.len());
    presets.get(next_index).cloned()
}

#[cfg(test)]
pub(crate) fn default_mode_mask(model_catalog: &ModelCatalog) -> Option<CollaborationModeMask> {
    default_mode_mask_with_config(model_catalog, CollaborationModesConfig::default())
}

pub(crate) fn default_mode_mask_with_config(
    model_catalog: &ModelCatalog,
    collaboration_modes_config: CollaborationModesConfig,
) -> Option<CollaborationModeMask> {
    mask_for_kind_with_config(model_catalog, ModeKind::Default, collaboration_modes_config)
}

#[cfg(test)]
pub(crate) fn plan_mask(model_catalog: &ModelCatalog) -> Option<CollaborationModeMask> {
    plan_mask_with_config(model_catalog, CollaborationModesConfig::default())
}

pub(crate) fn plan_mask_with_config(
    model_catalog: &ModelCatalog,
    collaboration_modes_config: CollaborationModesConfig,
) -> Option<CollaborationModeMask> {
    mask_for_kind_with_config(model_catalog, ModeKind::Plan, collaboration_modes_config)
}
