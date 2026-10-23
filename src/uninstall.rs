//! Removing the app from this computer, optionally together with every OneDrive sync setup.
//!
//! Package managers can't ask questions on removal (graphical software centres show no prompts,
//! and rpm can't prompt at all), so the choice is made here, in the app, before it removes itself.
//! Synced files are never touched.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::core::{self, AppSettings};

/// How this copy of the app got onto the computer, which decides how it's removed.
#[derive(Debug, PartialEq)]
pub enum Installed {
    AppImage(PathBuf),
    /// A .deb or .rpm, removed with the command shown (run through pkexec for the password).
    Package(Vec<String>),
    /// A package we can't remove ourselves (no pkexec, unknown package manager).
    PackageManual,
    /// Run from a build directory: nothing to remove.
    BuildDirectory,
}

pub fn installed() -> Installed {
    if let Ok(a) = std::env::var("APPIMAGE") {
        return Installed::AppImage(a.into());
    }
    let me = core::self_command();
    if !me.starts_with("/usr/") {
        return Installed::BuildDirectory;
    }
    let owned_by = |prog: &str, args: &[&str]| {
        core::which(prog).is_some()
            && Command::new(prog)
                .args(args)
                .arg(&me)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    };
    let remove: Option<Vec<&str>> = if owned_by("dpkg", &["-S"]) {
        Some(vec!["apt-get", "remove", "-y", core::APP_ID])
    } else if owned_by("rpm", &["-qf"]) {
        Some(if core::which("dnf").is_some() {
            vec!["dnf", "remove", "-y", core::APP_ID]
        } else if core::which("zypper").is_some() {
            vec!["zypper", "--non-interactive", "remove", core::APP_ID]
        } else {
            vec!["rpm", "-e", core::APP_ID]
        })
    } else {
        None
    };
    match remove {
        Some(cmd) if core::which("pkexec").is_some() => Installed::Package(
            std::iter::once("pkexec")
                .chain(cmd)
                .map(String::from)
                .collect(),
        ),
        _ => Installed::PackageManual,
    }
}

/// One line for the dialog saying what will happen to the app itself.
pub fn removal_summary(how: &Installed) -> String {
    match how {
        Installed::AppImage(p) => format!("The app file {} will be deleted.", p.display()),
        Installed::Package(_) => {
            "The app's package will then be removed; your computer will ask for your password."
                .into()
        }
        Installed::PackageManual => format!(
            "Afterwards, remove the \"{}\" package with your software manager.",
            core::APP_ID
        ),
        Installed::BuildDirectory => {
            "This copy isn't installed, so only your settings are removed.".into()
        }
    }
}

