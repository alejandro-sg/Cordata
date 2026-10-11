//! The hand-off between the network tasks and the UI thread. Nothing here can block either side.
//!
//! Producers (tokio tasks reading gRPC streams) call `push_*`: a short mutex hold, a map insert
//! or a deque push, never an `.await` — so a stalled UI cannot back-pressure the network task
//! into missing a heartbeat. The consumer (the UI thread) calls [`Inbox::drain`], which takes
//! everything in one go.
//!
//! Telemetry is *state*, so it is conflated here as it is on the daemon: one pending frame per
//! session, and a newer one replaces the older (carrying forward the daemon-wide sections the
//! newer lacks, and the one-off memory notice if there was one). A burst of 1,000 updates while the
//! UI is busy costs one map slot, not 1,000 queue entries — that is the "no channel buffer
//! overflow" property, by construction rather than by sizing a channel.
//!
//! Discrete events are a bounded queue. Overflow drops the oldest *non-critical* event;
//! connection changes and anything about an approval are small and rare, and never dropped.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use harness_proto::v1::TelemetryFrame;
use tokio::sync::Notify;

use crate::model::ClientEvent;

const MAX_EVENTS: usize = 1024;

#[derive(Default)]
struct State {
    frames: HashMap<String, TelemetryFrame>,
    events: VecDeque<ClientEvent>,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    wake: Notify,
    coalesced: AtomicU64,
    discarded: AtomicU64,
    pushed: AtomicU64,
}

#[derive(Clone, Default)]
pub struct Inbox {
    inner: Arc<Inner>,
}

