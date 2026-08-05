//! SuperGrok (xAI subscription OAuth) — weekly credit usage from the same
//! unofficial CLI billing surface the Grok Build CLI and community tools use:
//! `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`.
//!
//! Credentials come from `~/.grok/auth.json` after `grok login` (OIDC against
//! `auth.x.ai`). Access tokens last a few hours; we refresh via the documented
//! OIDC token endpoint and write rotated tokens **back** to that file (same
//! pattern as OpenAI Codex's `~/.codex/auth.json`), so the Grok CLI keeps a
//! live refresh token.
//!
//! Distinct from the `grok` vendor, which reads **prepaid Management API**
//! balance with a management key — SuperGrok is the subscription quota path.

pub mod creds;
pub mod fetch;
pub mod oauth;
pub mod types;
pub mod vendor;

pub use fetch::{FetchOutcome, fetch_snapshot};
