//! A GUI that works with OneDrive on Linux (unofficial) — a tray icon and settings window for the OneDrive Client for Linux.
//!
//!   linux-onedrive-gui              open the window (and start the tray icon if it isn't running)
//!   linux-onedrive-gui --tray       tray icon only (used at login)
//!   linux-onedrive-gui --client …   run the bundled onedrive client with the given arguments

mod core;
mod dbus;
mod graph;
mod share;
#[cfg(feature = "test-driver")]
mod testdrive;
mod tray;
mod ui;

use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--client") => run_client(&args[1..]),
        Some("--tray") => {
            // held for the life of the process; a second tray finds the name taken and leaves
            let _instance = match dbus::claim_name(core::TRAY_BUS_NAME) {
                dbus::Claim::TakenByAnother => return,
                dbus::Claim::Owned(c) => Some(c),
                dbus::Claim::Unavailable => None,
            };
            tray::run()
        }
        Some("--share-link") => args
            .get(1)
            .iter()
            .for_each(|p| share::share_link(std::path::Path::new(p))),
        Some("--view-online") => args
            .get(1)
            .iter()
            .for_each(|p| share::view_online(std::path::Path::new(p))),
        Some("--version") => println!("{} {}", core::APP_NAME, env!("CARGO_PKG_VERSION")),
        Some("--help") | Some("-h") => {
            println!(
                "{} {}\n\nUsage:\n  {} [--settings] [--account <config dir>]\n  {} --tray\n  {} --client <onedrive arguments>",
                core::APP_NAME,
                env!("CARGO_PKG_VERSION"),
                core::APP_ID,
                core::APP_ID,
                core::APP_ID
            )
        }
        _ => {
            let account = args
                .iter()
                .position(|a| a == "--account")
                .and_then(|i| args.get(i + 1))
                .cloned();
            ui::run(account)
        }
    }
}

fn run_client(args: &[String]) {
    let Some(bin) = core::client_binary() else {
        eprintln!(
            "The onedrive client wasn't found. Reinstall {} or install the 'onedrive' package.",
            core::APP_NAME
        );
        std::process::exit(127);
    };
    let err = std::process::Command::new(bin).args(args).exec();
    eprintln!("Couldn't start the onedrive client: {err}");
    std::process::exit(127);
}
