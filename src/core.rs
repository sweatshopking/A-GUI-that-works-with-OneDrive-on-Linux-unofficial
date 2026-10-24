//! Shared plumbing: finding the onedrive client, accounts, config files, systemd units,
//! app settings and log parsing. Used by both the tray and the settings window.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "drivedeck";
pub const APP_NAME: &str = "DriveDeck for OneDrive";
pub const MANAGED_MARK: &str = "# Managed by DriveDeck";
/// The mark units got before the app was renamed (0.4.x); `repair_managed_units` updates it.
pub const OLD_MANAGED_MARK: &str = "# Managed by linux-onedrive-gui";

/// A unit file this app wrote (now or under its old name).
pub fn is_managed(text: &str) -> bool {
    text.lines()
        .any(|l| l == MANAGED_MARK || l == OLD_MANAGED_MARK)
}
/// Session-bus name the tray owns while it runs, so only one tray ever starts.
pub const TRAY_BUS_NAME: &str = "io.github.sweatshopking.DriveDeck.Tray";

// ---------------------------------------------------------------- paths

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
}

pub fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/share"))
}

pub fn cache_home() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache"))
}

pub fn app_dir() -> PathBuf {
    config_home().join(APP_ID)
}

pub fn user_unit_dir() -> PathBuf {
    config_home().join("systemd/user")
}

/// The app's folder for things that only matter while you're logged in (the window's socket,
/// markers, the sign-in helper). It's in $XDG_RUNTIME_DIR, which only you can open; without one,
/// in your own cache folder rather than the shared /tmp, where another account could plant files.
pub fn runtime_dir() -> PathBuf {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(base) => base.join(APP_ID),
        None => std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".cache"))
            .join(APP_ID)
            .join("run"),
    };
    let _ = create_private_dir(&dir);
    dir
}

/// The command other processes should run to reach this app: the AppImage file when we are
/// inside one (its mount point changes every run), otherwise our own executable.
pub fn self_command() -> PathBuf {
    if let Ok(a) = std::env::var("APPIMAGE") {
        return PathBuf::from(a);
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(APP_ID));
    // After a package upgrade replaces our file, Linux reports the running one as
    // "<path> (deleted)"; the new version is at the original path.
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(original) => PathBuf::from(original),
        None => exe,
    }
}

/// Which file `self_command()` is right now: (device, inode, modification time).
fn program_file_identity() -> Option<(u64, u64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(self_command()).ok()?;
    Some((m.dev(), m.ino(), m.mtime()))
}

static STARTED_FROM: std::sync::OnceLock<Option<(u64, u64, i64)>> = std::sync::OnceLock::new();

/// Remembers which program file we were started from; call once at startup.
pub fn note_program_file() {
    STARTED_FROM.get_or_init(program_file_identity);
}

/// True once an update has put a different program file where we were started from (a
/// package upgrade, or a new AppImage saved over the old one). A missing file (moved, or
/// mid-install) doesn't count.
pub fn program_file_replaced() -> bool {
    match (STARTED_FROM.get(), program_file_identity()) {
        (Some(Some(then)), Some(now)) => now != *then,
        _ => false,
    }
}

/// The oldest OneDrive Client for Linux release this app works with. (Ubuntu's and Debian's own
/// `onedrive` packages are older than this and no longer supported by the client's author.)
pub const MIN_CLIENT_VERSION: (u32, u32, u32) = (2, 5, 0);
/// The client author's installation instructions, which cover every distribution.
pub const CLIENT_INSTALL_URL: &str =
    "https://github.com/abraunegg/onedrive/blob/master/docs/install.md";

/// The OneDrive Client for Linux installed on this computer: `onedrive` on PATH, or where a
/// build from source puts it (a desktop session's PATH often lacks ~/.local/bin). The app never
/// ships its own copy, so the client keeps getting updates from its own packages.
pub fn client_binary() -> Option<PathBuf> {
    // lets the automated tests use a stand-in client
    if let Some(p) = std::env::var_os("DRIVEDECK_CLIENT") {
        return Some(PathBuf::from(p));
    }
    which("onedrive").or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        [
            home.map(|h| h.join(".local/bin/onedrive")),
            Some(PathBuf::from("/usr/local/bin/onedrive")),
        ]
        .into_iter()
        .flatten()
        .find(|p| executable_file(p))
    })
}

fn executable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path_c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    path.is_file() && unsafe { libc::access(path_c.as_ptr(), libc::X_OK) == 0 }
}

pub fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(prog))
        .find(|p| executable_file(p))
}

/// A Command that runs the installed client (the bare name if it's missing, which then fails
/// with a clear "not found" that callers report).
pub fn client_command() -> Command {
    Command::new(client_binary().unwrap_or_else(|| PathBuf::from("onedrive")))
}

/// What we found when looking for the client.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientCheck {
    Missing,
    TooOld { path: PathBuf, version: String },
    Ok { path: PathBuf, version: String },
}

impl ClientCheck {
    pub fn is_ok(&self) -> bool {
        matches!(self, ClientCheck::Ok { .. })
    }
}

/// Looks for the client and checks its version (`onedrive --version`).
pub fn check_client() -> ClientCheck {
    let Some(path) = client_binary() else {
        return ClientCheck::Missing;
    };
    let out = Command::new(&path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    match parse_client_version(&text) {
        Some((v, label)) if v >= MIN_CLIENT_VERSION => ClientCheck::Ok {
            path,
            version: label,
        },
        Some((_, label)) => ClientCheck::TooOld {
            path,
            version: label,
        },
        // it runs but doesn't say which version it is: too old or not the right program
        None => ClientCheck::TooOld {
            path,
            version: "unknown".into(),
        },
    }
}

/// Reads "onedrive v2.5.11" (or "v2.5.11-3-gabc123", "2.4.25+np1") into (2, 5, 11) and the text.
pub fn parse_client_version(text: &str) -> Option<((u32, u32, u32), String)> {
    text.split_whitespace().find_map(|w| {
        let w = w.trim_start_matches('v');
        let mut nums = w
            .split(|c: char| !c.is_ascii_digit())
            .map(|n| n.parse::<u32>().ok());
        let (a, b) = (nums.next()??, nums.next()??);
        let c = nums.next().flatten().unwrap_or(0);
        w.contains('.').then(|| ((a, b, c), w.to_string()))
    })
}

// ---------------------------------------------------------------- app settings

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    pub from: String, // "HH:MM"
    pub to: String,
}

impl Schedule {
    /// True when syncing is allowed right now. Windows may cross midnight.
    pub fn allows(&self, now: chrono::NaiveTime) -> bool {
        if !self.enabled {
            return true;
        }
        let (Some(a), Some(b)) = (parse_hhmm(&self.from), parse_hhmm(&self.to)) else {
            return true;
        };
        if a <= b {
            now >= a && now < b
        } else {
            now >= a || now < b
        }
    }
}

pub fn parse_hhmm(s: &str) -> Option<chrono::NaiveTime> {
    chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AccountPrefs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// "personal", "business" or "sharepoint"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "is_default")]
    pub schedule: Schedule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_resync: Option<bool>,
    /// Unix time until which a manual "Sync now" overrides the schedule.
    #[serde(skip_serializing_if = "is_zero")]
    pub override_until: i64,
    /// Unix time until which syncing is paused from the tray.
    #[serde(skip_serializing_if = "is_zero")]
    pub pause_until: i64,
    /// The tray stopped it for a metered network or battery saver, so it starts it again after.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub device_held: bool,
    /// Desktop folders (Documents, Pictures…) backed up into this account
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<crate::backup::Backup>,
    /// The storage warning already given: 0 none, 90 nearly full, 100 full.
    #[serde(skip_serializing_if = "is_zero_u8")]
    pub quota_warned: u8,
}

impl AccountPrefs {
    pub fn paused(&self) -> bool {
        self.pause_until > Local::now().timestamp()
    }
    /// Whether the background service should be running right now. "Sync now" (the override)
    /// gets past a schedule and a metered network or battery saver for an hour.
    pub fn sync_allowed(&self) -> bool {
        let now = Local::now();
        let overridden = self.override_until > now.timestamp();
        !self.paused()
            && (self.schedule.allows(now.time()) || overridden)
            && (device_hold().is_none() || overridden)
    }
}

/// Which storage warning is due for an account using `used` of `total`, given the one already
/// given: Some(level) to warn now (and remember), None for no change. Back under 85% resets it,
/// so filling up again warns again.
pub fn quota_warning(used: u64, total: u64, warned: u8) -> Option<u8> {
    if total == 0 {
        return None;
    }
    let pct = used as f64 * 100.0 / total as f64;
    let level = if pct >= 100.0 {
        100
    } else if pct >= 90.0 {
        90
    } else if pct < 85.0 {
        0
    } else {
        warned // 85-90%: keep what was said, so it doesn't flap around 90%
    };
    (level != warned).then_some(level)
}

/// Why syncing is held back for the whole computer, as Windows does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The network connection is marked as metered (a phone hotspot, a capped plan)
    Metered,
    /// The power profile is set to power saving (battery saver)
    PowerSaver,
}

impl Hold {
    pub fn words(self) -> &'static str {
        match self {
            Hold::Metered => "metered connection",
            Hold::PowerSaver => "power saver",
        }
    }
}

/// The current hold, if the user wants syncing to pause for it (both on unless switched off in
/// App settings). Asked often, so the answer is kept for ten seconds.
pub fn device_hold() -> Option<Hold> {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<Hold>)>> =
        std::sync::Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, h)) = *c
        && at.elapsed() < std::time::Duration::from_secs(10)
    {
        return h;
    }
    // On a thread of its own: the tray icon's D-Bus service asks for this (the tooltip) from
    // inside its async runtime, where a blocking D-Bus call panics.
    let h = std::thread::spawn(|| {
        let s = AppSettings::load();
        if s.pause_on_metered.unwrap_or(true) && crate::dbus::network_metered() {
            Some(Hold::Metered)
        } else if s.pause_on_power_saver.unwrap_or(true) && crate::dbus::power_saver() {
            Some(Hold::PowerSaver)
        } else {
            None
        }
    })
    .join()
    .unwrap_or(None);
    *c = Some((std::time::Instant::now(), h));
    h
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct AppSettings {
    /// Keyed by config directory path.
    pub accounts: BTreeMap<String, AccountPrefs>,
    /// Config directories the user removed but whose (unmanaged) service still exists.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
    /// The see-through window: None follows the desktop (see ui::glass_wanted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glass: Option<bool>,
    /// Quitting the tray icon also stops syncing (until the app starts again)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stop_sync_on_quit: bool,
    /// Look for new versions once a day (None: yes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_updates: Option<bool>,
    /// The newest version the user was already told about
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_notified: Option<String>,
    /// Sync services started outside this app (see OutsideStart) the user chose to leave as they are
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outside_start_kept: Vec<String>,
    /// Pause syncing on a metered network (None: yes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_on_metered: Option<bool>,
    /// Pause syncing while battery saver is on (None: yes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_on_power_saver: Option<bool>,
    /// Sync badges on files in Dolphin, Nemo and Caja (None: yes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_badges: Option<bool>,
    /// Prevents a fallback/default value from overwriting settings that could not be loaded.
    #[serde(skip)]
    load_error: Option<String>,
}

impl AppSettings {
    pub fn path() -> PathBuf {
        app_dir().join("settings.json")
    }
    pub fn load() -> Self {
        let path = Self::path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                return Self {
                    load_error: Some(format!("Couldn't read {}: {e}", path.display())),
                    ..Self::default()
                };
            }
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            // keep the damaged file aside, then carry on with defaults; if no copy can be kept,
            // refuse to save so the only copy isn't overwritten
            let mut settings = Self::default();
            match preserve_damaged_settings(&path, &text) {
                Ok(aside) => eprintln!(
                    "{} couldn't be read ({e}); a copy was kept as {}",
                    path.display(),
                    aside.display()
                ),
                Err(copy_error) => {
                    settings.load_error = Some(format!(
                        "{} is damaged ({e}) and no copy could be kept ({copy_error}), so it isn't replaced",
                        path.display()
                    ));
                }
            }
            settings
        })
    }
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(error) = &self.load_error {
            return Err(std::io::Error::other(error.clone()));
        }
        create_private_dir(&app_dir())?;
        let mut tidy = self.clone();
        tidy.accounts.retain(|_, p| *p != AccountPrefs::default());
        atomic_write(
            &Self::path(),
            serde_json::to_string_pretty(&tidy).unwrap().as_bytes(),
        )
    }
    pub fn prefs(&self, confdir: &Path) -> AccountPrefs {
        self.accounts
            .get(&confdir.to_string_lossy().to_string())
            .cloned()
            .unwrap_or_default()
    }
    pub fn prefs_mut(&mut self, confdir: &Path) -> &mut AccountPrefs {
        self.accounts
            .entry(confdir.to_string_lossy().to_string())
            .or_default()
    }
}

