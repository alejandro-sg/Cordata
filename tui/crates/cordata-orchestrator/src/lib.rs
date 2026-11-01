//! The orchestrator, ported from `backend/orchestrator/` (migration plan, Phase 3): the run state machine and everything it
//! drives.
//!
//! Ported so far — the pure, deterministic core that the state machine is built on:
//!
//! * [`scope`]: which paths a step may change (`workers/scope.py`)
//! * [`plan`]: the plan artifact, its validation and rendering (`plan_artifact.py`)
//! * [`bandit`] + [`dispatch`]: the learned dispatch policy (`bandit.py` table, `dispatch.py`)
//! * [`synthesizer`]: copy-on-write step sandboxes, harvest, and the SYNTHESIZE consistency check (`synthesizer.py`)
//! * [`research`]: the evidence-checked research brief (`research.py`)
//! * [`secrets`] + [`handoff`]: secret masking and the handoff packet (`secret_patterns.py`, `handoff.py`)
//! * [`worker`] + [`checkpoints`]: the contracts the executor drives (`workers/base.py`, `checkpoints.py`)
//! * [`workers`]: the Claude Code and `agy` subprocess workers (`workers/*.py`)
//! * [`executor`]: the EXECUTE phase — checkpoints, quota-aware worker choice, handoffs, waves (`executor.py`)
//!
//! Not ported: the Ollama worker (an in-process model loop, not a subprocess), per-step reward attribution (`rlaif.py`), the
//! Auto Mode router and Laya Guard (a pattern guard stands in). Each module has a parity test against the Python
//! implementation it came from.

pub mod audit;
pub mod bandit;
pub mod checkpoints;
pub mod dispatch;
pub mod executor;
pub mod handoff;
pub mod machine;
pub mod plan;
pub mod research;
pub mod scope;
pub mod secrets;
pub mod synthesizer;
pub mod testkit;
pub mod worker;
pub mod workers;