/// Removes this app's own files and, unless `keep_setup`, every OneDrive sync setup for this user.
/// Returns notes for the user. Synced files are never deleted.
pub fn remove_user_files(keep_setup: bool) -> Vec<String> {
    let mut notes = Vec::new();
    let mut settings = AppSettings::load();
    settings.hidden.clear(); // "remove everything" includes accounts hidden from the app
    let accounts = core::discover(&settings);

    // each account's service is stopped below anyway; a tray that's still there afterwards
    // can't restart a service whose file is gone
    if !stop_tray() {
        notes.push(
            "The tray icon didn't close. Log out and back in once this is done to be sure it's gone."
                .into(),
        );
    }
    if keep_setup {
        // the background units run the OneDrive Client for Linux directly, so they carry on
        if accounts.iter().any(|a| a.unit.is_some()) {
            notes.push(
                "Your accounts keep syncing in the background with the OneDrive Client for Linux."
                    .into(),
            );
        }
    } else {
        for acc in &accounts {
            if let Err(e) = core::validate_confdir(&acc.confdir) {
                notes.push(format!(
                    "{} was left untouched because its settings folder couldn't be checked: {e}",
                    acc.name
                ));
                continue;
            }
            let resync = core::resync_unit(&acc.confdir);
            if core::unit_state(&resync).active {
                let (ok, err) = core::systemctl(&["stop", &resync]);
                if !ok {
                    notes.push(format!(
                        "{} was left untouched because its one-time sync job couldn't be stopped: {}",
                        acc.name,
                        err.trim()
                    ));
                    continue;
                }
            }
            if let Some(unit) = &acc.unit {
                let (ok, err) = core::systemctl(&["stop", unit]);
                if !ok {
                    notes.push(format!(
                        "{} was left untouched because its sync service couldn't be stopped: {}",
                        acc.name,
                        err.trim()
                    ));
                    continue;
                }
                let (ok, err) = core::systemctl(&["disable", unit]);
                if !ok {
                    notes.push(format!(
                        "{} was stopped, but its service couldn't be disabled. Its files and service were left intact: {}",
                        acc.name,
                        err.trim()
                    ));
                    continue;
                }
                let file = core::user_unit_dir().join(unit);
                if file.exists()
                    && let Err(e) = fs::remove_file(&file)
                {
                    notes.push(format!(
                            "{} was stopped, but its service file couldn't be removed. Its account files were left intact: {e}",
                            acc.name
                        ));
                    continue;
                }
            }
            if core::client_binary().is_some() {
                let _ = core::client_command()
                    .arg("--confdir")
                    .arg(&acc.confdir)
                    .arg("--logout")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            match core::delete_confdir(&acc.confdir) {
                Ok(gone) => {
                    notes.push(format!(
                        "{}: signed out and its sync settings removed. The files in {} are still there.",
                        acc.name,
                        acc.sync_dir.display()
                    ));
                    if !gone {
                        notes.push(format!(
                            "{} also holds files that aren't the OneDrive client's, so they and the folder were kept.",
                            acc.confdir.display()
                        ));
                    }
                }
                Err(e) => notes.push(format!("Couldn't delete {}: {e}", acc.confdir.display())),
            }
        }
        let (ok, err) = core::systemctl(&["daemon-reload"]);
        if !ok {
            notes.push(format!(
                "The account services were removed, but systemd couldn't reload its unit list: {}",
                err.trim()
            ));
        }
    }

    // the app's own bits
    let data = core::data_home();
    for f in [
        core::app_dir(),
        core::config_home()
            .join("autostart")
            .join(format!("{}.desktop", core::APP_ID)),
        data.join("applications")
            .join(format!("{}.desktop", core::APP_ID)),
        data.join("icons/hicolor/scalable/apps")
            .join(format!("{}.svg", core::APP_ID)),
        data.join("kio/servicemenus")
            .join(format!("{}.desktop", core::APP_ID)),
    ] {
        let result = if f.is_dir() {
            fs::remove_dir_all(&f)
        } else {
            fs::remove_file(&f)
        };
        if let Err(e) = result
            && e.kind() != std::io::ErrorKind::NotFound
        {
            notes.push(format!("Couldn't remove {}: {e}", f.display()));
        }
    }
    notes
}

/// Asks a running tray icon to go away, so it doesn't restart anything while we clean up.
fn stop_tray() -> bool {
    let Ok(owner) = crate::dbus::name_owner_pid_checked(core::TRAY_BUS_NAME) else {
        return false;
    };
    if let Some(pid) = owner
        && pid != std::process::id()
    {
        // signal through a pidfd, so a pid reused after this point can't be hit
        let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw_fd < 0 {
            return false;
        }
        use std::os::fd::FromRawFd;
        // SAFETY: pidfd_open returned a new owned file descriptor.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd as i32) };
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                std::os::fd::AsRawFd::as_raw_fd(&pidfd),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent != 0 {
            return false;
        }
        for _ in 0..100 {
            if crate::dbus::name_owner_pid(core::TRAY_BUS_NAME).is_none() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        return false;
    }
    true
}

/// Removes the app itself. Returns a note when the user has to finish by hand.
pub fn remove_program(how: &Installed) -> Result<(), String> {
    match how {
        Installed::AppImage(p) => fs::remove_file(p).map_err(|e| {
            format!(
                "Couldn't delete {}: {e}. Delete it yourself to finish.",
                p.display()
            )
        }),
        Installed::Package(cmd) => {
            let ok = Command::new(&cmd[0])
                .args(&cmd[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "The package wasn't removed (the password prompt may have been cancelled). Remove \"{}\" with your software manager to finish.",
                    core::APP_ID
                ))
            }
        }
        Installed::PackageManual => Err(format!(
            "Remove the \"{}\" package with your software manager to finish.",
            core::APP_ID
        )),
        Installed::BuildDirectory => Ok(()),
    }
}