/// Keeps each damaged settings file in its own exclusive backup. A pre-existing backup may
/// belong to an earlier incident and is not proof that the current contents were preserved.
fn preserve_damaged_settings(path: &Path, text: &str) -> std::io::Result<PathBuf> {
    static NEXT_DAMAGED: AtomicU64 = AtomicU64::new(0);
    let first = path.with_extension("json.damaged");
    // settings are read often: a backup that already holds exactly this text is enough
    let prefix = first
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    if let Some(dir) = first.parent()
        && let Ok(entries) = fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix)
                && fs::read(entry.path()).is_ok_and(|old| old == text.as_bytes())
            {
                return Ok(entry.path());
            }
        }
    }
    for attempt in 0..32 {
        let backup = if attempt == 0 {
            first.clone()
        } else {
            let n = NEXT_DAMAGED.fetch_add(1, Ordering::Relaxed);
            PathBuf::from(format!("{}-{}-{n}", first.display(), std::process::id()))
        };
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let saved = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(e) = saved {
            drop(file);
            let _ = fs::remove_file(&backup);
            return Err(e);
        }
        return Ok(backup);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "couldn't allocate a unique damaged-settings backup after 32 attempts",
    ))
}

/// Renames `from` to `to` only if nothing is at `to` (fails with AlreadyExists otherwise), so a
/// file that appears after a name was checked is never replaced.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (from, to) = (c(from)?, c(to)?);
    // SAFETY: both paths are NUL-terminated and live for the call
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path.file_name().unwrap_or_default().to_string_lossy();
    let mut created = None;
    for _ in 0..32 {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{base}.tmp-{}-{n}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => {
                created = Some((candidate, f));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let (tmp, mut f) = created.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "couldn't allocate a unique temporary file after 32 attempts",
        )
    })?;
    let written = (|| {
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Creates a directory readable only by the user (config dirs hold sign-in tokens).
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

// ---------------------------------------------------------------- client config file

/// The client's `config` file, edited line by line so comments and unknown keys survive.
pub struct ClientConfig {
    pub path: PathBuf,
    lines: Vec<String>,
    /// False when the file exists but isn't UTF-8 text: we then show what we can but never
    /// write it, since saving our partial view would throw the rest of it away.
    writable: bool,
    load_error: Option<String>,
}

/// An error for a file the app can't safely rewrite because it isn't UTF-8 text.
fn not_text_error(p: &Path) -> std::io::Error {
    std::io::Error::other(format!(
        "{} isn't plain UTF-8 text, so it wasn't changed. Edit it by hand, or save it as UTF-8 and try again.",
        p.display()
    ))
}

/// Whether a file exists but can't be read as UTF-8 text.
impl ClientConfig {
    pub fn load(confdir: &Path) -> Self {
        let path = confdir.join("config");
        let (lines, writable, load_error) = match fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => (s.lines().map(String::from).collect(), true, None),
                Err(e) => (
                    String::from_utf8_lossy(e.as_bytes())
                        .lines()
                        .map(String::from)
                        .collect(),
                    false,
                    Some(not_text_error(&path).to_string()),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), true, None),
            Err(e) => (
                Vec::new(),
                false,
                Some(format!("Couldn't read {}: {e}", path.display())),
            ),
        };
        ClientConfig {
            path,
            lines,
            writable,
            load_error,
        }
    }

    fn key_of(line: &str) -> Option<&str> {
        let l = line.trim();
        if l.starts_with('#') {
            return None;
        }
        Some(l.split_once('=')?.0.trim())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.lines
            .iter()
            .rev()
            .find(|l| Self::key_of(l) == Some(key))
            .map(|l| {
                let v = l.split_once('=').unwrap().1.trim();
                v.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(v)
                    .to_string()
            })
    }

    /// Every value of a key that may be given more than once (`skip_dir`), joined with `|` as
    /// the client joins them; None if it isn't set.
    pub fn get_joined(&self, key: &str) -> Option<String> {
        let all: Vec<String> = self
            .lines
            .iter()
            .filter(|l| Self::key_of(l) == Some(key))
            .map(|l| {
                let v = l.split_once('=').unwrap().1.trim();
                v.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(v)
                    .to_string()
            })
            .filter(|v| !v.is_empty())
            .collect();
        (!all.is_empty()).then(|| all.join("|"))
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key)
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    /// Sets (or with `None` removes) a key.
    pub fn set(&mut self, key: &str, value: Option<&str>) {
        let pos = self.lines.iter().position(|l| Self::key_of(l) == Some(key));
        self.lines.retain(|l| Self::key_of(l) != Some(key));
        if let Some(v) = value {
            let line = format!("{key} = \"{}\"", v.replace('"', "\\\""));
            let at = pos.unwrap_or(self.lines.len()).min(self.lines.len());
            self.lines.insert(at, line);
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        if !self.writable {
            return Err(std::io::Error::other(
                self.load_error
                    .clone()
                    .unwrap_or_else(|| not_text_error(&self.path).to_string()),
            ));
        }
        if let Some(d) = self.path.parent().filter(|d| !d.exists()) {
            create_private_dir(d)?;
        }
        let mut s = self.lines.join("\n");
        s.push('\n');
        atomic_write(&self.path, s.as_bytes())
    }

    pub fn sync_dir(&self) -> PathBuf {
        let raw = self.get("sync_dir").unwrap_or_else(|| "~/OneDrive".into());
        expand_tilde(&raw)
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else if p == "~" {
        home()
    } else {
        PathBuf::from(p)
    }
}

/// The selective-sync list. Empty means "sync everything".
pub fn read_sync_list(confdir: &Path) -> std::io::Result<Vec<String>> {
    let path = confdir.join("sync_list");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

pub fn write_sync_list(confdir: &Path, entries: &[String]) -> std::io::Result<()> {
    let p = confdir.join("sync_list");
    let existing = match fs::read(&p) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| not_text_error(&p))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if entries.is_empty() {
        if let Some(text) = existing {
            // keep any rules the user wrote by hand, which syncing everything can't honour
            let own_rules = text
                .lines()
                .map(str::trim)
                .any(|l| !l.is_empty() && !l.starts_with('#') && sync_list_folder(l).is_none());
            if own_rules {
                fs::copy(&p, confdir.join("sync_list.saved"))?;
            }
            fs::remove_file(p)?;
        }
        return Ok(());
    }
    let mut s =
        format!("# Written by {APP_NAME}. One folder per line; '/' is the top of your OneDrive.\n");
    for e in entries {
        s.push_str(e);
        s.push('\n');
    }
    atomic_write(&p, s.as_bytes())
}

/// Turns a sync_list line into the folder path it selects ("/A/B"), if it is a plain folder rule.
pub fn sync_list_folder(rule: &str) -> Option<String> {
    if rule.starts_with('!') || rule.starts_with('-') {
        return None;
    }
    let r = rule.trim_end_matches("/*").trim_end_matches('/');
    let r = if r.starts_with('/') {
        r.to_string()
    } else {
        format!("/{r}")
    };
    (r.len() > 1).then_some(r)
}

/// Filesystems that can't store Unix permissions, where the client must not try to set them.
pub fn needs_disable_permission_set(dir: &Path) -> bool {
    let probe = dir
        .ancestors()
        .find(|a| a.exists())
        .unwrap_or(Path::new("/"));
    let out = Command::new("findmnt")
        .args(["-no", "FSTYPE", "-T"])
        .arg(probe)
        .output();
    let fs = out
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    matches!(
        fs.as_str(),
        "ntfs" | "ntfs3" | "fuseblk" | "vfat" | "exfat" | "msdos" | "cifs" | "smb3"
    )
}

// ---------------------------------------------------------------- systemd

pub fn systemctl(args: &[&str]) -> (bool, String) {
    match Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, e.to_string()),
    }
}

pub fn unit_props(unit: &str, props: &str) -> HashMap<String, String> {
    systemctl(&["show", "-p", props, unit])
        .1
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnitState {
    pub exists: bool,
    pub active: bool,
    pub starting: bool,
    /// Failed and waiting to try again (systemd's Restart=on-failure)
    pub retrying: bool,
    pub failed: bool,
    pub enabled: bool,
    pub exit_status: Option<i32>,
}

pub fn unit_state(unit: &str) -> UnitState {
    let p = unit_props(
        unit,
        "LoadState,ActiveState,SubState,UnitFileState,ExecMainStatus",
    );
    let active = p.get("ActiveState").map(String::as_str).unwrap_or("");
    let sub = p.get("SubState").map(String::as_str).unwrap_or("");
    let retrying = active == "activating" && sub == "auto-restart";
    UnitState {
        exists: p.get("LoadState").map(String::as_str) == Some("loaded"),
        active: matches!(active, "active" | "activating" | "reloading"),
        starting: (active == "activating" && !retrying) || sub == "start-pre",
        retrying,
        failed: active == "failed",
        enabled: p.get("UnitFileState").map(String::as_str) == Some("enabled"),
        exit_status: p
            .get("ExecMainStatus")
            .and_then(|s| s.parse().ok())
            .filter(|s| *s != 0),
    }
}

/// Name of the transient unit used for a one-off resync of an account.
pub fn resync_unit(confdir: &Path) -> String {
    format!(
        "{APP_ID}-resync-{}.service",
        slug(&confdir.file_name().unwrap_or_default().to_string_lossy())
    )
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "account".into()
    } else {
        out
    }
}

pub fn systemd_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

/// Writes (or rewrites) the user unit we manage for an account.
pub fn write_managed_unit(unit: &str, name: &str, confdir: &Path) -> std::io::Result<()> {
    let client = client_binary().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the OneDrive Client for Linux isn't installed",
        )
    })?;
    fs::create_dir_all(user_unit_dir())?;
    let path = user_unit_dir().join(unit);
    match fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} isn't a regular managed unit file", path.display()),
            ));
        }
        Ok(_) => {
            let text = fs::read_to_string(&path)?;
            if !is_managed(&text) || confdir_from_exec(&text).as_deref() != Some(confdir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already belongs to another unit or account",
                        path.display()
                    ),
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let body = format!(
        "[Unit]\n\
         Description=OneDrive sync ({})\n\
         Documentation=https://github.com/sweatshopking/DriveDeck\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         {}\n\
         {MANAGED_MARK}\n\
         \n\
         [Service]\n\
         ExecStart={} --monitor --confdir {}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         RestartPreventExitStatus=126 78\n\
         TimeoutStopSec=90\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        unit_text(name),
        condition_line(&client),
        systemd_quote(&client.to_string_lossy()),
        systemd_quote(&confdir.to_string_lossy()),
    );
    atomic_write(&path, body.as_bytes())?;
    let (ok, err) = systemctl(&["daemon-reload"]);
    if !ok {
        return Err(std::io::Error::other(format!(
            "systemd couldn't reload the user unit files: {}",
            err.trim()
        )));
    }
    Ok(())
}

/// Marks an account as in use by the settings window (a preview's dry run, a sign-in), which runs
/// the client itself with the account's unit stopped. While the marker exists and its window is
/// alive, nothing else may start that unit: two clients on one account can damage its database.
pub struct BusyMarker(PathBuf, String, fs::File);

/// Linux process start time in clock ticks, used with a PID so a recycled PID isn't mistaken for
/// the process that created a runtime marker.
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end = stat.rfind(") ")? + 2;
    stat[end..].split_whitespace().nth(19)?.parse().ok()
}

fn busy_path(confdir: &Path) -> PathBuf {
    runtime_dir()
        .join("busy")
        .join(slug(&confdir.to_string_lossy()))
}

impl BusyMarker {
    pub fn try_new(confdir: &Path) -> std::io::Result<Self> {
        Self::try_new_at(busy_path(confdir))
    }

    fn try_new_at(p: PathBuf) -> std::io::Result<Self> {
        if let Some(dir) = p.parent() {
            create_private_dir(dir)?;
        }
        let pid = std::process::id();
        let start = process_start_time(pid).ok_or_else(|| {
            std::io::Error::other("couldn't identify the current process for the busy marker")
        })?;
        let token = format!("{pid}:{start}");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&p)?;
        // Hold an advisory lock for the marker's entire lifetime. Checking a stale PID and then
        // unlinking its path had a race: two contenders could both inspect the stale file, and
        // the slower one could remove the faster one's newly-created live marker.
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "this account is already in use by another operation",
                ));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
        let mut existing = String::new();
        file.read_to_string(&mut existing)?;
        let owner_is_live = if let Some((old_pid, old_start)) = existing.trim().split_once(':') {
            old_pid
                .parse::<u32>()
                .ok()
                .zip(old_start.parse::<u64>().ok())
                .is_some_and(|(pid, start)| {
                    Path::new(&format!("/proc/{pid}")).exists()
                        && process_start_time(pid) == Some(start)
                })
        } else {
            existing
                .trim()
                .parse::<u32>()
                .ok()
                .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
        };
        if owner_is_live {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "this account is already in use by another operation",
            ));
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "{token}")?;
        file.sync_all()?;
        Ok(BusyMarker(p, token, file))
    }
}

