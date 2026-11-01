//! The two deterministic checks the repo runs itself: refuse a credential in git (`secrets`) and refuse a tool call
//! that would write one or wreck the checkout (`guard`, the pre-tool hook).

pub mod guard;
pub mod secrets;
