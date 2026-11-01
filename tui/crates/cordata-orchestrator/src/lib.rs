//! The orchestrator, ported piece by piece from `backend/orchestrator/` (migration plan, Phase 3).
//!
//! Ported so far — the pure, deterministic core that the state machine is built on:
//!
//! * [`scope`]: which paths a step may change (`workers/scope.py`)
//! * [`plan`]: the plan artifact, its validation and rendering (`plan_artifact.py`)
//! * [`bandit`] + [`dispatch`]: the learned dispatch policy (`bandit.py` table, `dispatch.py`)
//!
//! Not yet ported: the state machine (`machine.py`), the step executor, the worker subprocess wrappers and the audit.
//! Each module has a parity test against the Python implementation, which stays the source of truth until the
//! state machine itself moves.

pub mod bandit;
pub mod dispatch;
pub mod plan;
pub mod scope;
