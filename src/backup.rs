//! "Back up important folders", as on Windows: Desktop, Documents, Pictures, Music and Videos
//! move into the OneDrive folder, so they upload, and stay where apps expect them (the old
//! place becomes a link to the new one, and the desktop's folder settings point at it).
//!
//! Nothing is ever deleted. Within one drive the files are moved; to another drive they're
//! copied and the original folder is kept, renamed, for the user to delete. Stopping leaves the
//! files in OneDrive and gives the folder back empty, as Windows does.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::core::{self, AccountInfo, AppSettings, ClientConfig};

/// A desktop folder that can be backed up: its xdg-user-dirs name and what people call it.
pub struct Known {
    pub key: &'static str,
    pub label: &'static str,
}

pub const FOLDERS: [Known; 5] = [
    Known {
        key: "DESKTOP",
        label: "Desktop",
    },
    Known {
        key: "DOCUMENTS",
        label: "Documents",
    },
    Known {
        key: "PICTURES",
        label: "Pictures",
    },
    Known {
        key: "MUSIC",
        label: "Music",
    },
    Known {
        key: "VIDEOS",
        label: "Videos",
    },
];

/// A folder being backed up, saved with the account's preferences.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Backup {
    pub key: String,
    /// Where the folder was (now a link to `target`)
    pub original: PathBuf,
    pub target: PathBuf,
}

/// Where a desktop folder is, from xdg-user-dirs (or None if it isn't set up).
pub fn xdg_dir(key: &str) -> Option<PathBuf> {
    let file = core::config_home().join("user-dirs.dirs");
    let text = fs::read_to_string(file).ok()?;
    let prefix = format!("XDG_{key}_DIR=");
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(&prefix))?;
    parse_xdg_value(line[prefix.len()..].trim(), &core::home())
}

fn parse_xdg_value(value: &str, home: &Path) -> Option<PathBuf> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut decoded = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let next = chars.next()?;
            if matches!(next, '\\' | '"' | '$' | '`') {
                decoded.push(next);
            } else {
                decoded.push('\\');
                decoded.push(next);
            }
        } else {
            decoded.push(ch);
        }
    }
    let path = if let Some(rest) = decoded.strip_prefix("$HOME") {
        if (!rest.is_empty() && !rest.starts_with('/')) || rest.contains(['$', '`']) {
            return None;
        }
        home.join(rest.trim_start_matches('/'))
    } else {
        if decoded.contains('$') || decoded.contains('`') {
            return None;
        }
        PathBuf::from(decoded)
    };
    path.is_absolute().then_some(path)
}

fn escape_xdg_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Points a desktop folder somewhere else (xdg-user-dirs-update, or the file by hand).
fn set_xdg_dir(key: &str, path: &Path) -> Result<(), String> {
    if core::which("xdg-user-dirs-update").is_some() {
        let ok = Command::new("xdg-user-dirs-update")
            .args(["--set", key])
            .arg(path)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Ok(());
        }
    }
    let file = core::config_home().join("user-dirs.dirs");
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("Couldn't read {}: {e}", file.display())),
    };
    let prefix = format!("XDG_{key}_DIR=");
    let value = match path.strip_prefix(core::home()) {
        Ok(rel) => format!("\"$HOME/{}\"", escape_xdg_quoted(&rel.to_string_lossy())),
        Err(_) => format!("\"{}\"", escape_xdg_quoted(&path.to_string_lossy())),
    };
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().starts_with(&prefix))
        .map(String::from)
        .collect();
    lines.push(format!("{prefix}{value}"));
    core::atomic_write(&file, (lines.join("\n") + "\n").as_bytes())
        .map_err(|e| format!("Couldn't update {}: {e}", file.display()))
}

