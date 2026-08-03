//! GitHub Copilot — premium-request quota read from `copilot_internal/user`
//! using a GitHub OAuth token `ai-usagebar login copilot` obtained itself
//! (device-code flow, see `device_flow.rs`). Unlike every other local-file
//! vendor here, there is no existing CLI/IDE credential to read: GitHub gates
//! `copilot_internal/*` by which OAuth App issued the token, and neither the
//! `gh` CLI's token nor a personal access token qualifies — confirmed live.
//! See `creds.rs` for storage and `fetch.rs`/`types.rs` for the wire call.

pub mod creds;
pub mod device_flow;
pub mod fetch;
pub mod types;
pub mod vendor;

pub use fetch::{FetchOutcome, fetch_snapshot};
