//! Which Claude account the `claude` **CLI** is signed into, and moving that
//! login between managed accounts.
//!
//! This is a separate identity from the Claude Desktop app's (see
//! [`crate::claude_desktop`]) — the two drift apart constantly, which is most
//! of why they are both worth reporting.
//!
//! The CLI keeps exactly **one** default login: `~/.claude/.credentials.json`
//! on Linux, the login-Keychain item `Claude Code-credentials` on macOS. A
//! named account instead lives under its own `CLAUDE_CONFIG_DIR`, in the
//! per-directory Keychain item [`crate::anthropic::keychain`] resolves. Making
//! a named account "the one plain `claude` uses" therefore means copying its
//! credential into that single default slot.
//!
//! Both slots then hold the same *rotating* refresh token, and whichever
//! client refreshes first invalidates the other's copy — the failure
//! [`crate::anthropic::creds`] documents. Two things keep that from happening:
//! the switch captures the outgoing credential back into its own account
//! first, so the freshest lineage is never dropped; and
//! [`crate::config::AnthropicConfig::account_target_with`] routes reads for
//! whichever label is *currently* active to the default slot, so only one copy
//! is ever live.
//!
//! Everything except [`KeychainStore`] is platform-agnostic and unit-tested
//! against a fake store, so the logic stays under CI's linter on Linux.

use std::path::Path;

use serde_json::{Map, Value};

use crate::config::AnthropicAccount;
use crate::error::{AppError, Result};

/// The `claude` CLI's per-config-dir state file. Holds a plaintext
/// `oauthAccount` identity marker (uuid, email, org) — never a token.
const CLAUDE_JSON: &str = ".claude.json";

/// Read/write access to the credential slots. Abstracted so the switch logic
/// can be tested without a macOS Keychain.
pub trait CredentialStore {
    fn read_default(&self) -> Result<Option<String>>;
    fn write_default(&self, blob: &str) -> Result<()>;
    fn read_named(&self, config_dir: &Path) -> Result<Option<String>>;
    fn write_named(&self, config_dir: &Path, blob: &str) -> Result<()>;
}

/// The real store. The `#[cfg]` pairs are confined here so no other logic in
/// this module is invisible to a Linux build — the same shape
/// [`crate::anthropic::creds::resolve`] uses.
pub struct KeychainStore;

/// Path to the home `.claude.json` that records the live CLI identity.
pub fn home_claude_json() -> Result<std::path::PathBuf> {
    Ok(crate::cache::home_dir()?.join(CLAUDE_JSON))
}

/// The identity marker inside an account's own `CLAUDE_CONFIG_DIR`.
pub fn marker_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join(CLAUDE_JSON)
}

