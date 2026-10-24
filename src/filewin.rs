//! A file's own small window, opened from the file manager's OneDrive menu: its version history
//! (open or restore an older version), or sharing it with particular people by email.

use std::path::{Path, PathBuf};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::core::{self, AccountInfo};
use crate::graph::{self, Version};
use crate::ui::{FileWindow, Theme, VersionRow};

const VERSION_CACHE_FILES: usize = 50;
const VERSION_CACHE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Versions,
    Share,
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

/// "23 Oct 2026, 14:05" in local time, or "Today, 14:05".
fn when(rfc3339: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let t = t.with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    if t.date_naive() == today {
        t.format("Today, %H:%M").to_string()
    } else if Some(t.date_naive()) == today.pred_opt() {
        t.format("Yesterday, %H:%M").to_string()
    } else {
        t.format("%-d %b %Y, %H:%M").to_string()
    }
}

/// Runs `work` off the UI thread, then `done` with the window.
fn bg<T: Send + 'static>(
    weak: &slint::Weak<FileWindow>,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&FileWindow, T) + Send + 'static,
) {
    let weak = weak.clone();
    std::thread::spawn(move || {
        let r = work();
        let _ = weak.upgrade_in_event_loop(move |ui| done(&ui, r));
    });
}

fn say(ui: &FileWindow, tone: i32, text: impl AsRef<str>) {
    ui.set_msg(ss(text));
    ui.set_msg_tone(tone);
}

/// Splits what was typed into email addresses; Err names the first one that isn't.
fn parse_emails(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for part in text.split([',', ';', ' ', '\n']).map(str::trim) {
        if part.is_empty() {
            continue;
        }
        let ok = part.split_once('@').is_some_and(|(user, domain)| {
            !user.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
        if !ok {
            return Err(format!("\"{part}\" isn't an email address."));
        }
        if !out.iter().any(|e: &String| e.eq_ignore_ascii_case(part)) {
            out.push(part.to_string());
        }
    }
    if out.is_empty() {
        return Err("Type at least one email address.".into());
    }
    Ok(out)
}

/// Where an opened old version is saved: "Report (23 Oct 2026 1405, 3f2a9c01).docx" in the app's cache.
fn version_copy_path(file: &Path, v: &Version) -> Result<PathBuf, String> {
    let stem = file
        .file_stem()
        .map_or_else(|| "file".into(), |s| s.to_string_lossy().to_string());
    let ext = file
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let stamp = chrono::DateTime::parse_from_rfc3339(&v.modified)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%-d %b %Y %H%M")
                .to_string()
        })
        .unwrap_or_else(|_| "older version".into());
    // two versions saved in the same minute need different names: a short, stable tag of the
    // version id (FNV-1a) tells them apart without a long id in the window title
    let tag = v.id.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    let suffix = format!(" ({stamp}, {tag:08x})");
    let fixed_len = suffix.len() + ext.len();
    if fixed_len >= 255 {
        return Err(
            "This version identifier and file extension are too long to save safely.".into(),
        );
    }
    let max_stem = 255 - fixed_len;
    let mut end = stem.len().min(max_stem);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return Err("This file name is too long to save a version copy safely.".into());
    }
    let dir = core::cache_home().join(core::APP_ID).join("versions");
    Ok(dir.join(format!("{}{}{ext}", &stem[..end], suffix)))
}

/// Keeps opened old versions as a small, reusable cache. A copy currently being opened stays.
fn trim_version_cache(
    dir: &Path,
    keep: &Path,
    max_files: usize,
    max_bytes: u64,
) -> std::io::Result<()> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_file() {
            files.push((
                metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                path,
                metadata.len(),
            ));
        }
    }
    files.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let bytes = files
        .iter()
        .fold(0u64, |total, file| total.saturating_add(file.2));
    let mut count = files.len();
    let mut bytes = bytes;
    for (_, path, size) in files {
        if count <= max_files && bytes <= max_bytes {
            break;
        }
        if path == keep {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(size);
        }
    }
    Ok(())
}

