//! Unified ID resolution for decision-provenance TUI commands.

use codex_state::StateRuntime;
use codex_state::decision_provenance::Crossroad;
use codex_state::decision_provenance::Decision;

const PREFIX_MATCH_LIMIT: usize = 2;
const MAX_DISPLAYED_CANDIDATES: usize = 2;

#[derive(Clone)]
pub(super) enum ShowTarget {
    Crossroad(Crossroad),
    Decision(Decision),
}

pub(super) async fn resolve_crossroad(
    state_db: &StateRuntime,
    requested_id: &str,
) -> anyhow::Result<Option<Crossroad>> {
    match resolve_record(state_db, requested_id).await? {
        None => Ok(None),
        Some(ShowTarget::Crossroad(crossroad)) => Ok(Some(crossroad)),
        Some(ShowTarget::Decision(_)) => Err(anyhow::anyhow!(
            "record id `{requested_id}` refers to a decision; expected a crossroad"
        )),
    }
}

pub(super) async fn resolve_decision_id(
    state_db: &StateRuntime,
    requested_id: &str,
) -> anyhow::Result<String> {
    match resolve_record(state_db, requested_id).await? {
        Some(ShowTarget::Decision(decision)) => Ok(decision.id),
        Some(ShowTarget::Crossroad(_)) => Err(anyhow::anyhow!(
            "record id `{requested_id}` refers to a crossroad; expected a decision"
        )),
        None => Err(anyhow::anyhow!("decision `{requested_id}` was not found")),
    }
}

pub(super) async fn resolve_show_target(
    state_db: &StateRuntime,
    requested_id: &str,
) -> anyhow::Result<ShowTarget> {
    resolve_record(state_db, requested_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("decision or crossroad `{requested_id}` was not found"))
}

async fn resolve_record(
    state_db: &StateRuntime,
    requested_id: &str,
) -> anyhow::Result<Option<ShowTarget>> {
    let exact_crossroad = state_db.get_crossroad(requested_id).await?;
    let exact_decision = state_db.get_decision(requested_id).await?;
    match (exact_crossroad, exact_decision) {
        (Some(crossroad), None) => return Ok(Some(ShowTarget::Crossroad(crossroad))),
        (None, Some(decision)) => return Ok(Some(ShowTarget::Decision(decision))),
        (Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "record id `{requested_id}` is ambiguous between a decision and crossroad"
            ));
        }
        (None, None) => {}
    }

    let crossroads = state_db
        .crossroads_with_id_prefix(requested_id, PREFIX_MATCH_LIMIT)
        .await?
        .into_iter()
        .filter(|crossroad| crossroad.id.starts_with(requested_id))
        .map(ShowTarget::Crossroad)
        .collect::<Vec<_>>();
    let decisions = state_db
        .decisions_with_id_prefix(requested_id, PREFIX_MATCH_LIMIT)
        .await?
        .into_iter()
        .filter(|decision| decision.id.starts_with(requested_id))
        .map(ShowTarget::Decision)
        .collect::<Vec<_>>();

    let mut matches = crossroads;
    matches.extend(decisions);
    match matches.as_slice() {
        [] => Ok(None),
        [record] => Ok(Some(record.clone())),
        _ => Err(ambiguous_id_error(requested_id, &matches)),
    }
}

fn ambiguous_id_error(requested_id: &str, matches: &[ShowTarget]) -> anyhow::Error {
    let mut candidates = matches
        .iter()
        .map(|record| match record {
            ShowTarget::Crossroad(crossroad) => format!("crossroad:{}", crossroad.id),
            ShowTarget::Decision(decision) => format!("decision:{}", decision.id),
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.truncate(MAX_DISPLAYED_CANDIDATES);
    anyhow::anyhow!(
        "record id `{requested_id}` is ambiguous; use a longer prefix (candidates: {})",
        candidates.join(", ")
    )
}
