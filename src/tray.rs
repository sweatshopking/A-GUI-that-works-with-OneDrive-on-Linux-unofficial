//! System tray icon: live status per account, the panel a click opens (src/popup.rs), a short
//! menu, and the sync schedule enforcer.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem, SubMenu};

use crate::core::{
    self, AccountInfo, AccountPrefs, AppSettings, LogEvent, Phase, Status, UnitState,
};
use crate::share;

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
    /// Told about the current big delete; a finished sync means it's over
    told_big_delete: bool,
    /// Large files being uploaded or downloaded right now (file manager badges)
    moving: HashSet<String>,
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
            told_big_delete: false,
            moving: HashSet::new(),
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
            big_delete: None,
            unit: self.unit.clone(),
        }
    }
}

pub struct Tray {
    accs: Vec<Acc>,
    /// Set while the OneDrive Client for Linux is missing or too old
    client_problem: Option<String>,
    /// "Quit" also stops syncing (App settings)
    stop_on_quit: bool,
    /// A newer version that's out
    update: Option<String>,
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

/// The open panel's process and when it opened, and when the last one closed.
struct PopupState {
    open: Option<(u32, Instant, Option<OwnedFd>, mpsc::Sender<()>)>,
    closed: Option<Instant>,
    /// the activation token the tray host handed over for the latest click on the icon
    token: Option<(String, Instant)>,
}

static POPUP: Mutex<PopupState> = Mutex::new(PopupState {
    open: None,
    closed: None,
    token: None,
});

/// Opens the panel, or closes it if it's open. `at`: where the tray icon was clicked.
fn toggle_popup(at: Option<(i32, i32)>) {
    let mut p = POPUP.lock().unwrap();
    if let Some((_, opened, pidfd, close)) = p.open.as_ref() {
        // the second click of a double-click: it was meant to open the panel, which it did
        if opened.elapsed() < Duration::from_millis(400) {
            return;
        }
        let signalled = if let Some(fd) = pidfd {
            // SAFETY: fd is a pidfd for this exact child process, so PID reuse cannot redirect it.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) == 0
            }
        } else {
            false
        };
        if !signalled {
            // The waiter owns the Child and can safely signal it before reaping it, even on
            // kernels without pidfd support.
            let _ = close.send(());
        }
        p.open = None;
        return;
    }
    // A click on the icon while the panel is open first takes focus from the panel, which
    // closes itself, and then arrives here: that click was meant to close it, not reopen it.
    if p.closed
        .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
    {
        return;
    }
    let mut c = Command::new(core::self_command());
    c.arg("--popup");
    if let Some((x, y)) = at {
        c.args([x.to_string(), y.to_string()]);
    }
    // the token that came with this click (a token works once, and only soon after)
    if let Some((token, when)) = p.token.take()
        && when.elapsed() < Duration::from_secs(2)
    {
        c.env("XDG_ACTIVATION_TOKEN", token);
    }
    let Ok(mut child) = c.stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return;
    };
    let pid = child.id();
    let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    let pidfd = if raw_fd >= 0 {
        // SAFETY: pidfd_open returned a new owned file descriptor.
        Some(unsafe { OwnedFd::from_raw_fd(raw_fd as i32) })
    } else {
        None
    };
    let (close_tx, close_rx) = mpsc::channel();
    p.open = Some((pid, Instant::now(), pidfd, close_tx));
    drop(p);
    thread::spawn(move || {
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
            match close_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => {
                    // This thread alone reaps the child. If it is still running, its PID cannot
                    // have been reused; use SIGTERM so the popup can unload its desktop script.
                    if matches!(child.try_wait(), Ok(None)) {
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGTERM);
                        }
                    }
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        let _ = child.wait();
        let mut p = POPUP.lock().unwrap();
        if p.open
            .as_ref()
            .is_some_and(|(open_pid, _, _, _)| *open_pid == pid)
        {
            p.open = None;
            // closed by itself (focus moved away); one closed by a click above isn't recorded,
            // so clicking again right away reopens it
            p.closed = Some(Instant::now());
        }
    });
}