/// The account backing up `key`, if any.
pub fn backed_up_by<'a>(
    settings: &AppSettings,
    accounts: &'a [AccountInfo],
    key: &str,
) -> Option<(&'a AccountInfo, Backup)> {
    accounts.iter().find_map(|a| {
        settings
            .prefs(&a.confdir)
            .backups
            .iter()
            .find(|b| b.key == key)
            .map(|b| (a, b.clone()))
    })
}

/// What backing up a folder would do, worked out before asking.
#[derive(Clone, Debug)]
pub struct Plan {
    pub key: String,
    pub label: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub files: u64,
    pub bytes: u64,
    /// Same drive: moved, instantly. Otherwise copied, and the original kept.
    pub same_drive: bool,
    /// Already has things in it (they're kept; clashing names get a suffix)
    pub target_has_files: bool,
    source_dev: u64,
    source_ino: u64,
    sync_dev: u64,
    target_dev: u64,
}

/// Counts files and bytes under a folder (not following links).
fn measure(dir: &Path) -> Result<(u64, u64), String> {
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = fs::read_dir(&d).map_err(|e| format!("Couldn't read {}: {e}", d.display()))?;
        for entry in rd {
            let e = entry.map_err(|e| format!("Couldn't enumerate {}: {e}", d.display()))?;
            let path = e.path();
            let file_type = e
                .file_type()
                .map_err(|e| format!("Couldn't inspect {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            let m = path
                .symlink_metadata()
                .map_err(|e| format!("Couldn't inspect {}: {e}", path.display()))?;
            if m.is_dir() {
                // The entry changed between reading its type and metadata.
                stack.push(path);
            } else {
                files += 1;
                bytes += m.len();
            }
        }
    }
    Ok((files, bytes))
}

/// Device for an existing path or its nearest existing ancestor. Only a missing tail is skipped.
fn device(path: &Path) -> Result<u64, String> {
    for ancestor in path.ancestors() {
        match fs::metadata(ancestor) {
            Ok(meta) => return Ok(meta.dev()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("Couldn't inspect {}: {e}", ancestor.display())),
        }
    }
    Err(format!(
        "Couldn't find a filesystem for {}.",
        path.display()
    ))
}

/// Checks common removable-drive mount roots before writing into a sync directory. If the drive
/// is unplugged, those directories still exist on the root filesystem and metadata alone would
/// make them look like a usable OneDrive location.
fn ensure_user_mount_available(path: &Path) -> Result<(), String> {
    let resolved = fs::canonicalize(path).map_err(|e| {
        format!(
            "Couldn't resolve the OneDrive folder {}: {e}",
            path.display()
        )
    })?;
    let roots = [
        Path::new("/run/media"),
        Path::new("/media"),
        Path::new("/mnt"),
    ];
    let Some(root) = roots
        .iter()
        .copied()
        .find(|root| resolved.starts_with(root))
    else {
        return Ok(());
    };
    let output = Command::new("findmnt")
        .args(["-rn", "-o", "TARGET", "-T"])
        .arg(&resolved)
        .output()
        .map_err(|e| {
            format!(
                "Couldn't check the mounted drive for {}: {e}",
                path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "Couldn't check whether the drive for {} is mounted.",
            path.display()
        ));
    }
    let mount = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !mount.starts_with(root) {
        return Err(format!(
            "The drive for {} isn't mounted. Connect it before backing up this folder.",
            path.display()
        ));
    }
    Ok(())
}

/// Whether the account's chosen folders (sync_list) leave `name` out.
fn excluded_by_sync_list(acc: &AccountInfo, name: &str) -> Result<bool, String> {
    let path = acc.confdir.join("sync_list");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(format!(
                "Couldn't check the sync-folder rules in {}: {e}",
                path.display()
            ));
        }
    };
    let rules: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if rules.is_empty()
        || !rules
            .iter()
            .any(|r| !r.starts_with('!') && !r.starts_with('-'))
    {
        return Ok(false);
    }
    let wanted = name.to_lowercase();
    Ok(!rules.iter().any(|r| {
        if r.starts_with('!') || r.starts_with('-') {
            return false;
        }
        let r = r
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_lowercase();
        // "/Documents/*" and "/Documents/**" sync all of it too
        let r = r.trim_end_matches("/**").trim_end_matches("/*");
        r == wanted || r == "*" || r == "**" || r.is_empty()
    }))
}

