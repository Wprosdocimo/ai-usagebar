//! Waybar widget binary. The library does all the work — this is just the
//! tokio bootstrap + clap parse.

use ai_usagebar::widget::cli::{Cli, Command, LoginVendor};
use ai_usagebar::widget::run::run;
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    if let Some(Command::Account { action }) = &cli.command {
        std::process::exit(ai_usagebar::account::run(action));
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => {
            // Catastrophic — emit the always-valid ⚠ JSON and exit 0.
            println!(
                r#"{{"text":"⚠","tooltip":"failed to create tokio runtime","class":"critical"}}"#
            );
            std::process::exit(0);
        }
    };
    if let Some(Command::Login { vendor }) = &cli.command {
        let code = match vendor {
            LoginVendor::Copilot => rt.block_on(login_copilot()),
        };
        std::process::exit(code);
    }
    // An administrative report, not the widget: it needs the runtime but must
    // not go through the always-exit-0 Waybar contract — a script piping this
    // deserves a real exit code.
    if let Some(Command::Usage { json }) = &cli.command {
        std::process::exit(rt.block_on(ai_usagebar::report::run(*json)));
    }
    let code = rt.block_on(run(cli));
    std::process::exit(code);
}

/// `ai-usagebar login copilot` — interactive GitHub device-code flow, then
/// persist the resulting token so the widget/TUI's `copilot` vendor can read
/// it. This is the one vendor here with no existing CLI/IDE credential to
/// read (see `ai_usagebar::copilot::creds` for why), so it's also the only
/// one with a login subcommand of its own.
async fn login_copilot() -> i32 {
    // A broken config.toml must not silently redirect the credentials to the
    // default path — warn, then carry on with defaults so the login still works.
    let config = match ai_usagebar::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "ai-usagebar login copilot: warning: could not load config ({e}); using defaults"
            );
            ai_usagebar::config::Config::default()
        }
    };
    let path = match config.copilot.credentials_path.clone() {
        Some(p) => p,
        None => match ai_usagebar::copilot::creds::default_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("ai-usagebar login copilot: {e}");
                return 1;
            }
        },
    };

    let client = match reqwest::Client::builder()
        .timeout(ai_usagebar::vendor::HTTP_CLIENT_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ai-usagebar login copilot: {e}");
            return 1;
        }
    };

    let token = match ai_usagebar::copilot::device_flow::login(&client).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ai-usagebar login copilot: {e}");
            return 1;
        }
    };

    let creds = ai_usagebar::copilot::creds::Credentials {
        access_token: token,
    };
    if let Err(e) = ai_usagebar::copilot::creds::write_to(&path, &creds) {
        eprintln!("ai-usagebar login copilot: {e}");
        return 1;
    }

    println!("Saved credentials to {}.", path.display());
    println!("Enable the vendor with `[copilot] enabled = true` in config.toml,");
    println!("or try it now: `ai-usagebar --vendor copilot`.");
    0
}
