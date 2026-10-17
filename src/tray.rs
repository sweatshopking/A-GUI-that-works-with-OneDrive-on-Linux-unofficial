//! System tray icon: live status per account, quick controls, and the sync schedule enforcer.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu};

use crate::core::{self, AccountInfo, AccountPrefs, AppSettings, LogEvent, Phase, Status, UnitState};

const RECENT_MAX: usize = 15;

#[derive(Clone)]
struct Activity {
    when: DateTime<Local>,
    kind: &'static str,
    path: String,
}

struct Acc {
    info: AccountInfo,
    prefs: AccountPrefs,
    unit: UnitState,
    resyncing: bool,
    log_phase: Phase,
    last_complete: Option<DateTime<Local>>,
    last_error: Option<String>,
    recent: VecDeque<Activity>,
}

impl Acc {
    fn new(info: AccountInfo, prefs: AccountPrefs) -> Self {
        Acc {
            info,
            prefs,
            unit: UnitState::default(),
            resyncing: false,
            log_phase: Phase::Unknown,
            last_complete: None,
            last_error: None,
            recent: VecDeque::new(),
        }
    }
    fn status(&self) -> Status {
        Status {
            phase: core::compose_phase(&self.info, &self.prefs, &self.unit, self.resyncing, self.log_phase),
            last_complete: self.last_complete,
            last_error: self.last_error.clone(),
            unit: self.unit.clone(),
            resyncing: self.resyncing,
        }
    }
}

pub struct Tray {
    accs: Vec<Acc>,
}

fn open_settings(account: Option<&str>) {
    let me = core::self_command();
    let mut c = Command::new(me);
    c.arg("--settings");
    if let Some(a) = account {
        c.args(["--account", a]);
    }
    if let Ok(mut ch) = c.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        thread::spawn(move || {
            let _ = ch.wait();
        });
    }
}

impl Tray {
    fn overall(&self) -> Phase {
        let ps: Vec<Phase> = self.accs.iter().map(|a| a.status().phase).collect();
        if ps.iter().any(|p| matches!(p, Phase::Error | Phase::NeedsResync | Phase::SignedOut)) {
            Phase::Error
        } else if ps.iter().any(|p| matches!(p, Phase::Syncing | Phase::Resyncing | Phase::Unknown)) {
            Phase::Syncing
        } else if ps.is_empty() || ps.iter().all(|p| matches!(p, Phase::Off | Phase::Stopped | Phase::Paused)) {
            Phase::Off
        } else {
            Phase::UpToDate
        }
    }