/// Checks a folder can be backed up into this account, and measures it.
pub fn plan(acc: &AccountInfo, all: &[AccountInfo], key: &str) -> Result<Plan, String> {
    let known = FOLDERS
        .iter()
        .find(|k| k.key == key)
        .ok_or("Unknown folder.")?;
    let label = known.label;
    let source = xdg_dir(key)
        .ok_or_else(|| format!("This computer doesn't have a {label} folder set up."))?;
    if !acc.sync_dir.is_dir() {
        return Err(format!(
            "The OneDrive folder {} isn't available. Connect its drive and try again.",
            acc.sync_dir.display()
        ));
    }
    ensure_user_mount_available(&acc.sync_dir)?;
    let sync_meta = fs::metadata(&acc.sync_dir)
        .map_err(|e| format!("Couldn't inspect {}: {e}", acc.sync_dir.display()))?;
    let home = core::home();
    if source == home || home.starts_with(&source) {
        return Err(format!(
            "Your {label} folder is set to {}, which holds everything in your home folder, so it can't be backed up. Point {label} at its own folder in your desktop's settings first.",
            source.display()
        ));
    }
    // XDG paths are editable and may resolve through `..` or symlinked parents. Do not let a
    // misplaced Desktop/Documents setting make the backup move a system or app-settings tree.
    if let Some(reason) = core::unsuitable_sync_folder(&source) {
        return Err(format!("Your {label} folder can't be backed up: {reason}"));
    }
    let meta = fs::symlink_metadata(&source)
        .map_err(|_| format!("{} doesn't exist.", source.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is already a link to somewhere else, so it's left alone.",
            source.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} isn't a folder.", source.display()));
    }
    for a in all {
        if core::paths_overlap(&source, &a.sync_dir) {
            return Err(format!(
                "Your {label} folder overlaps the OneDrive folder for account \"{}\" ({}).",
                a.name,
                a.sync_dir.display()
            ));
        }
    }
    let cfg = ClientConfig::load(&acc.confdir);
    if cfg.get_bool("download_only") {
        return Err("This account only downloads, so nothing here would be backed up. Change its direction under Options first.".into());
    }
    let name = source
        .file_name()
        .map_or_else(|| label.to_string(), |n| n.to_string_lossy().to_string());
    if excluded_by_sync_list(acc, &name)? {
        return Err(format!(
            "This account only syncs the folders you chose, and \"{name}\" isn't one of them. Add it under Folders first (you can create it online)."
        ));
    }
    let target = acc.sync_dir.join(&name);
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "There's a symbolic link called \"{name}\" in your OneDrive folder. Resolve or rename it before backing up this folder."
            ));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!(
                "There's a file called \"{name}\" in your OneDrive folder. Rename it first."
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Couldn't inspect {}: {e}", target.display())),
    }
    let target_dev = device(&target)?;
    let target_has_files = match fs::read_dir(&target) {
        Ok(mut entries) => entries
            .next()
            .transpose()
            .map_err(|e| format!("Couldn't inspect {}: {e}", target.display()))?
            .is_some(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("Couldn't inspect {}: {e}", target.display())),
    };
    let (files, bytes) = measure(&source)?;
    Ok(Plan {
        key: key.into(),
        label: label.into(),
        same_drive: meta.dev() == target_dev,
        source,
        target,
        files,
        bytes,
        target_has_files,
        source_dev: meta.dev(),
        source_ino: meta.ino(),
        sync_dev: sync_meta.dev(),
        target_dev,
    })
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this computer".into())
}