impl Drop for BusyMarker {
    fn drop(&mut self) {
        if fs::read_to_string(&self.0)
            .ok()
            .is_some_and(|token| token.trim() == self.1)
        {
            let _ = fs::remove_file(&self.0);
        }
        // Keep the lock until after the path check and unlink have completed.
        let _ = self.2.unlock();
    }
}

/// Whether the settings window is using this account right now.
pub fn account_busy(confdir: &Path) -> bool {
    let Some(value) = fs::read_to_string(busy_path(confdir)).ok() else {
        return false;
    };
    let value = value.trim();
    if let Some((pid, start)) = value.split_once(':') {
        let (Ok(pid), Ok(start)) = (pid.parse::<u32>(), start.parse::<u64>()) else {
            return false;
        };
        Path::new(&format!("/proc/{pid}")).exists() && process_start_time(pid) == Some(start)
    } else {
        value
            .parse::<u32>()
            .ok()
            .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
    }
}

fn hold_path(unit: &str) -> PathBuf {
    runtime_dir().join("hold").join(slug(unit))
}

/// Stops a unit that keeps restarting into big-delete protection (each run would stop again,
/// and the client may notify each time), and keeps automatic starts (schedules, the app
/// starting) from starting it again. Called while systemd waits to restart it. Starting it on
/// purpose (`unit_action`) lifts the hold. Kept in the runtime folder, so a new login tries
/// again.
pub fn hold_for_big_delete(unit: &str) {
    let p = hold_path(unit);
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(&p, b"");
    run_detached("systemctl", &["--user", "stop", unit]);
}

pub fn unit_held(unit: &str) -> bool {
    hold_path(unit).exists()
}

pub fn release_hold(unit: &str) {
    let _ = fs::remove_file(hold_path(unit));
}

/// Restarts the sync units that are running, so they pick up a client that was just updated.
/// `done`: ones already restarted (by `repair_managed_units`).
pub fn restart_active_units(settings: &AppSettings, done: &[String]) {
    for acc in discover(settings) {
        if let Some(u) = &acc.unit
            && !done.contains(u)
            && !account_busy(&acc.confdir)
            && unit_state(u).active
        {
            systemctl(&["try-restart", u]);
        }
    }
}

/// Stops every account's sync service ("Quit and stop syncing"). Doesn't wait: systemd finishes
/// stopping them after we're gone. A full resync under way is left to finish.
pub fn stop_all_units(settings: &AppSettings) {
    let units: Vec<String> = discover(settings)
        .into_iter()
        .filter_map(|a| a.unit)
        .collect();
    if units.is_empty() {
        return;
    }
    let mut args = vec!["stop", "--no-block"];
    args.extend(units.iter().map(String::as_str));
    let _ = systemctl(&args);
}

/// Starts the enabled sync units that aren't running, e.g. ones skipped while the client was
/// missing. Pauses and schedules are applied by the tray right after, as usual.
pub fn start_idle_units(settings: &AppSettings) {
    for acc in discover(settings) {
        let Some(u) = &acc.unit else { continue };
        if account_busy(&acc.confdir) {
            continue;
        }
        let st = unit_state(u);
        if st.enabled
            && !st.active
            && acc.signed_in
            && !unit_held(u)
            && settings.prefs(&acc.confdir).sync_allowed()
        {
            systemctl(&["start", u]);
        }
    }
}

/// This app's command for a .desktop file's Exec= line, quoted by the Desktop Entry rules:
/// `"`, `` ` ``, `$` and `\` escaped inside the quotes, the file format's own backslash escaping on
/// top of that, and `%` doubled so it isn't read as a field code.
pub fn desktop_exec() -> String {
    desktop_exec_arg(&self_command().to_string_lossy())
}

pub fn desktop_exec_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str(r"\\");
                out.push(c);
            }
            '\\' => out.push_str(r"\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Text that is safe in a unit file setting: one line, and `%` (systemd's specifier marker) escaped.
pub fn unit_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('%', "%%")
}

/// Why `folder` can't be used as an account's sync folder, if it can't.
pub fn unsuitable_sync_folder(folder: &Path) -> Option<String> {
    let shown = folder.display();
    if !folder.is_absolute() {
        return Some("The local folder must be a full path, like /home/you/OneDrive.".into());
    }
    let text = folder.to_string_lossy();
    if text.contains('"') || text.chars().any(char::is_control) {
        return Some(
            "The sync engine can't use a folder whose name contains double quotes. Pick or rename a folder without them."
                .into(),
        );
    }
    let Some(checked) = canonicalize_with_missing_tail(folder) else {
        return Some(
            "Couldn't safely resolve that folder path. Check its parent directories and try again."
                .into(),
        );
    };
    let Some(home) = canonicalize_with_missing_tail(&home()) else {
        return Some(
            "Couldn't safely resolve your home folder. Check the home-folder configuration.".into(),
        );
    };
    if checked == home || home.starts_with(&checked) {
        let what = if checked == home {
            "everything in your home folder"
        } else {
            "everything on this computer"
        };
        return Some(format!(
            "Syncing {shown} would upload {what}. Pick or create a folder inside your home folder, like {}.",
            home.join("OneDrive").display()
        ));
    }
    for own in [config_home(), data_home()] {
        let Some(own) = canonicalize_with_missing_tail(&own) else {
            return Some("Couldn't safely resolve the application settings folders.".into());
        };
        if checked.starts_with(&own) || own.starts_with(&checked) {
            return Some(format!(
                "{shown} holds your settings. Pick a different folder."
            ));
        }
    }
    let user_mounts = ["/run/media/", "/media/", "/mnt/"];
    let system = [
        "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/opt", "/proc", "/root", "/run",
        "/sbin", "/srv", "/sys", "/tmp", "/usr", "/var",
    ];
    let under = |top: &str| checked == Path::new(top) || checked.starts_with(format!("{top}/"));
    // inside the home folder is never a system folder, even where /home is really /var/home
    // (Fedora Silverblue, Kinoite, Bazzite)
    if !checked.starts_with(&home)
        && !user_mounts
            .iter()
            .any(|m| checked.to_string_lossy().starts_with(m))
        && system.iter().any(|t| under(t))
    {
        return Some(format!(
            "{shown} is a system folder. Pick a folder in your home folder or on a drive of your own."
        ));
    }
    if folder.is_file() {
        return Some(format!("{shown} is a file, not a folder."));
    }
    None
}

/// Resolve symlinked existing ancestors and normalize a not-yet-created suffix.
fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut resolved = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir => resolved = PathBuf::from("/"),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = fs::canonicalize(&resolved).ok()?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
            std::path::Component::Prefix(_) => return None,
        }
    }
    Some(resolved)
}

/// Resolves existing symlinks and normalizes any missing suffix of an absolute path.
pub fn resolve_path(path: &Path) -> Option<PathBuf> {
    canonicalize_with_missing_tail(path)
}

/// Whether two paths overlap after resolving symlinked existing ancestors.
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let Some(a) = canonicalize_with_missing_tail(a) else {
        return true;
    };
    let Some(b) = canonicalize_with_missing_tail(b) else {
        return true;
    };
    a.starts_with(&b) || b.starts_with(&a)
}

/// Makes a unit skip quietly, instead of failing every few seconds, if the client is uninstalled.
fn condition_line(program: &Path) -> String {
    format!(
        "ConditionPathExists={}",
        program.to_string_lossy().replace('%', "%%")
    )
}

/// Keeps our managed units running the installed client: follows it if it moves (e.g. from
/// /usr/local/bin to /usr/bin), and converts units from versions that ran it through this app.
/// Returns the units it restarted.
pub fn repair_managed_units() -> Vec<String> {
    let Some(client) = client_binary() else {
        return Vec::new(); // nothing to point them at; they skip quietly until the client is installed
    };
    let Ok(rd) = fs::read_dir(user_unit_dir()) else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        if !is_managed(&text) {
            continue;
        }
        let Some(confdir) = confdir_from_exec(&text) else {
            continue;
        };
        let exec = format!(
            "ExecStart={} --monitor --confdir {}",
            systemd_quote(&client.to_string_lossy()),
            systemd_quote(&confdir.to_string_lossy())
        );
        let mut new = Vec::new();
        for l in text.lines() {
            if l.starts_with("ConditionPathExists=") {
                continue; // rewritten below
            }
            if l == MANAGED_MARK || l == OLD_MANAGED_MARK {
                new.push(condition_line(&client));
                new.push(MANAGED_MARK.to_string());
                continue;
            }
            new.push(if l.starts_with("ExecStart=") {
                exec.clone()
            } else {
                l.to_string()
            });
        }
        let new = new.join("\n") + "\n";
        // only the old mark changed: no need to restart a sync for a comment
        let only_mark = new == text.replace(OLD_MANAGED_MARK, MANAGED_MARK);
        if new != text && atomic_write(&p, new.as_bytes()).is_ok() && !only_mark {
            changed.push(e.file_name().to_string_lossy().to_string());
        }
    }
    if changed.is_empty() {
        return changed;
    }
    systemctl(&["daemon-reload"]);
    for u in &changed {
        let st = unit_state(u);
        if st.enabled {
            // pick up the new command (a running unit keeps the old one until restarted)
            systemctl(&["try-restart", u]);
        }
    }
    changed
}

// ---------------------------------------------------------------- accounts

#[derive(Clone, Debug, PartialEq)]
pub struct AccountInfo {
    pub name: String,
    pub confdir: PathBuf,
    pub unit: Option<String>,
    pub managed: bool,
    pub sync_dir: PathBuf,
    pub drive_id: Option<String>,
    pub kind: String,
    pub signed_in: bool,
}

impl AccountInfo {
    pub fn key(&self) -> String {
        self.confdir.to_string_lossy().to_string()
    }
    pub fn kind_label(&self) -> &'static str {
        match self.kind.as_str() {
            "sharepoint" => "SharePoint library",
            "business" => "Work or school",
            "personal" => "Personal",
            _ => "OneDrive",
        }
    }
}

fn units_in(dir: &Path) -> Vec<(String, String)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            (n.starts_with("onedrive") && n.ends_with(".service") && !n.contains('@'))
                .then(|| (n, fs::read_to_string(e.path()).unwrap_or_default()))
        })
        .collect()
}

fn confdir_from_exec(text: &str) -> Option<PathBuf> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("ExecStart="))?;
    let mut words = shell_words(line.split_once('=')?.1);
    while let Some(w) = words.first() {
        if w == "--confdir" {
            return words
                .get(1)
                .map(|d| expand_tilde(&d.replace("%h", &home().to_string_lossy())));
        }
        if let Some(d) = w.strip_prefix("--confdir=") {
            return Some(expand_tilde(&d.replace("%h", &home().to_string_lossy())));
        }
        words.remove(0);
    }
    Some(config_home().join("onedrive"))
}

fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (Some(q), c) if c == q => quote = None,
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur).replace("%%", "%"));
                    any = false;
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur.replace("%%", "%"));
    }
    out
}

fn description_name(text: &str) -> Option<String> {
    let d = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Description="))?;
    let (a, b) = (d.find('(')?, d.rfind(')')?);
    // undo unit_text()'s escaping of systemd's % specifiers
    (b > a + 1).then(|| d[a + 1..b].replace("%%", "%"))
}

/// A sync service someone switched on outside this app for the whole computer: for every user at
/// login (`systemctl --global enable`, `system` false) or for this user at boot
/// (`systemctl enable onedrive@<user>`). The admin may have set it up on purpose, or an older
/// onedrive package left it behind; it keeps starting whatever this app does, so we only report
/// it, and switch it off only when asked.
#[derive(Debug, Clone, PartialEq)]
pub struct OutsideStart {
    pub unit: String,
    pub system: bool,
}

pub fn outside_starts() -> Vec<OutsideStart> {
    outside_starts_in(
        Path::new("/etc/systemd/user"),
        Path::new("/etc/systemd/system"),
        &login_name(),
    )
}

