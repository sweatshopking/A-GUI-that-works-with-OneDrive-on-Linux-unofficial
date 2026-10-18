//! Shared plumbing: finding the onedrive client, accounts, config files, systemd units,
//! app settings and log parsing. Used by both the tray and the settings window.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "linux-onedrive-gui";
pub const APP_NAME: &str = "A GUI that works with OneDrive on Linux (unofficial)";
pub const MANAGED_MARK: &str = "# Managed by linux-onedrive-gui";
/// Session-bus name the tray owns while it runs, so only one tray ever starts.
pub const TRAY_BUS_NAME: &str = "io.github.sweatshopking.LinuxOnedriveGui.Tray";

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

pub fn app_dir() -> PathBuf {
    config_home().join(APP_ID)
}

pub fn user_unit_dir() -> PathBuf {
    config_home().join("systemd/user")
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
    if let Some(p) = std::env::var_os("LINUX_ONEDRIVE_GUI_CLIENT") {
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
        .find(|p| p.is_file())
    })
}

pub fn which(prog: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(prog))
        .find(|p| p.is_file())
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
}

impl AccountPrefs {
    pub fn paused(&self) -> bool {
        self.pause_until > Local::now().timestamp()
    }
    /// Whether the background service should be running right now.
    pub fn sync_allowed(&self) -> bool {
        let now = Local::now();
        !self.paused()
            && (self.schedule.allows(now.time()) || self.override_until > now.timestamp())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct AppSettings {
    /// Keyed by config directory path.
    pub accounts: BTreeMap<String, AccountPrefs>,
    /// Config directories the user removed but whose (unmanaged) service still exists.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
}

impl AppSettings {
    pub fn path() -> PathBuf {
        app_dir().join("settings.json")
    }
    pub fn load() -> Self {
        let Ok(text) = fs::read_to_string(Self::path()) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            // keep a damaged file rather than overwrite it with defaults on the next save
            let aside = Self::path().with_extension("json.damaged");
            if !aside.exists() {
                let _ = fs::copy(Self::path(), &aside);
                eprintln!(
                    "{} couldn't be read ({e}); a copy was kept as {}",
                    Self::path().display(),
                    aside.display()
                );
            }
            Self::default()
        })
    }
    pub fn save(&self) -> std::io::Result<()> {
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

pub fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(name);
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp); // e.g. the disk is full: don't leave a half-written copy
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
}

/// An error for a file the app can't safely rewrite because it isn't UTF-8 text.
fn not_text_error(p: &Path) -> std::io::Error {
    std::io::Error::other(format!(
        "{} isn't plain UTF-8 text, so it wasn't changed. Edit it by hand, or save it as UTF-8 and try again.",
        p.display()
    ))
}

/// Whether a file exists but can't be read as UTF-8 text.
fn is_non_text(p: &Path) -> bool {
    matches!(fs::read_to_string(p), Err(e) if e.kind() == std::io::ErrorKind::InvalidData)
}