pub fn run(mode: Mode, path: &Path) {
    let located = crate::share::locate(path);
    let (acc, rel) = match located {
        Ok(found) => found,
        Err(e) => {
            crate::share::notify("OneDrive", &e);
            return;
        }
    };
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().to_string());
    if mode == Mode::Versions && path.is_dir() {
        crate::share::notify(
            "No version history for folders",
            &format!(
                "\"{name}\" is a folder. Open a file inside it to see its versions, or the recycle bin online for deleted items."
            ),
        );
        return;
    }
    if mode == Mode::Share && rel.is_empty() {
        crate::share::notify(
            "Couldn't share it",
            &format!(
                "That's the whole of \"{}\". Share a file or folder inside it instead.",
                acc.name
            ),
        );
        return;
    }
    let ui = match FileWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("The window couldn't open: {e}");
            return;
        }
    };
    let _ = slint::set_xdg_app_id(core::APP_ID);
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::dbus::appearance());
        });
        let a = rx
            .recv_timeout(Duration::from_millis(250))
            .unwrap_or_default();
        if let Some(dark) = a.dark {
            ui.set_dark(dark);
        }
        let theme = ui.global::<Theme>();
        theme.set_contrast(a.high_contrast);
        theme.set_reduce_motion(a.reduce_motion);
    }
    ui.set_mode(if mode == Mode::Versions { 0 } else { 1 });
    ui.set_file_name(ss(&name));
    ui.set_account(ss(&acc.name));
    ui.set_is_work(acc.kind != "personal");
    // an account the settings window hasn't looked at yet has no kind: ask Microsoft
    if mode == Mode::Share && acc.kind.is_empty() {
        let confdir = acc.confdir.clone();
        bg(
            &ui.as_weak(),
            move || graph::token(&confdir).and_then(|t| graph::account_kind(&t)),
            |ui, kind| {
                if let Ok(kind) = kind {
                    ui.set_is_work(kind != "personal");
                }
            },
        );
    }
    ui.on_close_window(|| {
        let _ = slint::quit_event_loop();
    });
    let weak = ui.as_weak();
    match mode {
        Mode::Versions => versions(&ui, &weak, acc, rel, path.to_path_buf()),
        Mode::Share => share(&ui, &weak, acc, rel),
    }
    #[cfg(feature = "test-driver")]
    let _driver = crate::testdrive::start(&ui);
    let _ = ui.run();
}

