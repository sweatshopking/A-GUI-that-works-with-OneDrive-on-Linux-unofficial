//! Conflicting copies: when a file changed both here and online, the client keeps this
//! computer's version under a new name ("Report-myhost-safeBackup-0001.docx") and downloads
//! OneDrive's. This finds those copies and settles them the way the user picks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;
use std::{io, os::unix::fs::MetadataExt};

/// A copy the client kept, and the file it clashed with.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// This computer's version, as the client renamed it
    pub copy: PathBuf,
    /// The file it clashed with (OneDrive's version); may be gone since
    pub original: PathBuf,
    /// The kept copy's last modification time, read during the scan.
    pub modified: Option<SystemTime>,
    copy_identity: Option<FileIdentity>,
    original_identity: Option<FileIdentity>,
}

#[derive(Clone, Debug, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

fn identity_from_metadata(m: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        mtime: m.mtime(),
        mtime_nsec: m.mtime_nsec(),
        ctime: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
    }
}

fn identity(path: &Path) -> Option<FileIdentity> {
    fs::symlink_metadata(path)
        .ok()
        .as_ref()
        .map(identity_from_metadata)
}

const MARK: &str = "-safeBackup-";

/// The file a kept copy belongs to: "name-host-safeBackup-0001.ext" -> "name.ext". The host
/// name can hold dashes too, so the longest "name" whose file exists wins; with none existing,
/// the part before the last dash ahead of the mark is the best guess.
fn original_of(copy: &Path) -> Option<PathBuf> {
    let file = copy.file_name()?.to_string_lossy().to_string();
    // The original filename may itself contain the marker; the client appends its own final one.
    let at = file.rfind(MARK)?;
    let (before, after) = (&file[..at], &file[at + MARK.len()..]);
    let digits = after.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let ext = &after[digits..];
    if !(ext.is_empty() || ext.starts_with('.')) {
        return None;
    }
    let dir = copy.parent()?;
    let splits: Vec<usize> = before.match_indices('-').map(|(i, _)| i).collect();
    for &i in splits.iter().rev() {
        let candidate = dir.join(format!("{}{ext}", &before[..i]));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let i = *splits.last()?;
    Some(dir.join(format!("{}{ext}", &before[..i])))
}

/// Finds kept copies under the OneDrive folder, newest first. Stops looking after `limit`
/// entries, so a huge folder can't hold things up. Folders that can't be read are skipped; the
/// note then says the list may be incomplete. Only an unreadable top folder is an error.
pub fn find(root: &Path, limit: usize) -> Result<(Vec<Conflict>, Option<String>), String> {
    let mut out: Vec<(SystemTime, Conflict)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let (mut seen, mut unreadable, mut stopped) = (0usize, 0usize, false);
    'walk: while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) if d == root => return Err(format!("Couldn't look in {}: {e}", d.display())),
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        for e in rd {
            let Ok(e) = e else {
                unreadable += 1;
                continue;
            };
            seen += 1;
            if seen > limit {
                stopped = true;
                break 'walk;
            }
            let p = e.path();
            let Ok(ft) = e.file_type() else {
                unreadable += 1;
                continue;
            };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file()
                && e.file_name().to_string_lossy().contains(MARK)
                && let Some(original) = original_of(&p)
            {
                let metadata = fs::symlink_metadata(&p).ok();
                let modified = metadata.as_ref().and_then(|m| m.modified().ok());
                let t = modified.unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((
                    t,
                    Conflict {
                        copy_identity: metadata.as_ref().map(identity_from_metadata),
                        original_identity: identity(&original),
                        copy: p,
                        original,
                        modified,
                    },
                ));
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.0));
    let note = if stopped {
        Some(format!(
            "Stopped looking after {limit} items, so there may be more copies than shown."
        ))
    } else if unreadable > 0 {
        Some(format!(
            "{unreadable} item(s) couldn't be read, so there may be more copies than shown."
        ))
    } else {
        None
    };
    Ok((out.into_iter().map(|(_, c)| c).collect(), note))
}

/// Moves a file to the desktop's Trash (never deletes it outright).
pub fn to_trash(p: &Path) -> Result<(), String> {
    let tries: [(&str, Vec<String>); 3] = [
        (
            "gio",
            vec!["trash".into(), "--".into(), p.to_string_lossy().into()],
        ),
        (
            "kioclient",
            vec!["move".into(), p.to_string_lossy().into(), "trash:/".into()],
        ),
        ("trash-put", vec!["--".into(), p.to_string_lossy().into()]),
    ];
    for (prog, args) in tries {
        if crate::core::which(prog).is_none() {
            continue;
        }
        let ok = Command::new(prog)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok && !p.exists() {
            return Ok(());
        }
    }
    Err("There's no Trash to move it to on this computer, so it was left as it is. Delete it yourself if you're sure.".into())
}

/// How to settle a conflict.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)] // the words people see: keep mine, keep online, keep both
pub enum Choice {
    /// This computer's version becomes the file (OneDrive keeps the other in version history)
    KeepMine,
    /// OneDrive's version stays; this computer's copy goes to the Trash
    KeepOnline,
    /// Both stay, the copy under a clearer name
    KeepBoth,
}

fn clear_name_numbered(c: &Conflict, n: u64) -> Result<PathBuf, String> {
    let dir = c.copy.parent().unwrap_or(Path::new("/"));
    let name = c
        .original
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().to_string());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name, String::new()),
    };
    let tag = if n == 1 {
        "this computer's copy".to_string()
    } else {
        format!("this computer's copy {n}")
    };
    let suffix = format!(" ({tag}){ext}");
    if suffix.len() >= 255 {
        return Err("The file extension is too long to make a Keep Both copy safely.".into());
    }
    let mut end = stem.len().min(255 - suffix.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return Err("The filename is too long to make a Keep Both copy safely.".into());
    }
    Ok(dir.join(format!("{}{suffix}", &stem[..end])))
}