impl ClientConfig {
    pub fn load(confdir: &Path) -> Self {
        let path = confdir.join("config");
        let (lines, writable) = match fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => (s.lines().map(String::from).collect(), true),
                Err(e) => (
                    String::from_utf8_lossy(e.as_bytes())
                        .lines()
                        .map(String::from)
                        .collect(),
                    false,
                ),
            },
            Err(_) => (Vec::new(), true),
        };
        ClientConfig {
            path,
            lines,
            writable,
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
            return Err(not_text_error(&self.path));
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
pub fn read_sync_list(confdir: &Path) -> Vec<String> {
    let bytes = fs::read(confdir.join("sync_list")).unwrap_or_default();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

pub fn write_sync_list(confdir: &Path, entries: &[String]) -> std::io::Result<()> {
    let p = confdir.join("sync_list");
    if is_non_text(&p) {
        return Err(not_text_error(&p));
    }
    if entries.is_empty() {
        if p.exists() {
            // keep any rules the user wrote by hand, which syncing everything can't honour
            let own_rules = fs::read_to_string(&p)
                .unwrap_or_default()
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
    let body = format!(
        "[Unit]\n\
         Description=OneDrive sync ({})\n\
         Documentation=https://github.com/sweatshopking/A-GUI-that-works-with-OneDrive-on-Linux-unofficial\n\
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
    atomic_write(&user_unit_dir().join(unit), body.as_bytes())?;
    systemctl(&["daemon-reload"]);
    Ok(())
}

/// Marks an account as in use by the settings window (a preview's dry run, a sign-in), which runs
/// the client itself with the account's unit stopped. While the marker exists and its window is
/// alive, nothing else may start that unit: two clients on one account can damage its database.
pub struct BusyMarker(PathBuf);

fn busy_path(confdir: &Path) -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join(APP_ID)
        .join("busy")
        .join(slug(&confdir.to_string_lossy()))
}

impl BusyMarker {
    pub fn new(confdir: &Path) -> Self {
        let p = busy_path(confdir);
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(&p, std::process::id().to_string());
        BusyMarker(p)
    }
}

impl Drop for BusyMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Whether the settings window is using this account right now.
pub fn account_busy(confdir: &Path) -> bool {
    fs::read_to_string(busy_path(confdir))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
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
        if st.enabled && !st.active && acc.signed_in && settings.prefs(&acc.confdir).sync_allowed()
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
    let home = home();
    if folder == home || home.starts_with(folder) {
        let what = if folder == home {
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
        if folder.starts_with(&own) || own.starts_with(folder) {
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
    let under = |top: &str| folder == Path::new(top) || folder.starts_with(format!("{top}/"));
    if !user_mounts.iter().any(|m| text.starts_with(m)) && system.iter().any(|t| under(t)) {
        return Some(format!(
            "{shown} is a system folder. Pick a folder in your home folder or on a drive of your own."
        ));
    }
    if folder.is_file() {
        return Some(format!("{shown} is a file, not a folder."));
    }
    None
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
pub fn repair_managed_units() {
    let Some(client) = client_binary() else {
        return; // nothing to point them at; they skip quietly until the client is installed
    };
    let Ok(rd) = fs::read_dir(user_unit_dir()) else {
        return;
    };
    let mut changed = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        if !text.contains(MANAGED_MARK) {
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
            if l == MANAGED_MARK {
                new.push(condition_line(&client));
            }
            new.push(if l.starts_with("ExecStart=") {
                exec.clone()
            } else {
                l.to_string()
            });
        }
        let new = new.join("\n") + "\n";
        if new != text && atomic_write(&p, new.as_bytes()).is_ok() {
            changed.push(e.file_name().to_string_lossy().to_string());
        }
    }
    if changed.is_empty() {
        return;
    }
    systemctl(&["daemon-reload"]);
    for u in changed {
        let st = unit_state(&u);
        if st.enabled {
            // pick up the new command (a running unit keeps the old one until restarted)
            systemctl(&["try-restart", &u]);
        }
    }
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
        let managed = text.contains(MANAGED_MARK);
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
    let unit = if base == "onedrive" {
        "onedrive.service".into()
    } else {
        format!(
            "onedrive-{}.service",
            slug(base.trim_start_matches("onedrive"))
        )
    };
    write_managed_unit(&unit, &acc.name, &acc.confdir)?;
    Ok(unit)
}

// ---------------------------------------------------------------- log parsing

#[derive(Clone, Debug)]
pub enum LogEvent {
    Syncing,
    Complete,
    Error(String),
    File { kind: &'static str, path: String },
    NeedsResync,
    NeedsLogin,
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
        .find(|p| {
            p.join("hicolor/scalable/status/linux-onedrive-gui-ok.svg")
                .exists()
        })
        .map(|p| p.canonicalize().unwrap_or(p))
}

/// Only the log lines that matter for status, however busy the log is, oldest first.
/// Falls back to a plain tail on systems whose journalctl lacks --grep.
pub fn journal_status_lines(unit: &str) -> Vec<String> {
    let pattern = "Starting a sync|Sync with Microsoft OneDrive is complete|ERROR|Unable to |--resync|--reauth|requires authentication|re-authenticate|re-authorise|invalid_grant";
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
}

impl Phase {
    /// Our own status icons (assets/icons), so the tray looks the same on every desktop.
    pub fn icon(self) -> &'static str {
        match self {
            Phase::Syncing | Phase::Unknown | Phase::Resyncing => "linux-onedrive-gui-sync",
            Phase::UpToDate => "linux-onedrive-gui-ok",
            Phase::Error | Phase::NeedsResync | Phase::SignedOut => "linux-onedrive-gui-error",
            Phase::Stopped | Phase::Off | Phase::Paused => "linux-onedrive-gui-off",
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
    pub unit: UnitState,
}

impl Status {
    pub fn text(&self, prefs: &AccountPrefs) -> String {
        match self.phase {
            Phase::Syncing if self.unit.starting => "Starting…".into(),
            Phase::Syncing => "Syncing…".into(),
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
            Phase::Paused => format!("Paused by schedule until {}", prefs.schedule.from),
            Phase::Resyncing => "Full resync running…".into(),
            Phase::SignedOut => "Signed out".into(),
            Phase::NeedsResync => "Needs a full resync".into(),
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
        Phase::Error | Phase::NeedsResync | Phase::SignedOut
    );
    if unit.retrying {
        // it failed and systemd will try again; say what went wrong rather than "Starting…"
        return if problem { log_phase } else { Phase::Error };
    }
    if !unit.active {
        if unit.failed && matches!(unit.exit_status, Some(126) | Some(78)) {
            return Phase::NeedsResync;
        }
        if matches!(log_phase, Phase::NeedsResync | Phase::SignedOut) {
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

fn restarted_recently(unit: &str) -> bool {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let mark = base.join(APP_ID).join("restarted").join(slug(unit));
    let recent = fs::metadata(&mark)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(5));
    if !recent {
        if let Some(dir) = mark.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(&mark, b"");
    }
    recent
}

/// The desktop's permission for the next program the window starts to come to the front
/// (xdg-activation on Wayland, startup notification on X11). Without it GNOME opens a browser
/// behind the window. The window asks for a fresh one on every click and key press.
static ACTIVATION_TOKEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_activation_token(token: String) {
    *ACTIVATION_TOKEN.lock().unwrap() = Some(token);
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
        assert_eq!(read_sync_list(&dir).len(), 1);
        assert!(write_sync_list(&dir, &["/Documents".into()]).is_err());
        assert!(write_sync_list(&dir, &[]).is_err());
        assert_eq!(fs::read(dir.join("sync_list")).unwrap(), list);
        fs::remove_dir_all(&dir).unwrap();
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