fn login_name() -> String {
    // SAFETY: getpwuid returns a pointer into static storage (or null); it is read straight away
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return std::env::var("USER").unwrap_or_default();
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

fn outside_starts_in(user_dir: &Path, system_dir: &Path, login: &str) -> Vec<OutsideStart> {
    let mut out: Vec<OutsideStart> = Vec::new();
    let system_unit = format!("onedrive@{login}.service");
    for (dir, system) in [(user_dir, false), (system_dir, true)] {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for wants in rd.flatten() {
            if !wants.file_name().to_string_lossy().ends_with(".wants") {
                continue;
            }
            let Ok(units) = fs::read_dir(wants.path()) else {
                continue;
            };
            for u in units.flatten() {
                let unit = u.file_name().to_string_lossy().into_owned();
                let ours = if system {
                    !login.is_empty() && unit == system_unit
                } else {
                    unit.starts_with("onedrive") && unit.ends_with(".service")
                };
                if ours && !out.iter().any(|o| o.unit == unit && o.system == system) {
                    out.push(OutsideStart { unit, system });
                }
            }
        }
    }
    out.sort_by(|a, b| (a.system, &a.unit).cmp(&(b.system, &b.unit)));
    out
}

/// Every account we can find: user units named onedrive*.service, plus config directories
/// (~/.config/onedrive*) that have a config or sign-in but no unit yet.
pub fn discover(settings: &AppSettings) -> Vec<AccountInfo> {
    let mut by_dir: BTreeMap<PathBuf, (Option<String>, bool, Option<String>)> = BTreeMap::new();
    let mut sys_units: Vec<(String, String)> = Vec::new();
    for d in [
        "/usr/lib/systemd/user",
        "/lib/systemd/user",
        "/etc/systemd/user",
    ] {
        sys_units.extend(units_in(Path::new(d)));
    }
    let user_units = units_in(&user_unit_dir());
    let user_names: Vec<&String> = user_units.iter().map(|(n, _)| n).collect();
    let mut all: Vec<(String, String, bool)> = user_units
        .iter()
        .map(|(n, t)| (n.clone(), t.clone(), true))
        .collect();
    for (n, t) in &sys_units {
        if !user_names.contains(&n) {
            all.push((n.clone(), t.clone(), false));
        }
    }
    for (unit, text, is_user) in all {
        let Some(dir) = confdir_from_exec(&text) else {
            continue;
        };
        if !is_user {
            // a distro-provided unit only counts once it is enabled or the account exists
            let st = unit_state(&unit);
            if !st.enabled && !st.active && !dir.join("refresh_token").exists() {
                continue;
            }
        }
        let managed = is_managed(&text);
        let entry = by_dir.entry(dir).or_insert((None, false, None));
        // prefer an enabled unit if several point at the same config dir
        if entry.0.is_none() || unit_state(&unit).enabled {
            *entry = (Some(unit), managed, description_name(&text));
        }
    }
    if let Ok(rd) = fs::read_dir(config_home()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            if n.starts_with("onedrive")
                && p.is_dir()
                && (p.join("config").exists() || p.join("refresh_token").exists())
            {
                by_dir.entry(p).or_insert((None, false, None));
            }
        }
    }

    let mut out: Vec<AccountInfo> = by_dir
        .into_iter()
        .filter(|(dir, _)| !settings.hidden.contains(&dir.to_string_lossy().to_string()))
        .map(|(confdir, (unit, managed, desc))| {
            let cfg = ClientConfig::load(&confdir);
            let prefs = settings.prefs(&confdir);
            let drive_id = cfg.get("drive_id").filter(|s| !s.is_empty());
            let kind = prefs.kind.clone().unwrap_or_else(|| {
                if drive_id.is_some() {
                    "sharepoint".into()
                } else {
                    String::new()
                }
            });
            let base = confdir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let name = prefs.name.clone().or(desc).unwrap_or_else(|| {
                if base == "onedrive" {
                    match kind.as_str() {
                        "personal" => "Personal".into(),
                        "business" => "Work or school".into(),
                        _ => "OneDrive".into(),
                    }
                } else {
                    base.trim_start_matches("onedrive")
                        .trim_start_matches(['-', '_', '.'])
                        .to_string()
                }
            });
            let name = if name.is_empty() { base.clone() } else { name };
            AccountInfo {
                name,
                sync_dir: cfg.sync_dir(),
                signed_in: confdir.join("refresh_token").exists(),
                confdir,
                unit,
                managed,
                drive_id,
                kind,
            }
        })
        .collect();
    out.sort_by_key(|a| {
        (
            a.confdir != config_home().join("onedrive"),
            a.name.to_lowercase(),
        )
    });
    out
}

/// The files the client keeps in a configuration folder, plus the "-shm"/"-wal" companions of its
/// databases and its resume files (`session_upload.*`, `resume_download.*`).
const CLIENT_FILES: &[&str] = &[
    "config",
    ".config.backup",
    ".config.hash",
    "refresh_token",
    "intune_account",
    "items.sqlite3",
    "items-dryrun.sqlite3",
    "sync_list",
    "sync_list.saved",
    ".sync_list.hash",
    "business_shared_items",
    ".business_shared_items.hash",
];

fn is_client_file(name: &str) -> bool {
    let db = name
        .strip_suffix("-shm")
        .or_else(|| name.strip_suffix("-wal"))
        .unwrap_or(name);
    CLIENT_FILES.contains(&db)
        || name.starts_with("session_upload.")
        || name.starts_with("resume_download.")
}

/// An account's settings folder must be a folder (a link to one is fine: dotfile managers make
/// those).
pub fn validate_confdir(confdir: &Path) -> std::io::Result<()> {
    let meta = fs::metadata(confdir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} isn't a folder",
            confdir.display()
        )));
    }
    Ok(())
}

/// Deletes an account's configuration (its settings, sign-in and database). Only the client's
/// own files are deleted, and then the folder if that left it empty: a unit written by hand can
/// point the client at any folder, even the home folder or the one with the synced files, and
/// nothing else in it may go. Returns true when the folder itself is gone.
pub fn delete_confdir(confdir: &Path) -> std::io::Result<bool> {
    match validate_confdir(confdir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e),
    }
    for entry in fs::read_dir(confdir)? {
        let e = entry?;
        let is_file = !e.file_type()?.is_dir();
        if is_file && is_client_file(&e.file_name().to_string_lossy()) {
            fs::remove_file(e.path())?;
        }
    }
    // a linked folder (dotfile managers) keeps the link and what it points at
    if fs::symlink_metadata(confdir)?.file_type().is_symlink() {
        return Ok(false);
    }
    match fs::remove_dir(confdir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e)
            if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                || e.raw_os_error() == Some(libc::ENOTEMPTY) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Makes sure an account has a background service, creating a managed one if needed.
pub fn ensure_unit(acc: &AccountInfo) -> std::io::Result<String> {
    if let Some(u) = &acc.unit {
        return Ok(u.clone());
    }
    let base = acc
        .confdir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let stem = if base == "onedrive" {
        "onedrive".to_string()
    } else {
        format!("onedrive-{}", slug(base.trim_start_matches("onedrive")))
    };
    for n in 1..=1000 {
        let unit = if n == 1 {
            format!("{stem}.service")
        } else {
            format!("{stem}-{n}.service")
        };
        let path = user_unit_dir().join(&unit);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match write_managed_unit(&unit, &acc.name, &acc.confdir) {
                    Ok(()) => return Ok(unit),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(_) => {
                let belongs_here = fs::read_to_string(&path).is_ok_and(|text| {
                    is_managed(&text)
                        && confdir_from_exec(&text).as_deref() == Some(acc.confdir.as_path())
                });
                if belongs_here {
                    write_managed_unit(&unit, &acc.name, &acc.confdir)?;
                    return Ok(unit);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "couldn't find an unused service name after 1,000 attempts",
    ))
}

// ---------------------------------------------------------------- log parsing

#[derive(Clone, Debug, PartialEq)]
pub enum LogEvent {
    Syncing,
    Complete,
    Error(String),
    File {
        kind: &'static str,
        path: String,
    },
    /// One file the client couldn't sync; the rest of the account is fine.
    Blocked {
        path: String,
        reason: BlockReason,
    },
    /// A new client run: it re-checks every pending file, so old blocks are re-reported if
    /// they still apply.
    ClientStart,
    /// The client stopped rather than delete many items online ("big delete" protection)
    BigDelete,
    /// How far a large file's transfer has got (the client reports these every 5% or so)
    Progress {
        up: bool,
        path: String,
        percent: u8,
        /// "hh:mm:ss" left, when the client could tell
        eta: Option<String>,
        done: bool,
    },
    NeedsResync,
    NeedsLogin,
}

/// Why the client left one file alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlockReason {
    /// Open for editing (checked out or locked) by someone in SharePoint or Office
    Locked,
    /// Shared with this account as read-only, so changes can't go up
    ReadOnly,
    /// Shared as view-only without download permission
    NoDownload,
    /// OneDrive turned the upload away for now
    NotNow,
}

/// The client's messages for a single file it couldn't sync, and why.
const BLOCKED_MESSAGES: [(&str, BlockReason); 4] = [
    (
        "Unable to upload this modified file as this is currently checked out or locked for editing by another user: ",
        BlockReason::Locked,
    ),
    (
        "Unable to upload this modified file as this was shared as read-only: ",
        BlockReason::ReadOnly,
    ),
    (
        "Unable to download this file as this was shared as read-only without download permission: ",
        BlockReason::NoDownload,
    ),
    (
        "Unable to upload this modified file at this point in time: ",
        BlockReason::NotNow,
    ),
];

/// A file left unsynced, kept until it syncs, goes away, or a new client run stops reporting it.
#[derive(Clone, Debug, PartialEq)]
pub struct Blocked {
    pub path: String,
    pub reason: BlockReason,
}

/// Keeps the list of blocked files up to date with one log event.
pub fn track_blocked(list: &mut Vec<Blocked>, ev: &LogEvent) {
    match ev {
        LogEvent::Blocked { path, reason } => {
            list.retain(|b| &b.path != path);
            list.push(Blocked {
                path: path.clone(),
                reason: *reason,
            });
        }
        // uploaded, downloaded or deleted since: nothing left to do for it
        LogEvent::File { path, .. } => list.retain(|b| &b.path != path),
        LogEvent::ClientStart => list.clear(),
        _ => {}
    }
}

impl BlockReason {
    /// A few words on why, for a list
    pub fn short(self) -> &'static str {
        match self {
            BlockReason::Locked => "Didn't upload: open for editing elsewhere",
            BlockReason::ReadOnly => "Didn't upload: shared with you read-only",
            BlockReason::NoDownload => "Not downloaded: view-only",
            BlockReason::NotNow => "Didn't upload: OneDrive turned it away for now",
        }
    }

    /// Changes the user made that aren't online. A view-only file is the sharer's choice, not
    /// something to keep flagging.
    pub fn needs_attention(self) -> bool {
        self != BlockReason::NoDownload
    }
}

/// The file's own name, safe to show: no control characters (a name can hold a newline) and
/// short enough for a menu or a line of text.
pub fn shown_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if name.chars().count() > 60 {
        let head: String = name.chars().take(57).collect();
        format!("{head}…")
    } else {
        name
    }
}

/// Plain words for the files that didn't sync, or "" when there are none.
pub fn blocked_text(list: &[Blocked]) -> String {
    let mut out = Vec::new();
    for b in list.iter().take(3) {
        let name = shown_name(&b.path);
        out.push(match b.reason {
            BlockReason::Locked => format!(
                "\u{201c}{name}\u{201d} didn't upload: it's open for editing in SharePoint or Office (by someone else, or by you on another device), which locks it. Your changes are still on this PC. Once it's closed there, press \u{201c}Sync now\u{201d} to upload them."
            ),
            BlockReason::ReadOnly => format!(
                "\u{201c}{name}\u{201d} didn't upload: it's shared with you as read-only, so your changes stay only on this PC."
            ),
            BlockReason::NoDownload => format!(
                "\u{201c}{name}\u{201d} isn't on this PC: it's shared as view-only, without permission to download. Open it on the OneDrive website instead."
            ),
            BlockReason::NotNow => format!(
                "\u{201c}{name}\u{201d} didn't upload: OneDrive turned it away for now. Your changes are still on this PC. Press \u{201c}Sync now\u{201d} to try again."
            ),
        });
    }
    if list.len() > 3 {
        out.push(format!("\u{2026}and {} more files.", list.len() - 3));
    }
    out.join("\n")
}

/// A large file's progress line: `Uploading: <path> ... 62%  |  ETA    00:01:20`, or
/// `... 100% | DONE in 00:00:03` at the end (`Downloading: ` likewise).
fn parse_progress(msg: &str) -> Option<LogEvent> {
    let (up, rest) = if let Some(r) = msg.strip_prefix("Uploading: ") {
        (true, r)
    } else {
        (false, msg.strip_prefix("Downloading: ")?)
    };
    // the path can hold " ... " itself; the progress is after the last one
    let (path, tail) = rest.rsplit_once(" ... ")?;
    let (pct, after) = tail.trim_start().split_once('%')?;
    let percent: u8 = pct.trim().parse().ok().filter(|p| *p <= 100)?;
    let after = after.trim_start().strip_prefix('|')?.trim();
    let done = after.starts_with("DONE");
    let eta = after
        .strip_prefix("ETA")
        .map(str::trim)
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_digit() || c == ':'))
        .map(String::from);
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    Some(LogEvent::Progress {
        up,
        path: path.strip_prefix("./").unwrap_or(path).to_string(),
        percent,
        eta,
        done,
    })
}

/// One finished upload, download, deletion or new folder.
#[derive(Clone, Debug, PartialEq)]
pub struct FileActivity {
    pub when: DateTime<Local>,
    /// "↑", "↓", "✕" or "+", as in LogEvent::File
    pub kind: &'static str,
    pub path: String,
}

