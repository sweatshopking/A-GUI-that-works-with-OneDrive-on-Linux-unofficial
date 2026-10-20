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
    blocked: Vec<core::Blocked>,
    /// Files newly blocked since the tray started, waiting for a notification
    fresh_blocked: Vec<core::Blocked>,
    /// Files already notified about this session (until they sync), so a client restart or a
    /// new login doesn't repeat the same notification
    told_blocked: HashSet<String>,
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
            blocked: Vec::new(),
            fresh_blocked: Vec::new(),
            told_blocked: HashSet::new(),
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
            blocked: self.blocked.clone(),
            unit: self.unit.clone(),
        }
    }
}

pub struct Tray {
    accs: Vec<Acc>,
    /// Set while the OneDrive Client for Linux is missing or too old
    client_problem: Option<String>,
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
        if self.client_problem.is_some()
            || ps
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
        for b in a.blocked.iter().take(3) {
            let why = match b.reason {
                core::BlockReason::Locked => "open for editing elsewhere",
                core::BlockReason::ReadOnly => "shared read-only",
                core::BlockReason::NoDownload => "view-only, can't download",
                core::BlockReason::NotNow => "turned away; try Sync now",
            };
            // a single _ would turn into a keyboard shortcut and vanish
            let name = core::shown_name(&b.path).replace('_', "__");
            items.push(
                StandardItem {
                    label: format!("Not synced: {name} ({why})"),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        if a.blocked.len() > 3 {
            items.push(
                StandardItem {
                    label: format!("…and {} more not synced", a.blocked.len() - 3),
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
        let can_sync = a.info.signed_in && !a.resyncing && self.client_problem.is_none();
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
                core::unit_action("restart", u);
            }
            None => {
                if let Ok(u) = core::ensure_unit(&a.info) {
                    a.info.unit = Some(u.clone());
                    core::unit_action("start", &u);
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
            core::unit_action(verb, u);
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
        let description = if let Some(p) = &self.client_problem {
            format!("{p}. Open Settings to fix it.")
        } else if lines.is_empty() {
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
        let mut m: Vec<MenuItem<Self>> = Vec::new();
        if let Some(p) = &self.client_problem {
            m.push(
                StandardItem {
                    label: p.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            m.push(
                StandardItem {
                    label: "How to fix this…".into(),
                    icon_name: "dialog-warning".into(),
                    activate: Box::new(|_: &mut Self| open_settings(None)),
                    ..Default::default()
                }
                .into(),
            );
            m.push(MenuItem::Separator);
        }
        m.extend((0..self.accs.len()).map(|i| self.account_menu(i)));
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
/// Applies log events to the account in one tray update (each update redraws the menu).
fn apply_events(
    handle: &ksni::blocking::Handle<Tray>,
    unit: &str,
    events: Vec<(DateTime<Local>, LogEvent)>,
) -> Option<bool> {
    handle.update(|t: &mut Tray| {
        let Some(a) = t
            .accs
            .iter_mut()
            .find(|a| a.info.unit.as_deref() == Some(unit))
        else {
            return false;
        };
        for (when, ev) in events {
            apply_one(a, when, ev);
        }
        true
    })
}

/// When the tray started: log lines from before then were already there, not news.
fn tray_started() -> DateTime<Local> {
    static STARTED: std::sync::OnceLock<DateTime<Local>> = std::sync::OnceLock::new();
    *STARTED.get_or_init(Local::now)
}

fn apply_one(a: &mut Acc, when: DateTime<Local>, ev: LogEvent) {
    core::track_blocked(&mut a.blocked, &ev);
    match &ev {
        LogEvent::Blocked { path, reason }
            if reason.needs_attention()
                && when.timestamp() >= tray_started().timestamp()
                && a.told_blocked.insert(path.clone()) =>
        {
            a.fresh_blocked.push(core::Blocked {
                path: path.clone(),
                reason: *reason,
            });
        }
        LogEvent::File { path, .. } => {
            a.told_blocked.remove(path);
        }
        _ => {}
    }
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
            a.log_phase = Phase::SignedOut;
            a.last_error = Some("Microsoft needs you to sign in again".into());
        }
        LogEvent::Blocked { .. } | LogEvent::ClientStart => {}
        LogEvent::File { kind, path } => {
            a.recent.retain(|r| r.path != path);
            a.recent.push_front(Activity { when, kind, path });
            a.recent.truncate(RECENT_MAX);
        }
    }
}

/// Follows one unit's journal for as long as the tray runs.
fn follow(handle: ksni::blocking::Handle<Tray>, unit: String) {
    // the status lines first, so "last synced" is right even when the recent log is busy
    let status: Vec<_> = core::journal_status_lines(&unit)
        .iter()
        .filter_map(|line| {
            let (when, msg) = core::split_journal_line(line)?;
            Some((when, core::parse_log(msg)?))
        })
        .collect();
    apply_events(&handle, &unit, status);
    // files blocked before the recent log, and not synced since
    let blocked = core::blocked_files(&unit);
    handle.update(|t: &mut Tray| {
        if let Some(a) = t
            .accs
            .iter_mut()
            .find(|a| a.info.unit.as_deref() == Some(unit.as_str()))
        {
            a.blocked = blocked;
        }
    });
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
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut raw = Vec::new();
        let mut batch = Vec::new();
        // Lines are read as bytes: a file name that isn't valid UTF-8 must not end the follow.
        // While more lines are already waiting (a busy sync), events are collected and applied
        // together, so a flood of file lines doesn't redraw the menu for every one of them.
        loop {
            raw.clear();
            match out.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&raw);
            if let Some((when, msg)) = core::split_journal_line(line.trim_end())
                && let Some(ev) = core::parse_log(msg)
            {
                batch.push((when, ev));
            }
            if batch.is_empty() || (!out.buffer().is_empty() && batch.len() < 500) {
                continue;
            }
            if apply_events(&handle, &unit, std::mem::take(&mut batch)) != Some(true) {
                let _ = child.kill();
                let _ = child.wait();
                return; // account was removed
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        thread::sleep(Duration::from_secs(5));
    }
}

/// Every few seconds: re-read accounts and settings, poll systemd, and apply schedules.
fn poll(handle: ksni::blocking::Handle<Tray>) {
    let mut following: HashSet<String> = HashSet::new();
    let mut last_phase: HashMap<String, Phase> = HashMap::new();
    tray_started();
    let mut replaced_seen = false;
    let mut round = 0u32;
    loop {
        if round.is_multiple_of(20) {
            let problem = match core::check_client() {
                core::ClientCheck::Ok { .. } => None,
                core::ClientCheck::Missing => {
                    Some("The OneDrive Client for Linux isn't installed".to_string())
                }
                core::ClientCheck::TooOld { version, .. } => Some(format!(
                    "The OneDrive Client for Linux ({version}) is too old"
                )),
            };
            let fixed = handle
                .update(|t: &mut Tray| {
                    let was = t.client_problem.take();
                    t.client_problem = problem;
                    was.is_some() && t.client_problem.is_none()
                })
                .unwrap_or(false);
            if fixed {
                // the client was just installed: point the units at it and get them going
                core::repair_managed_units();
                core::start_idle_units(&AppSettings::load());
            }
        }
        round = round.wrapping_add(1);
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
            if !managed_by_time
                || !st.enabled
                || *resyncing
                || !info.signed_in
                || core::account_busy(&info.confdir)
            {
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
        // tell the user about each file that newly couldn't sync (not ones from before the tray started)
        let fresh: Vec<(String, Vec<core::Blocked>)> = handle
            .update(|t: &mut Tray| {
                t.accs
                    .iter_mut()
                    .filter(|a| !a.fresh_blocked.is_empty())
                    .map(|a| (a.info.name.clone(), std::mem::take(&mut a.fresh_blocked)))
                    .collect()
            })
            .unwrap_or_default();
        for (name, list) in fresh {
            share::notify(&format!("OneDrive: {name}"), &core::blocked_text(&list));
        }
        // a unit that was removed stops its follower on its next log line; forget it so a re-add works
        following.retain(|u| {
            handle
                .update(|t: &mut Tray| t.accs.iter().any(|a| a.info.unit.as_deref() == Some(u)))
                .unwrap_or(false)
        });
        // after an update, hand over to the new version (once the new file has stayed put for a
        // whole round, so we don't catch an install half-way)
        if core::program_file_replaced() {
            if replaced_seen {
                restart_into_new_version();
            }
            replaced_seen = true;
        } else {
            replaced_seen = false;
        }
        thread::sleep(Duration::from_secs(3));
    }
}

/// Set on the tray we start after an update, so it waits for this one to leave.
pub const RESTART_ENV: &str = "LINUX_ONEDRIVE_GUI_TRAY_RESTART";

fn restart_into_new_version() {
    let started = Command::new(core::self_command())
        .arg("--tray")
        .env(RESTART_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if started.is_ok() {
        std::process::exit(0);
    }
}

pub fn run() {
    crate::ui::recover_interrupted_preview();
    core::repair_managed_units();
    core::start_idle_units(&AppSettings::load());
    // At login we can start before the panel is ready, and on GNOME there may be no tray at
    // all. Keep running either way: the icon appears whenever a tray shows up, and schedules
    // and pauses are enforced meanwhile.
    let handle = match (Tray {
        accs: Vec::new(),
        client_problem: None,
    })
    .assume_sni_available(true)
    .spawn()
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Couldn't start the tray icon service ({e}).");
            std::process::exit(1);
        }
    };
    let h = handle.clone();
    thread::spawn(move || poll(h));
    loop {
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc() -> Acc {
        Acc::new(
            AccountInfo {
                name: "Work".into(),
                confdir: "/c".into(),
                unit: Some("onedrive-work".into()),
                managed: true,
                sync_dir: "/s".into(),
                drive_id: None,
                kind: "business".into(),
                signed_in: true,
            },
            AccountPrefs::default(),
        )
    }

    fn blocked(path: &str, reason: core::BlockReason) -> LogEvent {
        LogEvent::Blocked {
            path: path.into(),
            reason,
        }
    }

    #[test]
    fn blocked_notifications() {
        use crate::core::BlockReason::*;
        let mut a = acc();
        let old = tray_started() - chrono::Duration::hours(1);
        let now = tray_started() + chrono::Duration::seconds(5);
        // from before the tray started: listed, not announced
        apply_one(&mut a, old, blocked("old.docx", Locked));
        assert_eq!(a.blocked.len(), 1);
        assert!(a.fresh_blocked.is_empty());
        // new: announced once
        apply_one(&mut a, now, blocked("a.docx", Locked));
        assert_eq!(a.fresh_blocked.len(), 1);
        a.fresh_blocked.clear();
        // the client restarts and reports it again: still listed, not announced again
        apply_one(&mut a, now, LogEvent::ClientStart);
        assert!(a.blocked.is_empty());
        apply_one(&mut a, now, blocked("a.docx", Locked));
        assert_eq!(a.blocked.len(), 1);
        assert!(a.fresh_blocked.is_empty());
        // view-only files never notify
        apply_one(&mut a, now, blocked("v.pdf", NoDownload));
        assert!(a.fresh_blocked.is_empty());
        // once it uploads, a later block is news again
        apply_one(
            &mut a,
            now,
            LogEvent::File {
                kind: "↑",
                path: "a.docx".into(),
            },
        );
        apply_one(&mut a, now, blocked("a.docx", Locked));
        assert_eq!(a.fresh_blocked.len(), 1);
    }
}
