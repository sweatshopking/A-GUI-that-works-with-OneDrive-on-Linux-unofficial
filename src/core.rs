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
pub const APP_NAME: &str = "Linux OneDrive GUI";
pub const MANAGED_MARK: &str = "# Managed by linux-onedrive-gui";

// ---------------------------------------------------------------- paths

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
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

/// The onedrive client binary we drive: a bundled copy next to us if there is one,
/// otherwise whatever `onedrive` is on PATH.
pub fn client_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LINUX_ONEDRIVE_GUI_CLIENT") {
        return Some(PathBuf::from(p));
    }
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        cands.push(dir.join("../lib").join(APP_ID).join("onedrive"));
        cands.push(dir.join("onedrive"));
    }
    if let Ok(appdir) = std::env::var("APPDIR") {
        cands.push(
            PathBuf::from(&appdir)
                .join("usr/lib")
                .join(APP_ID)
                .join("onedrive"),
        );
        cands.push(PathBuf::from(&appdir).join("usr/bin/onedrive"));
    }
    if let Some(p) = cands.into_iter().find(|p| p.is_file()) {
        return Some(p.canonicalize().unwrap_or(p));
    }
    which("onedrive")
}

pub fn which(prog: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(prog))
        .find(|p| p.is_file())
}

/// Builds a Command that runs the onedrive client with our launcher, so units and one-off
/// jobs work the same whether we were installed as a package or run as an AppImage.
pub fn client_command() -> Command {
    let mut c = Command::new(self_command());
    c.arg("--client");
    c
}

pub fn client_version() -> Option<String> {
    let bin = client_binary()?;
    let out = Command::new(bin).arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace()
        .last()
        .map(|v| v.trim_start_matches('v').to_string())
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
        fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
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
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)
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
}

impl ClientConfig {
    pub fn load(confdir: &Path) -> Self {
        let path = confdir.join("config");
        let lines = fs::read_to_string(&path)
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default();
        ClientConfig { path, lines }
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
    fs::read_to_string(confdir.join("sync_list"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

pub fn write_sync_list(confdir: &Path, entries: &[String]) -> std::io::Result<()> {
    let p = confdir.join("sync_list");
    if entries.is_empty() {
        if p.exists() {
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
    UnitState {
        exists: p.get("LoadState").map(String::as_str) == Some("loaded"),
        active: matches!(active, "active" | "activating" | "reloading"),
        starting: active == "activating"
            || p.get("SubState").map(String::as_str) == Some("start-pre"),
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

fn systemd_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

/// Writes (or rewrites) the user unit we manage for an account.
pub fn write_managed_unit(unit: &str, name: &str, confdir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(user_unit_dir())?;
    let body = format!(
        "[Unit]\n\
         Description=OneDrive sync ({name})\n\
         Documentation=https://github.com/sweatshopking/Linux-Onedrive-GUI\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         {MANAGED_MARK}\n\
         \n\
         [Service]\n\
         ExecStart={} --client --monitor --confdir {}\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         RestartPreventExitStatus=126 78\n\
         TimeoutStopSec=90\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(&self_command().to_string_lossy()),
        systemd_quote(&confdir.to_string_lossy()),
    );
    atomic_write(&user_unit_dir().join(unit), body.as_bytes())?;
    systemctl(&["daemon-reload"]);
    Ok(())
}

/// Keeps our managed units pointing at wherever this app now lives (AppImages get moved).
pub fn repair_managed_units() {
    let me = self_command().to_string_lossy().to_string();
    let Ok(rd) = fs::read_dir(user_unit_dir()) else {
        return;
    };
    let mut changed = false;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        if !text.contains(MANAGED_MARK) {
            continue;
        }
        let new: Vec<String> = text
            .lines()
            .map(|l| {
                if let Some(rest) = l.strip_prefix("ExecStart=")
                    && let Some(idx) = rest.find(" --client")
                {
                    let cur = rest[..idx].trim_matches('"').replace("%%", "%");
                    if cur != me {
                        return format!("ExecStart={}{}", systemd_quote(&me), &rest[idx..]);
                    }
                }
                l.to_string()
            })
            .collect();
        let new = new.join("\n") + "\n";
        if new != text && atomic_write(&p, new.as_bytes()).is_ok() {
            changed = true;
        }
    }
    if changed {
        systemctl(&["daemon-reload"]);
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
    (b > a + 1).then(|| d[a + 1..b].to_string())
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
    let pattern = "Starting a sync|Sync with Microsoft OneDrive is complete|ERROR|Unable to |--resync|requires authentication|re-authenticate|invalid_grant";
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
            "--grep",
            pattern,
        ])
        .output();
    match out {
        // exit status 1 with nothing on stderr just means "no matching lines"
        Ok(o) if o.status.success() || (o.status.code() == Some(1) && o.stderr.is_empty()) => {
            chronological(
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
fn chronological(mut lines: Vec<String>) -> Vec<String> {
    let stamp = |l: &String| {
        l.split_once(' ')
            .and_then(|(t, _)| t.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    lines.sort_by(|a, b| stamp(a).total_cmp(&stamp(b)));
    lines
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
    if !unit.active {
        if unit.failed && matches!(unit.exit_status, Some(126) | Some(78)) {
            return Phase::NeedsResync;
        }
        if log_phase == Phase::NeedsResync {
            return Phase::NeedsResync;
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

pub fn run_detached(prog: &str, args: &[&str]) {
    if let Ok(mut c) = Command::new(prog)
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
        let lines = vec![
            "20.5 h onedrive[1]: b".to_string(),
            "10.0 h onedrive[1]: a".to_string(),
        ];
        assert_eq!(
            chronological(lines),
            vec!["10.0 h onedrive[1]: a", "20.5 h onedrive[1]: b"]
        );
    }

    #[test]
    fn uris() {
        assert_eq!(
            file_uri(Path::new("/home/a b/Kate's #1.txt")),
            "file:///home/a%20b/Kate%27s%20%231.txt"
        );
    }

    #[test]
    fn log_lines() {
        assert!(matches!(
            parse_log("Uploading new file: a/b.txt ... done"),
            Some(LogEvent::File { kind: "↑", .. })
        ));
        assert!(parse_log("Uploading new file: a/b.txt ... failed").is_none());
        assert!(
            matches!(parse_log("Downloading file: ./x/y.pdf ... done"), Some(LogEvent::File { path, .. }) if path == "x/y.pdf")
        );
        assert!(matches!(
            parse_log("Sync with Microsoft OneDrive is complete"),
            Some(LogEvent::Complete)
        ));
    }
}
