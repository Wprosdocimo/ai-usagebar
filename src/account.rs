//! Administrative commands for named Claude accounts.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::widget::cli::AccountAction;

struct Registered {
    config_path: PathBuf,
    credential_file: PathBuf,
    account_dir: PathBuf,
    credential_display: String,
    already_existed: bool,
}

impl Registered {
    fn supports_scoped_login(&self) -> bool {
        self.credential_file
            .file_name()
            .and_then(|name| name.to_str())
            == Some(".credentials.json")
    }
}

/// Run an administrative account command and return its process exit code.
#[must_use]
pub fn run(action: &AccountAction) -> i32 {
    match action {
        AccountAction::Add { label, no_login } => add(label, !no_login),
    }
}

fn add(label: &str, login: bool) -> i32 {
    let registration = match register(label) {
        Ok(registration) => registration,
        Err(error) => {
            eprintln!("ai-usagebar account: could not add {label:?}: {error}");
            return 1;
        }
    };

    if registration.already_existed {
        println!(
            "Claude account {label:?} is already configured in {}.",
            registration.config_path.display()
        );
    } else {
        println!(
            "Added Claude account {label:?} to {}.",
            registration.config_path.display()
        );
        println!("  credentials_path = {}", registration.credential_display);
    }
    println!();

    if !registration.supports_scoped_login() {
        if login {
            eprintln!(
                "Automatic login requires this account's credentials_path to end in \
                 .credentials.json; it currently points to {}. Update that entry or \
                 keep managing its credential file manually.",
                registration.credential_file.display()
            );
            return 1;
        }
        println!("Registration kept unchanged; interactive login was not requested.");
        return 0;
    }

    if !login {
        print!("{}", manual_login_hint(&registration.account_dir));
        return 0;
    }

    println!(
        "Opening `claude` for {label:?} with an isolated CLAUDE_CONFIG_DIR; your \
         default Claude login is untouched."
    );
    println!();
    match login_claude_account(&registration.account_dir) {
        LoginOutcome::NotFound => {
            eprintln!("`claude` was not found on PATH. The account remains registered.\n");
            eprint!("{}", manual_login_hint(&registration.account_dir));
            1
        }
        LoginOutcome::Failed(code) => {
            eprintln!(
                "`claude` exited with status {code}. The account remains registered; retry with:\n"
            );
            eprint!("{}", manual_login_hint(&registration.account_dir));
            1
        }
        LoginOutcome::Ok => {
            // Touch metadata only: rewriting the pre-login contents here would
            // clobber config edits made while the interactive command ran.
            if let Err(error) = restamp_config(&registration.config_path) {
                eprintln!(
                    "warning: login finished, but config.toml could not be touched for live reload: {error}"
                );
            }
            println!();
            println!(
                "`claude` finished for {label:?}; the menu bar / TUI will refresh momentarily."
            );
            0
        }
    }
}

enum LoginOutcome {
    Ok,
    Failed(i32),
    NotFound,
}

fn login_claude_account(account_dir: &Path) -> LoginOutcome {
    match std::process::Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", account_dir)
        .status()
    {
        Ok(status) if status.success() => LoginOutcome::Ok,
        Ok(status) => LoginOutcome::Failed(status.code().unwrap_or(-1)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LoginOutcome::NotFound,
        Err(_) => LoginOutcome::Failed(-1),
    }
}

fn register(label: &str) -> Result<Registered> {
    let config_path = crate::config::resolved_path()
        .or_else(crate::config::default_path)
        .ok_or_else(|| {
            AppError::Other("could not resolve a config.toml path (no home directory?)".into())
        })?;
    let home = crate::cache::home_dir().ok();
    register_at(&config_path, label, home.as_deref())
}

fn register_at(config_path: &Path, label: &str, home: Option<&Path>) -> Result<Registered> {
    let original = match std::fs::read_to_string(config_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(AppError::io_at(config_path, error)),
    };
    let mut doc: toml_edit::DocumentMut = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original.parse().map_err(|error: toml_edit::TomlError| {
            AppError::Other(format!("config.toml is not valid TOML: {error}"))
        })?
    };

    // Honor an existing explicit or accounts_dir-discovered account. In
    // particular, do not send a re-login to a different default directory.
    let existing = if config_path.exists() {
        Config::load_from(config_path)?
            .anthropic
            .all_accounts()
            .into_iter()
            .find(|account| account.label == label)
    } else {
        None
    };
    let already_existed = existing.is_some();
    let credential_file = existing.map_or_else(
        || crate::config::default_account_credentials_path(config_path, label),
        |account| account.credentials_path,
    );
    let account_dir = credential_file
        .parent()
        .ok_or_else(|| AppError::Other("account credentials path has no parent directory".into()))?
        .to_path_buf();
    let credential_display = home.map_or_else(
        || credential_file.display().to_string(),
        |home| crate::config::tildify(&credential_file, home),
    );

    if !already_existed {
        crate::config::add_anthropic_account_to_doc(&mut doc, label, &credential_display)?;
    }

    let directory_was_missing = !account_dir.exists();
    std::fs::create_dir_all(&account_dir).map_err(|error| AppError::io_at(&account_dir, error))?;
    if !already_existed || directory_was_missing {
        restrict_account_dir(&account_dir)?;
    }
    if !already_existed {
        crate::cache::atomic_write(config_path, doc.to_string().as_bytes())?;
    }

    Ok(Registered {
        config_path: config_path.to_path_buf(),
        credential_file,
        account_dir,
        credential_display,
        already_existed,
    })
}

