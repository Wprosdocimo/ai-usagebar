//! SuperGrok (xAI subscription OAuth) usage through the official Grok Build
//! CLI's billing surface.
//!
//! Two transports, same credential-free billing response:
//! - `direct` — one HTTPS call to the documented `cli-chat-proxy.grok.com`
//!   billing endpoint using the login's long-lived `key` (needed since grok
//!   CLI 1.0.13 dropped the ACP extension; primary path).
//! - `acp` — the CLI's `x.ai/billing` ACP extension (fallback for CLI builds
//!   where the proxy endpoint is unreachable or reshaped).
//!
//! ai-usagebar never copies, caches, refreshes, or writes Grok tokens. The
//! `key` exists only inside one outgoing Authorization header; login files
//! are read solely as bytes (key lookup or one-way cache-scope digest) and
//! account selection, OIDC rotation, and `auth.json.lock` stay with Grok
//! Build.
//!
//! Distinct from the `grok` vendor, which reads **prepaid Management API**
//! balance with a management key — SuperGrok is the subscription quota path.

pub mod acp;
pub mod direct;
pub mod fetch;
pub mod scope;
pub mod types;
pub mod vendor;

pub use fetch::{FetchOutcome, fetch_snapshot};
