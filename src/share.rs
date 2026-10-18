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
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            // wl-copy and xclip keep running to serve the clipboard, so don't wait for them
            return true;
        }
    }
    false
}

/// Finds which account a local path belongs to, and its path inside that account.
fn locate(path: &Path) -> Result<(core::AccountInfo, String), String> {
    let path = path
        .canonicalize()
        .map_err(|_| format!("{} doesn't exist.", path.display()))?;
    let settings = AppSettings::load();
    let mut best: Option<(core::AccountInfo, String)> = None;
    for a in core::discover(&settings) {
        let root = a
            .sync_dir
            .canonicalize()
            .unwrap_or_else(|_| a.sync_dir.clone());
        if let Ok(rel) = path.strip_prefix(&root) {
            let rel = rel.to_string_lossy().to_string();
            // prefer the most specific folder if accounts are nested
            if best
                .as_ref()
                .is_none_or(|(b, _)| b.sync_dir.as_os_str().len() < a.sync_dir.as_os_str().len())
            {
                best = Some((a, rel));
            }
        }
    }
    best.ok_or_else(|| "That isn't inside any of your OneDrive folders.".into())
}

pub fn share_link(path: &Path) {
    let result = locate(path).and_then(|(acc, rel)| {
        // a link to the top of an account would expose everything in it
        if rel.is_empty() {
            return Err(format!(
                "That's the whole of \"{}\". Share a file or folder inside it instead.",
                acc.name
            ));
        }
        let t = graph::token(&acc.confdir)?;
        graph::create_link(&t, acc.drive_id.as_deref(), &rel)
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
            notify(title, &format!("{who} can view \"{name}\".\n{link}"));
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

/// Installs the Dolphin right-click actions for this user (packages ship their own copy).
pub fn install_service_menu(data_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = data_dir.join("kio/servicemenus");
    fs::create_dir_all(&dir)?;
    let body = include_str!("../packaging/linux-onedrive-gui-servicemenu.desktop").replace(
        "Exec=linux-onedrive-gui",
        &format!("Exec={}", core::desktop_exec()),
    );
    let file = dir.join(format!("{}.desktop", core::APP_ID));
    fs::write(&file, body)?;
    // KDE only runs user service menus that are marked executable
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
    Ok(file)
}
