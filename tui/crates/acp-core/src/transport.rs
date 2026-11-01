//! How the client reaches an agent. There is one way: the agent runs inside this process (`crate::native`) — no child
//! process, no socket.

/// Where to find the agent. Chosen once per session, at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendTransport {
    /// Run the agent inside this process (`crate::native`).
    Native,
}