pub fn settle(c: &Conflict, choice: Choice) -> Result<String, String> {
    if identity(&c.copy) != c.copy_identity || c.copy_identity.is_none() {
        return Err("That copy isn't there any more.".into());
    }
    if identity(&c.original) != c.original_identity {
        return Err("The OneDrive version changed since this list was loaded. Refresh the list and choose again.".into());
    }
    let name = |p: &Path| {
        p.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().to_string())
    };
    match choice {
        Choice::KeepMine => {
            // the version being replaced stays in OneDrive's version history
            fs::rename(&c.copy, &c.original).map_err(|e| format!("Couldn't replace it: {e}"))?;
            Ok(format!(
                "\"{}\" is now this computer's version. OneDrive's version stays in its version history.",
                name(&c.original)
            ))
        }
        Choice::KeepOnline => {
            to_trash(&c.copy)?;
            Ok(format!(
                "Kept OneDrive's version of \"{}\". This computer's copy is in the Trash.",
                name(&c.original)
            ))
        }
        Choice::KeepBoth => {
            let mut to = None;
            for n in 1..=10_000 {
                let candidate = clear_name_numbered(c, n)?;
                match crate::core::rename_noreplace(&c.copy, &candidate) {
                    Ok(()) => {
                        to = Some(candidate);
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(format!("Couldn't rename it: {e}")),
                }
            }
            let Some(to) = to else {
                return Err(
                    "Couldn't find an unused name for the local copy after 10,000 attempts.".into(),
                );
            };
            Ok(format!(
                "Kept both: this computer's version is \"{}\".",
                name(&to)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn originals() {
        let dir = std::env::temp_dir().join(format!("lodg-conflict-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("my-report.docx"), "online").unwrap();
        let copy = dir.join("my-report-dev-box-safeBackup-0001.docx");
        fs::write(&copy, "mine").unwrap();
        assert_eq!(original_of(&copy), Some(dir.join("my-report.docx")));
        let marked_original = dir.join("file-safeBackup-name.docx");
        let marked_copy = dir.join("file-safeBackup-name-pc-safeBackup-0001.docx");
        fs::write(&marked_original, "online").unwrap();
        assert_eq!(original_of(&marked_copy), Some(marked_original.clone()));
        // original gone: best guess drops the last dash part
        let lone = dir.join("notes-pc-safeBackup-0002");
        assert_eq!(original_of(&lone), Some(dir.join("notes")));
        assert_eq!(original_of(&dir.join("x-safeBackup-abc.txt")), None);
        let (found, note) = find(&dir, 1000).unwrap();
        assert_eq!(note, None);
        assert_eq!(found.len(), 1);
        assert!(found[0].modified.is_some());
        let msg = settle(&found[0], Choice::KeepBoth).unwrap();
        assert!(msg.contains("my-report (this computer's copy).docx"));
        assert!(dir.join("my-report (this computer's copy).docx").is_file());
        let long = Conflict {
            copy: dir.join("long-safeBackup-pc-safeBackup-0001.txt"),
            original: dir.join(format!("{}.txt", "x".repeat(245))),
            modified: None,
            copy_identity: None,
            original_identity: None,
        };
        let candidate = clear_name_numbered(&long, 1).unwrap();
        assert!(candidate.file_name().unwrap().to_string_lossy().len() <= 255);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_both_numbers_start_at_one_for_each_file() {
        let dir = std::env::temp_dir().join(format!("lodg-keepboth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in ["a", "b"] {
            fs::write(dir.join(format!("{name}.txt")), "online").unwrap();
            fs::write(dir.join(format!("{name}-pc-safeBackup-0001.txt")), "mine").unwrap();
        }
        let (found, _) = find(&dir, 1000).unwrap();
        assert_eq!(found.len(), 2);
        for c in &found {
            settle(c, Choice::KeepBoth).unwrap();
        }
        assert!(dir.join("a (this computer's copy).txt").is_file());
        assert!(dir.join("b (this computer's copy).txt").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_folders_are_noted_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lodg-unreadable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("locked")).unwrap();
        fs::write(dir.join("a.txt"), "online").unwrap();
        fs::write(dir.join("a-pc-safeBackup-0001.txt"), "mine").unwrap();
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::read_dir(dir.join("locked")).is_ok(); // true when run as root
        let (found, note) = find(&dir, 1000).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(note.is_some(), !readable);
        fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_limit_is_reported_with_the_partial_list() {
        let dir = std::env::temp_dir().join(format!("lodg-conflict-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("one"), "1").unwrap();
        fs::write(dir.join("two"), "2").unwrap();
        let (_, note) = find(&dir, 1).unwrap();
        assert!(note.unwrap().contains("after 1 items"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
