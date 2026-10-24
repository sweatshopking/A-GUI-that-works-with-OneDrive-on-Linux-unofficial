//! 0.5.0 renamed the app from "A GUI that works with OneDrive on Linux" (linux-onedrive-gui) to
//! DriveDeck. Moves this user's own bits to the new name once, when the window or tray starts.
//! Only what the old version made for itself is touched. The client's config files are left as
//! they are, so the old log folder (which they may name) stays where it is.
use std::fs;
use std::path::Path;

use crate::core;

pub const OLD_ID: &str = "linux-onedrive-gui";
pub const OLD_TRAY_BUS_NAME: &str = "io.github.sweatshopking.LinuxOnedriveGui.Tray";

pub fn migrate() {
    let config = core::config_home();
    let data = core::data_home();

    // settings
    let old_dir = config.join(OLD_ID);
    let new_dir = core::app_dir();
    if old_dir.is_dir()
        && !new_dir.exists()
        && let Err(e) = fs::rename(&old_dir, &new_dir)
    {
        eprintln!(
            "couldn't move {} to {}: {e}",
            old_dir.display(),
            new_dir.display()
        );
    }

    // start at login: the same entry (including any change the user made to it) under the new name
    let old_auto = config.join("autostart").join(format!("{OLD_ID}.desktop"));
    if let Ok(text) = fs::read_to_string(&old_auto) {
        let new_auto = crate::ui::autostart_file();
        let written = new_auto.exists()
            || core::atomic_write(&new_auto, renamed_autostart(&text).as_bytes()).is_ok();
        if written {
            let _ = fs::remove_file(&old_auto);
        }
    }

    // an AppImage's menu entry, icons and Dolphin actions
    let old_menu = data.join("applications").join(format!("{OLD_ID}.desktop"));
    if old_menu.exists() {
        let replaced =
            std::env::var_os("APPIMAGE").is_none() || crate::share::install_menu_entry().is_ok();
        if replaced {
            remove(&old_menu);
            remove(&data.join(format!("icons/hicolor/scalable/apps/{OLD_ID}.svg")));
            remove(&data.join(format!("kio/servicemenus/{OLD_ID}.desktop")));
        }
    }
    for f in old_user_icons(&data) {
        remove(&f);
    }

    // previews of earlier versions of files: only a cache
    let _ = fs::remove_dir_all(core::cache_home().join(OLD_ID));

    // a tray of the old version still running (an AppImage, or until the next login) would show
    // a second icon next to ours
    if crate::dbus::name_owner_pid(OLD_TRAY_BUS_NAME).is_some() {
        crate::uninstall::stop_tray_named(OLD_TRAY_BUS_NAME);
    }
}

/// The old version's status icons an AppImage kept in the user's icons.
pub fn old_user_icons(data: &Path) -> Vec<std::path::PathBuf> {
    ["ok", "sync", "error", "off"]
        .iter()
        .map(|s| data.join(format!("icons/hicolor/scalable/status/{OLD_ID}-{s}.svg")))
        .collect()
}

/// Every file the old version made for this user, for uninstalling (the settings folder moved).
pub fn old_user_files() -> Vec<std::path::PathBuf> {
    let (config, data) = (core::config_home(), core::data_home());
    let mut files = vec![
        config.join(OLD_ID),
        config.join("autostart").join(format!("{OLD_ID}.desktop")),
        data.join("applications").join(format!("{OLD_ID}.desktop")),
        data.join(format!("icons/hicolor/scalable/apps/{OLD_ID}.svg")),
        data.join(format!("kio/servicemenus/{OLD_ID}.desktop")),
    ];
    files.extend(old_user_icons(&data));
    files
}

fn remove(f: &Path) {
    if let Err(e) = fs::remove_file(f)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("couldn't remove {}: {e}", f.display());
    }
}

fn renamed_autostart(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.starts_with("Name=") {
                format!("Name={}", core::APP_NAME)
            } else if l.starts_with("Exec=") {
                format!("Exec={} --tray", core::desktop_exec())
            } else if l.starts_with("Icon=") {
                format!("Icon={}", core::APP_ID)
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_keeps_user_changes() {
        let old = "[Desktop Entry]\nType=Application\nName=A GUI that works with OneDrive on Linux (unofficial)\nExec=\"/usr/bin/linux-onedrive-gui\" --tray\nIcon=linux-onedrive-gui\nX-GNOME-Autostart-enabled=false\n";
        let new = renamed_autostart(old);
        assert!(new.contains("Name=DriveDeck for OneDrive\n"));
        assert!(new.contains("Icon=drivedeck\n"));
        assert!(new.contains(" --tray\n"));
        assert!(!new.contains("linux-onedrive-gui"));
        assert!(new.contains("X-GNOME-Autostart-enabled=false\n"));
    }
}
