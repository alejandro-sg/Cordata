//! The handoff packet: what one worker leaves for the next, in a form
//! any worker can resume from. Provider sessions cannot continue each other, so a handoff carries *state* — the step,
//! how far it got, the checkpoint, the diff so far, what failed — typed, bounded and secret-scrubbed when built,
//! fenced as untrusted data when rendered into the next worker's prompt. Persisted in `state.db` (`handoff_packets`).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use cordata_ledger::Ledger;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::secrets;

pub const REASONS: [&str; 7] = ["quota_soft_limit_reached", "quota_hard_limit_reached", "usage_low", "provider_rate_limit", "worker_failed", "worker_unavailable", "user_pause"];
pub const MAX_PATCH_CHARS: usize = 30_000;
pub const MAX_LIST: usize = 40;
pub const MAX_ITEM_CHARS: usize = 600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffPacket {
    pub handoff_id: String,
    pub session_id: String,
    pub timestamp: f64,
    pub reason: String,
    pub plan_id: String,
    pub step_index: i64,
    pub step_id: String,
    /// `pending` (nothing done yet) | `partial` (work kept) | `failed` (rolled back)
    pub step_status: String,
    pub checkpoint_hash: Option<String>,
    pub from_worker: String,
    #[serde(default)]
    pub to_worker: Option<String>,
    #[serde(default)]
    pub diff_stat: String,
    #[serde(default)]
    pub diff_patch: String,
    #[serde(default)]
    pub files_touched: Vec<String>,
    #[serde(default)]
    pub verifier_results: BTreeMap<String, String>,
    #[serde(default)]
    pub failed_attempts: Vec<String>,
    #[serde(default)]
    pub open_assumptions: Vec<String>,
    #[serde(default)]
    pub completed_steps: Vec<String>,
}

impl HandoffPacket {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("a packet serializes")
    }

    pub fn from_json(data: &Value) -> Option<Self> {
        serde_json::from_value(data.clone()).ok()
    }

    /// What goes into the next worker's `TaskPacket.handoff_context`: the packet minus ledger bookkeeping.
    pub fn context(&self) -> Value {
        let mut data = self.to_json();
        for key in ["handoff_id", "session_id", "timestamp", "to_worker"] {
            data.as_object_mut().expect("object").remove(key);
        }
        data
    }
}

/// The inputs to [`build`], with the Python defaults.
#[derive(Debug, Clone, Default)]
pub struct Build {
    pub session_id: String,
    pub reason: String,
    pub plan_id: String,
    pub step_index: i64,
    pub step_id: String,
    pub step_status: String,
    pub checkpoint_hash: Option<String>,
    pub from_worker: String,
    pub to_worker: Option<String>,
    pub diff_stat: String,
    pub diff_patch: String,
    pub files_touched: Vec<String>,
    pub verifier_results: BTreeMap<String, String>,
    pub failed_attempts: Vec<String>,
    pub open_assumptions: Vec<String>,
    pub completed_steps: Vec<String>,
}

fn clip(items: &[String], limit: usize) -> Vec<String> {
    items.iter().take(limit).map(|item| secrets::scrub(item).0.chars().take(MAX_ITEM_CHARS).collect()).collect()
}

fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    format!("handoff-{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

#[derive(Debug, thiserror::Error)]
#[error("unknown handoff reason {0:?}")]
pub struct UnknownReason(pub String);

/// A packet with every free-text field secret-scrubbed and bounded. `reason` must be one of [`REASONS`].
pub fn build(b: Build) -> Result<HandoffPacket, UnknownReason> {
    if !REASONS.contains(&b.reason.as_str()) {
        return Err(UnknownReason(b.reason));
    }
    let mut patch = secrets::scrub(&b.diff_patch).0;
    if patch.chars().count() > MAX_PATCH_CHARS {
        patch = patch.chars().take(MAX_PATCH_CHARS).collect::<String>() + "\n… (patch truncated)\n";
    }
    let stat: String = secrets::scrub(&b.diff_stat).0.chars().take(2000).collect();
    Ok(HandoffPacket {
        handoff_id: new_id(),
        session_id: b.session_id,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()),
        reason: b.reason,
        plan_id: b.plan_id,
        step_index: b.step_index,
        step_id: b.step_id,
        step_status: b.step_status,
        checkpoint_hash: b.checkpoint_hash,
        from_worker: b.from_worker,
        to_worker: b.to_worker,
        diff_stat: stat,
        diff_patch: patch,
        files_touched: clip(&b.files_touched, 200),
        verifier_results: b.verifier_results.iter().take(MAX_LIST).map(|(k, v)| (k.chars().take(120).collect(), clip(std::slice::from_ref(v), 1).remove(0))).collect(),
        failed_attempts: clip(&b.failed_attempts, MAX_LIST),
        open_assumptions: clip(&b.open_assumptions, MAX_LIST),
        completed_steps: clip(&b.completed_steps, 100),
    })
}

