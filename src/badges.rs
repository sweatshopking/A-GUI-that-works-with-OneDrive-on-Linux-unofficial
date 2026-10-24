//! Sync badges for file managers: the tray answers "what state is this file in?" on the session
//! bus, and says when the answer may have changed (docs/badges-design.md). The Dolphin, Nemo and
//! Caja plugins ask; with no tray running they show no badges.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::core::Phase;

pub const BUS_NAME: &str = "io.github.sweatshopking.DriveDeck.Badges";
pub const OBJECT_PATH: &str = "/io/github/sweatshopking/DriveDeck/Badges";
pub const INTERFACE: &str = "io.github.sweatshopking.DriveDeck.Badges";

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

/// The tray calls this whenever an account changes, and every few seconds. Plugins are told to
/// ask again only when something a badge depends on changed, and at most once a second: each
/// "Changed" makes them ask about every file they show, and a big sync changes state constantly.
pub fn publish(accs: Vec<AccState>) {
    static PENDING: Mutex<(Option<std::time::Instant>, bool)> = Mutex::new((None, false));
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let changed = *st != accs;
    if changed {
        *st = accs;
    }
    drop(st);
    let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    p.1 |= changed;
    let due =
        p.0.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1));
    if !(p.1 && due) {
        return; // the tray's next call (within a few seconds) sends it
    }
    *p = (Some(std::time::Instant::now()), false);
    drop(p);
    // on a thread of its own: the tray calls this from inside its icon's async runtime, where a
    // blocking D-Bus call panics
    std::thread::spawn(|| {
        if let Some(c) = CONN.get() {
            let _ = c.emit_signal(
                None::<&str>,
                OBJECT_PATH,
                INTERFACE,
                "Changed",
                &(Vec::<String>::new(),),
            );
        }
    });
}

/// Whether the user wants badges (App settings; on unless switched off). Asked on every log
/// line, so the answer is kept for a few seconds.
pub fn enabled() -> bool {
    static CACHE: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, on)) = *c
        && at.elapsed() < std::time::Duration::from_secs(3)
    {
        return on;
    }
    let on = crate::core::AppSettings::load().show_badges.unwrap_or(true);
    *c = Some((std::time::Instant::now(), on));
    on
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

const EXTENSION: &str = include_str!("../plugins/nemo-caja/drivedeck-badges.py");
pub const EMBLEMS: [(&str, &str); 4] = [
    (
        "emblem-drivedeck-synced.svg",
        include_str!("../plugins/emblems/emblem-drivedeck-synced.svg"),
    ),
    (
        "emblem-drivedeck-syncing.svg",
        include_str!("../plugins/emblems/emblem-drivedeck-syncing.svg"),
    ),
    (
        "emblem-drivedeck-error.svg",
        include_str!("../plugins/emblems/emblem-drivedeck-error.svg"),
    ),
    (
        "emblem-drivedeck-paused.svg",
        include_str!("../plugins/emblems/emblem-drivedeck-paused.svg"),
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
            .join("drivedeck-badges.py");
        let present = crate::core::which(fm).is_some();
        out.push((file, present.then_some(EXTENSION)));
    }
    out
}

/// Called when the tray starts, and when badges are switched on. Does nothing while they're off.
pub fn install_user_files() {
    if !enabled() {
        return;
    }
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

/// When badges are switched off: the Nemo / Caja extension and the pictures go (they're only
/// ours). Returns what couldn't be removed.
pub fn remove_user_files() -> Vec<String> {
    let mut errors = Vec::new();
    for (path, _) in user_files() {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                errors.push(format!("{}: {e}", path.display()))
            }
            _ => {}
        }
    }
    errors
}

/// Dolphin (Plasma 6) loads overlay plugins from the system's Qt 6 plugin folder, under
/// kf6/overlayicon. The .deb and .rpm install ours there; an AppImage carries a copy and offers
/// to install it (one password prompt).
pub const DOLPHIN_PLUGIN: &str = "drivedeck-badges.so";
const QT_PLUGIN_DIRS: [&str; 4] = [
    "/usr/lib/qt6/plugins",
    "/usr/lib64/qt6/plugins",
    "/usr/lib/x86_64-linux-gnu/qt6/plugins",
    "/usr/lib/aarch64-linux-gnu/qt6/plugins",
];

