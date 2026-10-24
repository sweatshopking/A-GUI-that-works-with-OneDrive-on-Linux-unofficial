//! File-manager actions: copy a sharing link for a synced file, or open it on the OneDrive website.
//! Also desktop notifications and clipboard helpers shared with the tray.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::core::{self, AppSettings};
use crate::graph;

/// Sends a desktop notification.
pub fn notify(summary: &str, body: &str) {
    crate::dbus::notify(core::APP_NAME, core::APP_ID, summary, body);
}

/// Puts text on the clipboard: KDE's Klipper first, then the usual command-line tools.
pub fn copy_to_clipboard(text: &str) -> bool {
    let klipper = crate::dbus::klipper_set(text);
    if klipper {
        return true;
    }
    for (prog, args) in [
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ] {
        if core::which(prog).is_none() {
            continue;
        }
        if let Ok(mut c) = Command::new(prog)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            use std::io::Write;
            let Some(mut i) = c.stdin.take() else {
                let _ = c.kill();
                continue;
            };
            if i.write_all(text.as_bytes()).is_err() {
                let _ = c.kill();
                continue;
            }
            drop(i);
            match c.try_wait() {
                Ok(Some(status)) if !status.success() => continue,
                Err(_) => continue,
                _ => {}
            }
            // wl-copy and xclip keep running to serve the clipboard, so don't wait for them
            return true;
        }
    }
    false
}

/// Finds which account a local path belongs to, and its path inside that account.
pub(crate) fn locate(path: &Path) -> Result<(core::AccountInfo, String), String> {
    let path = path
        .canonicalize()
        .map_err(|_| format!("{} doesn't exist.", path.display()))?;
    let settings = AppSettings::load();
    let mut best: Option<(core::AccountInfo, String, usize)> = None;
    for a in core::discover(&settings) {
        let root = a
            .sync_dir
            .canonicalize()
            .unwrap_or_else(|_| a.sync_dir.clone());
        if let Ok(rel) = path.strip_prefix(&root) {
            let rel = rel.to_string_lossy().to_string();
            let depth = root.components().count();
            // prefer the most specific folder if accounts are nested
            if best
                .as_ref()
                .is_none_or(|(_, _, best_depth)| depth > *best_depth)
            {
                best = Some((a, rel, depth));
            }
        }
    }
    best.map(|(account, rel, _)| (account, rel))
        .ok_or_else(|| "That isn't inside any of your OneDrive folders.".into())
}

/// Copies a sharing link to `path`: view-only, or one that lets people edit (change, add or
/// delete what's in a folder).
pub fn share_link(path: &Path, edit: bool) {
    let result = locate(path).and_then(|(acc, rel)| {
        // a link to the top of an account would expose everything in it
        if rel.is_empty() {
            return Err(format!(
                "That's the whole of \"{}\". Share a file or folder inside it instead.",
                acc.name
            ));
        }
        let t = graph::token(&acc.confdir)?;
        graph::create_link(&t, acc.drive_id.as_deref(), &rel, &acc.kind, edit)
    });
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    match result {
        Ok((link, scope)) => {
            let who = match scope {
                graph::LinkScope::Anyone => "Anyone with the link",
                graph::LinkScope::Organization => "People in your organization with the link",
            };
            let title = if copy_to_clipboard(&link) {
                "Link copied"
            } else {
                "Sharing link"
            };
            let can = match (edit, path.is_dir()) {
                (false, _) => "can view",
                (true, false) => "can edit",
                (true, true) => "can change, add and delete files in",
            };
            notify(title, &format!("{who} {can} \"{name}\".\n{link}"));
        }
        Err(e) => notify("Couldn't create a link", &format!("{name}: {e}")),
    }
}

pub fn view_online(path: &Path) {
    let result = locate(path).and_then(|(acc, rel)| {
        let t = graph::token(&acc.confdir)?;
        graph::item_web_url(&t, acc.drive_id.as_deref(), &rel)
    });
    match result {
        Ok(url) => core::run_detached("xdg-open", &[&url]),
        Err(e) => notify("Couldn't open it online", &e),
    }
}

fn service_menu_body() -> String {
    include_str!("../packaging/linux-onedrive-gui-servicemenu.desktop").replace(
        "Exec=linux-onedrive-gui",
        &format!("Exec={}", core::desktop_exec()),
    )
}

/// The tray's status pictures. Plasma looks a tray icon's name up in the icon theme before the
/// folder the app points it to, and without a match falls back to the plain app icon (no badge);
/// packages install these system-wide, an AppImage keeps a copy in the user's icons.
const STATUS_ICONS: [(&str, &str); 4] = [
    (
        "linux-onedrive-gui-ok.svg",
        include_str!("../assets/icons/hicolor/scalable/status/linux-onedrive-gui-ok.svg"),
    ),
    (
        "linux-onedrive-gui-sync.svg",
        include_str!("../assets/icons/hicolor/scalable/status/linux-onedrive-gui-sync.svg"),
    ),
    (
        "linux-onedrive-gui-error.svg",
        include_str!("../assets/icons/hicolor/scalable/status/linux-onedrive-gui-error.svg"),
    ),
    (
        "linux-onedrive-gui-off.svg",
        include_str!("../assets/icons/hicolor/scalable/status/linux-onedrive-gui-off.svg"),
    ),
];
const APP_ICON: &str = include_str!("../assets/linux-onedrive-gui.svg");

/// The status icons an AppImage keeps in the user's icons (for uninstall; the menu icon is
/// removed with the menu entry).
pub fn user_icon_files() -> Vec<PathBuf> {
    let status = core::data_home().join("icons/hicolor/scalable/status");
    STATUS_ICONS
        .iter()
        .map(|(name, _)| status.join(name))
        .collect()
}

/// For an AppImage: writes the tray's status icons, and updates the menu icon if the user added
/// one, so both match this version. Only writes what's missing or different.
pub fn refresh_user_icons() {
    if std::env::var_os("APPIMAGE").is_none() {
        return;
    }
    let icons = core::data_home().join("icons/hicolor/scalable");
    let app = icons.join("apps").join(format!("{}.svg", core::APP_ID));
    let mut files: Vec<(PathBuf, &str)> = STATUS_ICONS
        .iter()
        .map(|(name, body)| (icons.join("status").join(name), *body))
        .collect();
    if app.exists() {
        files.push((app, APP_ICON));
    }
    for (path, body) in files {
        if fs::read_to_string(&path).ok().as_deref() == Some(body) {
            continue;
        }
        let r = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| core::atomic_write(&path, body.as_bytes()));
        if let Err(e) = r {
            eprintln!("couldn't write {}: {e}", path.display());
        }
    }
}

/// Brings a user's own copy of the Dolphin actions (added from an AppImage) up to date with this
/// version. Does nothing if there's no copy, or it's current.
pub fn refresh_service_menu() {
    let data = core::data_home();
    let file = data.join(format!("kio/servicemenus/{}.desktop", core::APP_ID));
    match fs::read_to_string(&file) {
        Ok(old) if old != service_menu_body() => {
            let _ = install_service_menu(&data);
        }
        _ => {}
    }
}

/// Installs the Dolphin right-click actions for this user (packages ship their own copy).
pub fn install_service_menu(data_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = data_dir.join("kio/servicemenus");
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.desktop", core::APP_ID));
    core::atomic_write(&file, service_menu_body().as_bytes())?;
    // KDE only runs user service menus that are marked executable
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
    Ok(file)
}