/// Persists the packet and a `handoff` event. Returns whether the packet row was written.
pub fn save(ledger: &Ledger, packet: &HandoffPacket) -> bool {
    if ledger.save_handoff(&packet.to_json()).is_err() {
        return false;
    }
    let json = packet.to_json();
    let summary: serde_json::Map<String, Value> = ["handoff_id", "reason", "plan_id", "step_id", "step_status", "from_worker", "to_worker", "diff_stat"]
        .iter()
        .map(|k| ((*k).to_owned(), json[*k].clone()))
        .collect();
    let _ = ledger.append_event("handoff", &Value::Object(summary), Some(&packet.session_id), Some("orchestrator"));
    true
}

pub fn load(ledger: &Ledger, handoff_id: &str) -> Option<HandoffPacket> {
    HandoffPacket::from_json(&ledger.load_handoff(handoff_id)?)
}

pub fn for_session(ledger: &Ledger, session_id: &str) -> Vec<HandoffPacket> {
    ledger.handoffs_for_session(session_id).iter().filter_map(HandoffPacket::from_json).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Build {
        Build { session_id: "run-p".into(), reason: "provider_rate_limit".into(), plan_id: "plan-1".into(), step_index: 1, step_id: "s2".into(), step_status: "partial".into(), from_worker: "claude:sonnet".into(), ..Default::default() }
    }

    #[test]
    fn scrubs_bounds_and_persists() {
        let secret = format!("sk-ant-api03-{}", "Z".repeat(40));
        let packet = build(Build { diff_patch: format!("+KEY={secret}\n{}", "x".repeat(50_000)), failed_attempts: vec![format!("leaked {secret}")], ..base() }).unwrap();
        assert!(!packet.to_json().to_string().contains(&secret), "every free-text field is scrubbed");
        assert!(packet.diff_patch.chars().count() < 31_000 && packet.diff_patch.contains("truncated"));
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("s.db"), vec![]).unwrap();
        assert!(save(&ledger, &packet));
        assert_eq!(load(&ledger, &packet.handoff_id).unwrap(), packet);
        assert_eq!(for_session(&ledger, "run-p"), vec![packet.clone()]);
        assert_eq!(ledger.recent_events("handoff", 5)[0]["handoff_id"], packet.handoff_id.as_str());
    }

    #[test]
    fn unknown_reasons_are_refused_and_context_drops_bookkeeping() {
        assert!(build(Build { reason: "because".into(), ..base() }).is_err());
        let ctx = build(base()).unwrap().context();
        for gone in ["handoff_id", "session_id", "timestamp", "to_worker"] {
            assert!(ctx.get(gone).is_none(), "{gone}");
        }
        assert_eq!(ctx["reason"], "provider_rate_limit");
    }

    #[test]
    fn lists_are_clipped() {
        let many: Vec<String> = (0..100).map(|i| format!("item {i} {}", "y".repeat(900))).collect();
        let p = build(Build { failed_attempts: many.clone(), files_touched: many, ..base() }).unwrap();
        assert_eq!(p.failed_attempts.len(), MAX_LIST);
        assert_eq!(p.files_touched.len(), 100);
        assert!(p.failed_attempts[0].chars().count() <= MAX_ITEM_CHARS);
    }
}
