//! Linux OneDrive GUI — a tray icon and settings window for the OneDrive Client for Linux.
//!
//!   linux-onedrive-gui              open the window (and start the tray icon if it isn't running)
//!   linux-onedrive-gui --tray       tray icon only (used at login)
//!   linux-onedrive-gui --client …   run the bundled onedrive client with the given arguments

mod core;
mod graph;
mod tray;
mod ui;

use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--client") => run_client(&args[1..]),
        Some("--tray") => {
            if tray::already_running() {
                return;
            }
            tray::run()
        }
        Some("--version") => println!("{} {}", core::APP_NAME, env!("CARGO_PKG_VERSION")),
        Some("--help") | Some("-h") => {
            println!("{} {}\n\nUsage:\n  {} [--settings] [--account <config dir>]\n  {} --tray\n  {} --client <onedrive arguments>",
                core::APP_NAME, env!("CARGO_PKG_VERSION"), core::APP_ID, core::APP_ID, core::APP_ID)
        }
        _ => {
            let account = args.iter().position(|a| a == "--account").and_then(|i| args.get(i + 1)).cloned();
            ui::run(account)
        }
    }
}

fn run_client(args: &[String]) {
    let Some(bin) = core::client_binary() else {
        eprintln!("The onedrive client wasn't found. Reinstall {} or install the 'onedrive' package.", core::APP_NAME);
        std::process::exit(127);
    };
    let err = std::process::Command::new(bin).args(args).exec();
    eprintln!("Couldn't start the onedrive client: {err}");
    std::process::exit(127);
}