/// A large file on its way up or down.
#[derive(Clone, Debug, PartialEq)]
pub struct Transfer {
    pub up: bool,
    pub path: String,
    pub percent: u8,
    pub eta: Option<String>,
    pub when: DateTime<Local>,
}

/// A transfer with no news for this long is taken to have stopped (the client was stopped, or
/// gave up on it without saying).
const TRANSFER_STALE_SECS: i64 = 600;

/// Recent file activity (newest first, at most `max`) and the transfers still under way, from
/// `journalctl` lines oldest first.
pub fn files_from(
    lines: &[String],
    now: DateTime<Local>,
    max: usize,
) -> (Vec<FileActivity>, Vec<Transfer>) {
    let mut recent: VecDeque<FileActivity> = VecDeque::new();
    let mut transfers: Vec<Transfer> = Vec::new();
    for line in lines {
        let Some((when, msg)) = split_journal_line(line) else {
            continue;
        };
        match parse_log(msg) {
            Some(LogEvent::Progress {
                up,
                path,
                percent,
                eta,
                done,
            }) => {
                transfers.retain(|t| t.path != path);
                if !done {
                    transfers.push(Transfer {
                        up,
                        path,
                        percent,
                        eta,
                        when,
                    });
                }
            }
            Some(LogEvent::File { kind, path }) => {
                transfers.retain(|t| t.path != path);
                recent.retain(|r| r.path != path);
                recent.push_front(FileActivity { when, kind, path });
                recent.truncate(max);
            }
            // a new run: whatever was under way was cut off
            Some(LogEvent::ClientStart) => transfers.clear(),
            _ => {}
        }
    }
    transfers.retain(|t| (now - t.when).num_seconds() < TRANSFER_STALE_SECS);
    (recent.into(), transfers)
}

/// The log lines `files_from` reads.
const FILE_LINE_PREFIXES: [&str; 10] = [
    "Uploading new file: ",
    "Uploading modified file: ",
    "Downloading file: ",
    "Deleting local file: ",
    "Successfully deleted item from Microsoft OneDrive: ",
    "Created local directory: ",
    "Creating local directory: ",
    "Uploading: ",
    "Downloading: ",
    "Reading configuration file: ",
];

/// Recent file activity and transfers under way for one unit.
pub fn account_files(unit: &str, max: usize) -> (Vec<FileActivity>, Vec<Transfer>) {
    let pattern = FILE_LINE_PREFIXES
        .iter()
        .map(|p| regex_escape(p.trim_end()))
        .collect::<Vec<_>>()
        .join("|");
    // newest first with -n, then turned around; plenty more than `max`, as transfers and
    // repeated files take lines too
    let lines = journal_grep(unit, &pattern, &["-n", "1500", "--reverse"])
        .map(oldest_first)
        .unwrap_or_else(|| journal_tail(unit, 1500));
    files_from(&lines, Local::now(), max)
}

/// The kind of file, for its icon: 0 other, 1 folder, 2 document, 3 spreadsheet, 4
/// presentation, 5 PDF, 6 picture, 7 video, 8 audio, 9 archive, 10 code, 11 text.
pub fn file_kind(path: &str, folder: bool) -> i32 {
    if folder {
        return 1;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return 0;
    };
    match ext.to_ascii_lowercase().as_str() {
        "doc" | "docx" | "docm" | "odt" | "rtf" | "pages" | "dot" | "dotx" => 2,
        "xls" | "xlsx" | "xlsm" | "ods" | "csv" | "tsv" | "numbers" => 3,
        "ppt" | "pptx" | "pptm" | "odp" | "key" | "pps" | "ppsx" => 4,
        "pdf" => 5,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "bmp" | "tif" | "tiff"
        | "svg" | "avif" | "raw" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2"
        | "ico" => 6,
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "wmv" | "mpg" | "mpeg" | "3gp" => 7,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" | "aiff" | "mid" => 8,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" => 9,
        "rs" | "py" | "js" | "ts" | "html" | "htm" | "css" | "json" | "xml" | "sh" | "c"
        | "cpp" | "h" | "hpp" | "java" | "go" | "rb" | "php" | "yaml" | "yml" | "toml" | "sql"
        | "kt" | "swift" | "cs" => 10,
        "txt" | "md" | "log" | "ini" | "cfg" | "conf" => 11,
        _ => 0,
    }
}

const BIG_DELETE_COUNT: &str = "ERROR: The total number of items being deleted is: ";
const DELETING_ONLINE: &str = "Deleting item from Microsoft OneDrive: ";

/// What a big delete the client stopped for would have removed: the item it was deleting
/// online (a folder, usually) and how many items that is in all.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BigDelete {
    pub path: Option<String>,
    pub count: Option<u64>,
}

/// Reads the newest big delete's details from `journalctl --reverse` lines (newest first).
fn big_delete_from(newest_first: &[String]) -> BigDelete {
    let mut out = BigDelete::default();
    for line in newest_first {
        let Some((_, msg)) = split_journal_line(line) else {
            continue;
        };
        let msg = msg.trim();
        if out.count.is_none() {
            if let Some(n) = msg.strip_prefix(BIG_DELETE_COUNT) {
                out.count = n.trim().parse().ok();
            }
        } else if let Some(p) = msg.strip_prefix(DELETING_ONLINE) {
            // the item it was about to delete, logged just before the count
            let p = p.trim();
            out.path = Some(p.strip_prefix("./").unwrap_or(p).to_string());
            break;
        }
    }
    out
}

/// Details of the newest big delete in the unit's log.
pub fn big_delete(unit: &str) -> BigDelete {
    let pattern = format!(
        "{}|{}",
        regex_escape(BIG_DELETE_COUNT.trim_end()),
        regex_escape(DELETING_ONLINE.trim_end())
    );
    journal_grep(unit, &pattern, &["-n", "20", "--reverse"])
        .map(|lines| big_delete_from(&lines))
        .unwrap_or_default()
}

/// Plain words for a big delete the client stopped for.
pub fn big_delete_text(b: &BigDelete) -> String {
    // the client names what it was deleting (a folder, usually) and counts it with its contents
    let what = match (&b.path, b.count) {
        (Some(p), Some(n)) => format!(
            "\u{201c}{}\u{201d} and what was in it ({n} items in all)",
            shown_path(p)
        ),
        (Some(p), None) => format!("\u{201c}{}\u{201d} and what was in it", shown_path(p)),
        (None, Some(n)) => format!("{n} items"),
        (None, None) => "Many items".to_string(),
    };
    format!(
        "{what} were deleted on this computer. Syncing stopped before deleting them online too, in case that was a mistake.\nIf it was a mistake, put them back (for example from the Trash) and press \u{201c}Sync now\u{201d}. Otherwise choose what to do below."
    )
}

/// A relative path, safe to show: no control characters, and shortened from the front.
pub fn shown_path(path: &str) -> String {
    let p: Vec<char> = path
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if p.len() > 80 {
        let tail: String = p[p.len() - 77..].iter().collect();
        format!("\u{2026}{tail}")
    } else {
        p.into_iter().collect()
    }
}

pub fn parse_log(msg: &str) -> Option<LogEvent> {
    let msg = msg.trim();
    if msg.starts_with("Starting a sync with Microsoft OneDrive") {
        return Some(LogEvent::Syncing);
    }
    if msg.starts_with("Sync with Microsoft OneDrive is complete") {
        return Some(LogEvent::Complete);
    }
    if msg.contains("--resync") && (msg.contains("required") || msg.contains("must")) {
        return Some(LogEvent::NeedsResync);
    }
    if msg.contains("requires authentication")
        || msg.contains("Please authorise this application")
        || msg.contains("re-authenticate")
        || msg.contains("re-authorise")
        || msg.contains("--reauth")
        || msg.contains("invalid_grant")
    {
        return Some(LogEvent::NeedsLogin);
    }
    // the client's three lines before it exits; checked before the general ERROR rule
    if msg.contains("An attempt to remove a large volume of data from OneDrive has been detected")
        || msg.starts_with(BIG_DELETE_COUNT)
        || msg.starts_with("ERROR: To delete a large volume of data use --force")
    {
        return Some(LogEvent::BigDelete);
    }
    if let Some(p) = parse_progress(msg) {
        return Some(p);
    }
    for (prefix, reason) in BLOCKED_MESSAGES {
        if let Some(path) = msg.strip_prefix(prefix) {
            let path = path.trim();
            let path = path.strip_prefix("./").unwrap_or(path).to_string();
            return Some(LogEvent::Blocked { path, reason });
        }
    }
    if msg.starts_with("Reading configuration file: ") {
        return Some(LogEvent::ClientStart);
    }
    if msg.starts_with("ERROR") || msg.contains("ERROR:") || msg.starts_with("Unable to ") {
        return Some(LogEvent::Error(msg.to_string()));
    }
    for (prefix, kind, needs_done) in [
        ("Uploading new file: ", "↑", true),
        ("Uploading modified file: ", "↑", true),
        ("Downloading file: ", "↓", true),
        ("Deleting local file: ", "✕", false),
        (
            "Successfully deleted item from Microsoft OneDrive: ",
            "✕",
            false,
        ),
        ("Created local directory: ", "+", false),
        // older clients said "Creating"
        ("Creating local directory: ", "+", false),
    ] {
        if let Some(rest) = msg.strip_prefix(prefix) {
            if needs_done && !msg.ends_with("done") {
                return None;
            }
            let path = rest.split(" ... ").next().unwrap_or(rest).trim();
            // newer clients print paths relative to the sync folder as "./a/b"
            let path = path.strip_prefix("./").unwrap_or(path).to_string();
            return Some(LogEvent::File { kind, path });
        }
    }
    None
}

/// Splits a `journalctl -o short-unix` line into (time, message).
pub fn split_journal_line(line: &str) -> Option<(DateTime<Local>, &str)> {
    let (ts, rest) = line.split_once(' ')?;
    let (_, msg) = rest.split_once("]: ")?;
    let secs = ts.split('.').next()?.parse::<i64>().ok()?;
    Some((Local.timestamp_opt(secs, 0).single()?, msg))
}

/// Where our icons live when we're not installed system-wide (AppImage or a source checkout).
pub fn icon_theme_path() -> Option<PathBuf> {
    let mut cands = Vec::new();
    if let Ok(appdir) = std::env::var("APPDIR") {
        cands.push(PathBuf::from(appdir).join("usr/share/icons"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        cands.push(dir.join("../share/icons"));
        cands.push(dir.join("../../../assets/icons")); // target/<profile>/<bin> in a checkout
        cands.push(dir.join("../../../../assets/icons"));
    }
    cands
        .into_iter()
        .find(|p| p.join("hicolor/scalable/status/drivedeck-ok.svg").exists())
        .map(|p| p.canonicalize().unwrap_or(p))
}

/// Only the log lines that matter for status, however busy the log is, oldest first.
/// Falls back to a plain tail on systems whose journalctl lacks --grep.
pub fn journal_status_lines(unit: &str) -> Vec<String> {
    let pattern = "Reading configuration file: |Starting a sync|Sync with Microsoft OneDrive is complete|ERROR|Unable to |--resync|--reauth|requires authentication|re-authenticate|re-authorise|invalid_grant";
    let out = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "-n",
            "60",
            "-o",
            "short-unix",
            "--no-pager",
            "-q",
            // newest first, the one order every systemd version agrees on with --grep and -n
            "--reverse",
            "--grep",
            pattern,
        ])
        .output();
    match out {
        // exit status 1 with nothing on stderr just means "no matching lines"
        Ok(o) if o.status.success() || (o.status.code() == Some(1) && o.stderr.is_empty()) => {
            oldest_first(
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(String::from)
                    .collect(),
            )
        }
        _ => journal_tail(unit, 400),
    }
}

/// Sorts `short-unix` journal lines by their timestamp. journalctl --grep together with -n
/// returns the newest match first, unlike a plain tail.
/// Turns `journalctl --reverse` output around. The journal's own order is kept rather than
/// sorting by time: lines the client wrote together share a timestamp, and sorting can't tell
/// "sync started" from "sync complete" then.
fn oldest_first(mut newest_first: Vec<String>) -> Vec<String> {
    newest_first.reverse();
    newest_first
}