/// `newer` replaces `older`; whatever `newer` does not carry but `older` did is carried forward.
fn merge(older: TelemetryFrame, mut newer: TelemetryFrame, spill: &mut Vec<TelemetryFrame>) -> TelemetryFrame {
    newer.dropped += older.dropped;
    if newer.omlx.is_none() {
        newer.omlx = older.omlx;
    }
    if newer.daemon.is_none() {
        newer.daemon = older.daemon;
    }
    if newer.diffs.is_none() {
        newer.diffs = older.diffs;
    }
    if newer.pending.is_none() {
        newer.pending = older.pending;
    }
    if let Some(event) = older.memory_event {
        if newer.memory_event.is_some() {
            // A one-off notice cannot be folded into another one: keep it as its own frame.
            spill.push(TelemetryFrame { seq: older.seq, memory_event: Some(event), ..Default::default() });
        } else {
            newer.memory_event = Some(event);
        }
    }
    newer
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_telemetry(&self, frame: TelemetryFrame) {
        let mut spill = Vec::new();
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            let key = frame.session_id.clone();
            let merged = match state.frames.remove(&key) {
                Some(older) => {
                    self.inner.coalesced.fetch_add(1, Ordering::Relaxed);
                    merge(older, frame, &mut spill)
                }
                None => frame,
            };
            state.frames.insert(key, merged);
            for extra in spill {
                state.events.push_back(ClientEvent::Telemetry(Box::new(extra)));
            }
            // A spilled notice is an event like any other: it obeys the same bound, or a UI that stalls while
            // memory notices keep arriving grows this queue without limit.
            self.trim(&mut state);
        }
        self.inner.pushed.fetch_add(1, Ordering::Relaxed);
        self.inner.wake.notify_one();
    }

    pub fn push_event(&self, event: ClientEvent) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.events.push_back(event);
            self.trim(&mut state);
        }
        self.inner.pushed.fetch_add(1, Ordering::Relaxed);
        self.inner.wake.notify_one();
    }

    /// Hold the discrete queue to `MAX_EVENTS` by dropping the oldest non-critical events.
    fn trim(&self, state: &mut State) {
        while state.events.len() > MAX_EVENTS {
            match state.events.iter().position(|e| !e.is_critical()) {
                Some(index) => {
                    state.events.remove(index);
                    self.inner.discarded.fetch_add(1, Ordering::Relaxed);
                }
                None => break, // all critical: keep them all; they are small and rare
            }
        }
    }

    /// Forget telemetry from a connection that is gone. Called on disconnect, so a frame from the
    /// old connection can never be applied after the new connection's first one.
    pub fn clear_telemetry(&self) {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner()).frames.clear();
    }

    /// Everything pending, discrete events first (in order), then the newest frame per session.
    pub fn drain(&self) -> Vec<ClientEvent> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<ClientEvent> = state.events.drain(..).collect();
        let mut frames: Vec<TelemetryFrame> = state.frames.drain().map(|(_, frame)| frame).collect();
        frames.sort_by_key(|frame| frame.seq);
        out.extend(frames.into_iter().map(|frame| ClientEvent::Telemetry(Box::new(frame))));
        out
    }

    /// Resolves when something has been pushed since the last drain. Runtime-agnostic: it is a
    /// plain waker-based future, so a GPUI foreground task can await it.
    pub async fn notified(&self) {
        self.inner.wake.notified().await;
    }

    pub fn len(&self) -> usize {
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.events.len() + state.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many telemetry frames were replaced by a newer one before the UI looked.
    pub fn coalesced(&self) -> u64 {
        self.inner.coalesced.load(Ordering::Relaxed)
    }

    pub fn discarded(&self) -> u64 {
        self.inner.discarded.load(Ordering::Relaxed)
    }

    pub fn pushed(&self) -> u64 {
        self.inner.pushed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_proto::v1::{DaemonState, LabState, MemoryEvent, OmlxState};

    fn frame(seq: u64, session: &str, lab_seq: u64) -> TelemetryFrame {
        TelemetryFrame { seq, session_id: session.into(), lab: Some(LabState { lab_seq, ..Default::default() }), ..Default::default() }
    }

    #[test]
    fn a_burst_costs_one_slot_not_a_thousand() {
        let inbox = Inbox::new();
        for n in 1..=1000 {
            inbox.push_telemetry(frame(n, "s1", n));
            assert!(inbox.len() <= 1, "the inbox grew to {} at frame {n}", inbox.len());
        }
        assert_eq!(inbox.coalesced(), 999);
        let drained = inbox.drain();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            ClientEvent::Telemetry(f) => assert_eq!(f.lab.as_ref().unwrap().lab_seq, 1000),
            other => panic!("{other:?}"),
        }
        assert!(inbox.is_empty());
    }

    #[test]
    fn sessions_are_conflated_separately() {
        let inbox = Inbox::new();
        inbox.push_telemetry(frame(1, "a", 1));
        inbox.push_telemetry(frame(2, "b", 1));
        inbox.push_telemetry(frame(3, "a", 2));
        let drained = inbox.drain();
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn a_newer_frame_keeps_the_sections_the_older_one_carried() {
        let inbox = Inbox::new();
        let mut first = frame(1, "s", 1);
        first.omlx = Some(OmlxState { up: true, ..Default::default() });
        first.daemon = Some(DaemonState { sessions: 3, ..Default::default() });
        first.dropped = 4;
        inbox.push_telemetry(first);
        inbox.push_telemetry(frame(2, "s", 2));
        let ClientEvent::Telemetry(merged) = inbox.drain().remove(0) else { panic!() };
        assert!(merged.omlx.is_some_and(|o| o.up) && merged.daemon.is_some_and(|d| d.sessions == 3), "sections were lost in the merge");
        assert_eq!(merged.dropped, 4, "the daemon's own drop count survives the merge");
    }

    #[test]
    fn two_memory_notices_both_survive() {
        let inbox = Inbox::new();
        let mut a = frame(1, "s", 1);
        a.memory_event = Some(MemoryEvent { event: "lease_load".into(), ..Default::default() });
        let mut b = frame(2, "s", 2);
        b.memory_event = Some(MemoryEvent { event: "lease_unload".into(), ..Default::default() });
        inbox.push_telemetry(a);
        inbox.push_telemetry(b);
        let events: Vec<_> = inbox
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                ClientEvent::Telemetry(f) => f.memory_event.map(|m| m.event),
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 2, "{events:?}");
    }

    #[test]
    fn spilled_memory_notices_obey_the_bound() {
        // Two notices per frame cannot be folded into one, so every merge spills one into the event queue. A UI that
        // never drains must still see a bounded queue.
        let inbox = Inbox::new();
        for n in 0..(MAX_EVENTS as u64 * 3) {
            let frame = TelemetryFrame {
                seq: n,
                session_id: "s".into(),
                memory_event: Some(harness_proto::v1::MemoryEvent { event: format!("lease_load {n}"), ..Default::default() }),
                ..Default::default()
            };
            inbox.push_telemetry(frame);
        }
        assert!(inbox.len() <= MAX_EVENTS + 1, "the queue grew to {}", inbox.len());
        assert!(inbox.discarded() > 0, "nothing was discarded");
    }

    #[test]
    fn overflow_drops_ordinary_events_but_never_approvals() {
        use crate::model::{ConnectionState, Reply};
        use harness_proto::v1::{InterventionEvent, InterventionRequest, intervention_event};
        let inbox = Inbox::new();
        inbox.push_event(ClientEvent::Intervention(InterventionEvent { event: Some(intervention_event::Event::Request(InterventionRequest { intervention_id: "keep".into(), ..Default::default() })) }));
        inbox.push_event(ClientEvent::Connection(ConnectionState::Connecting));
        for n in 0..(MAX_EVENTS * 2) {
            inbox.push_event(ClientEvent::Reply(Reply::Diff(harness_proto::v1::DiffArtifact { unified: n.to_string(), ..Default::default() })));
        }
        let drained = inbox.drain();
        assert!(drained.len() <= MAX_EVENTS, "{}", drained.len());
        assert!(drained.iter().any(|e| matches!(e, ClientEvent::Intervention(_))), "an approval request was dropped");
        assert!(drained.iter().any(|e| matches!(e, ClientEvent::Connection(_))), "a connection change was dropped");
        assert!(inbox.discarded() > 0);
    }

    #[test]
    fn clearing_forgets_the_old_connection() {
        let inbox = Inbox::new();
        inbox.push_telemetry(frame(5, "s", 5));
        inbox.clear_telemetry();
        assert!(inbox.drain().is_empty());
    }
}