/// A name in `dir` that isn't taken: the same name, or "name (from host)", "name (from host 2)"…
fn free_name(dir: &Path, name: &str, host: &str) -> Result<PathBuf, String> {
    let p = dir.join(name);
    match fs::symlink_metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(p),
        Err(e) => return Err(format!("Couldn't inspect {}: {e}", p.display())),
        Ok(_) => {}
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for n in 1..=10_000 {
        let tag = if n == 1 {
            format!("from {host}")
        } else {
            format!("from {host} {n}")
        };
        let suffix = format!(" ({tag}){ext}");
        if suffix.len() >= 255 {
            return Err(format!(
                "The suffix for {name} is too long to make a collision-safe name."
            ));
        }
        let mut end = stem.len().min(255 - suffix.len());
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return Err(format!(
                "The name for {name} is too long to make a collision-safe copy."
            ));
        }
        let candidate = dir.join(format!("{}{suffix}", &stem[..end]));
        match fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(e) => return Err(format!("Couldn't inspect {}: {e}", candidate.display())),
            Ok(_) => {}
        }
    }
    Err(format!(
        "Couldn't find an unused name for {name} after 10,000 attempts."
    ))
}

/// Copies an entry into a unique staging directory, then publishes it with a no-replace rename.
/// Passing a destination that appears mid-copy to `cp` can make it nest the entry inside that
/// new directory while still exiting successfully.
fn copy_noreplace(
    source: &Path,
    target_dir: &Path,
    name: &str,
    host: &str,
) -> Result<PathBuf, String> {
    static NEXT_STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    for _ in 0..10_000 {
        let dest = free_name(target_dir, name, host)?;
        let n = NEXT_STAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stage = target_dir.join(format!(".lodg-stage-{}-{n}", std::process::id()));
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(&stage) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't prepare a safe copy: {e}")),
        }
        let staged = source
            .file_name()
            .map(|name| stage.join(name))
            .ok_or_else(|| format!("{} has no usable name", source.display()))?;
        let copied = Command::new("cp")
            .args(["-a", "--reflink=auto", "--"])
            .arg(source)
            .arg(&stage)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !copied {
            let _ = fs::remove_dir_all(&stage);
            return Err(format!(
                "Copying {} failed (is there enough space?)",
                source.display()
            ));
        }
        match core::rename_noreplace(&staged, &dest) {
            Ok(()) => {
                fs::remove_dir(&stage).map_err(|e| {
                    format!(
                        "The copy was published, but couldn't clean up {}: {e}",
                        stage.display()
                    )
                })?;
                return Ok(dest);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::remove_dir_all(&stage).map_err(|cleanup| {
                    format!("The destination changed during copy ({e}); couldn't clean up {}: {cleanup}", stage.display())
                })?;
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(format!("Couldn't publish the completed copy: {e}"));
            }
        }
    }
    Err(format!(
        "Couldn't find an unused destination for {name} after 10,000 attempts."
    ))
}

/// What happened, for telling the user.
pub struct Done {
    /// Items whose name was taken in OneDrive, saved under a new name
    pub renamed: usize,
    /// Copied to another drive: the original folder, kept under this name
    pub kept_original: Option<PathBuf>,
    /// Why the desktop's folder setting couldn't be updated, if it couldn't
    pub desktop_setting: Option<String>,
}

