//! The only part of a Claude Desktop switch that touches the outside world:
//! stopping the app, restarting it, and writing the rollback archive.
//!
//! It sits behind a trait so the switch orchestration can be exercised against
//! a recorder — the step *ordering* is the dangerous part (snapshotting the
//! outgoing account must happen before `config.json` is rewritten, and the
//! relaunch must happen even when an earlier step failed), and ordering is
//! exactly what a test can pin down without a Mac.

use std::cell::RefCell;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::error::{AppError, Result};

/// Host-level effects of a switch.
pub trait AppControl {
    /// Stop the Claude Desktop app. Must return only once it is really gone —
    /// its SQLite and LevelDB stores cannot be copied while it is writing them.
    fn quit(&self);
    /// Start it again.
    fn relaunch(&self);
    /// Write `members` (relative to `root`) into a gzipped tar at `archive`.
    fn archive(&self, archive: &Path, root: &Path, members: &[&str]) -> Result<()>;
}

/// The real thing. Every command is a macOS system binary at a fixed path, the
/// same approach `anthropic::keychain` takes with `security(1)` and `shasum(1)`
/// — no extra dependency, and nothing to resolve off `PATH`. On any other
/// platform these simply fail, which is harmless: there is no Claude Desktop
/// app to find there in the first place, so a switch never gets this far.
pub struct DesktopApp;

/// How long to give the app to shut down cleanly before insisting.
const QUIT_GRACE: Duration = Duration::from_secs(2);

impl AppControl for DesktopApp {
    fn quit(&self) {
        // Graceful first so the app tears down its own child processes; the
        // signal is the fallback. Neither touches a `claude` CLI process —
        // `-x` matches the executable name exactly.
        let _ = Command::new("/usr/bin/osascript")
            .args(["-e", "tell application \"Claude\" to quit"])
            .output();
        std::thread::sleep(QUIT_GRACE);
        let _ = Command::new("/usr/bin/pkill")
            .args(["-x", "Claude"])
            .output();
    }

    fn relaunch(&self) {
        let _ = Command::new("/usr/bin/open")
            .args(["-a", "Claude"])
            .output();
    }

    fn archive(&self, archive: &Path, root: &Path, members: &[&str]) -> Result<()> {
        if let Some(parent) = archive.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io_at(parent, e))?;
        }
        let output = Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(archive)
            .arg("-C")
            .arg(root)
            .args(members)
            .output()
            .map_err(|e| AppError::Other(format!("could not run `tar`: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(&output.stderr);
        Err(AppError::Other(format!(
            "could not write the rollback archive {} (tar exited {}): {}",
            archive.display(),
            output.status.code().unwrap_or(-1),
            detail.trim()
        )))
    }
}

/// Records what *would* happen instead of doing it. Backs `--dry-run`, and is
/// the fixture the ordering tests assert against.
#[derive(Debug, Default)]
pub struct Recorder {
    steps: RefCell<Vec<String>>,
}

impl Recorder {
    pub fn steps(&self) -> Vec<String> {
        self.steps.borrow().clone()
    }

    pub fn record(&self, step: impl Into<String>) {
        self.steps.borrow_mut().push(step.into());
    }
}

impl AppControl for Recorder {
    fn quit(&self) {
        self.record("quit");
    }

    fn relaunch(&self) {
        self.record("relaunch");
    }

    fn archive(&self, archive: &Path, _root: &Path, members: &[&str]) -> Result<()> {
        self.record(format!(
            "archive {} [{}]",
            archive.display(),
            members.join(", ")
        ));
        Ok(())
    }
}
