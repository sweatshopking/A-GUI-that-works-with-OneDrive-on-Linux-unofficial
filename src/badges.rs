//! Sync badges for file managers: the tray answers "what state is this file in?" on the session
//! bus, and says when the answer may have changed (docs/badges-design.md). The Dolphin, Nemo and
//! Caja plugins ask; with no tray running they show no badges.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::core::Phase;

pub const BUS_NAME: &str = "io.github.sweatshopking.LinuxOnedriveGui.Badges";
pub const OBJECT_PATH: &str = "/io/github/sweatshopking/LinuxOnedriveGui/Badges";
pub const INTERFACE: &str = "io.github.sweatshopking.LinuxOnedriveGui.Badges";

/// One account, as the badges see it.
#[derive(Clone, Debug, PartialEq)]
pub struct AccState {
    pub root: PathBuf,
    pub phase: Phase,
    /// files (relative to root) being uploaded or downloaded right now
    pub moving: HashSet<String>,
    /// files (relative to root) the client left unsynced
    pub blocked: HashSet<String>,
}

static STATE: Mutex<Vec<AccState>> = Mutex::new(Vec::new());
static CONN: OnceLock<zbus::blocking::Connection> = OnceLock::new();

/// The tray calls this whenever an account changes; plugins are told to ask again only when
/// something a badge depends on actually changed.
pub fn publish(accs: Vec<AccState>) {
    let mut st = STATE.lock().unwrap();
    if *st == accs {
        return;
    }
    *st = accs;
    drop(st);
    if let Some(c) = CONN.get() {
        let _ = c.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            INTERFACE,
            "Changed",
            &(Vec::<String>::new(),),
        );
    }
}

/// "synced", "syncing", "error", "paused", or "" (not in a synced folder: no badge).
pub fn status_of(accs: &[AccState], path: &Path) -> &'static str {
    // the deepest root wins, for a SharePoint library synced inside another account's folder
    let Some((a, rel)) = accs
        .iter()
        .filter_map(|a| Some((a, path.strip_prefix(&a.root).ok()?)))
        .max_by_key(|(a, _)| a.root.components().count())
    else {
        return "";
    };
    let rel = rel.to_string_lossy();
    // a folder takes the state of anything inside it
    let inside = |set: &HashSet<String>| {
        rel.is_empty() && !set.is_empty()
            || set.iter().any(|f| {
                f == rel.as_ref()
                    || f.strip_prefix(rel.as_ref())
                        .is_some_and(|tail| tail.starts_with('/'))
            })
    };
    if inside(&a.blocked) {
        return "error";
    }
    if inside(&a.moving) {
        return "syncing";
    }
    match a.phase {
        Phase::Paused | Phase::Off | Phase::Stopped => "paused",
        Phase::Error | Phase::SignedOut | Phase::NeedsResync | Phase::BigDelete => "error",
        // before the first log line, don't claim anything
        Phase::Unknown => "",
        Phase::Syncing | Phase::Resyncing | Phase::UpToDate => "synced",
    }
}

const EXTENSION: &str = include_str!("../plugins/nemo-caja/linux-onedrive-gui-badges.py");
pub const EMBLEMS: [(&str, &str); 4] = [
    (
        "emblem-linux-onedrive-gui-synced.svg",
        include_str!("../plugins/emblems/emblem-linux-onedrive-gui-synced.svg"),
    ),
    (
        "emblem-linux-onedrive-gui-syncing.svg",
        include_str!("../plugins/emblems/emblem-linux-onedrive-gui-syncing.svg"),
    ),
    (
        "emblem-linux-onedrive-gui-error.svg",
        include_str!("../plugins/emblems/emblem-linux-onedrive-gui-error.svg"),
    ),
    (
        "emblem-linux-onedrive-gui-paused.svg",
        include_str!("../plugins/emblems/emblem-linux-onedrive-gui-paused.svg"),
    ),
];

