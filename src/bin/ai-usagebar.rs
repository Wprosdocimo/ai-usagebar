//! Waybar widget binary. The library does all the work — this is just the
//! tokio bootstrap + clap parse.

use ai_usagebar::widget::cli::{Cli, Command};
use ai_usagebar::widget::run::run;
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    if let Some(Command::Account { action }) = &cli.command {
        std::process::exit(ai_usagebar::account::run(action));
    }
    if let Some(Command::Settings { action }) = &cli.command {
        std::process::exit(ai_usagebar::tui::settings::run_cli(action));
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
    // An administrative report, not the widget: it needs the runtime but must
    // not go through the always-exit-0 Waybar contract — a script piping this
    // deserves a real exit code.
    if let Some(Command::Usage { json }) = &cli.command {
        std::process::exit(rt.block_on(ai_usagebar::report::run(*json)));
    }
    let code = rt.block_on(run(cli));
    std::process::exit(code);
}