fn restamp_config(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| AppError::io_at(path, error))?;
    let times = std::fs::FileTimes::new().set_modified(std::time::SystemTime::now());
    file.set_times(times)
        .map_err(|error| AppError::io_at(path, error))
}

#[cfg(unix)]
fn restrict_account_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| AppError::io_at(path, error))
}

#[cfg(not(unix))]
fn restrict_account_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn manual_login_hint(account_dir: &Path) -> String {
    format!(
        "Sign in later with:\n\n  {}\n\nThe account appears automatically after login.\n",
        shell_login_command(account_dir)
    )
}

#[cfg(not(windows))]
fn shell_login_command(account_dir: &Path) -> String {
    let escaped = account_dir.display().to_string().replace('\'', "'\\''");
    format!("CLAUDE_CONFIG_DIR='{escaped}' claude")
}

#[cfg(windows)]
fn shell_login_command(account_dir: &Path) -> String {
    let escaped = account_dir.display().to_string().replace('\'', "''");
    format!("$env:CLAUDE_CONFIG_DIR = '{escaped}'; claude")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_uses_isolated_standard_layout() {
        let temporary = tempfile::TempDir::new().unwrap();
        let config_path = temporary.path().join("config.toml");
        let registration = register_at(&config_path, "work", Some(temporary.path())).unwrap();

        assert!(!registration.already_existed);
        assert_eq!(
            registration.credential_file,
            temporary.path().join("accounts/work/.credentials.json")
        );
        assert!(registration.supports_scoped_login());
        let written = std::fs::read_to_string(config_path).unwrap();
        assert!(written.contains("label = \"work\""));
        assert!(written.contains("credentials_path = \"~/accounts/work/.credentials.json\""));
    }

    #[cfg(unix)]
    #[test]
    fn registration_restricts_the_account_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::TempDir::new().unwrap();
        let config_path = temporary.path().join("config.toml");
        let registration = register_at(&config_path, "work", None).unwrap();
        let mode = std::fs::metadata(registration.account_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn existing_account_keeps_its_configured_path() {
        let temporary = tempfile::TempDir::new().unwrap();
        let config_path = temporary.path().join("config.toml");
        let credentials = temporary.path().join("custom/work.json");
        std::fs::write(
            &config_path,
            format!(
                "[[anthropic.accounts]]\nlabel = \"work\"\ncredentials_path = {:?}\n",
                credentials.display().to_string()
            ),
        )
        .unwrap();

        let registration = register_at(&config_path, "work", None).unwrap();
        assert!(registration.already_existed);
        assert_eq!(registration.credential_file, credentials);
        assert!(!registration.supports_scoped_login());
    }

    #[test]
    fn login_hint_quotes_paths_with_spaces() {
        let command = shell_login_command(Path::new("/tmp/Claude Accounts/work"));
        assert!(command.contains("Claude Accounts"));
        #[cfg(not(windows))]
        assert_eq!(
            command,
            "CLAUDE_CONFIG_DIR='/tmp/Claude Accounts/work' claude"
        );
    }

    #[test]
    fn restamp_never_rewrites_config_contents() {
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("config.toml");
        std::fs::write(&path, "# edited while login ran\n").unwrap();
        restamp_config(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "# edited while login ran\n"
        );
    }
}