/// Backs the folder up: moves (or copies) everything into OneDrive, links the old place to the
/// new one, and records it with the account.
pub fn start(acc: &AccountInfo, plan: &Plan) -> Result<Done, String> {
    let _busy = core::BusyMarker::try_new(&acc.confdir).map_err(|e| {
        format!(
            "Couldn't protect this account from another operation, so the folder wasn't backed up: {e}"
        )
    })?;
    let resync = core::resync_unit(&acc.confdir);
    if core::unit_state(&resync).active {
        return Err(
            "A one-time sync is running. Wait for it to finish before backing up this folder."
                .into(),
        );
    }
    let was_active = acc
        .unit
        .as_deref()
        .is_some_and(|unit| core::unit_state(unit).active);
    if let Some(unit) = acc.unit.as_deref() {
        let (ok, err) = core::systemctl(&["stop", unit]);
        if !ok {
            return Err(format!(
                "Couldn't stop the sync engine, so the folder wasn't changed: {}",
                err.trim()
            ));
        }
    }

    let operation = start_files(acc, plan);
    let restarted = if was_active {
        acc.unit.as_deref().map(|unit| {
            let (ok, err) = core::systemctl(&["start", unit]);
            if ok {
                Ok(())
            } else {
                Err(err.trim().to_string())
            }
        })
    } else {
        None
    };
    match (operation, restarted) {
        (Ok(done), Some(Ok(()))) | (Ok(done), None) => Ok(done),
        (Err(e), Some(Ok(()))) | (Err(e), None) => Err(e),
        (Ok(_), Some(Err(restart))) => Err(format!(
            "The folder backup finished, but the sync engine couldn't be restarted: {restart}"
        )),
        (Err(operation), Some(Err(restart))) => Err(format!(
            "{operation}; the sync engine couldn't be restarted: {restart}"
        )),
    }
}

