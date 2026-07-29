//! Capturing a **new** Claude Desktop account, so this machine can switch to
//! it later.
//!
//! Unlike the `claude` CLI — where `CLAUDE_CONFIG_DIR=<dir> claude` isolates a
//! login into its own directory — the Desktop app has exactly one login slot
//! and no way to ask for a second. The only way to obtain a second account's
//! credential is to sign the app out, have the user sign in as that account,
//! and save what the app then writes. That is what this does, and why it is
//! interactive and destructive-looking in the middle.
//!
//! It is safe to cancel. The live login is copied out **before** anything is
//! cleared, and put straight back if the sign-in times out or the user walks
//! away. The account that was active is also saved into its own profile first,
//! so switching back to it afterwards restores exactly what it looked like.
//!
//! Ported from claude-acc's `add` (<https://github.com/ohmaseclaro/claude-acc>),
//! along with the two empirical rules that make the polling reliable: a login
//! only counts once *both* `lastKnownAccountUuid` and the token cache are
//! written, and the org id has to be recovered from either the new session
//! folder or the dxt allowlist keys.

use std::path::Path;
use std::time::{Duration, Instant};

use super::app::AppControl;
use super::{Paths, merge};
use crate::error::{AppError, Result};

/// Everything the app remembers an account by. Backed up before a capture
/// clears it, restored verbatim if the capture does not complete.
const LOGIN_STATE_FILES: [&str; 4] = [
    "config.json",
    "Cookies",
    "Cookies-journal",
    "bridge-state.json",
];
const LOGIN_STATE_DIRS: [&str; 3] = ["Local Storage", "Session Storage", "IndexedDB"];

/// How long to wait for each interactive step.
#[derive(Debug, Clone, Copy)]
pub struct WaitOpts {
    pub login: Duration,
    pub org: Duration,
    pub poll: Duration,
}

impl Default for WaitOpts {
    fn default() -> Self {
        Self {
            login: Duration::from_secs(300),
            org: Duration::from_secs(120),
            poll: Duration::from_secs(2),
        }
    }
}

#[derive(Debug)]
pub struct Captured {
    pub label: String,
    pub account_uuid: String,
    /// Absent when the app had not created a session folder yet; history is
    /// then seeded on the first switch instead.
    pub org_uuid: Option<String>,
    pub seeded_sessions: usize,
    pub seeded_routines: usize,
}

#[derive(Debug)]
pub enum CaptureOutcome {
    Captured(Box<Captured>),
    /// The account signed in is one this machine already knows. Nothing was
    /// saved and the app is left signed into it.
    AlreadySaved(String),
    /// Nobody signed in within the timeout; the previous login was restored.
    TimedOut,
}

/// Sign the Desktop app out, wait for the user to sign in as a new account,
/// and save it under `label`.
pub fn capture_profile(
    paths: &Paths,
    label: &str,
    email: Option<&str>,
    app: &dyn AppControl,
    wait: WaitOpts,
    notes: &mut Vec<String>,
) -> Result<CaptureOutcome> {
    crate::config::validate_account_label(label)?;
    let profiles = super::load_profiles(&paths.profiles_dir);
    if profiles.iter().any(|profile| profile.label == label) {
        return Err(AppError::Credentials(format!(
            "a Claude Desktop account {label:?} already exists in {}; pick another name",
            paths.profiles_dir.display()
        )));
    }
    let known_accounts: Vec<String> = profiles.iter().map(|p| p.account_uuid.clone()).collect();
    let known_orgs: Vec<String> = profiles.iter().filter_map(|p| p.org_uuid.clone()).collect();

    let config_json = paths.config_json();
    let outgoing = super::active_account_uuid(&config_json)
        .and_then(|uuid| super::label_for_uuid(&profiles, &uuid).map(str::to_string));

    app.quit();
    // Save the account we are about to sign out, so switching back to it later
    // restores its browser state rather than only its credential.
    if let Some(previous) = &outgoing {
        super::snapshot_profile(paths, previous, notes);
    }
    // The safety net runs regardless: the live login may belong to no profile
    // at all, and it still has to survive a cancelled capture.
    backup_login_state(paths, notes)?;
    clear_login_state(paths, notes)?;
    app.relaunch();

    let Some(account_uuid) = poll_for_login(&config_json, wait) else {
        app.quit();
        restore_login_state(paths, notes);
        app.relaunch();
        return Ok(CaptureOutcome::TimedOut);
    };
    if let Some(existing) = super::label_for_uuid(&profiles, &account_uuid) {
        return Ok(CaptureOutcome::AlreadySaved(existing.to_string()));
    }
    if known_accounts.iter().any(|known| known == &account_uuid) {
        return Ok(CaptureOutcome::AlreadySaved(account_uuid));
    }

    let org_uuid = poll_for_org(paths, &account_uuid, &known_orgs, wait);
    app.quit();
    super::snapshot_profile(paths, label, notes);
    write_meta(paths, label, email, &account_uuid, org_uuid.as_deref())?;

    // Seed the new account with everything this machine already has, so its
    // first login is not an empty sidebar.
    let (seeded_sessions, seeded_routines) = match &org_uuid {
        Some(org) => super::merge_history_into(paths, &account_uuid, org, notes),
        None => {
            notes.push(
                "no organisation recorded yet, so history was not seeded — open one chat in \
                 the app, then run `ai-usagebar account switch <label> --desktop` to pull it in"
                    .into(),
            );
            (0, 0)
        }
    };
    app.relaunch();

    Ok(CaptureOutcome::Captured(Box::new(Captured {
        label: label.to_string(),
        account_uuid,
        org_uuid,
        seeded_sessions,
        seeded_routines,
    })))
}