    fn account_menu(&self, idx: usize) -> MenuItem<Self> {
        let a = &self.accs[idx];
        let st = a.status();
        let text = st.text(&a.prefs);
        let key = a.info.key();
        let mut items: Vec<MenuItem<Self>> =
            vec![StandardItem { label: text.clone(), icon_name: st.phase.icon().into(), enabled: false, ..Default::default() }.into()];
        if st.phase == Phase::Error {
            if let Some(e) = &a.last_error {
                items.push(StandardItem { label: e.chars().take(90).collect(), enabled: false, ..Default::default() }.into());
            }
        }
        if matches!(st.phase, Phase::SignedOut | Phase::NeedsResync) {
            let k = key.clone();
            items.push(
                StandardItem {
                    label: if st.phase == Phase::SignedOut { "Sign in…".into() } else { "Fix in settings…".into() },
                    icon_name: "dialog-password".into(),
                    activate: Box::new(move |_: &mut Self| open_settings(Some(&k))),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open folder".into(),
                icon_name: "folder-open".into(),
                activate: Box::new(move |t: &mut Self| core::open_path(&t.accs[idx].info.sync_dir)),
                ..Default::default()
            }
            .into(),
        );
        let can_sync = a.info.signed_in && !a.resyncing;
        items.push(
            StandardItem {
                label: "Sync now".into(),
                icon_name: "view-refresh".into(),
                enabled: can_sync,
                activate: Box::new(move |t: &mut Self| t.sync_now(idx)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            CheckmarkItem {
                label: "Background sync".into(),
                checked: a.unit.enabled,
                enabled: a.info.signed_in && !a.resyncing,
                activate: Box::new(move |t: &mut Self| t.toggle(idx)),
                ..Default::default()
            }
            .into(),
        );
        let recent: Vec<MenuItem<Self>> = if a.recent.is_empty() {
            vec![StandardItem { label: "Nothing yet".into(), enabled: false, ..Default::default() }.into()]
        } else {
            a.recent
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    StandardItem {
                        label: format!("{} {}   {}", r.kind, r.path.replace('_', "__"), core::short_time(r.when)),
                        activate: Box::new(move |t: &mut Self| {
                            let a = &t.accs[idx];
                            if let Some(r) = a.recent.get(i) {
                                core::show_in_folder(&a.info.sync_dir.join(&r.path));
                            }
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect()
        };
        items.push(MenuItem::Separator);
        items.push(SubMenu { label: "Recent files".into(), icon_name: "document-open-recent".into(), submenu: recent, ..Default::default() }.into());
        let k = key.clone();
        items.push(
            StandardItem {
                label: "Settings…".into(),
                icon_name: "configure".into(),
                activate: Box::new(move |_: &mut Self| open_settings(Some(&k))),
                ..Default::default()
            }
            .into(),
        );
        SubMenu { label: format!("{} — {}", a.info.name, text), icon_name: st.phase.icon().into(), submenu: items, ..Default::default() }.into()
    }

    fn sync_now(&mut self, idx: usize) {
        let a = &mut self.accs[idx];
        // a manual sync outside the schedule window is allowed for an hour
        if !a.prefs.schedule.allows(Local::now().time()) {
            let mut s = AppSettings::load();
            s.prefs_mut(&a.info.confdir).override_until = Local::now().timestamp() + 3600;
            let _ = s.save();
            a.prefs = s.prefs(&a.info.confdir);
        }
        match &a.info.unit {
            Some(u) => {
                a.unit.starting = true;
                a.unit.active = true;
                core::run_detached("systemctl", &["--user", "restart", u]);
            }
            None => {
                if let Ok(u) = core::ensure_unit(&a.info) {
                    a.info.unit = Some(u.clone());
                    core::run_detached("systemctl", &["--user", "start", &u]);
                }
            }
        }
    }

    fn toggle(&mut self, idx: usize) {
        let a = &mut self.accs[idx];
        let on = !a.unit.enabled;
        let unit = match &a.info.unit {
            Some(u) => u.clone(),
            None => match core::ensure_unit(&a.info) {
                Ok(u) => {
                    a.info.unit = Some(u.clone());
                    u
                }
                Err(_) => return,
            },
        };
        a.unit.enabled = on;
        a.unit.active = on;
        a.unit.starting = on;
        core::run_detached("systemctl", &["--user", if on { "enable" } else { "disable" }, "--now", &unit]);
    }
}

impl ksni::Tray for Tray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        core::APP_ID.into()
    }
    fn title(&self) -> String {
        "OneDrive".into()
    }
    fn icon_name(&self) -> String {
        self.overall().icon().into()
    }
    fn status(&self) -> ksni::Status {
        if self.overall() == Phase::Error { ksni::Status::NeedsAttention } else { ksni::Status::Active }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let lines: Vec<String> =
            self.accs.iter().map(|a| format!("{}: {}", a.info.name, a.status().text(&a.prefs))).collect();
        let description = if lines.is_empty() { "No accounts yet — open Settings to add one".into() } else { lines.join("\n") };
        ksni::ToolTip { title: "OneDrive".into(), description, ..Default::default() }
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m: Vec<MenuItem<Self>> = (0..self.accs.len()).map(|i| self.account_menu(i)).collect();
        if m.is_empty() {
            m.push(StandardItem { label: "No accounts yet".into(), enabled: false, ..Default::default() }.into());
        }
        m.push(MenuItem::Separator);
        m.push(
            StandardItem {
                label: "Settings and accounts…".into(),
                icon_name: "configure".into(),
                activate: Box::new(|_: &mut Self| open_settings(None)),
                ..Default::default()
            }
            .into(),
        );
        m.push(
            StandardItem {
                label: "Quit tray icon".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        m
    }
}

/// Follows one unit's journal for as long as the tray runs.
fn follow(handle: ksni::blocking::Handle<Tray>, unit: String) {
    loop {
        let child = Command::new("journalctl")
            .args(["--user", "-u", &unit, "-f", "-n", "400", "-o", "short-unix", "--no-pager", "-q"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            thread::sleep(Duration::from_secs(10));
            continue;
        };
        let out = child.stdout.take().unwrap();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let Some((when, msg)) = core::split_journal_line(&line) else { continue };
            let Some(ev) = core::parse_log(msg) else { continue };
            let alive = handle.update(|t: &mut Tray| {
                let Some(a) = t.accs.iter_mut().find(|a| a.info.unit.as_deref() == Some(unit.as_str())) else { return false };
                match ev {
                    LogEvent::Syncing => a.log_phase = Phase::Syncing,
                    LogEvent::Complete => {
                        a.log_phase = Phase::UpToDate;
                        a.last_complete = Some(when);
                        a.last_error = None;
                    }
                    LogEvent::Error(e) => {
                        a.log_phase = Phase::Error;
                        a.last_error = Some(e);
                    }
                    LogEvent::NeedsResync => a.log_phase = Phase::NeedsResync,
                    LogEvent::NeedsLogin => {
                        a.log_phase = Phase::Error;
                        a.last_error = Some("Microsoft needs you to sign in again".into());
                    }
                    LogEvent::File { kind, path } => {
                        a.recent.retain(|r| r.path != path);
                        a.recent.push_front(Activity { when, kind, path });
                        a.recent.truncate(RECENT_MAX);
                    }
                }
                true
            });
            if alive != Some(true) {
                let _ = child.kill();
                let _ = child.wait();
                return; // account was removed
            }
        }
        let _ = child.wait();
        thread::sleep(Duration::from_secs(5));
    }
}

/// Every few seconds: re-read accounts and settings, poll systemd, and apply schedules.
fn poll(handle: ksni::blocking::Handle<Tray>) {
    let mut following: HashSet<String> = HashSet::new();
    loop {
        let settings = AppSettings::load();
        let infos = core::discover(&settings);
        let states: Vec<(UnitState, bool)> = infos
            .iter()
            .map(|i| {
                let st = i.unit.as_deref().map(core::unit_state).unwrap_or_default();
                (st, core::unit_state(&core::resync_unit(&i.confdir)).active)
            })
            .collect();

        // schedules: start or stop enabled services to match their window
        let now = Local::now();
        for (info, (st, resyncing)) in infos.iter().zip(&states) {
            let prefs = settings.prefs(&info.confdir);
            let Some(unit) = &info.unit else { continue };
            if !prefs.schedule.enabled || !st.enabled || *resyncing || !info.signed_in {
                continue;
            }
            let allowed = prefs.schedule.allows(now.time()) || prefs.override_until > now.timestamp();
            if allowed && !st.active && !st.failed {
                core::run_detached("systemctl", &["--user", "start", unit]);
            } else if !allowed && st.active {
                core::run_detached("systemctl", &["--user", "stop", unit]);
            }
        }

        for i in &infos {
            if let Some(u) = &i.unit {
                if following.insert(u.clone()) {
                    let (h, u) = (handle.clone(), u.clone());
                    thread::spawn(move || follow(h, u));
                }
            }
        }

        handle.update(|t: &mut Tray| {
            let mut old: Vec<Acc> = std::mem::take(&mut t.accs);
            for (info, (st, resyncing)) in infos.into_iter().zip(states) {
                let prefs = settings.prefs(&info.confdir);
                let mut a = match old.iter().position(|o| o.info.confdir == info.confdir) {
                    Some(p) => old.remove(p),
                    None => Acc::new(info.clone(), prefs.clone()),
                };
                if st.starting && !a.unit.starting {
                    a.log_phase = Phase::Unknown; // a fresh process: the old "complete" no longer applies
                }
                a.info = info;
                a.prefs = prefs;
                a.unit = st;
                a.resyncing = resyncing;
                t.accs.push(a);
            }
        });
        // a unit that was removed stops its follower on its next log line; forget it so a re-add works
        following.retain(|u| handle.update(|t: &mut Tray| t.accs.iter().any(|a| a.info.unit.as_deref() == Some(u))).unwrap_or(false));
        thread::sleep(Duration::from_secs(3));
    }
}

pub fn run() {
    core::repair_managed_units();
    let handle = match (Tray { accs: Vec::new() }).spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Couldn't create the tray icon ({e}). Is a system tray available?");
            std::process::exit(1);
        }
    };
    let h = handle.clone();
    thread::spawn(move || poll(h));
    loop {
        thread::park();
    }
}

/// True if a tray instance is already registered on the session bus.
pub fn already_running() -> bool {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten().any(|e| {
                let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { return false };
                if pid == me {
                    return false;
                }
                let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
                let args: Vec<&[u8]> = cmd.split(|b| *b == 0).collect();
                let is_us = args.first().is_some_and(|a| String::from_utf8_lossy(a).contains(core::APP_ID));
                is_us && args.iter().any(|a| *a == b"--tray")
            })
        })
        .unwrap_or(false)
}
