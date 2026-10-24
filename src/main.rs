//! DriveDeck for OneDrive — a tray icon and settings window for the OneDrive Client for Linux.
//!
//!   drivedeck              open the window (and start the tray icon if it isn't running)
//!   drivedeck --tray       tray icon only (used at login)
//!   drivedeck --popup [x y] the panel a click on the tray icon opens (x, y: where)
//!   drivedeck --client …   run the installed onedrive client (kept for units written by
//!                                   older versions, which ran the client through the app)

mod activation;
mod advanced;
mod backup;
mod clientinstall;
mod conflict;
mod core;
mod dbus;
mod filewin;
mod glass;
mod graph;
mod popup;
mod rename;
mod share;
#[cfg(feature = "test-driver")]
mod testdrive;
mod tray;
mod ui;
mod uninstall;
mod update;

use std::os::unix::process::CommandExt;

/// An AppImage stays mounted until every process holding its runtime's keep-alive pipe has
/// exited, and that pipe isn't close-on-exec. So everything we start (log readers, the tray that
/// replaces us after an update) would inherit it, and keep an old version mounted, with its
/// runtime running, for as long as the new version runs. We keep our own copy; children get none.
/// Nothing we start runs from inside our mount: we relaunch ourselves through the AppImage file.
fn keep_inherited_files_to_ourselves() {
    if std::env::var_os("APPIMAGE").is_none() {
        return;
    }
    // SAFETY: only sets the close-on-exec flag on descriptors >= 3; runs before any other thread
    let r = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if r != 0 {
        // kernels before 5.11
        let fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .filter(|&fd| fd > 2)
            .collect();
        for fd in fds {
            // SAFETY: as above
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }
}

fn main() {
    keep_inherited_files_to_ourselves();
    #[cfg(feature = "test-driver")]
    testdrive::take_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    core::note_program_file();
    match args.first().map(String::as_str) {
        Some("--client") => run_client(&args[1..]),
        Some("--tray") => {
            // A tray restarting itself after an update waits for the old one to finish leaving.
            let tries = if std::env::var_os(tray::RESTART_ENV).is_some() {
                40
            } else {
                1
            };
            let mut claim = dbus::Claim::TakenByAnother;
            let mut unavailable = 0;
            for _ in 0..tries + 8 {
                claim = dbus::claim_name(core::TRAY_BUS_NAME);
                match claim {
                    dbus::Claim::Owned(_) => break,
                    // the session bus can be slow to accept connections when many programs
                    // start at once (login, or several windows): try again before running
                    // without the one-tray guard
                    dbus::Claim::Unavailable => {
                        unavailable += 1;
                        if unavailable > 8 {
                            break;
                        }
                    }
                    dbus::Claim::TakenByAnother if tries == 1 => break,
                    dbus::Claim::TakenByAnother => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            // held for the life of the process; a second tray finds the name taken and leaves
            let _instance = match claim {
                dbus::Claim::TakenByAnother => return,
                dbus::Claim::Owned(c) => Some(c),
                dbus::Claim::Unavailable => None,
            };
            rename::migrate();
            tray::run()
        }
        Some("--popup") => {
            let at = match (args.get(1), args.get(2)) {
                (Some(x), Some(y)) => x.parse().ok().zip(y.parse().ok()),
                _ => None,
            };
            popup::run(at)
        }
        Some("--share-link") => args
            .get(1)
            .iter()
            .for_each(|p| share::share_link(std::path::Path::new(p), false)),
        Some("--share-edit-link") => args
            .get(1)
            .iter()
            .for_each(|p| share::share_link(std::path::Path::new(p), true)),
        Some("--versions") => args
            .get(1)
            .iter()
            .for_each(|p| filewin::run(filewin::Mode::Versions, std::path::Path::new(p))),
        Some("--share-people") => args
            .get(1)
            .iter()
            .for_each(|p| filewin::run(filewin::Mode::Share, std::path::Path::new(p))),
        Some("--view-online") => args
            .get(1)
            .iter()
            .for_each(|p| share::view_online(std::path::Path::new(p))),
        Some("--version") => println!("{} {}", core::APP_NAME, env!("CARGO_PKG_VERSION")),
        Some("--help") | Some("-h") => {
            println!(
                "{} {}\n\nUsage:\n  {} [--settings] [--account <config dir>]\n  {} --tray\n  {} --share-link <file>\n  {} --share-edit-link <file>\n  {} --share-people <file>\n  {} --versions <file>\n  {} --view-online <file>",
                core::APP_NAME,
                env!("CARGO_PKG_VERSION"),
                core::APP_ID,
                core::APP_ID,
                core::APP_ID,
                core::APP_ID,
                core::APP_ID,
                core::APP_ID,
                core::APP_ID
            )
        }
        _ => {
            // (a window of an older version, handed "account\ntoken", passes it all on here)
            let account = args
                .iter()
                .position(|a| a == "--account")
                .and_then(|i| args.get(i + 1))
                .map(|a| a.split('\n').next().unwrap_or_default().to_string());
            // before anything reads the settings
            rename::migrate();
            ui::run(account)
        }
    }
}

fn run_client(args: &[String]) {
    let Some(bin) = core::client_binary() else {
        eprintln!(
            "The OneDrive Client for Linux isn't installed. See {}",
            core::CLIENT_INSTALL_URL
        );
        std::process::exit(127);
    };
    let err = std::process::Command::new(bin).args(args).exec();
    eprintln!("Couldn't start the onedrive client: {err}");
    std::process::exit(127);
}