fn poll_for_login(config_json: &Path, wait: WaitOpts) -> Option<String> {
    let deadline = Instant::now() + wait.login;
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(config_json)
            && let Some(uuid) = merge::logged_in_account(&bytes)
        {
            return Some(uuid);
        }
        std::thread::sleep(wait.poll);
    }
    None
}

/// The account's own session folder is the reliable signal; the config's dxt
/// allowlist keys are the fallback for an account that has not opened a chat.
fn poll_for_org(
    paths: &Paths,
    account_uuid: &str,
    known_orgs: &[String],
    wait: WaitOpts,
) -> Option<String> {
    let deadline = Instant::now() + wait.org;
    let account_dir = paths.sessions_root().join(account_uuid);
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&account_dir)
            && let Some(org) = entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .find_map(|entry| entry.file_name().to_str().map(str::to_string))
        {
            return Some(org);
        }
        if let Ok(bytes) = std::fs::read(paths.config_json())
            && let Some(org) = merge::new_org_in_config(&bytes, known_orgs)
        {
            return Some(org);
        }
        std::thread::sleep(wait.poll);
    }
    None
}

/// The one place that writes `meta.json`. A switch must never touch it — this
/// is the file that says which account a profile *is*.
fn write_meta(
    paths: &Paths,
    label: &str,
    email: Option<&str>,
    account_uuid: &str,
    org_uuid: Option<&str>,
) -> Result<()> {
    let meta = serde_json::json!({
        "label": label,
        "email": email,
        "accountUuid": account_uuid,
        "orgUuid": org_uuid,
        "savedAt": chrono::Local::now().timestamp(),
    });
    crate::cache::atomic_write(
        &paths.profile_dir(label).join(super::META_JSON),
        &serde_json::to_vec_pretty(&meta)?,
    )
}

fn backup_login_state(paths: &Paths, notes: &mut Vec<String>) -> Result<()> {
    let backup = paths.prelogin_dir();
    if backup.exists() {
        std::fs::remove_dir_all(&backup).map_err(|e| AppError::io_at(&backup, e))?;
    }
    std::fs::create_dir_all(&backup).map_err(|e| AppError::io_at(&backup, e))?;
    super::restrict(&backup, 0o700, notes);
    for name in LOGIN_STATE_FILES {
        let source = paths.data_dir.join(name);
        if source.is_file() {
            super::copy_file(&source, &backup.join(name))?;
        }
    }
    for name in LOGIN_STATE_DIRS {
        let source = paths.data_dir.join(name);
        if source.is_dir() {
            super::replace_dir(&source, &backup.join(name))?;
        }
    }
    Ok(())
}

/// Best-effort by design: this runs when the capture has already failed, and a
/// partial restore reported plainly beats an error that hides what happened.
fn restore_login_state(paths: &Paths, notes: &mut Vec<String>) {
    let backup = paths.prelogin_dir();
    if !backup.is_dir() {
        notes.push("no pre-login backup to restore".into());
        return;
    }
    for name in LOGIN_STATE_FILES {
        let source = backup.join(name);
        let live = paths.data_dir.join(name);
        if source.is_file() {
            if let Err(error) = super::copy_file(&source, &live) {
                notes.push(format!("could not restore {name}: {error}"));
            }
        } else if live.is_file()
            && let Err(error) = std::fs::remove_file(&live)
        {
            notes.push(format!("could not clear {name}: {error}"));
        }
    }
    for name in LOGIN_STATE_DIRS {
        let source = backup.join(name);
        if source.is_dir()
            && let Err(error) = super::replace_dir(&source, &paths.data_dir.join(name))
        {
            notes.push(format!("could not restore {name}: {error}"));
        }
    }
}

