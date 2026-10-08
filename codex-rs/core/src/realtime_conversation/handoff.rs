use super::realtime_delegation_with_routing_input;
use crate::context::RealtimeDelegationSource;
use crate::realtime_classifier::RealtimeHandoffRoutingDecision;
use crate::realtime_classifier::classify_realtime_handoff;
use crate::session::session::Session;
use async_channel::Sender;
use codex_api::RealtimeEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RealtimeConversationRealtimeEvent;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tracing::debug;

pub(super) const REALTIME_HANDOFF_DEDUPE_CAPACITY: usize = 1_024;
pub(super) const REALTIME_HANDOFF_CLASSIFIER_CONCURRENCY: usize = 4;
pub(super) const REALTIME_FANOUT_REORDER_WINDOW: u64 = 64;

#[derive(Debug, Default)]
pub(super) struct RealtimeHandoffDeduper {
    handoff_ids: HashSet<String>,
    handoff_order: VecDeque<String>,
}

impl RealtimeHandoffDeduper {
    pub(super) fn is_duplicate(&mut self, handoff_id: &str) -> bool {
        if handoff_id.is_empty() {
            return false;
        }

        if self.handoff_ids.contains(handoff_id) {
            return true;
        }

        let handoff_id = handoff_id.to_string();
        self.handoff_ids.insert(handoff_id.clone());
        self.handoff_order.push_back(handoff_id);
        if self.handoff_order.len() > REALTIME_HANDOFF_DEDUPE_CAPACITY
            && let Some(evicted_id) = self.handoff_order.pop_front()
        {
            self.handoff_ids.remove(&evicted_id);
        }
        false
    }
}

pub(super) struct PendingRealtimeHandoff {
    pub(super) sequence: u64,
    pub(super) event: RealtimeEvent,
    pub(super) text: String,
    pub(super) routing_input: String,
    pub(super) routing_decision: RealtimeHandoffRoutingDecision,
}

pub(super) enum ReadyRealtimeEvent {
    Forward(RealtimeEvent),
    Handoff {
        event: RealtimeEvent,
        text: String,
        routing_input: Option<String>,
        routing_decision: Option<RealtimeHandoffRoutingDecision>,
    },
}

pub(super) enum RealtimeFanoutHandling {
    Pending,
    Ready {
        sequence: u64,
        event: ReadyRealtimeEvent,
    },
}

pub(super) async fn handle_realtime_fanout_event(
    session: &Arc<Session>,
    event: RealtimeEvent,
    route_handoff_deduper: &mut RealtimeHandoffDeduper,
    pending_handoff_tx: &Sender<PendingRealtimeHandoff>,
    next_sequence: &mut u64,
) -> RealtimeFanoutHandling {
    let maybe_routed_text = match &event {
        RealtimeEvent::HandoffRequested(handoff) => {
            let routed_text = realtime_delegation_with_routing_input(handoff);
            if routed_text.is_some() && route_handoff_deduper.is_duplicate(&handoff.handoff_id) {
                debug!(
                    handoff_id = %handoff.handoff_id,
                    "ignoring duplicate realtime handoff"
                );
                let sequence = *next_sequence;
                *next_sequence += 1;
                return RealtimeFanoutHandling::Ready {
                    sequence,
                    event: ReadyRealtimeEvent::Forward(event),
                };
            }
            routed_text
        }
        _ => None,
    };

    let sequence = *next_sequence;
    *next_sequence += 1;
    match maybe_routed_text {
        Some((text, Some(routing_input))) => {
            let handoff_id = match &event {
                RealtimeEvent::HandoffRequested(handoff) => handoff.handoff_id.clone(),
                _ => unreachable!("routing input only comes from a handoff event"),
            };
            let session = Arc::clone(session);
            let pending_handoff_tx = pending_handoff_tx.clone();
            tokio::spawn(async move {
                let routing_decision =
                    classify_realtime_handoff(&session, &routing_input, &handoff_id).await;
                let _ = pending_handoff_tx
                    .send(PendingRealtimeHandoff {
                        sequence,
                        event,
                        text,
                        routing_input,
                        routing_decision,
                    })
                    .await;
            });
            RealtimeFanoutHandling::Pending
        }
        Some((text, None)) => RealtimeFanoutHandling::Ready {
            sequence,
            event: ReadyRealtimeEvent::Handoff {
                event,
                text,
                routing_input: None,
                routing_decision: None,
            },
        },
        None => RealtimeFanoutHandling::Ready {
            sequence,
            event: ReadyRealtimeEvent::Forward(event),
        },
    }
}

pub(super) async fn finish_ready_realtime_event(
    session: &Arc<Session>,
    submission_id: &str,
    route_handoffs: &Arc<RealtimeHandoffAdmission>,
    event: ReadyRealtimeEvent,
) -> Result<(), &'static str> {
    match event {
        ReadyRealtimeEvent::Forward(event) => {
            send_realtime_fanout_event(session, submission_id, event).await;
            Ok(())
        }
        ReadyRealtimeEvent::Handoff {
            mut event,
            text,
            routing_input,
            routing_decision,
        } => {
            if let (RealtimeEvent::HandoffRequested(handoff), Some(decision)) =
                (&mut event, routing_decision.as_ref())
            {
                handoff.routing = Some(decision.routing.clone());
            }
            send_realtime_fanout_event(session, submission_id, event).await;
            debug!("[realtime-text] realtime conversation text output");
            route_handoffs
                .route(
                    session,
                    text,
                    RealtimeDelegationSource::Handoff,
                    routing_input,
                    routing_decision,
                )
                .await
        }
    }
}