/// The account UUID recorded in a `.claude.json`, if any.
pub fn account_uuid_in(claude_json: &Path) -> Option<String> {
    oauth_account_in(claude_json)?
        .get("accountUuid")?
        .as_str()
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// The e-mail recorded alongside it, for display only.
pub fn account_email_in(claude_json: &Path) -> Option<String> {
    oauth_account_in(claude_json)?
        .get("emailAddress")?
        .as_str()
        .filter(|email| !email.is_empty())
        .map(str::to_string)
}

/// Which managed label the live CLI login belongs to.
///
/// `None` when the CLI is signed into something we do not manage — a plain
/// `claude` login done by hand, say. Callers surface that as "unknown" rather
/// than hiding it: it is exactly the case where the named copies may have gone
/// stale behind our back.
pub fn resolve_active_label(
    home_claude_json: &Path,
    accounts: &[AnthropicAccount],
) -> Option<String> {
    let live = account_uuid_in(home_claude_json)?;
    accounts
        .iter()
        .find(|account| {
            account_uuid_in(&account.config_dir().join(CLAUDE_JSON)) == Some(live.clone())
        })
        .map(|account| account.label.clone())
}

/// Set (or clear) the `oauthAccount` marker in a `.claude.json`, preserving
/// every other key. That document is tens of kilobytes of unrelated CLI state,
/// so it is merged into, never replaced.
pub fn merge_oauth_account(existing: &[u8], oauth_account: Option<&Value>) -> Result<Vec<u8>> {
    let mut document: Value = if existing.iter().all(u8::is_ascii_whitespace) {
        Value::Object(Map::new())
    } else {
        serde_json::from_slice(existing)?
    };
    let object = document
        .as_object_mut()
        .ok_or_else(|| AppError::Other(format!("{CLAUDE_JSON} is not a JSON object")))?;
    match oauth_account {
        Some(account) => {
            object.insert("oauthAccount".into(), account.clone());
        }
        // Better an absent marker than one that names the previous account.
        None => {
            object.remove("oauthAccount");
        }
    }
    Ok(serde_json::to_vec(&document)?)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CliSwitchOpts {
    /// Overwrite the default slot even though the live login belongs to no
    /// managed account. That login cannot be captured anywhere first, so this
    /// genuinely destroys it.
    pub force: bool,
    /// Validate everything and report, without writing.
    pub dry_run: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliSwitchOutcome {
    AlreadyActive,
    /// `outgoing` is the label whose credential was captured back first, when
    /// there was one.
    Switched {
        outgoing: Option<String>,
    },
    /// `dry_run` was set; nothing was written.
    WouldSwitch {
        outgoing: Option<String>,
    },
}

/// Make `label` the account plain `claude` uses.
///
/// Ordering is deliberate. The outgoing credential is captured back **before**
/// anything is overwritten — that is the only irreversible step, and a failure
/// there aborts. The default slot is then written **before** the identity
/// marker, because the credential is the source of truth: a marker claiming an
/// account whose token is not there is worse than the reverse.
pub fn switch_cli_account(
    home_claude_json: &Path,
    accounts: &[AnthropicAccount],
    label: &str,
    opts: CliSwitchOpts,
    store: &dyn CredentialStore,
) -> Result<CliSwitchOutcome> {
    let target = accounts
        .iter()
        .find(|account| account.label == label)
        .ok_or_else(|| {
            let known: Vec<&str> = accounts.iter().map(|a| a.label.as_str()).collect();
            AppError::Credentials(format!(
                "no Claude CLI account {label:?} in [[anthropic.accounts]] or accounts_dir; \
                 known: {known:?}"
            ))
        })?;

    let active = resolve_active_label(home_claude_json, accounts);
    if active.as_deref() == Some(label) {
        return Ok(CliSwitchOutcome::AlreadyActive);
    }
    if active.is_none() && !opts.force {
        return Err(AppError::Credentials(format!(
            "the `claude` CLI is signed into an account that is not managed here, so \
             switching to {label:?} would overwrite a login that cannot be saved first. \
             Register it with `ai-usagebar account add <label>`, or pass --force to \
             discard it."
        )));
    }

    // Fail before touching anything if the target has never been signed in.
    let target_blob = store.read_named(&target.config_dir())?.ok_or_else(|| {
        AppError::Credentials(format!(
            "no stored credential for {label:?}; sign it in once with \
             `ai-usagebar account add {label}`"
        ))
    })?;
    let oauth_account = oauth_account_in(&target.config_dir().join(CLAUDE_JSON));

    if opts.dry_run {
        return Ok(CliSwitchOutcome::WouldSwitch { outgoing: active });
    }

    if let Some(outgoing) = &active {
        let outgoing_dir = accounts
            .iter()
            .find(|account| &account.label == outgoing)
            .map(AnthropicAccount::config_dir);
        if let (Some(dir), Some(blob)) = (outgoing_dir, store.read_default()?) {
            store.write_named(&dir, &blob)?;
        }
    }
    store.write_default(&target_blob)?;

    let existing = std::fs::read(home_claude_json).unwrap_or_default();
    let merged = merge_oauth_account(&existing, oauth_account.as_ref())?;
    crate::cache::atomic_write(home_claude_json, &merged)?;

    Ok(CliSwitchOutcome::Switched { outgoing: active })
}

fn oauth_account_in(claude_json: &Path) -> Option<Value> {
    let bytes = std::fs::read(claude_json).ok()?;
    let document: Value = serde_json::from_slice(&bytes).ok()?;
    document
        .get("oauthAccount")
        .filter(|v| v.is_object())
        .cloned()
}

#[cfg(not(target_os = "macos"))]
fn unsupported() -> AppError {
    AppError::Credentials(
        "switching the `claude` CLI login is supported on macOS only (elsewhere, run \
         `CLAUDE_CONFIG_DIR=<account dir> claude` to use a specific account)"
            .into(),
    )
}

impl CredentialStore for KeychainStore {
    fn read_default(&self) -> Result<Option<String>> {
        #[cfg(target_os = "macos")]
        return super::keychain::read_raw();
        #[cfg(not(target_os = "macos"))]
        Err(unsupported())
    }

    fn write_default(&self, blob: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        return super::keychain::write_raw(blob);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = blob;
            Err(unsupported())
        }
    }

    fn read_named(&self, config_dir: &Path) -> Result<Option<String>> {
        #[cfg(target_os = "macos")]
        return super::keychain::read_raw_for(config_dir);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = config_dir;
            Err(unsupported())
        }
    }

    fn write_named(&self, config_dir: &Path, blob: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        return super::keychain::write_raw_for(config_dir, blob);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (config_dir, blob);
            Err(unsupported())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeStore {
        default: RefCell<Option<String>>,
        named: RefCell<BTreeMap<PathBuf, String>>,
    }

    impl CredentialStore for FakeStore {
        fn read_default(&self) -> Result<Option<String>> {
            Ok(self.default.borrow().clone())
        }
        fn write_default(&self, blob: &str) -> Result<()> {
            *self.default.borrow_mut() = Some(blob.to_string());
            Ok(())
        }
        fn read_named(&self, config_dir: &Path) -> Result<Option<String>> {
            Ok(self.named.borrow().get(config_dir).cloned())
        }
        fn write_named(&self, config_dir: &Path, blob: &str) -> Result<()> {
            self.named
                .borrow_mut()
                .insert(config_dir.to_path_buf(), blob.to_string());
            Ok(())
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn marker(uuid: &str, email: &str) -> String {
        format!(r#"{{"oauthAccount":{{"accountUuid":"{uuid}","emailAddress":"{email}"}}}}"#)
    }

    struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        accounts: Vec<AnthropicAccount>,
        store: FakeStore,
    }

    /// Two accounts, `work` and `personal`; the CLI is signed into `personal`.
    fn fixture() -> Fixture {
        let root = tempfile::TempDir::new().unwrap();
        let home = root.path().join("home").join(CLAUDE_JSON);
        write(&home, &marker("uuid-personal", "me@personal.test"));

        let accounts: Vec<AnthropicAccount> = ["work", "personal"]
            .iter()
            .map(|label| AnthropicAccount {
                label: (*label).to_string(),
                credentials_path: root
                    .path()
                    .join("accounts")
                    .join(label)
                    .join(".credentials.json"),
            })
            .collect();
        write(
            &accounts[0].config_dir().join(CLAUDE_JSON),
            &marker("uuid-work", "me@work.test"),
        );
        write(
            &accounts[1].config_dir().join(CLAUDE_JSON),
            &marker("uuid-personal", "me@personal.test"),
        );

        let store = FakeStore::default();
        *store.default.borrow_mut() = Some("personal-live".into());
        store
            .named
            .borrow_mut()
            .insert(accounts[0].config_dir(), "work-saved".into());
        store
            .named
            .borrow_mut()
            .insert(accounts[1].config_dir(), "personal-stale".into());

        Fixture {
            _root: root,
            home,
            accounts,
            store,
        }
    }

    #[test]
    fn the_live_login_resolves_to_its_label() {
        let f = fixture();
        assert_eq!(
            resolve_active_label(&f.home, &f.accounts).as_deref(),
            Some("personal")
        );
        assert_eq!(
            account_email_in(&f.home).as_deref(),
            Some("me@personal.test")
        );
    }

    #[test]
    fn an_unmanaged_login_resolves_to_nothing() {
        let f = fixture();
        write(&f.home, &marker("uuid-stranger", "who@example.test"));
        assert_eq!(resolve_active_label(&f.home, &f.accounts), None);
    }

    #[test]
    fn a_missing_marker_file_is_not_an_error() {
        let f = fixture();
        std::fs::remove_file(&f.home).unwrap();
        assert_eq!(resolve_active_label(&f.home, &f.accounts), None);
        assert_eq!(account_uuid_in(&f.home), None);
    }

    #[test]
    fn switching_captures_the_outgoing_credential_first() {
        let f = fixture();

        let outcome = switch_cli_account(
            &f.home,
            &f.accounts,
            "work",
            CliSwitchOpts::default(),
            &f.store,
        )
        .unwrap();

        assert_eq!(
            outcome,
            CliSwitchOutcome::Switched {
                outgoing: Some("personal".into())
            }
        );
        // The freshest lineage was written back into personal's own slot before
        // the default slot was overwritten.
        assert_eq!(
            f.store.named.borrow()[&f.accounts[1].config_dir()],
            "personal-live"
        );
        assert_eq!(f.store.default.borrow().as_deref(), Some("work-saved"));
        assert_eq!(
            resolve_active_label(&f.home, &f.accounts).as_deref(),
            Some("work")
        );
    }

    #[test]
    fn switching_is_idempotent() {
        let f = fixture();
        let outcome = switch_cli_account(
            &f.home,
            &f.accounts,
            "personal",
            CliSwitchOpts::default(),
            &f.store,
        )
        .unwrap();
        assert_eq!(outcome, CliSwitchOutcome::AlreadyActive);
        assert_eq!(f.store.default.borrow().as_deref(), Some("personal-live"));
    }

    #[test]
    fn a_dry_run_validates_without_writing() {
        let f = fixture();
        let before = std::fs::read(&f.home).unwrap();

        let outcome = switch_cli_account(
            &f.home,
            &f.accounts,
            "work",
            CliSwitchOpts {
                dry_run: true,
                ..CliSwitchOpts::default()
            },
            &f.store,
        )
        .unwrap();

        assert_eq!(
            outcome,
            CliSwitchOutcome::WouldSwitch {
                outgoing: Some("personal".into())
            }
        );
        assert_eq!(f.store.default.borrow().as_deref(), Some("personal-live"));
        assert_eq!(std::fs::read(&f.home).unwrap(), before);
    }

    #[test]
    fn an_unmanaged_live_login_is_refused_without_force() {
        let f = fixture();
        write(&f.home, &marker("uuid-stranger", "who@example.test"));

        let error = switch_cli_account(
            &f.home,
            &f.accounts,
            "work",
            CliSwitchOpts::default(),
            &f.store,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--force"), "{error}");
        assert_eq!(f.store.default.borrow().as_deref(), Some("personal-live"));

        switch_cli_account(
            &f.home,
            &f.accounts,
            "work",
            CliSwitchOpts {
                force: true,
                ..CliSwitchOpts::default()
            },
            &f.store,
        )
        .unwrap();
        assert_eq!(f.store.default.borrow().as_deref(), Some("work-saved"));
    }

    #[test]
    fn switching_to_an_account_that_never_signed_in_changes_nothing() {
        let f = fixture();
        f.store
            .named
            .borrow_mut()
            .remove(&f.accounts[0].config_dir());

        let error = switch_cli_account(
            &f.home,
            &f.accounts,
            "work",
            CliSwitchOpts::default(),
            &f.store,
        )
        .unwrap_err();
        assert!(error.to_string().contains("account add work"), "{error}");
        assert_eq!(f.store.default.borrow().as_deref(), Some("personal-live"));
    }

    #[test]
    fn merging_the_marker_preserves_unrelated_state() {
        let existing = br#"{"firstStartTime":"2026-01-01","oauthAccount":{"accountUuid":"old"},
            "projects":{"/tmp/x":{"allowedTools":[]}}}"#;
        let replacement = serde_json::json!({"accountUuid": "new", "emailAddress": "a@b.test"});

        let bytes = merge_oauth_account(existing, Some(&replacement)).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["oauthAccount"]["accountUuid"], "new");
        assert_eq!(value["firstStartTime"], "2026-01-01");
        assert!(value["projects"]["/tmp/x"].is_object());
    }

    #[test]
    fn merging_no_marker_clears_a_stale_one() {
        let existing = br#"{"oauthAccount":{"accountUuid":"old"},"autoUpdates":true}"#;
        let bytes = merge_oauth_account(existing, None).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("oauthAccount").is_none());
        assert_eq!(value["autoUpdates"], true);
    }

    #[test]
    fn merging_into_an_absent_file_starts_a_fresh_document() {
        let replacement = serde_json::json!({"accountUuid": "new"});
        let bytes = merge_oauth_account(b"", Some(&replacement)).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["oauthAccount"]["accountUuid"], "new");
    }
}