fn start_files(acc: &AccountInfo, plan: &Plan) -> Result<Done, String> {
    if !acc.sync_dir.is_dir() {
        return Err(format!(
            "The OneDrive folder {} isn't available. Connect its drive and try again.",
            acc.sync_dir.display()
        ));
    }
    ensure_user_mount_available(&acc.sync_dir)?;
    let source_meta = fs::symlink_metadata(&plan.source)
        .map_err(|e| format!("Couldn't inspect {}: {e}", plan.source.display()))?;
    if source_meta.file_type().is_symlink()
        || !source_meta.is_dir()
        || source_meta.dev() != plan.source_dev
        || source_meta.ino() != plan.source_ino
    {
        return Err("The source folder changed after the backup was reviewed. Review it again before continuing.".into());
    }
    let sync_meta = fs::metadata(&acc.sync_dir)
        .map_err(|e| format!("Couldn't inspect {}: {e}", acc.sync_dir.display()))?;
    if !sync_meta.is_dir() || sync_meta.dev() != plan.sync_dev {
        return Err("The OneDrive folder or its mounted drive changed after the backup was reviewed. Connect the right drive and review again.".into());
    }
    let expected_target = acc.sync_dir.join(
        plan.source
            .file_name()
            .ok_or("The source folder has no usable name.")?,
    );
    if expected_target != plan.target || core::paths_overlap(&plan.source, &acc.sync_dir) {
        return Err("The backup paths changed or overlap. Review the folder backup again.".into());
    }
    match fs::symlink_metadata(&plan.target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err("The OneDrive destination became a symbolic link after review. Resolve it and review the backup again.".into());
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(
                "The OneDrive destination became a file after review. Review the backup again."
                    .into(),
            );
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Couldn't inspect {}: {e}", plan.target.display())),
    }
    if device(&plan.target)? != plan.target_dev {
        return Err("The destination filesystem changed after the backup was reviewed. Review the backup again.".into());
    }
    fs::create_dir_all(&plan.target)
        .map_err(|e| format!("Couldn't create {}: {e}", plan.target.display()))?;
    if device(&plan.target)? != plan.target_dev {
        return Err(
            "The destination filesystem changed while preparing the backup. No files were moved."
                .into(),
        );
    }
    let host = hostname();
    let mut renamed = 0;
    let entries: Vec<_> = fs::read_dir(&plan.source)
        .map_err(|e| format!("Couldn't read {}: {e}", plan.source.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("Couldn't enumerate {}: {e}", plan.source.display()))?;
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if plan.same_drive {
            let dest = free_name(&plan.target, &name, &host).map_err(|e| {
                format!("Stopped at \"{name}\": {e}. Everything moved so far is in {}; the rest is still in {}.", plan.target.display(), plan.source.display())
            })?;
            if dest
                .file_name()
                .map(|n| n.to_string_lossy() != name.as_str())
                == Some(true)
            {
                renamed += 1;
            }
            core::rename_noreplace(&e.path(), &dest).map_err(|err| {
                format!(
                    "Stopped at \"{name}\": {err}. Everything moved so far is in {}; the rest is still in {}.",
                    plan.target.display(),
                    plan.source.display()
                )
            })?;
        } else {
            let dest = copy_noreplace(&e.path(), &plan.target, &name, &host).map_err(|err| {
                format!(
                    "Stopped at \"{name}\": {err}. Your {} folder is still in {}; completed copies are in {}.",
                    plan.label, plan.source.display(), plan.target.display()
                )
            })?;
            if dest
                .file_name()
                .map(|n| n.to_string_lossy() != name.as_str())
                == Some(true)
            {
                renamed += 1;
            }
        }
    }
    // the old place: emptied (same drive) and removed, or kept under a new name (copied)
    let kept_original = if plan.same_drive {
        fs::remove_dir(&plan.source).map_err(|e| {
            format!(
                "Everything was moved, but {} couldn't be replaced: {e}",
                plan.source.display()
            )
        })?;
        None
    } else {
        let parent = plan.source.parent().unwrap_or(Path::new("/"));
        let base = format!("{} (copied to OneDrive)", plan.label);
        let kept = free_name(parent, &base, &host)?;
        core::rename_noreplace(&plan.source, &kept).map_err(|e| {
            format!(
                "Everything was copied, but {} couldn't be renamed: {e}",
                plan.source.display()
            )
        })?;
        Some(kept)
    };
    std::os::unix::fs::symlink(&plan.target, &plan.source).map_err(|e| {
        format!(
            "Your files are in {}, but the link at {} couldn't be made: {e}",
            plan.target.display(),
            plan.source.display()
        )
    })?;
    // recorded before anything else can fail, so the app can always stop the backup again
    let mut s = AppSettings::load();
    let p = s.prefs_mut(&acc.confdir);
    p.backups.retain(|b| b.key != plan.key);
    p.backups.push(Backup {
        key: plan.key.clone(),
        original: plan.source.clone(),
        target: plan.target.clone(),
    });
    s.save().map_err(|e| {
        format!(
            "Your files are in {}, but the backup couldn't be recorded: {e}",
            plan.target.display()
        )
    })?;
    // apps that ask the desktop where the folder is (and sandboxed apps) get the real place;
    // the link at the old place works meanwhile, so a failure here is only a warning
    let desktop_setting = set_xdg_dir(&plan.key, &plan.target).err();
    Ok(Done {
        renamed,
        kept_original,
        desktop_setting,
    })
}