/// Where the Dolphin plugin is installed, if it is.
pub fn dolphin_plugin_path() -> Option<PathBuf> {
    QT_PLUGIN_DIRS
        .iter()
        .map(|d| Path::new(d).join("kf6/overlayicon").join(DOLPHIN_PLUGIN))
        .find(|p| p.exists())
}

/// The system's Qt 6 plugin folder, if KDE Frameworks 6 (Plasma 6) is installed in it.
fn system_plugin_dir() -> Option<PathBuf> {
    QT_PLUGIN_DIRS
        .iter()
        .map(Path::new)
        .find(|d| d.join("kf6").is_dir())
        .map(Path::to_path_buf)
}

/// The plugin an AppImage carries.
fn bundled_plugin() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("APPDIR")?)
        .join("usr/share/drivedeck")
        .join(DOLPHIN_PLUGIN);
    p.exists().then_some(p)
}

/// Whether to offer installing the Dolphin plugin: an AppImage, on Plasma 6, with Dolphin, and
/// the plugin not installed yet.
pub fn dolphin_offer() -> bool {
    bundled_plugin().is_some()
        && crate::core::which("dolphin").is_some()
        && system_plugin_dir().is_some()
        && dolphin_plugin_path().is_none()
}

/// Installs the AppImage's Dolphin plugin for everyone on the computer (asks for the password).
/// The file goes to the installer through a pipe: root can't read inside an AppImage's mount.
pub fn install_dolphin_plugin() -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let src = bundled_plugin().ok_or("This copy of the app has no Dolphin plugin.")?;
    let dir = system_plugin_dir()
        .ok_or("Dolphin badges need Plasma 6.")?
        .join("kf6/overlayicon");
    let bytes = std::fs::read(&src).map_err(|e| format!("Couldn't read the plugin: {e}"))?;
    let script = r#"umask 022; mkdir -p "$1" && cat > "$1/.drivedeck-badges.so.new" && mv -f "$1/.drivedeck-badges.so.new" "$1/drivedeck-badges.so""#;
    let mut child = Command::new(crate::clientinstall::elevate())
        .args(["/bin/sh", "-c", script, "sh"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Couldn't ask for the password: {e}"))?;
    // a closed password window ends the installer before it reads anything
    let _ = child.stdin.take().map(|mut i| i.write_all(&bytes));
    let out = child
        .wait_with_output()
        .map_err(|e| format!("The installer didn't finish: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(126) => Err("Nothing was changed (the password window was closed).".into()),
        Some(127) => Err("Nothing was changed: your computer didn't allow it.".into()),
        _ => Err(format!(
            "That didn't work: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// For uninstalling an AppImage: removes the Dolphin plugin it installed, unless a package owns
/// that file. Ok(false) when there was nothing of ours to remove.
pub fn remove_dolphin_plugin() -> Result<bool, String> {
    let Some(path) = dolphin_plugin_path() else {
        return Ok(false);
    };
    let owned = |cmd: &str, arg: &str| {
        crate::core::which(cmd).is_some()
            && std::process::Command::new(cmd)
                .args([arg])
                .arg(&path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    };
    if owned("dpkg", "-S") || owned("rpm", "-qf") {
        return Ok(false);
    }
    let out = std::process::Command::new(crate::clientinstall::elevate())
        .args(["/bin/rm", "-f", "--"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't ask for the password: {e}"))?;
    if out.status.success() {
        Ok(true)
    } else {
        Err(format!(
            "The Dolphin badge plugin is still at {} (remove it with sudo rm).",
            path.display()
        ))
    }
}

struct Badges;

#[zbus::interface(name = "io.github.sweatshopking.DriveDeck.Badges")]
impl Badges {
    /// One answer per path, in the same order.
    fn get_status(&self, paths: Vec<String>) -> Vec<String> {
        let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
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