impl Tray {
    fn overall(&self) -> Phase {
        let ps: Vec<Phase> = self.accs.iter().map(|a| a.status().phase).collect();
        if self.client_problem.is_some()
            || ps.iter().any(|p| {
                matches!(
                    p,
                    Phase::Error | Phase::NeedsResync | Phase::SignedOut | Phase::BigDelete
                )
            })
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

    /// One line per account: its status at a glance; a click opens its settings (where any
    /// fix is).
    fn account_line(&self, idx: usize) -> MenuItem<Self> {
        let a = &self.accs[idx];
        let st = a.status();
        let k = a.info.key();
        StandardItem {
            // a single _ would turn into a keyboard shortcut and vanish
            label: format!("{} — {}", a.info.name, st.text(&a.prefs)).replace('_', "__"),
            icon_name: st.phase.icon().into(),
            activate: Box::new(move |_: &mut Self| open_settings(Some(&k))),
            ..Default::default()
        }
        .into()
    }

    fn sync_now(&mut self, idx: usize) {
        // the menu can be clicked after an account it was built with has gone
        let Some(a) = self.accs.get_mut(idx) else {
            return;
        };
        match core::sync_now(&a.info) {
            Ok(u) => {
                a.info.unit = Some(u);
                a.unit.starting = true;
                a.unit.active = true;
                a.last_error = None;
            }
            Err(e) => a.last_error = Some(e),
        }
        a.prefs = AppSettings::load().prefs(&a.info.confdir);
    }

    /// Pauses for `secs` seconds (-1 = until 06:00 tomorrow, 0 = resume).
    fn pause(&mut self, idx: usize, secs: i64) {
        let Some(a) = self.accs.get_mut(idx) else {
            return;
        };
        match core::pause(&a.info, secs) {
            Ok(until) => {
                a.prefs = AppSettings::load().prefs(&a.info.confdir);
                if a.info.unit.is_some() {
                    a.unit.active = until == 0;
                }
                a.last_error = None;
            }
            Err(e) => a.last_error = Some(e),
        }
    }
}

impl ksni::Tray for Tray {
    // a click opens the panel; the menu is for a right-click
    const MENU_ON_ACTIVATE: bool = false;

    fn provide_xdg_activation_token(&mut self, token: String) {
        POPUP.lock().unwrap().token = Some((token, Instant::now()));
    }

