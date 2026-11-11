//! The orchestrator: the run state machine and everything it drives.
//!
//! * [`scope`]: which paths a step may change
//! * [`plan`]: the plan artifact, its validation and rendering
//! * [`bandit`] + [`dispatch`]: the learned dispatch policy
//! * [`synthesizer`]: copy-on-write step sandboxes, harvest, and the SYNTHESIZE consistency check
//! * [`research`]: the evidence-checked research brief
//! * [`secrets`] + [`handoff`]: secret masking and the handoff packet
//! * [`worker`] + [`checkpoints`]: the contracts the executor drives
//! * [`workers`]: the Claude Code and `agy` subprocess workers
//! * [`executor`]: the EXECUTE phase: checkpoints, quota-aware worker choice, handoffs, waves

pub mod audit;
pub mod bandit;
pub mod checkpoints;
pub mod dispatch;
pub mod evolution;
pub mod executor;
pub mod handoff;
pub mod knowledge;
pub mod machine;
pub mod plan;
pub mod recursion;
pub mod research;
pub mod scope;
pub mod secrets;
pub mod synthesizer;
pub mod tape;
pub mod taste;
pub mod testkit;
pub mod worker;
pub mod workers;