pub(super) async fn finish_ready_realtime_events(
    session: &Arc<Session>,
    submission_id: &str,
    route_handoffs: &Arc<RealtimeHandoffAdmission>,
    ready_events: &mut std::collections::BTreeMap<u64, ReadyRealtimeEvent>,
    next_sequence: &mut u64,
) -> Result<(), &'static str> {
    while let Some(event) = ready_events.remove(next_sequence) {
        *next_sequence += 1;
        finish_ready_realtime_event(session, submission_id, route_handoffs, event).await?;
    }
    Ok(())
}

async fn send_realtime_fanout_event(session: &Session, submission_id: &str, event: RealtimeEvent) {
    session
        .send_event_raw(Event {
            id: submission_id.to_string(),
            msg: EventMsg::RealtimeConversationRealtime(RealtimeConversationRealtimeEvent {
                payload: event,
            }),
        })
        .await;
}

/// Serializes turn admission with the safety cutoff for one voice session.
#[derive(Debug)]
pub(crate) struct RealtimeHandoffAdmission {
    gate: Semaphore,
    pub(super) retired: AtomicBool,
    pub(super) shutting_down: AtomicBool,
    transcript_tail_admitted: AtomicBool,
}

/// Tracks the realtime sessions whose handoffs have been admitted to one turn.
#[derive(Debug, Default)]
pub(crate) struct RealtimeHandoffAdmissions {
    state: Mutex<RealtimeHandoffAdmissionsState>,
}

#[derive(Debug, Default)]
struct RealtimeHandoffAdmissionsState {
    admissions: Vec<Arc<RealtimeHandoffAdmission>>,
    retired: bool,
}

impl RealtimeHandoffAdmissions {
    pub(crate) async fn register(&self, admission: Arc<RealtimeHandoffAdmission>) -> bool {
        let retire_immediately = {
            let mut state = self.state.lock().await;
            if state.retired {
                true
            } else {
                if !state
                    .admissions
                    .iter()
                    .any(|existing| Arc::ptr_eq(existing, &admission))
                {
                    state.admissions.push(Arc::clone(&admission));
                }
                false
            }
        };

        if retire_immediately {
            admission.close_admission();
            false
        } else {
            true
        }
    }

    pub(crate) async fn retire_all(&self) {
        let admissions = {
            let mut state = self.state.lock().await;
            state.retired = true;
            state.admissions.clone()
        };

        for admission in admissions {
            admission.retire().await;
        }
    }
}

impl RealtimeHandoffAdmission {
    pub(crate) fn new() -> Self {
        Self {
            gate: Semaphore::new(1),
            retired: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            transcript_tail_admitted: AtomicBool::new(false),
        }
    }

    pub(super) async fn route(
        self: &Arc<Self>,
        session: &Arc<Session>,
        text: String,
        source: RealtimeDelegationSource,
        routing_input: Option<String>,
        routing_decision: Option<RealtimeHandoffRoutingDecision>,
    ) -> Result<(), &'static str> {
        let Some(_permit) = self.acquire_route_permit(source).await else {
            return Ok(());
        };
        if !session
            .register_realtime_handoff_admission_for_active_turn(Arc::clone(self))
            .await
        {
            return Err("realtime handoff was rejected after its turn admission closed");
        }
        session
            .route_realtime_text_input(
                text,
                source,
                routing_input,
                routing_decision,
                Arc::clone(self),
            )
            .await
            .map_err(|_| "realtime handoff was rejected")?;
        Ok(())
    }

    pub(super) async fn acquire_route_permit(
        &self,
        source: RealtimeDelegationSource,
    ) -> Option<SemaphorePermit<'_>> {
        let permit = self.gate.acquire().await.ok()?;
        if self.retired.load(Ordering::Acquire)
            || (self.shutting_down.load(Ordering::Acquire)
                && source != RealtimeDelegationSource::TranscriptTailFlush)
        {
            return None;
        }
        if source == RealtimeDelegationSource::TranscriptTailFlush
            && self.transcript_tail_admitted.swap(true, Ordering::AcqRel)
        {
            return None;
        }
        Some(permit)
    }

    pub(super) async fn begin_shutdown(&self, allow_transcript_tail: bool) {
        self.shutting_down.store(true, Ordering::Release);
        if !allow_transcript_tail {
            self.retired.store(true, Ordering::Release);
        }
        let Ok(_permit) = self.gate.acquire().await else {
            return;
        };
    }

    pub(super) async fn retire(&self) {
        self.close_admission();
        let Ok(_permit) = self.gate.acquire().await else {
            return;
        };
    }

    fn close_admission(&self) {
        self.retired.store(true, Ordering::Release);
    }
}