thread_local! {
    /// The versions shown, newest first (the UI thread's own copy)
    static VERSIONS: std::cell::RefCell<Vec<Version>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// (Re)reads the versions and shows them.
fn load_versions(weak: &slint::Weak<FileWindow>, acc: &AccountInfo, rel: &str) {
    if let Some(ui) = weak.upgrade() {
        ui.set_busy(true);
    }
    let (acc, rel) = (acc.clone(), rel.to_string());
    bg(
        weak,
        move || {
            graph::token(&acc.confdir)
                .and_then(|t| graph::versions(&t, acc.drive_id.as_deref(), &rel))
        },
        |ui, r| {
            ui.set_busy(false);
            match r {
                Ok(vs) => {
                    let rows: Vec<VersionRow> = vs
                        .iter()
                        .map(|v| {
                            let mut detail = graph::human_size(v.size);
                            if !v.by.is_empty() {
                                detail.push_str(&format!("  ·  saved by {}", v.by));
                            }
                            VersionRow {
                                when: ss(when(&v.modified)),
                                detail: ss(detail),
                            }
                        })
                        .collect();
                    ui.set_versions(ModelRc::new(VecModel::from(rows)));
                    if vs.len() <= 1 {
                        say(
                            ui,
                            3,
                            "There's only the current version so far. OneDrive keeps older ones as the file changes.",
                        );
                    }
                    VERSIONS.with(|l| *l.borrow_mut() = vs);
                }
                Err(e) => say(ui, 2, e),
            }
        },
    );
}

fn version_at(i: i32) -> Option<Version> {
    VERSIONS.with(|l| l.borrow().get(usize::try_from(i).ok()?).cloned())
}

fn versions(
    ui: &FileWindow,
    weak: &slint::Weak<FileWindow>,
    acc: AccountInfo,
    rel: String,
    path: PathBuf,
) {
    load_versions(weak, &acc, &rel);
    {
        let (weak, acc, rel) = (weak.clone(), acc.clone(), rel.clone());
        ui.on_open_version(move |i| {
            let Some(v) = version_at(i) else { return };
            if i == 0 {
                // the current version is the file itself
                core::open_path(&path);
                return;
            }
            let dest = match version_copy_path(&path, &v) {
                Ok(path) => path,
                Err(e) => {
                    if let Some(ui) = weak.upgrade() {
                        say(&ui, 2, e);
                    }
                    return;
                }
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_busy(true);
                say(&ui, 3, "Downloading that version…");
            }
            let (acc, rel) = (acc.clone(), rel.clone());
            bg(
                &weak,
                move || {
                    if let Some(d) = dest.parent() {
                        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                        let _ =
                            trim_version_cache(d, &dest, VERSION_CACHE_FILES, VERSION_CACHE_BYTES);
                    }
                    let t = graph::token(&acc.confdir)?;
                    graph::download_version(&t, acc.drive_id.as_deref(), &rel, &v.id, &dest)?;
                    if let Some(d) = dest.parent() {
                        let _ =
                            trim_version_cache(d, &dest, VERSION_CACHE_FILES, VERSION_CACHE_BYTES);
                    }
                    Ok::<_, String>(dest)
                },
                |ui, r| {
                    ui.set_busy(false);
                    match r {
                        Ok(dest) => {
                            say(ui, 3, format!("Opened a copy: {}", dest.display()));
                            core::open_path(&dest);
                        }
                        Err(e) => say(ui, 2, e),
                    }
                },
            );
        });
    }
    {
        let weak = weak.clone();
        ui.on_restore_version(move |i| {
            let Some(v) = version_at(i) else { return };
            if let Some(ui) = weak.upgrade() {
                ui.set_busy(true);
                say(&ui, 3, "Restoring…");
            }
            let (acc, rel, weak2) = (acc.clone(), rel.clone(), weak.clone());
            let (acc2, rel2) = (acc.clone(), rel.clone());
            bg(
                &weak,
                move || {
                    let t = graph::token(&acc.confdir)?;
                    graph::restore_version(&t, acc.drive_id.as_deref(), &rel, &v.id)
                },
                move |ui, r| {
                    ui.set_busy(false);
                    match r {
                        Ok(()) => {
                            say(ui, 0, "Restored. This computer gets it at the next sync (\"Sync now\" in the tray icon's menu does it right away).");
                            load_versions(&weak2, &acc2, &rel2);
                        }
                        Err(e) => say(ui, 2, e),
                    }
                },
            );
        });
    }
}

fn share(ui: &FileWindow, weak: &slint::Weak<FileWindow>, acc: AccountInfo, rel: String) {
    let weak2 = weak.clone();
    ui.on_send(move || {
        let Some(ui) = weak2.upgrade() else { return };
        let emails = match parse_emails(&ui.get_emails()) {
            Ok(e) => e,
            Err(e) => return say(&ui, 2, e),
        };
        let (edit, message) = (ui.get_can_edit(), ui.get_message().to_string());
        ui.set_busy(true);
        say(&ui, 3, "Sending…");
        let (acc, rel) = (acc.clone(), rel.clone());
        let count = emails.len();
        bg(
            &weak2,
            move || {
                let t = graph::token(&acc.confdir)?;
                graph::invite(&t, acc.drive_id.as_deref(), &rel, &emails, edit, &message)
            },
            move |ui, r| {
                ui.set_busy(false);
                match r {
                    Ok(()) => {
                        ui.set_emails(ss(""));
                        ui.set_message(ss(""));
                        say(
                            ui,
                            0,
                            if count == 1 {
                                "Invitation sent.".to_string()
                            } else {
                                format!("{count} invitations sent.")
                            },
                        );
                    }
                    Err(e) => say(ui, 2, e),
                }
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        assert_eq!(
            parse_emails("a@b.com, c@d.org;a@B.com").unwrap(),
            vec!["a@b.com".to_string(), "c@d.org".to_string()]
        );
        assert!(parse_emails("").is_err());
        assert!(parse_emails("bob").is_err());
        assert!(parse_emails("a@b").is_err());
        assert!(parse_emails("a@.com").is_err());
    }

    #[test]
    fn version_cache_name_fits_linux_component_limit() {
        let file = Path::new(&format!("{}.txt", "a".repeat(245))).to_path_buf();
        let version = Version {
            id: "version-id".into(),
            modified: "2026-10-23T00:00:00Z".into(),
            size: 0,
            by: String::new(),
        };
        let path = version_copy_path(&file, &version).unwrap();
        assert!(path.file_name().unwrap().to_string_lossy().len() <= 255);
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(").txt")
        );
    }

    #[test]
    fn version_cache_is_bounded_and_keeps_the_requested_copy() {
        let dir = std::env::temp_dir().join(format!("lodg-version-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = [dir.join("old"), dir.join("middle"), dir.join("current")];
        for (path, contents) in paths.iter().zip(["123", "45", "6"]) {
            std::fs::write(path, contents).unwrap();
        }

        trim_version_cache(&dir, &paths[2], 2, u64::MAX).unwrap();
        assert!(paths[2].is_file());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        trim_version_cache(&dir, &paths[2], usize::MAX, 1).unwrap();
        assert_eq!(std::fs::read_to_string(&paths[2]).unwrap(), "6");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