    fn activate(&mut self, x: i32, y: i32) {
        // some trays don't say where (0, 0): the panel then goes by the mouse
        toggle_popup((x != 0 || y != 0).then_some((x, y)));
    }

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
        if let Some(v) = &self.update {
            m.push(
                StandardItem {
                    label: format!("Version {v} is out — get it…"),
                    icon_name: "system-software-update".into(),
                    activate: Box::new(|_: &mut Self| {
                        core::run_detached("xdg-open", &[crate::update::RELEASES_URL])
                    }),
                    ..Default::default()
                }
                .into(),
            );
            m.push(MenuItem::Separator);
        }
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
        m.extend((0..self.accs.len()).map(|i| self.account_line(i)));
        if self.accs.is_empty() {
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
                label: "Show activity…".into(),
                icon_name: "view-list-details".into(),
                activate: Box::new(|_: &mut Self| toggle_popup(None)),
                ..Default::default()
            }
            .into(),
        );
        let syncable: Vec<usize> = (0..self.accs.len())
            .filter(|&i| {
                let a = &self.accs[i];
                a.info.signed_in && !a.resyncing && self.client_problem.is_none()
            })
            .collect();
        {
            let which = syncable.clone();
            m.push(
                StandardItem {
                    label: "Sync now".into(),
                    icon_name: "view-refresh".into(),
                    enabled: !which.is_empty(),
                    activate: Box::new(move |t: &mut Self| {
                        for &i in &which {
                            t.sync_now(i);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        // pausing applies to the accounts syncing in the background
        let pausable: Vec<usize> = syncable
            .iter()
            .copied()
            .filter(|&i| self.accs[i].unit.enabled)
            .collect();
        let paused: Vec<usize> = pausable
            .iter()
            .copied()
            .filter(|&i| self.accs[i].prefs.paused())
            .collect();
        if !paused.is_empty() {
            m.push(
                StandardItem {
                    label: "Resume syncing".into(),
                    icon_name: "media-playback-start".into(),
                    activate: Box::new(move |t: &mut Self| {
                        for &i in &paused {
                            t.pause(i, 0);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
        } else if !pausable.is_empty() {
            let submenu = [
                ("For 1 hour", 3600),
                ("For 4 hours", 4 * 3600),
                ("Until tomorrow morning", -1),
            ]
            .into_iter()
            .map(|(label, secs)| {
                let which = pausable.clone();
                StandardItem {
                    label: label.into(),
                    activate: Box::new(move |t: &mut Self| {
                        for &i in &which {
                            t.pause(i, secs);
                        }
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();
            m.push(
                SubMenu {
                    label: "Pause syncing".into(),
                    icon_name: "media-playback-pause".into(),
                    submenu,
                    ..Default::default()
                }
                .into(),
            );
        }
        match self.accs.len() {
            0 => {}
            1 => m.push(
                StandardItem {
                    label: "Open folder".into(),
                    icon_name: "folder-open".into(),
                    activate: Box::new(|t: &mut Self| {
                        if let Some(a) = t.accs.first() {
                            core::open_path(&a.info.sync_dir);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            ),
            n => {
                let submenu = (0..n)
                    .map(|i| {
                        StandardItem {
                            label: self.accs[i].info.name.replace('_', "__"),
                            activate: Box::new(move |t: &mut Self| {
                                if let Some(a) = t.accs.get(i) {
                                    core::open_path(&a.info.sync_dir);
                                }
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect();
                m.push(
                    SubMenu {
                        label: "Open folder".into(),
                        icon_name: "folder-open".into(),
                        submenu,
                        ..Default::default()
                    }
                    .into(),
                );
            }
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
                label: if self.stop_on_quit {
                    "Quit and stop syncing".into()
                } else {
                    "Quit tray icon".into()
                },
                icon_name: "application-exit".into(),
                activate: Box::new(|_| {
                    // read again: the setting may have changed since the menu was built
                    let settings = AppSettings::load();
                    if settings.stop_sync_on_quit {
                        core::stop_all_units(&settings);
                    }
                    std::process::exit(0)
                }),
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
        publish_badges(t);
        true
    })
}

/// Hands the file manager badges the accounts' current state.
fn publish_badges(t: &Tray) {
    crate::badges::publish(
        t.accs
            .iter()
            .filter(|a| a.info.signed_in)
            .map(|a| crate::badges::AccState {
                root: a.info.sync_dir.clone(),
                phase: a.status().phase,
                moving: a.moving.clone(),
                blocked: a.blocked.iter().map(|b| b.path.clone()).collect(),
            })
            .collect(),
    );
}

/// When the tray started: log lines from before then were already there, not news.
fn tray_started() -> DateTime<Local> {
    static STARTED: std::sync::OnceLock<DateTime<Local>> = std::sync::OnceLock::new();
    *STARTED.get_or_init(Local::now)
}

fn apply_one(a: &mut Acc, when: DateTime<Local>, ev: LogEvent) {
    core::track_blocked(&mut a.blocked, &ev);
    match &ev {
        LogEvent::Progress { path, done, .. } if !done => {
            a.moving.insert(path.clone());
        }
        LogEvent::Progress { path, .. } | LogEvent::File { path, .. } => {
            a.moving.remove(path);
        }
        // a finished run or a new one: nothing is half-moved any more
        LogEvent::Complete | LogEvent::ClientStart | LogEvent::Error(_) => a.moving.clear(),
        _ => {}
    }
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
            a.told_big_delete = false;
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
        LogEvent::BigDelete => a.log_phase = Phase::BigDelete,
        // a new run checks again; until it says otherwise, the old big delete no longer applies
        LogEvent::ClientStart if a.log_phase == Phase::BigDelete => a.log_phase = Phase::Unknown,
        LogEvent::Blocked { .. } | LogEvent::ClientStart | LogEvent::Progress { .. } => {}
        LogEvent::File { .. } => {}
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
                // the popup reads progress itself; here it would only redraw the menu
                && !matches!(ev, LogEvent::Progress { .. })
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

fn set_device_held(confdir: &std::path::Path, held: bool) {
    let mut s = AppSettings::load();
    s.prefs_mut(confdir).device_held = held;
    let _ = s.save();
}

/// Every few seconds: re-read accounts and settings, poll systemd, and apply schedules.
fn poll(handle: ksni::blocking::Handle<Tray>) {
    let mut following: HashSet<String> = HashSet::new();
    let mut last_phase: HashMap<String, Phase> = HashMap::new();
    tray_started();
    let mut replaced_seen = false;
    let mut round = 0u32;
    // first look for a new version a minute in (not in the rush of logging in), then daily
    let mut next_update_check = Instant::now() + Duration::from_secs(60);
    // storage warnings: a few minutes in, then every six hours
    let mut next_quota_check = Instant::now() + Duration::from_secs(180);
    loop {
        if UPDATE_CHECK_FAILED.swap(false, Ordering::Relaxed) {
            // offline at login, or GitHub busy: try again within the hour
            next_update_check = Instant::now() + Duration::from_secs(3600);
        }
        if Instant::now() >= next_update_check {
            next_update_check = Instant::now() + Duration::from_secs(24 * 3600);
            if AppSettings::load().check_updates.unwrap_or(true) {
                let h = handle.clone();
                thread::spawn(move || check_for_update(&h));
            }
        }
        if Instant::now() >= next_quota_check {
            next_quota_check = Instant::now() + Duration::from_secs(6 * 3600);
            thread::spawn(check_storage);
        }
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
            let hold = core::device_hold();
            let managed_by_time = prefs.schedule.enabled
                || prefs.pause_until > 0
                || hold.is_some()
                || prefs.device_held;
            if !managed_by_time
                || !st.enabled
                || *resyncing
                || !info.signed_in
                || core::account_busy(&info.confdir)
                || core::unit_held(unit)
            {
                continue;
            }
            let allowed = prefs.sync_allowed();
            if allowed && !st.active && !st.failed {
                expired_pause |= prefs.pause_until > 0 && !prefs.paused();
                if prefs.device_held {
                    set_device_held(&info.confdir, false);
                }
                core::run_detached("systemctl", &["--user", "start", unit]);
            } else if !allowed && st.active {
                // remember a stop for the network or battery, so it restarts when that's over
                if hold.is_some() && !prefs.device_held {
                    set_device_held(&info.confdir, true);
                }
                core::run_detached("systemctl", &["--user", "stop", unit]);
            } else if prefs.device_held && hold.is_none() && !st.active && st.failed {
                set_device_held(&info.confdir, false);
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
            t.stop_on_quit = settings.stop_sync_on_quit;
            // switched off since one was found: stop offering it
            if !settings.check_updates.unwrap_or(true) {
                t.update = None;
            }
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
        let phases: Vec<(String, String, Phase, Option<String>, bool)> = handle
            .update(|t: &mut Tray| {
                t.accs
                    .iter()
                    .map(|a| {
                        (
                            a.info.key(),
                            a.info.name.clone(),
                            a.status().phase,
                            a.info.unit.clone(),
                            a.unit.retrying,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (key, name, phase, unit, retrying) in &phases {
            if *phase == Phase::BigDelete
                && *retrying
                && let Some(u) = unit
            {
                // each restart would stop again (and the client may notify each time); only
                // while it waits to restart, as a run that just started may have been fixed
                core::hold_for_big_delete(u);
            }
            let before = last_phase.insert(key.clone(), *phase);
            let bad = |p: Phase| {
                matches!(
                    p,
                    Phase::Error | Phase::NeedsResync | Phase::SignedOut | Phase::BigDelete
                )
            };
            // A big delete is told about once until a sync finishes (the log decides, so a
            // quick fix-and-repeat between two polls isn't missed); "Sync now" while it's still
            // there stops again without telling again.
            let big_delete_news = *phase == Phase::BigDelete
                && !handle
                    .update(|t: &mut Tray| {
                        let a = t.accs.iter_mut().find(|a| a.info.key() == *key)?;
                        Some(std::mem::replace(&mut a.told_big_delete, true))
                    })
                    .flatten()
                    .unwrap_or(true);
            let newly_bad =
                *phase != Phase::BigDelete && before.is_some_and(|b| bad(*phase) && !bad(b));
            if big_delete_news || newly_bad {
                let body = match phase {
                    Phase::BigDelete => {
                        "Many files were deleted on this computer, so syncing stopped before deleting them online. Click the OneDrive icon to choose what to do."
                    }
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
        // pauses, schedules and stopped services change badges without any log line
        handle.update(|t: &mut Tray| publish_badges(t));
        thread::sleep(Duration::from_secs(3));
    }
}

static UPDATE_CHECK_FAILED: AtomicBool = AtomicBool::new(false);

/// Warns once when an account's storage is nearly full (90%) and again when it's full, as the
/// Windows app does. Full storage stops uploads.
fn check_storage() {
    let settings = AppSettings::load();
    for info in core::discover(&settings) {
        if !info.signed_in {
            continue;
        }
        let Ok(tok) = crate::graph::token(&info.confdir) else {
            continue;
        };
        let Ok(q) = crate::graph::quota(&tok, info.drive_id.as_deref()) else {
            continue;
        };
        let warned = settings.prefs(&info.confdir).quota_warned;
        let Some(level) = core::quota_warning(q.used, q.total, warned) else {
            continue;
        };
        let mut s = AppSettings::load();
        s.prefs_mut(&info.confdir).quota_warned = level;
        let _ = s.save();
        let amount = format!(
            "{} of {}",
            crate::graph::human_size(q.used),
            crate::graph::human_size(q.total)
        );
        let body = match level {
            100 => format!(
                "{} is full ({amount}). New and changed files won't upload until you free up space or get more storage.",
                info.name
            ),
            90 => format!(
                "{} is almost full ({amount}). Free up space or get more storage before uploads stop.",
                info.name
            ),
            _ => continue,
        };
        share::notify(&format!("OneDrive: {}", info.name), &body);
    }
}

/// Looks for a newer version; shows it in the menu, and tells the user once per version.
fn check_for_update(handle: &ksni::blocking::Handle<Tray>) {
    let latest = match crate::update::latest_version() {
        Ok(Some(v)) => v,
        // nothing to offer: look again tomorrow, as usual
        Ok(None) => return,
        Err(()) => {
            UPDATE_CHECK_FAILED.store(true, Ordering::Relaxed);
            return;
        }
    };
    if !crate::update::is_newer(&latest, env!("CARGO_PKG_VERSION")) {
        return;
    }
    let v = latest;
    handle.update(|t: &mut Tray| t.update = Some(v.clone()));
    let mut s = AppSettings::load();
    if s.update_notified.as_deref() != Some(v.as_str()) {
        s.update_notified = Some(v.clone());
        let _ = s.save();
        share::notify(
            "OneDrive: a new version is out",
            &format!(
                "Version {v} of this app is available. Click the OneDrive tray icon (or right-click it) to get it."
            ),
        );
    }
}

/// Set on the tray we start after an update, so it waits for this one to leave.
pub const RESTART_ENV: &str = "DRIVEDECK_TRAY_RESTART";

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
    // if an interrupted preview can't be put right, keep the tray (so the user still sees what's
    // going on) but start nothing: recovery keeps that account marked busy meanwhile
    let recovered = crate::ui::recover_interrupted_preview();
    if let Err(e) = &recovered {
        share::notify(
            "Preview recovery needs attention",
            &format!(
                "Sync services weren't started because preview settings couldn't be put back: {e}"
            ),
        );
    }
    // an AppImage update doesn't touch the Dolphin actions it once added for this user
    share::refresh_service_menu();
    share::refresh_user_icons();
    core::repair_managed_units();
    if recovered.is_ok() {
        core::start_idle_units(&AppSettings::load());
    }
    // At login we can start before the panel is ready, and on GNOME there may be no tray at
    // all. Keep running either way: the icon appears whenever a tray shows up, and schedules
    // and pauses are enforced meanwhile.
    let handle = match (Tray {
        accs: Vec::new(),
        client_problem: None,
        stop_on_quit: AppSettings::load().stop_sync_on_quit,
        update: None,
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
    crate::badges::serve();
    crate::badges::install_user_files();
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
    fn big_delete_phase() {
        let mut a = acc();
        let now = tray_started();
        apply_one(&mut a, now, LogEvent::Syncing);
        apply_one(&mut a, now, LogEvent::BigDelete);
        assert_eq!(a.log_phase, Phase::BigDelete);
        // a new run: no longer known until it says
        apply_one(&mut a, now, LogEvent::ClientStart);
        assert_eq!(a.log_phase, Phase::Unknown);
        // a client start doesn't touch other phases
        apply_one(&mut a, now, LogEvent::Complete);
        apply_one(&mut a, now, LogEvent::ClientStart);
        assert_eq!(a.log_phase, Phase::UpToDate);
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
