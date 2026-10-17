//! System tray icon: live status per account, quick controls, and the sync schedule enforcer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu};

use crate::core::{
    self, AccountInfo, AccountPrefs, AppSettings, LogEvent, Phase, Status, UnitState,
};
use crate::share;

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
            phase: core::compose_phase(
                &self.info,
                &self.prefs,
                &self.unit,
                self.resyncing,
                self.log_phase,
            ),
            last_complete: self.last_complete,
            last_error: self.last_error.clone(),
            unit: self.unit.clone(),
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
        if ps
            .iter()
            .any(|p| matches!(p, Phase::Error | Phase::NeedsResync | Phase::SignedOut))
        {
            Phase::Error
        } else if ps
            .iter()
            .any(|p| matches!(p, Phase::Syncing | Phase::Resyncing | Phase::Unknown))
        {
            Phase::Syncing
        } else if ps.is_empty()
            || ps
                .iter()
                .all(|p| matches!(p, Phase::Off | Phase::Stopped | Phase::Paused))
        {
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
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: text.clone(),
                icon_name: st.phase.icon().into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];
        if st.phase == Phase::Error
            && let Some(e) = &a.last_error
        {
            items.push(
                StandardItem {
                    label: e.chars().take(90).collect(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        if matches!(st.phase, Phase::SignedOut | Phase::NeedsResync) {
            let k = key.clone();
            items.push(
                StandardItem {
                    label: if st.phase == Phase::SignedOut {
                        "Sign in…".into()
                    } else {
                        "Fix in settings…".into()
                    },
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
        if a.unit.enabled && a.info.signed_in {
            if a.prefs.paused() {
                items.push(
                    StandardItem {
                        label: "Resume syncing".into(),
                        icon_name: "media-playback-start".into(),
                        activate: Box::new(move |t: &mut Self| t.pause(idx, 0)),
                        ..Default::default()
                    }
                    .into(),
                );
            } else {
                let choices = [
                    ("For 1 hour", 3600),
                    ("For 4 hours", 4 * 3600),
                    ("Until tomorrow morning", -1),
                ];
                let submenu = choices
                    .into_iter()
                    .map(|(label, secs)| {
                        StandardItem {
                            label: label.into(),
                            activate: Box::new(move |t: &mut Self| t.pause(idx, secs)),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect();
                items.push(
                    SubMenu {
                        label: "Pause syncing".into(),
                        icon_name: "media-playback-pause".into(),
                        submenu,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        let recent: Vec<MenuItem<Self>> = if a.recent.is_empty() {
            vec![
                StandardItem {
                    label: "Nothing yet".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            ]
        } else {
            a.recent
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    StandardItem {
                        label: format!(
                            "{} {}   {}",
                            r.kind,
                            r.path.replace('_', "__"),
                            core::short_time(r.when)
                        ),
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
        items.push(
            SubMenu {
                label: "Recent files".into(),
                icon_name: "document-open-recent".into(),
                submenu: recent,
                ..Default::default()
            }
            .into(),
        );
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
        SubMenu {
            label: format!("{} — {}", a.info.name, text),
            icon_name: st.phase.icon().into(),
            submenu: items,
            ..Default::default()
        }
        .into()
    }

    fn sync_now(&mut self, idx: usize) {
        let a = &mut self.accs[idx];
        // a manual sync outside the schedule window is allowed for an hour
        if !a.prefs.sync_allowed() {
            let mut s = AppSettings::load();
            let p = s.prefs_mut(&a.info.confdir);
            p.override_until = Local::now().timestamp() + 3600;
            p.pause_until = 0;
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

    /// Pauses for `secs` seconds (-1 = until 06:00 tomorrow, 0 = resume).
    fn pause(&mut self, idx: usize, secs: i64) {
        let a = &mut self.accs[idx];
        let now = Local::now();
        let until = match secs {
            0 => 0,
            -1 => (now.date_naive() + chrono::Days::new(1))
                .and_hms_opt(6, 0, 0)
                .and_then(|t| t.and_local_timezone(Local).single())
                .map(|t| t.timestamp())
                .unwrap_or(now.timestamp() + 12 * 3600),
            s => now.timestamp() + s,
        };
        let mut s = AppSettings::load();
        let p = s.prefs_mut(&a.info.confdir);
        p.pause_until = until;
        p.override_until = 0;
        let _ = s.save();
        a.prefs = s.prefs(&a.info.confdir);
        if let Some(u) = &a.info.unit {
            let verb = if until == 0 { "start" } else { "stop" };
            a.unit.active = until == 0;
            core::run_detached("systemctl", &["--user", verb, u]);
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
        core::run_detached(
            "systemctl",
            &[
                "--user",
                if on { "enable" } else { "disable" },
                "--now",
                &unit,
            ],
        );
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
    fn icon_theme_path(&self) -> String {
        core::icon_theme_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn status(&self) -> ksni::Status {
        if self.overall() == Phase::Error {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let lines: Vec<String> = self
            .accs
            .iter()
            .map(|a| format!("{}: {}", a.info.name, a.status().text(&a.prefs)))
            .collect();
        let description = if lines.is_empty() {
            "No accounts yet — open Settings to add one".into()
        } else {
            lines.join("\n")
        };
        ksni::ToolTip {
            title: "OneDrive".into(),
            description,
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut m: Vec<MenuItem<Self>> =
            (0..self.accs.len()).map(|i| self.account_menu(i)).collect();
        if m.is_empty() {
            m.push(
                StandardItem {
                    label: "No accounts yet".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
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

/// Applies one log event to the account using `unit`. Returns Some(false) once that account is gone.
fn apply_event(
    handle: &ksni::blocking::Handle<Tray>,
    unit: &str,
    when: DateTime<Local>,
    ev: LogEvent,
) -> Option<bool> {
    handle.update(|t: &mut Tray| {
        let Some(a) = t
            .accs
            .iter_mut()
            .find(|a| a.info.unit.as_deref() == Some(unit))
        else {
            return false;
        };
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
    })
}

/// Follows one unit's journal for as long as the tray runs.
fn follow(handle: ksni::blocking::Handle<Tray>, unit: String) {
    // the status lines first, so "last synced" is right even when the recent log is busy
    for line in core::journal_status_lines(&unit) {
        if let Some((when, msg)) = core::split_journal_line(&line)
            && let Some(ev) = core::parse_log(msg)
        {
            apply_event(&handle, &unit, when, ev);
        }
    }
    loop {
        let child = Command::new("journalctl")
            .args([
                "--user",
                "-u",
                &unit,
                "-f",
                "-n",
                "400",
                "-o",
                "short-unix",
                "--no-pager",
                "-q",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            thread::sleep(Duration::from_secs(10));
            continue;
        };
        let out = child.stdout.take().unwrap();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let Some((when, msg)) = core::split_journal_line(&line) else {
                continue;
            };
            let Some(ev) = core::parse_log(msg) else {
                continue;
            };
            let alive = apply_event(&handle, &unit, when, ev);
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
    let mut last_phase: HashMap<String, Phase> = HashMap::new();
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

        // schedules and pauses: start or stop enabled services to match
        let mut expired_pause = false;
        for (info, (st, resyncing)) in infos.iter().zip(&states) {
            let prefs = settings.prefs(&info.confdir);
            let Some(unit) = &info.unit else { continue };
            let managed_by_time = prefs.schedule.enabled || prefs.pause_until > 0;
            if !managed_by_time || !st.enabled || *resyncing || !info.signed_in {
                continue;
            }
            let allowed = prefs.sync_allowed();
            if allowed && !st.active && !st.failed {
                expired_pause |= prefs.pause_until > 0 && !prefs.paused();
                core::run_detached("systemctl", &["--user", "start", unit]);
            } else if !allowed && st.active {
                core::run_detached("systemctl", &["--user", "stop", unit]);
            }
        }

        if expired_pause {
            let mut s = AppSettings::load();
            for p in s.accounts.values_mut() {
                if p.pause_until > 0 && !p.paused() {
                    p.pause_until = 0;
                }
            }
            let _ = s.save();
        }

        for i in &infos {
            if let Some(u) = &i.unit
                && following.insert(u.clone())
            {
                let (h, u) = (handle.clone(), u.clone());
                thread::spawn(move || follow(h, u));
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
        // tell the user when an account newly needs attention
        let phases: Vec<(String, String, Phase)> = handle
            .update(|t: &mut Tray| {
                t.accs
                    .iter()
                    .map(|a| (a.info.key(), a.info.name.clone(), a.status().phase))
                    .collect()
            })
            .unwrap_or_default();
        for (key, name, phase) in &phases {
            let before = last_phase.insert(key.clone(), *phase);
            let bad = |p: Phase| matches!(p, Phase::Error | Phase::NeedsResync | Phase::SignedOut);
            if let Some(before) = before
                && bad(*phase)
                && !bad(before)
            {
                let body = match phase {
                    Phase::SignedOut => {
                        "Microsoft needs you to sign in again. Open the tray menu to fix it."
                    }
                    Phase::NeedsResync => {
                        "The sync engine needs a full resync. Open Settings to review and apply it."
                    }
                    _ => "Syncing hit an error. Open the tray menu for details.",
                };
                share::notify(&format!("OneDrive: {name}"), body);
            }
        }
        // a unit that was removed stops its follower on its next log line; forget it so a re-add works
        following.retain(|u| {
            handle
                .update(|t: &mut Tray| t.accs.iter().any(|a| a.info.unit.as_deref() == Some(u)))
                .unwrap_or(false)
        });
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
