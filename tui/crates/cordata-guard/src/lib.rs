//! The two deterministic checks the repo runs itself: refuse a credential in git (`secrets`) and refuse a tool call
//! that would write one or wreck the checkout (`guard`, the pre-tool hook); `redact` masks credential-shaped text.

pub mod guard;
pub mod home;
pub mod redact;
pub mod secrets;
