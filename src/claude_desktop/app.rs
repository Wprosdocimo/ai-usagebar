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
    fn quit(&self) -> Result<()>;
    /// Start it again.
    fn relaunch(&self) -> Result<()>;
    /// Write `members` (relative to `root`) into a gzipped tar at `archive`.
    fn archive(&self, archive: &Path, root: &Path, members: &[&str]) -> Result<()>;
    /// Restore a rollback archive after removing every path the attempted
    /// identity swap could have created.
    fn restore(&self, archive: &Path, root: &Path, cleanup_members: &[&str]) -> Result<()>;
}

/// The real thing. Every command is a macOS system binary at a fixed path, the
/// same approach `anthropic::keychain` takes with `security(1)` and `shasum(1)`
/// — no extra dependency, and nothing to resolve off `PATH`. On any other
/// platform these simply fail, which is harmless: there is no Claude Desktop
/// app to find there in the first place, so a switch never gets this far.
pub struct DesktopApp;

/// How long to give the app to shut down cleanly before insisting.
const QUIT_GRACE: Duration = Duration::from_secs(2);
const QUIT_POLL: Duration = Duration::from_millis(100);
const QUIT_POLLS: usize = 20;

#[cfg(unix)]
fn set_private_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| AppError::io_at(path, error))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn is_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", "Claude"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn wait_until_stopped() -> bool {
    for _ in 0..QUIT_POLLS {
        if !is_running() {
            return true;
        }
        std::thread::sleep(QUIT_POLL);
    }
    !is_running()
}

impl AppControl for DesktopApp {
    fn quit(&self) -> Result<()> {
        // Graceful first so the app tears down its own child processes; the
        // signal is the fallback. Neither touches a `claude` CLI process —
        // `-x` matches the executable name exactly.
        let _ = Command::new("/usr/bin/osascript")
            .args(["-e", "tell application \"Claude\" to quit"])
            .output();
        std::thread::sleep(QUIT_GRACE);
        if wait_until_stopped() {
            return Ok(());
        }
        let _ = Command::new("/usr/bin/pkill")
            .args(["-x", "Claude"])
            .output();
        if wait_until_stopped() {
            return Ok(());
        }
        let _ = Command::new("/usr/bin/pkill")
            .args(["-KILL", "-x", "Claude"])
            .output();
        if wait_until_stopped() {
            Ok(())
        } else {
            Err(AppError::Other(
                "Claude Desktop did not stop; no account data was changed".into(),
            ))
        }
    }

    fn relaunch(&self) -> Result<()> {
        let output = Command::new("/usr/bin/open")
            .args(["-a", "Claude"])
            .output()
            .map_err(|e| AppError::Other(format!("could not relaunch Claude Desktop: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AppError::Other(format!(
                "could not relaunch Claude Desktop (open exited {})",
                output.status.code().unwrap_or(-1)
            )))
        }
    }

    fn archive(&self, archive: &Path, root: &Path, members: &[&str]) -> Result<()> {
        if let Some(parent) = archive.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::io_at(parent, e))?;
            // The archive contains cookies, credentials, and browser state.
            // Restrict the directory before tar creates the file so even the
            // brief pre-chmod window is contained on Unix hosts.
            set_private_mode(parent, 0o700)?;
        }
        let output = Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(archive)
            .arg("-C")
            .arg(root)
            .arg("--")
            .args(members)
            .output()
            .map_err(|e| AppError::Other(format!("could not run `tar`: {e}")))?;
        if output.status.success() {
            set_private_mode(archive, 0o600)?;
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

    fn restore(&self, archive: &Path, root: &Path, cleanup_members: &[&str]) -> Result<()> {
        for member in cleanup_members {
            let path = root.join(member);
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {
                    std::fs::remove_dir_all(&path).map_err(|e| AppError::io_at(&path, e))?;
                }
                Ok(_) => {
                    std::fs::remove_file(&path).map_err(|e| AppError::io_at(&path, e))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(AppError::io_at(&path, error)),
            }
        }
        let output = Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(root)
            .output()
            .map_err(|e| AppError::Other(format!("could not run `tar` for rollback: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AppError::Other(format!(
                "could not restore {} (tar exited {})",
                archive.display(),
                output.status.code().unwrap_or(-1)
            )))
        }
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
    fn quit(&self) -> Result<()> {
        self.record("quit");
        Ok(())
    }

    fn relaunch(&self) -> Result<()> {
        self.record("relaunch");
        Ok(())
    }

    fn archive(&self, archive: &Path, _root: &Path, members: &[&str]) -> Result<()> {
        self.record(format!(
            "archive {} [{}]",
            archive.display(),
            members.join(", ")
        ));
        Ok(())
    }

    fn restore(&self, archive: &Path, _root: &Path, members: &[&str]) -> Result<()> {
        self.record(format!(
            "restore {} [{}]",
            archive.display(),
            members.join(", ")
        ));
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rollback_archives_and_their_directory_are_private() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("Claude");
        let backup_dir = temp.path().join("backups");
        let archive = backup_dir.join("rollback.tar.gz");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.json"), b"secret state").unwrap();

        DesktopApp
            .archive(&archive, &root, &["config.json"])
            .unwrap();

        let dir_mode = std::fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777;
        let archive_mode = std::fs::metadata(&archive).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(archive_mode, 0o600);
    }
}
