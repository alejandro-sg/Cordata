//! The orchestrator, ported piece by piece from `backend/orchestrator/` (migration plan, Phase 3).
//!
//! Ported so far — the pure, deterministic core that the state machine is built on:
//!
//! * [`scope`]: which paths a step may change (`workers/scope.py`)
//! * [`plan`]: the plan artifact, its validation and rendering (`plan_artifact.py`)
//! * [`bandit`] + [`dispatch`]: the learned dispatch policy (`bandit.py` table, `dispatch.py`)
//! * [`secrets`] + [`handoff`]: secret masking and the handoff packet (`secret_patterns.py`, `handoff.py`)
//! * [`worker`] + [`checkpoints`]: the contracts the executor drives (`workers/base.py`, `checkpoints.py`)
//! * [`executor`]: the EXECUTE phase — checkpoints, quota-aware worker choice, handoffs, waves (`executor.py`)
//!
//! Not yet ported: the state machine (`machine.py`), the worker subprocess wrappers, the shadow-git checkpoint store,
//! the copy-on-write sandboxes and the audit.
//! Each module has a parity test against the Python implementation, which stays the source of truth until the
//! state machine itself moves.

pub mod bandit;
pub mod checkpoints;
pub mod dispatch;
pub mod executor;
pub mod handoff;
pub mod plan;
pub mod scope;
pub mod secrets;
pub mod testkit;
pub mod worker;