/// Stops backing a folder up. Its files stay in OneDrive; the folder comes back empty at its
/// old place, and the desktop points there again.
pub fn stop(acc: &AccountInfo, key: &str) -> Result<Backup, String> {
    let mut s = AppSettings::load();
    let b = s
        .prefs(&acc.confdir)
        .backups
        .iter()
        .find(|b| b.key == key)
        .cloned()
        .ok_or("That folder isn't being backed up.")?;
    match fs::symlink_metadata(&b.original) {
        Ok(m) if m.file_type().is_symlink() => {
            let link_target = fs::read_link(&b.original)
                .map_err(|e| format!("Couldn't inspect the link: {e}"))?;
            if link_target != b.target {
                return Err(format!(
                    "{} now points somewhere unexpected, so the backup record was kept. Resolve the link before stopping the backup.",
                    b.original.display()
                ));
            }
            fs::remove_file(&b.original).map_err(|e| format!("Couldn't remove the link: {e}"))?;
            fs::create_dir_all(&b.original).map_err(|e| e.to_string())?;
        }
        // something else is there now: leave it be
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&b.original).map_err(|e| e.to_string())?
        }
        Err(e) => return Err(format!("Couldn't inspect {}: {e}", b.original.display())),
    }
    set_xdg_dir(&b.key, &b.original)?;
    s.prefs_mut(&acc.confdir).backups.retain(|x| x.key != key);
    s.save().map_err(|e| e.to_string())?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_value_parses_home_and_escaped_paths() {
        let home = Path::new("/home/test");
        assert_eq!(
            parse_xdg_value("\"$HOME/My Docs\"", home),
            Some(home.join("My Docs"))
        );
        assert_eq!(
            parse_xdg_value("\"/tmp/a\\\"b\\\\c\"", home),
            Some(PathBuf::from("/tmp/a\"b\\c"))
        );
        assert_eq!(parse_xdg_value("\"$HOMEevil\"", home), None);
        assert_eq!(parse_xdg_value("\"$(touch /tmp/x)\"", home), None);
    }

    #[test]
    fn xdg_quoted_paths_escape_shell_expansions() {
        assert_eq!(
            escape_xdg_quoted("a$HOME`x`\\\""),
            "a\\$HOME\\`x\\`\\\\\\\""
        );
    }

    #[test]
    fn staged_copy_publishes_the_entry_at_its_final_name() {
        let dir = std::env::temp_dir().join(format!("lodg-copy-{}", std::process::id()));
        let source = dir.join("source");
        let target = dir.join("target");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("item.txt"), "contents").unwrap();
        let dest = copy_noreplace(&source.join("item.txt"), &target, "item.txt", "host").unwrap();
        assert_eq!(fs::read_to_string(dest).unwrap(), "contents");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn measure_counts_but_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("lodg-measure-{}", std::process::id()));
        let source = dir.join("source");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("root.txt"), "ab").unwrap();
        fs::write(source.join("nested/item.txt"), "wxyz").unwrap();
        symlink("nested", source.join("shortcut")).unwrap();
        let link_bytes = fs::symlink_metadata(source.join("shortcut")).unwrap().len();

        assert_eq!(measure(&source).unwrap(), (3, 2 + 4 + link_bytes));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_list_rules() {
        let dir = std::env::temp_dir().join(format!("lodg-synclist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let acc = AccountInfo {
            name: "x".into(),
            confdir: dir.clone(),
            unit: None,
            managed: false,
            sync_dir: "/s".into(),
            drive_id: None,
            kind: "personal".into(),
            signed_in: true,
        };
        let excluded = |rules: &str| {
            fs::write(dir.join("sync_list"), rules).unwrap();
            excluded_by_sync_list(&acc, "Documents").unwrap()
        };
        // only exclusions: everything else still syncs
        assert!(!excluded("!/Junk/*\n"));
        // the whole folder, however it's written
        assert!(!excluded("/Documents\n"));
        assert!(!excluded("/Documents/*\n"));
        assert!(!excluded("Documents/**\n"));
        // only part of it syncs, or another folder
        assert!(excluded("/Documents/Work\n"));
        assert!(excluded("/Pictures\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_names() {
        let dir = std::env::temp_dir().join(format!("lodg-backup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "a.txt", "pc").unwrap(), dir.join("a.txt"));
        fs::write(dir.join("a.txt"), "").unwrap();
        assert_eq!(
            free_name(&dir, "a.txt", "pc").unwrap(),
            dir.join("a (from pc).txt")
        );
        fs::write(dir.join("a (from pc).txt"), "").unwrap();
        assert_eq!(
            free_name(&dir, "a.txt", "pc").unwrap(),
            dir.join("a (from pc 2).txt")
        );
        fs::create_dir_all(dir.join("Notes")).unwrap();
        assert_eq!(
            free_name(&dir, "Notes", "pc").unwrap(),
            dir.join("Notes (from pc)")
        );
        assert_eq!(
            free_name(&dir, ".bashrc", "pc").unwrap(),
            dir.join(".bashrc")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