/// The files still blocked: the client's latest run reported them and they haven't synced since.
pub fn blocked_files(unit: &str) -> Vec<Blocked> {
    // the block messages and client starts only, newest first, then turned around
    let mut pattern = String::from("Reading configuration file: ");
    for (prefix, _) in BLOCKED_MESSAGES {
        pattern.push('|');
        pattern.push_str(&regex_escape(prefix.trim_end()));
    }
    let Some(lines) = journal_grep(unit, &pattern, &["-n", "200", "--reverse"]) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    let mut since = None;
    for line in oldest_first(lines) {
        let Some((when, msg)) = split_journal_line(&line) else {
            continue;
        };
        if let Some(ev) = parse_log(msg) {
            match ev {
                LogEvent::Blocked { .. } if since.is_none() => since = Some(when),
                LogEvent::ClientStart => since = None,
                _ => {}
            }
            track_blocked(&mut list, &ev);
        }
    }
    let Some(since) = since.filter(|_| !list.is_empty()) else {
        return list;
    };
    // a file blocked earlier may have synced since, which only the busy file lines show, so
    // replay every line about those files (and client starts) from the first block on
    let mut pattern = String::from("Reading configuration file: ");
    for b in &list {
        pattern.push('|');
        pattern.push_str(&regex_escape(&b.path));
    }
    let since = format!("@{}", since.timestamp());
    let Some(lines) = journal_grep(unit, &pattern, &["--since", &since]) else {
        return list;
    };
    let mut replayed = Vec::new();
    for line in &lines {
        if let Some((_, msg)) = split_journal_line(line)
            && let Some(ev) = parse_log(msg)
        {
            track_blocked(&mut replayed, &ev);
        }
    }
    replayed
}

/// Lines of the unit's journal matching a PCRE2 pattern, or None if journalctl can't grep.
fn journal_grep(unit: &str, pattern: &str, extra: &[&str]) -> Option<Vec<String>> {
    let o = Command::new("journalctl")
        .args(["--user", "-u", unit, "-o", "short-unix", "--no-pager", "-q"])
        .args(extra)
        .args(["--grep", pattern])
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    // exit status 1 with nothing on stderr just means "no matching lines"
    if !(o.status.success() || (o.status.code() == Some(1) && o.stderr.is_empty())) {
        return None;
    }
    Some(
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(String::from)
            .collect(),
    )
}

/// Escapes text for journalctl's --grep (PCRE2), so a file name matches literally.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if "\\.^$|?*+()[]{}".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

pub fn journal_tail(unit: &str, lines: usize) -> Vec<String> {
    Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "-n",
            &lines.to_string(),
            "-o",
            "short-unix",
            "--no-pager",
            "-q",
        ])
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Unknown,
    Syncing,
    UpToDate,
    Error,
    Stopped,
    Off,
    Paused,
    Resyncing,
    SignedOut,
    NeedsResync,
    /// Stopped rather than delete many items online; the user decides
    BigDelete,
}

impl Phase {
    /// Our own status icons (assets/icons), so the tray looks the same on every desktop.
    pub fn icon(self) -> &'static str {
        match self {
            Phase::Syncing | Phase::Unknown | Phase::Resyncing => "drivedeck-sync",
            Phase::UpToDate => "drivedeck-ok",
            Phase::Error | Phase::NeedsResync | Phase::SignedOut | Phase::BigDelete => {
                "drivedeck-error"
            }
            Phase::Stopped | Phase::Off | Phase::Paused => "drivedeck-off",
        }
    }
}

pub fn short_time(t: DateTime<Local>) -> String {
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%b %-d, %H:%M").to_string()
    }
}

/// Live status of one account, combining systemd, the log and our own settings.
#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    pub last_complete: Option<DateTime<Local>>,
    pub last_error: Option<String>,
    /// Files the client left unsynced while the account itself is fine
    pub blocked: Vec<Blocked>,
    /// What the big delete would remove (read only while the phase is BigDelete)
    pub big_delete: Option<BigDelete>,
    pub unit: UnitState,
}

impl Status {
    /// Files the user changed that couldn't go up.
    pub fn unsynced_changes(&self) -> usize {
        self.blocked
            .iter()
            .filter(|b| b.reason.needs_attention())
            .count()
    }

    pub fn text(&self, prefs: &AccountPrefs) -> String {
        match self.phase {
            Phase::Syncing if self.unit.starting => "Starting…".into(),
            Phase::Syncing => "Syncing…".into(),
            Phase::UpToDate if self.unsynced_changes() > 0 => match self.unsynced_changes() {
                1 => "1 file not synced".into(),
                n => format!("{n} files not synced"),
            },
            Phase::UpToDate => match self.last_complete {
                Some(t) => format!("Up to date ({})", short_time(t)),
                None => "Up to date".into(),
            },
            Phase::Error => "Error".into(),
            Phase::Stopped => "Stopped".into(),
            Phase::Off => "Background sync off".into(),
            Phase::Paused if prefs.paused() => {
                match Local.timestamp_opt(prefs.pause_until, 0).single() {
                    Some(t) => format!("Paused until {}", short_time(t)),
                    None => "Paused".into(),
                }
            }
            Phase::Paused => match device_hold() {
                Some(h) if prefs.schedule.allows(Local::now().time()) => {
                    format!("Paused: {}", h.words())
                }
                _ => format!("Paused by schedule until {}", prefs.schedule.from),
            },
            // a full resync, or the deletions the user approved after a big delete
            Phase::Resyncing => "Applying changes…".into(),
            Phase::SignedOut => "Signed out".into(),
            Phase::NeedsResync => "Needs a full resync".into(),
            Phase::BigDelete => "Stopped to protect your files".into(),
            Phase::Unknown => "Waiting for first sync".into(),
        }
    }
}

/// Works out the phase from a fresh look at systemd plus log-derived state.
pub fn compose_phase(
    acc: &AccountInfo,
    prefs: &AccountPrefs,
    unit: &UnitState,
    resyncing: bool,
    log_phase: Phase,
) -> Phase {
    if resyncing {
        return Phase::Resyncing;
    }
    if !acc.signed_in {
        return Phase::SignedOut;
    }
    if unit.starting {
        return Phase::Syncing;
    }
    let problem = matches!(
        log_phase,
        Phase::Error | Phase::NeedsResync | Phase::SignedOut | Phase::BigDelete
    );
    if unit.retrying {
        // it failed and systemd will try again; say what went wrong rather than "Starting…"
        return if problem { log_phase } else { Phase::Error };
    }
    if !unit.active {
        if unit.failed && matches!(unit.exit_status, Some(126) | Some(78)) {
            return Phase::NeedsResync;
        }
        if matches!(
            log_phase,
            Phase::NeedsResync | Phase::SignedOut | Phase::BigDelete
        ) {
            return log_phase;
        }
        if unit.enabled && !prefs.sync_allowed() {
            return Phase::Paused;
        }
        if unit.failed {
            return Phase::Error;
        }
        return if unit.enabled {
            Phase::Stopped
        } else {
            Phase::Off
        };
    }
    log_phase
}

/// Starts, restarts or stops a sync unit in the background. A unit that failed, or that systemd
/// stopped for restarting too often (5 starts in 10 s, easy with a few quick "Sync now" clicks),
/// refuses to start until its failure is cleared, so that is cleared first. Repeated restarts of
/// one unit within a few seconds, from the tray or the window, are dropped: the first one
/// already starts a fresh sync.
pub fn unit_action(verb: &'static str, unit: &str) {
    if verb != "stop" {
        // started on purpose: if it was held after a big delete, the user wants another try
        release_hold(unit);
    }
    if verb == "restart" && restarted_recently(unit) {
        return;
    }
    let unit = unit.to_string();
    std::thread::spawn(move || {
        if verb != "stop" {
            let _ = systemctl(&["reset-failed", &unit]);
        }
        let _ = systemctl(&[verb, &unit]);
    });
}

/// "Sync now": a fresh sync right away. Outside the schedule it's allowed for an hour, and a
/// pause is lifted. Returns the unit (made now if the account had none).
pub fn sync_now(info: &AccountInfo) -> Result<String, String> {
    let mut s = AppSettings::load();
    if !s.prefs(&info.confdir).sync_allowed() {
        let p = s.prefs_mut(&info.confdir);
        p.override_until = Local::now().timestamp() + 3600;
        p.pause_until = 0;
        s.save()
            .map_err(|e| format!("Couldn't save the sync override: {e}"))?;
    }
    match &info.unit {
        Some(u) => {
            unit_action("restart", u);
            Ok(u.clone())
        }
        None => {
            let u = ensure_unit(info).map_err(|e| e.to_string())?;
            unit_action("start", &u);
            Ok(u)
        }
    }
}

/// Pauses for `secs` seconds (-1 = until 06:00 tomorrow, 0 = resume). Returns when it ends
/// (0 for resumed).
/// "Until tomorrow morning": the next 06:00 at least two hours away, so a pause chosen after
/// midnight lasts until that morning rather than the one after.
pub fn next_morning(now: DateTime<Local>) -> i64 {
    [0u64, 1, 2]
        .iter()
        .filter_map(|d| {
            (now.date_naive() + chrono::Days::new(*d))
                .and_hms_opt(6, 0, 0)?
                .and_local_timezone(Local)
                .earliest()
        })
        .map(|t| t.timestamp())
        .find(|t| *t >= now.timestamp() + 2 * 3600)
        .unwrap_or(now.timestamp() + 12 * 3600)
}

pub fn pause(info: &AccountInfo, secs: i64) -> Result<i64, String> {
    let now = Local::now();
    let until = match secs {
        0 => 0,
        -1 => next_morning(now),
        s => now.timestamp() + s,
    };
    let mut s = AppSettings::load();
    let p = s.prefs_mut(&info.confdir);
    p.pause_until = until;
    p.override_until = 0;
    let allowed = p.sync_allowed();
    s.save()
        .map_err(|e| format!("Couldn't save the pause setting: {e}"))?;
    if let Some(u) = &info.unit {
        if until != 0 {
            unit_action("stop", u);
        } else if allowed && unit_state(u).enabled {
            // resuming doesn't start an account whose background sync is off, or whose
            // schedule says not now (the tray would only stop it again)
            unit_action("start", u);
        }
    }
    Ok(until)
}

fn restarted_recently(unit: &str) -> bool {
    restart_guard_at(&runtime_dir().join("restarted").join(slug(unit)))
        // the throttle only stops duplicates; if its marker can't be used, restart anyway
        .unwrap_or(false)
}

fn restart_guard_at(mark: &Path) -> std::io::Result<bool> {
    if let Some(dir) = mark.parent() {
        create_private_dir(dir)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(mark)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(true),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let metadata = file.metadata()?;
    let recent = metadata.len() > 0
        && metadata
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < std::time::Duration::from_secs(5));
    if !recent {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"restart")?;
        file.sync_all()?;
    }
    Ok(recent)
}

/// The desktop's permission for the next program the window starts to come to the front
/// (xdg-activation on Wayland, startup notification on X11). Without it GNOME opens a browser
/// behind the window. The window asks for a fresh one on every click and key press.
static ACTIVATION_TOKEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_activation_token(token: String) {
    *ACTIVATION_TOKEN.lock().unwrap() = Some(token);
}

pub fn current_activation_token() -> Option<String> {
    ACTIVATION_TOKEN.lock().unwrap().clone()
}

/// The token this process was started with to show its window in front (from the program that
/// started it, after a click there), if any.
pub fn startup_token() -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|t| !t.is_empty()))
}

/// Lets a program started from the window (a browser, a file manager) show up in front of it.
pub fn activate(cmd: &mut Command) -> &mut Command {
    if let Some(t) = ACTIVATION_TOKEN.lock().unwrap().as_deref() {
        cmd.env("XDG_ACTIVATION_TOKEN", t)
            .env("DESKTOP_STARTUP_ID", t);
    }
    cmd
}

pub fn run_detached(prog: &str, args: &[&str]) {
    if let Ok(mut c) = activate(&mut Command::new(prog))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = c.wait();
        });
    }
}

pub fn open_path(p: &Path) {
    run_detached("xdg-open", &[&p.to_string_lossy()]);
}

/// A `file://` URI for a path, percent-encoding everything but unreserved characters and `/`.
pub fn file_uri(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Decodes `%XX` escapes; anything that isn't a valid escape is kept as it is.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(&h), Some(&l)) = (b.get(i + 1), b.get(i + 2))
            && let (Some(h), Some(l)) = (hex(h), hex(l))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// The local path of a `file://` URI (the inverse of [`file_uri`]).
pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    rest.starts_with('/')
        .then(|| PathBuf::from(std::ffi::OsString::from_vec(percent_decode(rest))))
}