fn clear_login_state(paths: &Paths, notes: &mut Vec<String>) -> Result<()> {
    let config_json = paths.config_json();
    if let Ok(bytes) = std::fs::read(&config_json) {
        match merge::clear_config_tokens(&bytes) {
            Ok(cleared) => crate::cache::atomic_write(&config_json, &cleared)?,
            Err(error) => notes.push(format!("could not clear the Desktop config: {error}")),
        }
    }
    for name in LOGIN_STATE_FILES
        .iter()
        .filter(|name| **name != "config.json")
    {
        let live = paths.data_dir.join(name);
        if live.is_file()
            && let Err(error) = std::fs::remove_file(&live)
        {
            notes.push(format!("could not clear {name}: {error}"));
        }
    }
    for name in LOGIN_STATE_DIRS {
        let live = paths.data_dir.join(name);
        if live.is_dir()
            && let Err(error) = std::fs::remove_dir_all(&live)
        {
            notes.push(format!("could not clear {name}: {error}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_desktop::app::Recorder;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, Paths) {
        let root = tempfile::TempDir::new().unwrap();
        let data = root.path().join("data");
        write(
            &data.join("config.json"),
            r#"{"lastKnownAccountUuid":"uuid-here","oauth:tokenCache":"live-a",
                "oauth:tokenCacheV2":"live-b","autoUpdates":true}"#,
        );
        write(&data.join("Cookies"), "live-cookies");
        write(
            &data.join("bridge-state.json"),
            r#"{"remoteSessionId":"cse"}"#,
        );
        write(&data.join("Local Storage/leveldb/CURRENT"), "live-ldb");
        let paths = Paths::at(
            data,
            root.path().join("profiles"),
            root.path().join("backups"),
        );
        (root, paths)
    }

    /// The safety net: whatever a capture clears has to come back byte for byte.
    #[test]
    fn a_cleared_login_is_restored_exactly() {
        let (_root, paths) = fixture();
        let mut notes = Vec::new();

        backup_login_state(&paths, &mut notes).unwrap();
        clear_login_state(&paths, &mut notes).unwrap();

        let cleared: serde_json::Value =
            serde_json::from_slice(&std::fs::read(paths.config_json()).unwrap()).unwrap();
        assert!(cleared.get("oauth:tokenCacheV2").is_none());
        assert!(cleared.get("lastKnownAccountUuid").is_none());
        assert_eq!(cleared["autoUpdates"], true, "settings must survive");
        assert!(!paths.data_dir.join("Cookies").exists());
        assert!(!paths.data_dir.join("Local Storage").exists());

        restore_login_state(&paths, &mut notes);

        assert!(notes.is_empty(), "{notes:?}");
        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(paths.config_json()).unwrap()).unwrap();
        assert_eq!(restored["oauth:tokenCacheV2"], "live-b");
        assert_eq!(restored["lastKnownAccountUuid"], "uuid-here");
        assert_eq!(
            std::fs::read_to_string(paths.data_dir.join("Cookies")).unwrap(),
            "live-cookies"
        );
        assert_eq!(
            std::fs::read_to_string(paths.data_dir.join("Local Storage/leveldb/CURRENT")).unwrap(),
            "live-ldb"
        );
    }

    /// A file the app created *after* the backup must not survive the restore,
    /// or the new account's cookies would leak into the old one's session.
    #[test]
    fn restoring_clears_state_the_backup_does_not_have() {
        let (_root, paths) = fixture();
        let mut notes = Vec::new();
        std::fs::remove_file(paths.data_dir.join("Cookies")).unwrap();

        backup_login_state(&paths, &mut notes).unwrap();
        write(&paths.data_dir.join("Cookies"), "someone else's");
        restore_login_state(&paths, &mut notes);

        assert!(!paths.data_dir.join("Cookies").exists(), "{notes:?}");
    }

    #[test]
    fn a_timed_out_capture_puts_the_previous_login_back() {
        let (_root, paths) = fixture();
        let recorder = Recorder::default();
        let mut notes = Vec::new();
        let before = std::fs::read(paths.config_json()).unwrap();

        let outcome = capture_profile(
            &paths,
            "work",
            None,
            &recorder,
            WaitOpts {
                login: Duration::from_millis(1),
                org: Duration::from_millis(1),
                poll: Duration::from_millis(1),
            },
            &mut notes,
        )
        .unwrap();

        assert!(matches!(outcome, CaptureOutcome::TimedOut), "{outcome:?}");
        assert_eq!(std::fs::read(paths.config_json()).unwrap(), before);
        assert!(!paths.profile_dir("work").exists(), "nothing was saved");
        // Quit, reopen at the login screen, quit again, reopen restored.
        assert_eq!(recorder.steps(), ["quit", "relaunch", "quit", "relaunch"]);
    }

    #[test]
    fn capturing_a_label_that_already_exists_is_refused_before_signing_out() {
        let (_root, paths) = fixture();
        write(
            &paths.profile_dir("work").join("meta.json"),
            r#"{"accountUuid":"uuid-work"}"#,
        );
        let recorder = Recorder::default();
        let before = std::fs::read(paths.config_json()).unwrap();

        let error = capture_profile(
            &paths,
            "work",
            None,
            &recorder,
            WaitOpts::default(),
            &mut Vec::new(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("already exists"), "{error}");
        assert!(recorder.steps().is_empty(), "the app must not be touched");
        assert_eq!(std::fs::read(paths.config_json()).unwrap(), before);
    }

    #[test]
    fn an_unusable_label_is_refused() {
        let (_root, paths) = fixture();
        let recorder = Recorder::default();
        assert!(
            capture_profile(
                &paths,
                "../escape",
                None,
                &recorder,
                WaitOpts::default(),
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(recorder.steps().is_empty());
    }
}