/// The per-user files: the badge pictures, and the Nemo / Caja extension for whichever of them
/// is installed. Written only when missing or out of date; returns every path (for uninstall).
pub fn user_files() -> Vec<(std::path::PathBuf, Option<&'static str>)> {
    let data = crate::core::data_home();
    let mut out: Vec<_> = EMBLEMS
        .iter()
        .map(|(name, body)| {
            (
                data.join("icons/hicolor/scalable/emblems").join(name),
                Some(*body),
            )
        })
        .collect();
    for fm in ["nemo", "caja"] {
        let file = data
            .join(format!("{fm}-python/extensions"))
            .join("linux-onedrive-gui-badges.py");
        let present = crate::core::which(fm).is_some();
        out.push((file, present.then_some(EXTENSION)));
    }
    out
}

/// Called when the tray starts.
pub fn install_user_files() {
    for (path, body) in user_files() {
        let Some(body) = body else { continue };
        if std::fs::read_to_string(&path).ok().as_deref() == Some(body) {
            continue;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = crate::core::atomic_write(&path, body.as_bytes()) {
            eprintln!("couldn't write {}: {e}", path.display());
        }
    }
}

struct Badges;

#[zbus::interface(name = "io.github.sweatshopking.LinuxOnedriveGui.Badges")]
impl Badges {
    /// One answer per path, in the same order.
    fn get_status(&self, paths: Vec<String>) -> Vec<String> {
        let st = STATE.lock().unwrap();
        paths
            .iter()
            .map(|p| status_of(&st, Path::new(p)).to_string())
            .collect()
    }
}

/// Puts the badge service on the session bus. Another tray already serving it (a second login
/// on the same bus) just means this one stays quiet.
pub fn serve() {
    let conn = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.name(BUS_NAME))
        .and_then(|b| b.serve_at(OBJECT_PATH, Badges))
        .and_then(|b| b.build());
    match conn {
        Ok(c) => {
            let _ = CONN.set(c);
        }
        Err(e) => eprintln!("file manager badges unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(root: &str, phase: Phase, moving: &[&str], blocked: &[&str]) -> AccState {
        AccState {
            root: root.into(),
            phase,
            moving: moving.iter().map(|s| s.to_string()).collect(),
            blocked: blocked.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn states() {
        let a = [acc(
            "/h/OneDrive",
            Phase::UpToDate,
            &["Docs/big.mkv"],
            &["Work/locked.xlsx"],
        )];
        let s = |p: &str| status_of(&a, Path::new(p));
        assert_eq!(s("/h/elsewhere/x"), "");
        assert_eq!(s("/h/OneDriveX/x"), "");
        assert_eq!(s("/h/OneDrive/a.txt"), "synced");
        assert_eq!(s("/h/OneDrive/Docs/big.mkv"), "syncing");
        assert_eq!(s("/h/OneDrive/Docs"), "syncing");
        assert_eq!(s("/h/OneDrive/Doc"), "synced");
        assert_eq!(s("/h/OneDrive/Work"), "error");
        assert_eq!(s("/h/OneDrive/Work/locked.xlsx"), "error");
        assert_eq!(s("/h/OneDrive"), "error");
    }

    #[test]
    fn phases_and_nesting() {
        let a = [
            acc("/h/OneDrive", Phase::Paused, &[], &[]),
            acc("/h/OneDrive/Team", Phase::UpToDate, &[], &[]),
        ];
        assert_eq!(status_of(&a, Path::new("/h/OneDrive/x")), "paused");
        assert_eq!(status_of(&a, Path::new("/h/OneDrive/Team/x")), "synced");
        let b = [acc("/h/OneDrive", Phase::Unknown, &[], &[])];
        assert_eq!(status_of(&b, Path::new("/h/OneDrive/x")), "");
        let c = [acc("/h/OneDrive", Phase::SignedOut, &[], &[])];
        assert_eq!(status_of(&c, Path::new("/h/OneDrive/x")), "error");
    }
}