/// Shows a file in the user's file manager with it selected, falling back to opening its folder.
pub fn show_in_folder(p: &Path) {
    if p.exists() && crate::dbus::show_items(&file_uri(p)) {
        return;
    }
    if let Some(parent) = p.parent().and_then(|p| p.ancestors().find(|a| a.exists())) {
        open_path(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_starts_found() {
        let root = std::env::temp_dir().join(format!("lodg-os-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (user, system) = (root.join("user"), root.join("system"));
        let link = |dir: &Path, wants: &str, unit: &str| {
            fs::create_dir_all(dir.join(wants)).unwrap();
            std::os::unix::fs::symlink("/usr/lib/systemd/user/x", dir.join(wants).join(unit))
                .unwrap();
        };
        link(&user, "default.target.wants", "onedrive.service");
        link(&user, "default.target.wants", "pipewire.service");
        link(&user, "graphical-session.target.wants", "onedrive.service");
        link(&system, "multi-user.target.wants", "onedrive@josh.service");
        link(
            &system,
            "multi-user.target.wants",
            "onedrive@someone.service",
        );
        let found = outside_starts_in(&user, &system, "josh");
        assert_eq!(
            found,
            vec![
                OutsideStart {
                    unit: "onedrive.service".into(),
                    system: false
                },
                OutsideStart {
                    unit: "onedrive@josh.service".into(),
                    system: true
                },
            ]
        );
        assert_eq!(
            outside_starts_in(&root.join("none"), &root.join("none"), "josh"),
            vec![]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn schedule_windows() {
        let t = |s: &str| parse_hhmm(s).unwrap();
        let night = Schedule {
            enabled: true,
            from: "23:00".into(),
            to: "07:00".into(),
        };
        assert!(night.allows(t("23:30")));
        assert!(night.allows(t("03:00")));
        assert!(!night.allows(t("12:00")));
        let day = Schedule {
            enabled: true,
            from: "09:00".into(),
            to: "17:00".into(),
        };
        assert!(day.allows(t("09:00")) && !day.allows(t("17:00")));
        assert!(Schedule::default().allows(t("12:00")));
    }

    #[test]
    fn schedule_times() {
        assert_eq!(parse_hhmm("7:00"), parse_hhmm("07:00"));
        assert!(parse_hhmm(" 23:30 ").is_some());
        for bad in ["24:00", "07:60", "7", "", "ab:cd", "-1:00"] {
            assert!(parse_hhmm(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn sync_folder_safety() {
        let home = home();
        let bad = |p: &Path| unsuitable_sync_folder(p).is_some();
        assert!(bad(&home));
        assert!(bad(Path::new("/")));
        assert!(bad(home.parent().unwrap()));
        assert!(bad(Path::new("/usr/share/x")));
        assert!(bad(Path::new("/tmp")));
        assert!(bad(&config_home().join("onedrive")));
        assert!(bad(Path::new("relative/path")));
        assert!(bad(&home.join("My \"Quoted\" Drive")));
        assert!(!bad(&home.join("OneDrive")));
        assert!(!bad(&home.join("Ünïcode & spaces #1")));
        assert!(!bad(Path::new("/run/media/me/USB/OneDrive")));
        assert!(!bad(Path::new("/mnt/data/OneDrive")));
    }

    #[test]
    fn desktop_exec_quoting() {
        assert_eq!(
            desktop_exec_arg("/home/a/x.AppImage"),
            r#""/home/a/x.AppImage""#
        );
        assert_eq!(
            desktop_exec_arg("/home/a b/100% \"$x\".AppImage"),
            r#""/home/a b/100%% \\"\\$x\\".AppImage""#
        );
        assert_eq!(desktop_exec_arg(r"/a\b"), r#""/a\\\\b""#);
    }

    #[test]
    fn unit_text_is_safe() {
        assert_eq!(unit_text("100% Work"), "100%% Work");
        assert_eq!(
            description_name(&format!(
                "Description=OneDrive sync ({})\n",
                unit_text("100% Work")
            )),
            Some("100% Work".into())
        );
        assert_eq!(unit_text("two\nlines"), "two lines");
    }

    #[test]
    fn sync_everything_keeps_own_rules() {
        let dir = std::env::temp_dir().join(format!("lodg-sl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("sync_list"), "# mine\n!/Junk/*\n/Documents\n").unwrap();
        write_sync_list(&dir, &[]).unwrap();
        assert!(!dir.join("sync_list").exists());
        assert!(
            fs::read_to_string(dir.join("sync_list.saved"))
                .unwrap()
                .contains("!/Junk/*")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deleting_a_config_folder_never_takes_other_files() {
        let conf = std::env::temp_dir().join(format!("lodg-dc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&conf);
        let fill = || {
            fs::create_dir_all(&conf).unwrap();
            for f in [
                "config",
                "refresh_token",
                "items.sqlite3-wal",
                "session_upload.abc",
            ] {
                fs::write(conf.join(f), "x").unwrap();
            }
        };
        // the client's own folder goes completely
        fill();
        assert!(delete_confdir(&conf).unwrap());
        assert!(!conf.exists());
        // anything else in it stays, and so does the folder
        fill();
        fs::create_dir_all(conf.join("OneDrive")).unwrap();
        fs::write(conf.join("OneDrive/report.docx"), "mine").unwrap();
        fs::write(conf.join("notes.txt"), "mine").unwrap();
        fs::write(conf.join("config.bak-2026"), "mine").unwrap();
        assert!(!delete_confdir(&conf).unwrap());
        let mut left: Vec<_> = fs::read_dir(&conf)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["OneDrive", "config.bak-2026", "notes.txt"]);
        assert!(conf.join("OneDrive/report.docx").exists());
        fs::remove_dir_all(&conf).unwrap();
    }

    #[test]
    fn runtime_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = runtime_dir();
        assert!(!d.starts_with("/tmp"));
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn busy_marker_lock_has_one_owner_and_releases_cleanly() {
        let dir = std::env::temp_dir().join(format!("lodg-busy-{}", std::process::id()));
        let path = dir.join("marker");
        let _ = fs::remove_dir_all(&dir);
        let first = BusyMarker::try_new_at(path.clone()).unwrap();
        assert!(BusyMarker::try_new_at(path.clone()).is_err());
        drop(first);
        assert!(!path.exists());
        let second = BusyMarker::try_new_at(path.clone()).unwrap();
        drop(second);
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_throttle_is_written_and_checked_under_one_lock() {
        let dir = std::env::temp_dir().join(format!("lodg-restart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mark = dir.join("unit");
        assert!(!restart_guard_at(&mark).unwrap());
        assert!(restart_guard_at(&mark).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_that_are_not_utf8_are_never_rewritten() {
        let dir = std::env::temp_dir().join(format!("lodg-nt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // "Bücher" in Latin-1, as an older editor might have saved it
        let config = b"sync_dir = \"~/B\xfccher\"\nmonitor_interval = \"300\"\n".to_vec();
        fs::write(dir.join("config"), &config).unwrap();
        let mut c = ClientConfig::load(&dir);
        assert_eq!(c.get("monitor_interval").as_deref(), Some("300"));
        c.set("rate_limit", Some("1000"));
        assert!(c.save().is_err());
        assert_eq!(fs::read(dir.join("config")).unwrap(), config);
        let list = b"/B\xfccher\n".to_vec();
        fs::write(dir.join("sync_list"), &list).unwrap();
        assert_eq!(read_sync_list(&dir).unwrap().len(), 1);
        assert!(write_sync_list(&dir, &["/Documents".into()]).is_err());
        assert!(write_sync_list(&dir, &[]).is_err());
        assert_eq!(fs::read(dir.join("sync_list")).unwrap(), list);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_client_config_is_not_replaced() {
        let dir = std::env::temp_dir().join(format!("lodg-unreadable-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("config")).unwrap();
        let mut config = ClientConfig::load(&dir);
        config.set("sync_dir", Some("~/OneDrive"));
        assert!(config.save().is_err());
        assert!(dir.join("config").is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_with_load_errors_cannot_be_saved() {
        let settings = AppSettings {
            load_error: Some("simulated settings read failure".into()),
            ..AppSettings::default()
        };
        assert!(settings.save().is_err());
    }

    #[test]
    fn client_versions() {
        let v = |t: &str| parse_client_version(t).map(|(n, _)| n);
        assert_eq!(v("onedrive v2.5.11"), Some((2, 5, 11)));
        assert_eq!(v("onedrive v2.5.11-12-g1a2b3c4"), Some((2, 5, 11)));
        assert_eq!(v("onedrive v2.4.25+np1"), Some((2, 4, 25)));
        assert_eq!(v("onedrive 2.6"), Some((2, 6, 0)));
        assert_eq!(v("onedrive"), None);
        assert_eq!(v(""), None);
        assert!(v("onedrive v2.4.25").unwrap() < MIN_CLIENT_VERSION);
        assert!(v("onedrive v2.5.0").unwrap() >= MIN_CLIENT_VERSION);
        assert!(v("onedrive v3.0.0").unwrap() >= MIN_CLIENT_VERSION);
    }

    #[test]
    fn exec_parsing() {
        let u = "ExecStart=\"/home/a b/x.AppImage\" --client --monitor --confdir \"/home/x/.config/onedrive-work\"\n";
        assert_eq!(
            confdir_from_exec(u),
            Some(PathBuf::from("/home/x/.config/onedrive-work"))
        );
        let u = "ExecStart=/usr/bin/onedrive --monitor\n";
        assert_eq!(confdir_from_exec(u), Some(config_home().join("onedrive")));
    }

    #[test]
    fn sync_list_rules() {
        assert_eq!(sync_list_folder("/Documents/"), Some("/Documents".into()));
        assert_eq!(sync_list_folder("Pictures/*"), Some("/Pictures".into()));
        assert_eq!(sync_list_folder("!/Junk"), None);
    }

    #[test]
    fn journal_order() {
        // as `journalctl --reverse` prints them: newest first, the last two written together
        let lines: Vec<String> = [
            "20.5 h onedrive[1]: Sync with Microsoft OneDrive is complete",
            "20.5 h onedrive[1]: Starting a sync with Microsoft OneDrive",
            "10.0 h onedrive[1]: a",
        ]
        .map(String::from)
        .into();
        let ordered = oldest_first(lines);
        assert_eq!(ordered[0], "10.0 h onedrive[1]: a");
        let last = split_journal_line(ordered.last().unwrap()).unwrap().1;
        assert!(matches!(parse_log(last), Some(LogEvent::Complete)));
    }

    #[test]
    fn uris() {
        assert_eq!(
            file_uri(Path::new("/home/a b/Kate's #1.txt")),
            "file:///home/a%20b/Kate%27s%20%231.txt"
        );
        for p in ["/home/a b/Kate's #1.txt", "/tmp/café/100%", "/"] {
            assert_eq!(path_from_uri(&file_uri(Path::new(p))), Some(p.into()));
        }
        assert_eq!(
            path_from_uri("file://localhost/srv/x"),
            Some("/srv/x".into())
        );
        assert_eq!(path_from_uri("https://example.com/x"), None);
        assert_eq!(path_from_uri("file://server/share"), None);
    }

    #[test]
    fn blocked_files_are_tracked() {
        let locked = "Unable to upload this modified file as this is currently checked out or locked for editing by another user: LSAs FILES/Agendas/28.LSA agenda 20.10.26.docx";
        let ev = parse_log(locked).unwrap();
        assert!(
            matches!(&ev, LogEvent::Blocked { path, reason: BlockReason::Locked } if path == "LSAs FILES/Agendas/28.LSA agenda 20.10.26.docx")
        );
        assert!(matches!(
            parse_log("Unable to upload this modified file as this was shared as read-only: ./a/b.txt"),
            Some(LogEvent::Blocked { path, reason: BlockReason::ReadOnly }) if path == "a/b.txt"
        ));
        assert!(matches!(
            parse_log(
                "Unable to download this file as this was shared as read-only without download permission: x.pdf"
            ),
            Some(LogEvent::Blocked {
                reason: BlockReason::NoDownload,
                ..
            })
        ));
        assert!(matches!(
            parse_log("Unable to upload this modified file at this point in time: x.pdf"),
            Some(LogEvent::Blocked {
                reason: BlockReason::NotNow,
                ..
            })
        ));
        // other "Unable to" lines are still account errors
        assert!(matches!(
            parse_log(
                "Unable to upload modified file as the creation of the upload session URL failed"
            ),
            Some(LogEvent::Error(_))
        ));
        assert!(matches!(
            parse_log("Reading configuration file: /home/j/.config/onedrive/config"),
            Some(LogEvent::ClientStart)
        ));
        // the failed upload line that follows the block is not a file event
        assert!(
            parse_log("Uploading modified file: LSAs FILES/Agendas/28.LSA agenda 20.10.26.docx ... failed!").is_none()
        );

        let path = "LSAs FILES/Agendas/28.LSA agenda 20.10.26.docx";
        let mut list = Vec::new();
        track_blocked(&mut list, &ev);
        // a finished sync doesn't mean the file went up
        track_blocked(&mut list, &LogEvent::Complete);
        track_blocked(&mut list, &LogEvent::Syncing);
        assert_eq!(list.len(), 1);
        // blocked again: still one entry
        track_blocked(&mut list, &ev);
        assert_eq!(list.len(), 1);
        // another file's upload leaves it alone
        track_blocked(
            &mut list,
            &parse_log("Uploading new file: other.docx ... done").unwrap(),
        );
        assert_eq!(list.len(), 1);
        // its own upload clears it
        track_blocked(
            &mut list,
            &parse_log(&format!("Uploading modified file: {path} ... done")).unwrap(),
        );
        assert!(list.is_empty());
        // a new client run forgets old blocks (it reports them again if they still apply)
        track_blocked(&mut list, &ev);
        track_blocked(&mut list, &LogEvent::ClientStart);
        assert!(list.is_empty());
    }

    #[test]
    fn blocked_words() {
        assert_eq!(blocked_text(&[]), "");
        let one = |p: &str, r| Blocked {
            path: p.into(),
            reason: r,
        };
        let t = blocked_text(&[one("a/b/Agenda.docx", BlockReason::Locked)]);
        assert!(t.starts_with("\u{201c}Agenda.docx\u{201d} didn't upload: it's open for editing"));
        assert!(t.contains("Sync now"));
        let many: Vec<_> = (0..5)
            .map(|i| one(&format!("f{i}.txt"), BlockReason::ReadOnly))
            .collect();
        let t = blocked_text(&many);
        assert_eq!(t.lines().count(), 4);
        assert!(t.ends_with("…and 2 more files."));

        let mut st = Status {
            phase: Phase::UpToDate,
            last_complete: None,
            last_error: None,
            big_delete: None,
            blocked: vec![one("a.txt", BlockReason::Locked)],
            unit: UnitState::default(),
        };
        let prefs = AccountPrefs::default();
        assert_eq!(st.text(&prefs), "1 file not synced");
        st.blocked.push(one("b.txt", BlockReason::Locked));
        assert_eq!(st.text(&prefs), "2 files not synced");
        st.blocked.clear();
        assert_eq!(st.text(&prefs), "Up to date");
    }

    #[test]
    fn blocked_names_are_safe() {
        assert_eq!(shown_name("a/b/c.docx"), "c.docx");
        // a name with a newline stays on one line
        assert_eq!(shown_name("dir/evil\nname.txt"), "evil name.txt");
        let long = format!("x/{}", "y".repeat(200));
        let shown = shown_name(&long);
        assert_eq!(shown.chars().count(), 58);
        assert!(shown.ends_with('…'));
        let t = blocked_text(&[Blocked {
            path: "two\nlines.txt".into(),
            reason: BlockReason::Locked,
        }]);
        assert_eq!(t.lines().count(), 1);

        // a view-only file is mentioned but doesn't flag the account
        let mut st = Status {
            phase: Phase::UpToDate,
            last_complete: None,
            last_error: None,
            big_delete: None,
            blocked: vec![Blocked {
                path: "v.pdf".into(),
                reason: BlockReason::NoDownload,
            }],
            unit: UnitState::default(),
        };
        assert_eq!(st.unsynced_changes(), 0);
        assert_eq!(st.text(&AccountPrefs::default()), "Up to date");
        st.blocked.push(Blocked {
            path: "w.docx".into(),
            reason: BlockReason::ReadOnly,
        });
        assert_eq!(st.text(&AccountPrefs::default()), "1 file not synced");
    }

    #[test]
    fn quota_levels() {
        assert_eq!(quota_warning(50, 100, 0), None);
        assert_eq!(quota_warning(91, 100, 0), Some(90));
        assert_eq!(quota_warning(91, 100, 90), None);
        assert_eq!(quota_warning(87, 100, 90), None);
        assert_eq!(quota_warning(100, 100, 90), Some(100));
        assert_eq!(quota_warning(80, 100, 100), Some(0));
        assert_eq!(quota_warning(5, 0, 0), None);
    }

    #[test]
    fn big_delete() {
        // the client's lines, as in sync.d
        for l in [
            "ERROR: An attempt to remove a large volume of data from OneDrive has been detected. Exiting client to preserve your data on Microsoft OneDrive",
            "ERROR: The total number of items being deleted is: 1532",
            "ERROR: To delete a large volume of data use --force or increase the config value 'classify_as_big_delete' to a larger value",
        ] {
            assert!(matches!(parse_log(l), Some(LogEvent::BigDelete)), "{l}");
        }
        // newest first, as journalctl --reverse gives them; an older ordinary delete is ignored
        let lines: Vec<String> = [
            "1700000010.0 h onedrive[1]: ERROR: The total number of items being deleted is: 1532",
            "1700000009.0 h onedrive[1]: Deleting item from Microsoft OneDrive: ./Documents/Old projects",
            "1700000001.0 h onedrive[1]: Deleting item from Microsoft OneDrive: other.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let b = big_delete_from(&lines);
        assert_eq!(b.count, Some(1532));
        assert_eq!(b.path.as_deref(), Some("Documents/Old projects"));
        let t = big_delete_text(&b);
        assert!(t.starts_with(
            "\u{201c}Documents/Old projects\u{201d} and what was in it (1532 items in all) were deleted"
        ));
        let t = big_delete_text(&BigDelete {
            path: None,
            count: Some(1532),
        });
        assert!(t.starts_with("1532 items were deleted"));
        // nothing found: still says something sensible
        let t = big_delete_text(&BigDelete::default());
        assert!(t.starts_with("Many items were deleted"));
        assert_eq!(big_delete_from(&[]), BigDelete::default());
        // long or hostile paths stay on one short line
        let long = format!("{}/end", "d".repeat(200));
        let p = shown_path(&long);
        assert_eq!(p.chars().count(), 78);
        assert!(p.starts_with('…') && p.ends_with("/end"));
        assert_eq!(shown_path("a\nb"), "a b");

        // the phase: shown while stopped or waiting to restart, not while a new run starts
        let acc = AccountInfo {
            name: "x".into(),
            confdir: "/c".into(),
            unit: Some("u".into()),
            managed: true,
            sync_dir: "/s".into(),
            drive_id: None,
            kind: "personal".into(),
            signed_in: true,
        };
        let prefs = AccountPrefs::default();
        let stopped = UnitState {
            exists: true,
            enabled: true,
            ..Default::default()
        };
        assert_eq!(
            compose_phase(&acc, &prefs, &stopped, false, Phase::BigDelete),
            Phase::BigDelete
        );
        let retrying = UnitState {
            retrying: true,
            active: true,
            ..stopped.clone()
        };
        assert_eq!(
            compose_phase(&acc, &prefs, &retrying, false, Phase::BigDelete),
            Phase::BigDelete
        );
        let starting = UnitState {
            starting: true,
            active: true,
            ..stopped.clone()
        };
        assert_eq!(
            compose_phase(&acc, &prefs, &starting, false, Phase::BigDelete),
            Phase::Syncing
        );
        // the approved deletions running
        assert_eq!(
            compose_phase(&acc, &prefs, &stopped, true, Phase::BigDelete),
            Phase::Resyncing
        );
    }

    #[test]
    fn transfers_and_recent_files() {
        // as the client writes them (two spaces after a short percentage, one after 100%)
        let p = |l: &str| parse_log(l);
        assert_eq!(
            p("Uploading: Music/a ... b.mp3 ... 62%  |  ETA    00:01:20"),
            Some(LogEvent::Progress {
                up: true,
                path: "Music/a ... b.mp3".into(),
                percent: 62,
                eta: Some("00:01:20".into()),
                done: false
            })
        );
        assert!(matches!(
            p("Uploading: x.mp3 ... 0%   |  ETA    --:--:--"),
            Some(LogEvent::Progress {
                percent: 0,
                eta: None,
                done: false,
                ..
            })
        ));
        assert!(matches!(
            p("Downloading: ./d/x.iso ... 100% | DONE in 00:00:03"),
            Some(LogEvent::Progress { up: false, percent: 100, done: true, ref path, .. }) if path == "d/x.iso"
        ));
        // not progress: still the usual file lines
        assert!(matches!(
            p("Uploading modified file: x.mp3 ... done"),
            Some(LogEvent::File { .. })
        ));
        assert!(p("Uploading: x.mp3 ... 250%  |  ETA 00:00:01").is_none());
        assert!(p("Uploading: x.mp3 ... soon").is_none());

        let t0 = 1_700_000_000i64;
        let line = |dt: i64, m: &str| format!("{}.0 h onedrive[1]: {m}", t0 + dt);
        let now = Local.timestamp_opt(t0 + 30, 0).unwrap();
        let lines = vec![
            line(0, "Downloading file: a.txt ... done"),
            line(1, "Uploading: big.mp4 ... 0%   |  ETA    --:--:--"),
            line(2, "Uploading: big.mp4 ... 40%  |  ETA    00:00:09"),
            line(3, "Downloading: done.iso ... 95%  |  ETA    00:00:01"),
            line(4, "Downloading: done.iso ... 100% | DONE in 00:00:20"),
            line(5, "Downloading file: done.iso ... done"),
            line(6, "Uploading new file: ./b.docx ... done"),
            line(7, "Downloading file: a.txt ... done"),
        ];
        let (recent, transfers) = files_from(&lines, now, 10);
        assert_eq!(
            recent.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["a.txt", "b.docx", "done.iso"]
        );
        assert_eq!(transfers.len(), 1);
        assert_eq!(
            (transfers[0].path.as_str(), transfers[0].percent),
            ("big.mp4", 40)
        );
        // the client restarting cuts transfers off; so does a long silence
        let mut cut = lines.clone();
        cut.push(line(8, "Reading configuration file: /c/config"));
        assert!(files_from(&cut, now, 10).1.is_empty());
        let later = Local
            .timestamp_opt(t0 + 2 + TRANSFER_STALE_SECS, 0)
            .unwrap();
        assert!(files_from(&lines, later, 10).1.is_empty());
        // at most `max`, newest kept
        assert_eq!(files_from(&lines, now, 1).0[0].path, "a.txt");

        assert_eq!(file_kind("a/Report.DOCX", false), 2);
        assert_eq!(file_kind("x.tar.gz", false), 9);
        assert_eq!(file_kind("Makefile", false), 0);
        assert_eq!(file_kind(".bashrc", false), 0);
        assert_eq!(file_kind("Photos", true), 1);
    }

    #[test]
    fn pause_until_morning() {
        let at = |d: u32, h: u32, m: u32| {
            Local
                .with_ymd_and_hms(2026, 10, d, h, m, 0)
                .single()
                .unwrap()
        };
        let hours = |from: DateTime<Local>| (next_morning(from) - from.timestamp()) / 3600;
        // after midnight: this morning
        assert_eq!(next_morning(at(21, 1, 0)), at(21, 6, 0).timestamp());
        // in the evening: tomorrow morning
        assert_eq!(next_morning(at(20, 22, 0)), at(21, 6, 0).timestamp());
        // just before six: not a few minutes, the next morning
        assert_eq!(next_morning(at(21, 5, 30)), at(22, 6, 0).timestamp());
        assert!((2..=26).contains(&hours(at(21, 12, 0))));
    }

    #[test]
    fn repeated_keys_are_joined() {
        let dir = std::env::temp_dir().join(format!("lodg-cfg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config"),
            "skip_dir = \"Temp\"\n# skip_dir = \"commented\"\nskip_dir = \"Projects/Old\"\nsync_dir = \"~/x\"\n",
        )
        .unwrap();
        let mut c = ClientConfig::load(&dir);
        assert_eq!(
            c.get_joined("skip_dir").as_deref(),
            Some("Temp|Projects/Old")
        );
        assert_eq!(c.get_joined("skip_file"), None);
        // writing it back makes one line in place of the first
        c.set("skip_dir", Some("Temp|Projects/Old|Cache"));
        assert_eq!(
            c.get_joined("skip_dir").as_deref(),
            Some("Temp|Projects/Old|Cache")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grep_escape() {
        assert_eq!(regex_escape("a (1).txt"), "a \\(1\\)\\.txt");
        assert_eq!(regex_escape("x|y$[z]"), "x\\|y\\$\\[z\\]");
        assert_eq!(regex_escape("Bahá'í"), "Bahá'í");
    }

    #[test]
    fn log_lines() {
        assert!(matches!(
            parse_log("Uploading new file: a/b.txt ... done"),
            Some(LogEvent::File { kind: "↑", .. })
        ));
        assert!(parse_log("Uploading new file: a/b.txt ... failed").is_none());
        assert!(
            matches!(parse_log("Created local directory: ./Pictures/2026"), Some(LogEvent::File { kind: "+", path }) if path == "Pictures/2026")
        );
        assert!(
            matches!(parse_log("Downloading file: ./x/y.pdf ... done"), Some(LogEvent::File { path, .. }) if path == "x/y.pdf")
        );
        assert!(matches!(
            parse_log("Sync with Microsoft OneDrive is complete"),
            Some(LogEvent::Complete)
        ));
        // what the client prints when Microsoft no longer accepts the saved sign-in
        for revoked in [
            "ERROR: Check your configuration as your refresh_token may be empty or invalid. You may need to issue a --reauth and re-authorise this client.",
            "ERROR: You will need to issue a --reauth and re-authorise this client to obtain a fresh auth token.",
        ] {
            assert!(matches!(parse_log(revoked), Some(LogEvent::NeedsLogin)));
        }
        assert!(matches!(
            parse_log(
                "An application configuration change has been detected where a --resync is required"
            ),
            Some(LogEvent::NeedsResync)
        ));
    }
}
