//! gRPC client for the daemon's telemetry bridge.
//!
//! * [`client`] — connects, streams, reconnects with backoff, degrades to offline; talks to the
//!   UI only through the [`inbox`] and `try_send`, so it can never block it.
//! * [`model`] — the plain, synchronous state the UI renders ([`LabModel`]).
//! * [`inbox`] — the latest-wins hand-off between the two.
//! * [`redact`] — masks credential-shaped text before it is drawn.
//! * [`config`] — finds the daemon and its token.
//! * [`words`] — telemetry in plain language: "Reading supervisor.py", "Needs you".
//! * [`testkit`] — a scripted fake daemon, for this crate's tests and the app's.

pub mod client;
pub mod config;
pub mod inbox;
pub mod model;
pub mod redact;
pub mod testkit;
pub mod words;

pub use client::{ClientConfig, Command, ConfigSource, Handle, Knob, PodControl, SendError, spawn};
pub use config::{ConfigError, DaemonConfig};
pub use inbox::Inbox;
pub use model::{ClientEvent, ConnectionState, FeedItem, LabModel, MemoryHitNotice, Notice, OpenIntervention, Reply};
pub use words::Tone;
