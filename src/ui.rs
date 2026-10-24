//! The settings window: accounts, sign-in, folders, options, activity and the resync preview.
//!
//! All slow work (systemctl, the client, Microsoft Graph) runs on worker threads via [`bg`];
//! results come back to the UI thread, where the shared [`Ctx`] lives.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::advanced::{self, Effect, Kind};
use crate::clientinstall;
use crate::core::{self, AccountInfo, AppSettings, ClientConfig, LogEvent, Phase, Status};
use crate::graph;

slint::include_modules!();

const INTERVALS: [u32; 6] = [300, 600, 900, 1800, 3600, 7200];
const RATES_MB: [u64; 7] = [0, 1, 2, 5, 10, 25, 50];
const DEFAULT_SKIP_FILE: &str = "~*|.~*|*.tmp|*.swp|*.partial";

// ---------------------------------------------------------------- shared state

#[derive(Default)]
struct Tree {
    rows: Vec<Node>,
    cache: HashMap<String, Vec<graph::Folder>>,
    selected: BTreeSet<String>,
    original: BTreeSet<String>,
    extra_rules: Vec<String>,
    sync_all_original: bool,
    root_files_original: bool,
    loaded_for: Option<String>,
}

#[derive(Clone)]
struct Node {
    path: String,
    name: String,
    depth: usize,
    expanded: bool,
    has_children: bool,
    loading: bool,
}

impl Tree {
    fn covered(&self, path: &str) -> bool {
        self.selected
            .iter()
            .any(|s| path == s || path.starts_with(&format!("{s}/")))
    }
    fn inherited(&self, path: &str) -> bool {
        self.selected
            .iter()
            .any(|s| path.starts_with(&format!("{s}/")))
    }
}

struct SignIn {
    key: String,
    child: Child,
    stdin: Option<ChildStdin>,
    out: Arc<Mutex<String>>,
    readers: Vec<std::thread::JoinHandle<()>>,
    /// polls since the client exited, while the last of its output comes in
    exited_polls: u32,
    url: Option<String>,
    url_file: Option<PathBuf>,
    enable_after: bool,
    was_active: bool,
    finished: bool,
    _busy: core::BusyMarker,
}

struct Preview {
    key: String,
    title: String,
    unit: Option<String>,
    was_active: bool,
    snapshot: Vec<(PathBuf, Option<Vec<u8>>)>,
    out: Arc<Mutex<String>>,
    cancelled: Arc<AtomicBool>,
    running: bool,
    folders_changed: bool,
    mode: PreviewMode,
    note: String,
    _busy: core::BusyMarker,
}

pub(crate) struct Snapshot {
    pub(crate) settings: AppSettings,
    pub(crate) infos: Vec<AccountInfo>,
    pub(crate) statuses: Vec<Status>,
}

#[derive(Clone)]
struct CachedToken {
    access: String,
    expires: Instant,
    graph: String,
}

struct Ctx {
    win: slint::Weak<AppWindow>,
    settings: AppSettings,
    accounts: Vec<AccountInfo>,
    statuses: Vec<Status>,
    current: Option<String>, // config dir of the open account
    tree: Tree,
    tokens: HashMap<String, CachedToken>,
    activity: Vec<String>,
    libraries: Vec<graph::Library>,
    signin: Option<SignIn>,
    preview: Option<Preview>,
    /// What the client's installer has printed so far, while it runs.
    install_out: Option<Arc<Mutex<String>>>,
    /// how much of `install_out` has been looked at for progress steps
    install_seen: usize,
    /// "Not now" on the offer to update the client: don't offer again while the window is open.
    update_dismissed: bool,
    refreshing: bool,
    /// asked for again while one was running, so that one may be out of date
    refresh_again: bool,
    /// The user closed the window while a dry run was still stopping; close once it has.
    close_when_idle: bool,
    rows: Rc<VecModel<FolderRow>>,
    /// The folder backup waiting for the user to confirm it
    backup_ask: Option<BackupAsk>,
    /// Conflicting copies found in the open account's folder
    conflicts: Vec<crate::conflict::Conflict>,
}

enum BackupAsk {
    Start(AccountInfo, crate::backup::Plan),
    Stop(AccountInfo, String),
}

type Shared = Rc<RefCell<Ctx>>;

thread_local! {
    static CTX: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

fn with_ctx(f: impl FnOnce(&Shared)) {
    let ctx = CTX.with(|c| c.borrow().clone());
    if let Some(c) = ctx {
        f(&c);
    }
}

/// Runs `work` on a worker thread, then `done` back on the UI thread.
fn bg<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&Shared, T) + Send + 'static,
) {
    std::thread::spawn(move || {
        let r = work();
        let _ = slint::invoke_from_event_loop(move || with_ctx(|c| done(c, r)));
    });
}

impl Ctx {
    fn ui(&self) -> AppWindow {
        self.win.upgrade().expect("window gone")
    }
    fn account(&self, key: &str) -> Option<&AccountInfo> {
        self.accounts.iter().find(|a| a.key() == key)
    }
    fn current_account(&self) -> Option<AccountInfo> {
        self.current
            .as_deref()
            .and_then(|k| self.account(k))
            .cloned()
    }
    fn token(&self, key: &str) -> Option<CachedToken> {
        self.tokens
            .get(key)
            .filter(|t| t.expires > Instant::now())
            .cloned()
    }
}

/// The open account. A function rather than `ctx.borrow().current_account()` inline so the borrow
/// ends here: inside `if let` a temporary borrow would otherwise last for the whole block.
fn current(ctx: &Shared) -> Option<AccountInfo> {
    ctx.borrow().current_account()
}

fn token_for(confdir: &Path, cached: Option<CachedToken>) -> Result<CachedToken, String> {
    if let Some(t) = cached {
        return Ok(t);
    }
    graph::token(confdir).map(|t| CachedToken {
        access: t.access,
        expires: t.expires,
        graph: t.graph,
    })
}

fn as_graph(t: &CachedToken) -> graph::Token {
    graph::Token {
        access: t.access.clone(),
        expires: t.expires,
        graph: t.graph.clone(),
    }
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

// ---------------------------------------------------------------- status refresh

struct LogState {
    phase: Phase,
    last_complete: Option<chrono::DateTime<Local>>,
    last_error: Option<String>,
    blocked: Vec<core::Blocked>,
    big_delete: Option<core::BigDelete>,
}

fn log_state(unit: Option<&str>) -> LogState {
    let Some(unit) = unit else {
        return LogState {
            phase: Phase::Unknown,
            last_complete: None,
            last_error: None,
            blocked: Vec::new(),
            big_delete: None,
        };
    };
    let (mut phase, mut last, mut err) = (Phase::Unknown, None, None);
    for line in core::journal_status_lines(unit) {
        let Some((when, msg)) = core::split_journal_line(&line) else {
            continue;
        };
        match core::parse_log(msg) {
            Some(LogEvent::Syncing) => phase = Phase::Syncing,
            Some(LogEvent::Complete) => {
                phase = Phase::UpToDate;
                last = Some(when);
                err = None;
            }
            Some(LogEvent::Error(e)) => {
                phase = Phase::Error;
                err = Some(e);
            }
            Some(LogEvent::NeedsResync) => phase = Phase::NeedsResync,
            Some(LogEvent::BigDelete) => phase = Phase::BigDelete,
            // a new run checks again; until it says otherwise, the old big delete no longer applies
            Some(LogEvent::ClientStart) if phase == Phase::BigDelete => phase = Phase::Unknown,
            Some(LogEvent::NeedsLogin) => {
                phase = Phase::SignedOut;
                err = Some("Microsoft needs you to sign in again.".into());
            }
            _ => {}
        }
    }
    LogState {
        phase,
        last_complete: last,
        last_error: err,
        blocked: core::blocked_files(unit),
        big_delete: (phase == Phase::BigDelete).then(|| core::big_delete(unit)),
    }
}

pub(crate) fn take_snapshot() -> Snapshot {
    // Refresh desktop power and network state here, off the UI event loop. Status text is then
    // rendered from the cached answer without waiting on D-Bus.
    let _ = core::device_hold();
    let settings = AppSettings::load();
    let (infos, mut unit_states) = core::discover_with_states(&settings);
    let statuses = infos
        .iter()
        .map(|i| {
            let unit = i
                .unit
                .as_deref()
                .map(|u| core::cached_unit_state(&mut unit_states, u, core::unit_state))
                .unwrap_or_default();
            let resync_unit = core::resync_unit(&i.confdir);
            let resyncing =
                core::cached_unit_state(&mut unit_states, &resync_unit, core::unit_state).active;
            let log = log_state(i.unit.as_deref());
            let prefs = settings.prefs(&i.confdir);
            Status {
                phase: core::compose_phase(i, &prefs, &unit, resyncing, log.phase),
                last_complete: log.last_complete,
                last_error: log.last_error,
                blocked: log.blocked,
                big_delete: log.big_delete,
                unit,
            }
        })
        .collect();
    Snapshot {
        settings,
        infos,
        statuses,
    }
}

pub(crate) fn tone(s: &Status) -> i32 {
    match s.phase {
        // the account is fine, but a file the user changed isn't online
        Phase::UpToDate if s.unsynced_changes() > 0 => 2,
        Phase::UpToDate => 0,
        Phase::Syncing | Phase::Resyncing | Phase::Unknown => 1,
        Phase::Error | Phase::NeedsResync | Phase::SignedOut | Phase::BigDelete => 2,
        Phase::Off | Phase::Stopped | Phase::Paused => 3,
    }
}

fn refresh(ctx: &Shared) {
    // demo screenshots fill the window with made-up data that live refreshes would overwrite
    #[cfg(feature = "test-driver")]
    if std::env::var_os("LODG_DEMO").is_some() {
        return;
    }
    let mut c = ctx.borrow_mut();
    if c.refreshing {
        // what's being read now may already be out of date (an account was just added, say)
        c.refresh_again = true;
        return;
    }
    c.refreshing = true;
    drop(c);
    read_and_apply_snapshot();
}

/// The timer's regular refresh. Unlike a refresh after a change, it doesn't ask for the one
/// being read to be read again: on a slow computer every read would be thrown away, and the
/// window would stop updating.
fn refresh_tick(ctx: &Shared) {
    if !ctx.borrow().refreshing {
        refresh(ctx);
    }
}

fn read_and_apply_snapshot() {
    bg(take_snapshot, |ctx, snap| {
        if std::mem::take(&mut ctx.borrow_mut().refresh_again) {
            // something changed while this was being read: read again rather than show it
            read_and_apply_snapshot();
            return;
        }
        ctx.borrow_mut().refreshing = false;
        apply_snapshot(ctx, snap);
    });
}

// ---------------------------------------------------------------- look

/// What the window's look depends on, read off the desktop.
#[derive(Clone, Copy, Debug, Default)]
struct Look {
    appearance: crate::dbus::Appearance,
    /// the desktop blurs what's behind see-through windows
    blurs: bool,
}

static LOOK: Mutex<Look> = Mutex::new(Look {
    appearance: crate::dbus::Appearance {
        dark: None,
        high_contrast: false,
        reduce_motion: false,
    },
    blurs: false,
});
static GLASS: AtomicBool = AtomicBool::new(false);
static BLUR_SENT: AtomicBool = AtomicBool::new(false);
/// the desktop has told us of a change since the window opened (see read_look)
static APPEARANCE_CHANGED: AtomicBool = AtomicBool::new(false);

/// Call before the window hides. On Wayland hiding destroys the window and showing makes a new
/// one, so its blur is dropped now and asked for again once the new one exists.
fn window_hiding() {
    crate::glass::forget();
    BLUR_SENT.store(false, Ordering::Relaxed);
}

/// Reads the desktop's settings, waiting at most `wait` (the window is opening meanwhile). An
/// answer that comes later (the desktop's settings service can be slow to start) is applied
/// to `ui` when it arrives.
fn read_look(ui: &AppWindow, wait: Duration) -> Look {
    // None while waiting; Some(None) once the wait is over and the answer should go to the window
    let shared = Arc::new((Mutex::new(None::<Option<Look>>), std::sync::Condvar::new()));
    let theirs = shared.clone();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let look = Look {
            appearance: crate::dbus::appearance(),
            blurs: crate::glass::desktop_blurs(),
        };
        let (slot, ready) = &*theirs;
        let mut slot = slot.lock().unwrap();
        if slot.is_some() {
            drop(slot);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    // a change the desktop reported meanwhile is newer than this answer
                    let look = if APPEARANCE_CHANGED.load(Ordering::Relaxed) {
                        Look {
                            blurs: look.blurs,
                            ..*LOOK.lock().unwrap()
                        }
                    } else {
                        look
                    };
                    apply_look(&ui, AppSettings::load().glass, look);
                }
            });
        } else {
            *slot = Some(Some(look));
            ready.notify_one();
        }
    });
    let (slot, ready) = &*shared;
    let (mut slot, _) = ready
        .wait_timeout_while(slot.lock().unwrap(), wait, |s| s.is_none())
        .unwrap();
    // takes the answer, or marks the wait over so a late one goes to the window
    slot.replace(None).flatten().unwrap_or_default()
}

/// Whether the window is see-through: as set, or by default wherever the desktop blurs what's
/// behind it and hasn't asked for high contrast. Without the blur, glass just looks washed out.
fn glass_wanted(setting: Option<bool>, look: Look) -> bool {
    setting.unwrap_or(look.blurs && !look.appearance.high_contrast)
}

fn glass_note(setting: Option<bool>, look: Look) -> &'static str {
    match setting {
        None if glass_wanted(None, look) => {
            "Glass is on: your desktop blurs what's behind windows, so the window is see-through."
        }
        None if look.appearance.high_contrast => {
            "Glass is off because your desktop asks for high contrast."
        }
        None => {
            "Glass is off: your desktop doesn't blur what's behind windows, so the window stays solid and easy to read. If yours does (Hyprland with blur switched on, for example), choose On."
        }
        Some(true) => {
            "Always see-through. If the window looks washed out, your desktop doesn't blur behind it: choose Automatic."
        }
        Some(false) => "Always solid.",
    }
}

fn apply_look(ui: &AppWindow, setting: Option<bool>, look: Look) {
    *LOOK.lock().unwrap() = look;
    if let Some(dark) = look.appearance.dark {
        ui.set_dark(dark);
    }
    let theme = ui.global::<Theme>();
    theme.set_contrast(look.appearance.high_contrast);
    theme.set_reduce_motion(look.appearance.reduce_motion);
    let glass = glass_wanted(setting, look);
    theme.set_glass(glass);
    GLASS.store(glass, Ordering::Relaxed);
    {
        use slint::winit_030::WinitWindowAccessor;
        ui.window()
            .with_winit_window(|w| crate::glass::set(w, glass));
    }
    ui.set_app_glass(match setting {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    });
    ui.set_app_glass_note(ss(glass_note(setting, look)));
}

/// The letter on an account's badge: the first letter or digit of its name, as a capital.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

fn apply_snapshot(ctx: &Shared, snap: Snapshot) {
    // a service that keeps restarting into big-delete protection is stopped while it waits to
    // restart (the tray does this too, but it may not be running). A run that just started is
    // left alone: its log may not have caught up yet, and the user may have fixed things.
    for (a, s) in snap.infos.iter().zip(&snap.statuses) {
        if s.phase == Phase::BigDelete
            && s.unit.retrying
            && let Some(u) = &a.unit
        {
            core::hold_for_big_delete(u);
        }
    }
    let mut c = ctx.borrow_mut();
    c.settings = snap.settings;
    c.accounts = snap.infos;
    c.statuses = snap.statuses;
    let ui = c.ui();
    let rows: Vec<AccountRow> = c
        .accounts
        .iter()
        .zip(&c.statuses)
        .map(|(a, s)| AccountRow {
            name: ss(&a.name),
            initial: ss(initial(&a.name)),
            kind: ss(a.kind_label()),
            status: ss(s.text(&c.settings.prefs(&a.confdir))),
            tone: tone(s),
        })
        .collect();
    ui.set_accounts(ModelRc::new(VecModel::from(rows)));
    // keep the selection on the same account even if the list order changed
    match c.current.clone() {
        Some(k) => match c.accounts.iter().position(|a| a.key() == k) {
            Some(i) => {
                ui.set_current(i as i32);
                show_status(&c, i);
            }
            None => {
                c.current = None;
                ui.set_current(-1);
            }
        },
        None if ui.get_current() >= 0 => ui.set_current(-1),
        None => {}
    }
}

fn show_status(c: &Ctx, i: usize) {
    let (a, s) = (&c.accounts[i], &c.statuses[i]);
    let prefs = c.settings.prefs(&a.confdir);
    let ui = c.ui();
    ui.set_s_kind(ss(a.kind_label()));
    ui.set_s_folder(ss(a.sync_dir.to_string_lossy()));
    ui.set_mv_from(ss(a.sync_dir.to_string_lossy()));
    ui.set_s_personal(a.kind == "personal");
    ui.set_s_status(ss(s.text(&prefs)));
    ui.set_s_tone(tone(s));
    ui.set_s_last(ss(s
        .last_complete
        .map(core::short_time)
        .unwrap_or_else(|| "—".into())));
    let err = match s.phase {
        Phase::Error => s.last_error.clone().unwrap_or_else(|| "The sync service stopped with an error. Try \"Sync now\".".into()),
        Phase::NeedsResync => "The sync engine needs a full resync before it can continue. Open the Folders tab and press \"Apply changes…\", or turn on \"Allow full resyncs\" in Options and save.".into(),
        Phase::SignedOut if a.signed_in => "Microsoft no longer accepts the saved sign-in (this happens after a password change, or when access is revoked). Sign in again to continue.".into(),
        Phase::BigDelete => core::big_delete_text(&s.big_delete.clone().unwrap_or_default()),
        _ => String::new(),
    };
    let blocked = core::blocked_text(&s.blocked);
    let err = match (err.is_empty(), blocked.is_empty()) {
        (_, true) => err,
        (true, false) => blocked,
        (false, false) => format!("{err}\n{blocked}"),
    };
    ui.set_s_error(ss(err));
    ui.set_s_signed_in(a.signed_in && s.phase != Phase::SignedOut);
    ui.set_s_bg_enabled(s.unit.enabled);
    ui.set_s_big_delete(s.phase == Phase::BigDelete);
    ui.set_s_allow_resync(prefs.allow_resync.unwrap_or(true));
}

// ---------------------------------------------------------------- selecting an account

fn select(ctx: &Shared, i: i32) {
    let ui = ctx.borrow().ui();
    ui.set_current(i);
    ui.set_s_msg(ss(""));
    if i < 0 {
        ctx.borrow_mut().current = None;
        if i == -1 {
            prepare_add_page(ctx);
        } else {
            prepare_app_page(ctx);
        }
        return;
    }
    let Some(acc) = ctx.borrow().accounts.get(i as usize).cloned() else {
        return;
    };
    ctx.borrow_mut().current = Some(acc.key());
    ui.set_s_name(ss(&acc.name));
    ui.set_s_quota(ss(""));
    ui.set_s_owner(ss(""));
    show_status(&ctx.borrow(), i as usize);
    load_options(ctx, &acc);
    load_backups(ctx, &acc);
    load_conflicts(ctx, &acc);
    load_folders(ctx, &acc, false);
    load_activity(ctx, &acc);
    load_quota(ctx, &acc);
}

fn load_quota(ctx: &Shared, acc: &AccountInfo) {
    if !acc.signed_in {
        return;
    }
    let (key, confdir, drive) = (acc.key(), acc.confdir.clone(), acc.drive_id.clone());
    let cached = ctx.borrow().token(&key);
    let needs_kind = acc.kind.is_empty();
    bg(
        move || {
            let t = token_for(&confdir, cached)?;
            let q = graph::quota(&as_graph(&t), drive.as_deref())?;
            let kind = if needs_kind {
                graph::account_kind(&as_graph(&t)).ok()
            } else {
                None
            };
            Ok::<_, String>((t, q, kind))
        },
        move |ctx, r| {
            let mut c = ctx.borrow_mut();
            match r {
                Ok((t, q, kind)) => {
                    c.tokens.insert(key.clone(), t);
                    if let Some(k) = kind {
                        let mut s = AppSettings::load();
                        if let Some(a) = c.account(&key) {
                            s.prefs_mut(&a.confdir).kind = Some(k);
                            let _ = s.save();
                        }
                        c.settings = s;
                    }
                    if c.current.as_deref() != Some(key.as_str()) {
                        return;
                    }
                    let ui = c.ui();
                    if q.total > 0 {
                        ui.set_s_quota(ss(format!(
                            "{} of {} used",
                            graph::human_size(q.used),
                            graph::human_size(q.total)
                        )));
                        ui.set_s_quota_frac((q.used as f64 / q.total as f64) as f32);
                    } else if q.used > 0 {
                        // SharePoint libraries often report usage without a total
                        ui.set_s_quota(ss(format!("{} used", graph::human_size(q.used))));
                        ui.set_s_quota_frac(0.0);
                    }
                    ui.set_s_owner(ss(q.owner));
                }
                Err(e) => {
                    if c.current.as_deref() == Some(key.as_str()) {
                        c.ui().set_s_msg(ss(e));
                    }
                }
            }
        },
    );
}

// ---------------------------------------------------------------- options tab

/// The `INTERVALS` entry shown for the account's `monitor_interval`: the nearest one up.
fn interval_choice(cfg: &ClientConfig) -> usize {
    let interval: u32 = cfg
        .get("monitor_interval")
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    INTERVALS
        .iter()
        .position(|v| *v >= interval)
        .unwrap_or(INTERVALS.len() - 1)
}

/// The `RATES_MB` entry shown for the account's `rate_limit`: the nearest one up.
fn rate_choice(cfg: &ClientConfig) -> usize {
    let rate: u64 = cfg
        .get("rate_limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if rate == 0 {
        0
    } else {
        RATES_MB
            .iter()
            .position(|m| *m * 1_048_576 >= rate)
            .unwrap_or(RATES_MB.len() - 1)
    }
}

fn load_options(ctx: &Shared, acc: &AccountInfo) {
    let c = ctx.borrow();
    let ui = c.ui();
    let cfg = ClientConfig::load(&acc.confdir);
    let prefs = c.settings.prefs(&acc.confdir);
    ui.set_o_interval(interval_choice(&cfg) as i32);
    ui.set_o_rate(rate_choice(&cfg) as i32);
    ui.set_o_sched_on(prefs.schedule.enabled);
    ui.set_o_from(ss(if prefs.schedule.from.is_empty() {
        "23:00"
    } else {
        &prefs.schedule.from
    }));
    ui.set_o_to(ss(if prefs.schedule.to.is_empty() {
        "07:00"
    } else {
        &prefs.schedule.to
    }));
    ui.set_o_recycle(cfg.get_bool("use_recycle_bin"));
    ui.set_o_notify(!cfg.get_bool("disable_notifications"));
    ui.set_o_skip_hidden(cfg.get_bool("skip_dotfiles"));
    ui.set_o_skip_files(ss(cfg
        .get("skip_file")
        .unwrap_or_else(|| DEFAULT_SKIP_FILE.into())));
    ui.set_o_allow_resync(prefs.allow_resync.unwrap_or(true));
    ui.set_o_direction(if cfg.get_bool("download_only") {
        1
    } else if cfg.get_bool("upload_only") {
        2
    } else {
        0
    });
    ui.set_o_cleanup(cfg.get_bool("cleanup_local_files"));
    ui.set_o_keep_online(cfg.get_bool("no_remote_delete"));
    ui.set_o_skip_dirs(ss(cfg.get_joined("skip_dir").unwrap_or_default()));
    ui.set_o_skip_symlinks(cfg.get_bool("skip_symlinks"));
    ui.set_o_nosync(cfg.get_bool("check_nosync"));
    // OneDrive for work or school only (not personal, and not a SharePoint library)
    ui.set_o_is_business(acc.kind == "business");
    ui.set_o_shared(cfg.get_bool("sync_business_shared_items"));
    ui.set_o_ipv4(cfg.get("ip_protocol_version").as_deref() == Some("1"));
    ui.set_o_big_delete(ss(advanced::current(&advanced::BIG_DELETE, &cfg)));
    ui.set_o_adv(ModelRc::new(VecModel::from(adv_rows(acc, &cfg))));
    ui.set_o_adv_groups(ModelRc::new(VecModel::from(adv_groups(acc))));
    ui.set_o_adv_tab(0);
    ui.set_o_adv_query(ss(""));
    ui.set_o_adv_any_match(true);
    ui.set_o_msg(ss(""));
}

/// The advanced options shown for an account: those that apply to its kind.
fn adv_specs(acc: &AccountInfo) -> Vec<&'static advanced::Spec> {
    advanced::SPECS
        .iter()
        .filter(|s| advanced::applies(s, &acc.kind))
        .collect()
}

fn adv_rows(acc: &AccountInfo, cfg: &ClientConfig) -> Vec<AdvRow> {
    let specs = adv_specs(acc);
    specs
        .iter()
        .map(|s| {
            let value = advanced::current(s, cfg);
            let (kind, unit, placeholder, mut choices, choice, default_choice) = match &s.kind {
                Kind::Bool => (0, "", "", Vec::new(), 0, 0),
                Kind::Number { unit, .. } => (1, *unit, s.default, Vec::new(), 0, 0),
                Kind::Text { placeholder, .. } => (2, "", *placeholder, Vec::new(), 0, 0),
                Kind::Choice(list) => (
                    3,
                    "",
                    "",
                    list.iter().map(|(_, words)| ss(*words)).collect(),
                    list.iter()
                        .position(|(v, _)| *v == value)
                        .unwrap_or(list.len()),
                    list.iter().position(|(v, _)| *v == s.default).unwrap_or(0),
                ),
            };
            // a value set by hand that isn't one of the choices stays selectable (and unchanged)
            if kind == 3 && choice == choices.len() {
                choices.push(ss(format!("{value} (set by hand)")));
            }
            let is_text = kind == 1 || kind == 2;
            AdvRow {
                group: ss(s.group),
                label: ss(s.label),
                hint: ss(s.hint),
                kind,
                checked: kind == 0 && value == "true",
                text: ss(if is_text { value.as_str() } else { "" }),
                unit: ss(unit),
                placeholder: ss(placeholder),
                choices: ModelRc::new(VecModel::from(choices)),
                choice: choice as i32,
                default_checked: false,
                default_text: ss(if is_text { s.default } else { "" }),
                default_choice: default_choice as i32,
                risky: s.risky,
                effect: match s.effect {
                    Effect::Restart => 0,
                    Effect::Resync => 1,
                    Effect::SignIn => 2,
                },
                needs: match s.needs {
                    None => -1,
                    Some("use_recycle_bin") => -2,
                    Some(k) => specs
                        .iter()
                        .position(|o| o.key == k)
                        .map_or(-1, |i| i as i32),
                },
                matched: true,
                error: ss(""),
            }
        })
        .collect()
}

/// The advanced groups in table order, for the tabs above the settings.
fn adv_groups(acc: &AccountInfo) -> Vec<SharedString> {
    let mut groups: Vec<&str> = Vec::new();
    for s in adv_specs(acc) {
        if !groups.contains(&s.group) {
            groups.push(s.group);
        }
    }
    groups.into_iter().map(ss).collect()
}

/// Marks the advanced settings whose name, explanation, group or config key holds every word
/// typed in the search box.
fn adv_search(ctx: &Shared, query: &str) {
    let Some(acc) = current(ctx) else { return };
    let ui = ctx.borrow().ui();
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let specs = adv_specs(&acc);
    let model = ui.get_o_adv();
    let mut any = false;
    for (i, s) in specs.iter().enumerate() {
        let Some(mut row) = model.row_data(i) else {
            break;
        };
        let hay = format!(
            "{} {} {} {}",
            s.label,
            s.hint,
            s.group,
            s.key.replace('_', " ")
        )
        .to_lowercase();
        let m = words.iter().all(|w| hay.contains(w.as_str()));
        any |= m;
        if row.matched != m {
            row.matched = m;
            model.set_row_data(i, row);
        }
    }
    ui.set_o_adv_any_match(any);
}

/// A changed advanced option, checked and ready to write.
struct AdvChange {
    key: &'static str,
    value: Option<String>,
    effect: Effect,
}

/// Reads and checks the Advanced section. Problems are shown under the rows they're about.
fn adv_collect(
    acc: &AccountInfo,
    ui: &AppWindow,
    cfg: &ClientConfig,
) -> Result<Vec<AdvChange>, ()> {
    let specs = adv_specs(acc);
    let model = ui.get_o_adv();
    if model.row_count() != specs.len() {
        // not loaded for this account (shouldn't happen): change nothing
        return Ok(Vec::new());
    }
    let mut values: HashMap<&'static str, String> = HashMap::new();
    let mut changes = Vec::new();
    let mut errors: Vec<(usize, String)> = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let row = model.row_data(i).unwrap();
        let cur = advanced::current(s, cfg);
        let raw = match &s.kind {
            Kind::Bool => row.checked.to_string(),
            Kind::Number { .. } | Kind::Text { .. } => row.text.trim().to_string(),
            Kind::Choice(list) => list
                .get(row.choice.max(0) as usize)
                .map_or_else(|| cur.clone(), |(v, _)| v.to_string()),
        };
        // an untouched value set by hand is left alone, even one the client would reject
        if raw == cur {
            values.insert(s.key, cur);
            continue;
        }
        match advanced::check(s, &raw) {
            Ok(v) => {
                let v = if s.key == "log_dir" || s.key == "recycle_bin_path" {
                    advanced::expand_home(&v)
                } else {
                    v
                };
                values.insert(s.key, v.clone());
                if let Some(value) = advanced::change(s, cfg, &v) {
                    changes.push(AdvChange {
                        key: s.key,
                        value,
                        effect: s.effect,
                    });
                }
            }
            Err(e) => errors.push((i, e)),
        }
    }
    let on = |k: &str| values.get(k).is_some_and(|v| v == "true");
    let pos = |k: &str| specs.iter().position(|s| s.key == k);
    if on("mirror_local_state") {
        if !on("local_first") {
            errors.extend(pos("mirror_local_state").map(|i| {
                (
                    i,
                    "needs \"Treat this computer as the source of truth\" on too".to_string(),
                )
            }));
        } else if ui.get_o_direction() == 2 {
            errors.extend(
                pos("mirror_local_state")
                    .map(|i| (i, "can't be used with Upload only".to_string())),
            );
        }
    }
    if on("webhook_enabled")
        && values
            .get("webhook_public_url")
            .is_none_or(|v| v.is_empty())
    {
        errors.extend(pos("webhook_public_url").map(|i| (i, "needs an address".to_string())));
    }
    // the client's default log folder (/var/log/onedrive) isn't writable by you: use our own
    if on("enable_logging") && values.get("log_dir").is_none_or(|v| v.is_empty()) {
        let name = acc
            .confdir
            .file_name()
            .map_or_else(|| "onedrive".into(), |n| n.to_string_lossy().to_string());
        let dir = core::data_home().join(core::APP_ID).join("logs").join(name);
        changes.retain(|c| c.key != "log_dir");
        changes.push(AdvChange {
            key: "log_dir",
            value: Some(dir.to_string_lossy().to_string()),
            effect: Effect::Restart,
        });
        if let Some(i) = pos("log_dir")
            && let Some(mut row) = model.row_data(i)
        {
            row.text = ss(dir.to_string_lossy());
            model.set_row_data(i, row);
        }
    }
    for c in &changes {
        if c.key == "log_dir"
            && let Some(d) = &c.value
            && let Err(e) = fs::create_dir_all(d)
        {
            errors.extend(pos("log_dir").map(|i| (i, format!("couldn't create it: {e}"))));
        }
    }
    for i in 0..model.row_count() {
        let mut row = model.row_data(i).unwrap();
        let err = errors
            .iter()
            .filter(|(j, _)| *j == i)
            .map(|(_, e)| e.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if row.error.as_str() != err {
            row.error = ss(err);
            model.set_row_data(i, row);
        }
    }
    if errors.is_empty() {
        Ok(changes)
    } else {
        // show the tab holding the first problem
        ui.set_o_adv_open(true);
        ui.set_o_adv_query(ss(""));
        let first = errors.iter().map(|(i, _)| *i).min().unwrap_or(0);
        if let Some(t) = adv_groups(acc)
            .iter()
            .position(|g| g.as_str() == specs[first].group)
        {
            ui.set_o_adv_tab(t as i32);
        }
        Err(())
    }
}

/// Shows the client's own view of every setting (`--display-config`).
fn show_client_config(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    let ui = ctx.borrow().ui();
    ui.set_cfg_text(ss(""));
    ui.set_cfg_busy(true);
    ui.set_cfg_visible(true);
    bg(
        move || {
            let Some(bin) = core::client_binary() else {
                return "The OneDrive Client for Linux isn't installed.".to_string();
            };
            match Command::new(bin)
                .arg("--confdir")
                .arg(&acc.confdir)
                .arg("--display-config")
                .stdin(Stdio::null())
                .output()
            {
                Ok(o) => {
                    let mut t = String::from_utf8_lossy(&o.stdout).to_string();
                    t.push_str(&String::from_utf8_lossy(&o.stderr));
                    t.trim().to_string()
                }
                Err(e) => format!("Couldn't run the client: {e}"),
            }
        },
        |ctx, text| {
            let ui = ctx.borrow().ui();
            ui.set_cfg_text(ss(text));
            ui.set_cfg_busy(false);
        },
    );
}

/// Opens the folder the client writes its log file to, once logging is on.
fn open_log_folder(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    let ui = ctx.borrow().ui();
    let cfg = ClientConfig::load(&acc.confdir);
    if !cfg.get_bool("enable_logging") {
        ui.set_o_adv_open(true);
        ui.set_o_msg(ss(
            "Turn on \"Write a log file\" under Advanced and save first. Until then the app reads the system journal instead.",
        ));
        return;
    }
    let dir = cfg
        .get("log_dir")
        .filter(|d| !d.trim().is_empty())
        .map_or_else(
            || PathBuf::from("/var/log/onedrive"),
            |d| PathBuf::from(advanced::expand_home(d.trim())),
        );
    if !dir.is_dir() {
        ui.set_o_msg(ss(format!(
            "{} doesn't exist yet. The client creates it when it next writes to the log.",
            dir.display()
        )));
        return;
    }
    core::open_path(&dir);
}

/// Checks the folders to skip, as the client would: `|`-separated names or patterns, none empty,
/// and not `.*` (the client refuses it: it would hide every change).
fn clean_skip_dirs(text: &str) -> Result<Option<String>, String> {
    let parts: Vec<&str> = text.split('|').map(str::trim).collect();
    if parts.iter().all(|p| p.is_empty()) {
        return Ok(None);
    }
    if parts.iter().any(|p| p.is_empty()) {
        return Err(
            "Folders to skip: remove the empty entry (two | in a row, or one at an end).".into(),
        );
    }
    if parts.contains(&".*") {
        return Err("Folders to skip: \".*\" isn't allowed (it would hide all your changes). To skip hidden folders, use \"Don't sync hidden files and folders\".".into());
    }
    Ok(Some(parts.join("|")))
}

fn save_options(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    let ui = ctx.borrow().ui();
    let sched_on = ui.get_o_sched_on();
    let (from, to) = (
        ui.get_o_from().trim().to_string(),
        ui.get_o_to().trim().to_string(),
    );
    let (a, b) = (core::parse_hhmm(&from), core::parse_hhmm(&to));
    if sched_on && (a.is_none() || b.is_none() || a == b) {
        ui.set_o_msg(ss(
            "Schedule times must be two different 24-hour times like 23:00 and 07:00.",
        ));
        return;
    }
    let skip_dirs = match clean_skip_dirs(&ui.get_o_skip_dirs()) {
        Ok(v) => v,
        Err(e) => {
            ui.set_o_msg(ss(e));
            return;
        }
    };
    let cfg = ClientConfig::load(&acc.confdir);
    let big_delete = {
        let raw = ui.get_o_big_delete().trim().to_string();
        let raw = if raw.is_empty() {
            advanced::BIG_DELETE.default.to_string()
        } else {
            raw
        };
        if raw == advanced::current(&advanced::BIG_DELETE, &cfg) {
            None
        } else {
            match advanced::check(&advanced::BIG_DELETE, &raw) {
                Ok(v) => advanced::change(&advanced::BIG_DELETE, &cfg, &v),
                Err(e) => {
                    ui.set_o_msg(ss(format!("The number of items for a big delete {e}.")));
                    return;
                }
            }
        }
    };
    let Ok(adv) = adv_collect(&acc, &ui, &cfg) else {
        ui.set_o_msg(ss(
            "Nothing was saved: fix the settings marked in red under Advanced.",
        ));
        return;
    };

    // our own settings first: they never need the sync engine restarted
    let mut s = AppSettings::load();
    {
        let p = s.prefs_mut(&acc.confdir);
        p.schedule = core::Schedule {
            enabled: sched_on,
            from,
            to,
        };
        p.allow_resync = Some(ui.get_o_allow_resync());
    }
    if let Err(e) = s.save() {
        ui.set_o_msg(ss(format!("Couldn't save settings: {e}")));
        return;
    }
    ctx.borrow_mut().settings = s.clone();
    ui.set_f_allowed(ui.get_o_allow_resync());
    render_tree(ctx);

    let mut plain: Vec<(String, Option<String>)> = Vec::new();
    let mut resync: Vec<(String, Option<String>)> = Vec::new();
    let want = |list: &mut Vec<(String, Option<String>)>, key: &str, val: Option<String>| {
        if cfg.get(key) != val {
            list.push((key.to_string(), val));
        }
    };
    // the menus only offer a few values: leave a hand-set one alone unless its menu was changed
    let choice = ui.get_o_interval().clamp(0, 5) as usize;
    if choice != interval_choice(&cfg) {
        let interval = INTERVALS[choice];
        want(
            &mut plain,
            "monitor_interval",
            (interval != 300)
                .then(|| interval.to_string())
                .or_else(|| cfg.get("monitor_interval").map(|_| "300".into())),
        );
    }
    let choice = ui.get_o_rate().clamp(0, 6) as usize;
    if choice != rate_choice(&cfg) {
        let rate = RATES_MB[choice] * 1_048_576;
        want(
            &mut plain,
            "rate_limit",
            (rate > 0).then(|| rate.to_string()),
        );
    }
    want(
        &mut plain,
        "use_recycle_bin",
        ui.get_o_recycle().then(|| "true".into()),
    );
    want(
        &mut plain,
        "disable_notifications",
        (!ui.get_o_notify()).then(|| "true".into()),
    );
    // off removes only our "IPv4 only"; another value set by hand (2, IPv6 only) is kept
    want(
        &mut plain,
        "ip_protocol_version",
        if ui.get_o_ipv4() {
            Some("1".into())
        } else {
            cfg.get("ip_protocol_version").filter(|v| v != "1")
        },
    );
    let direction = ui.get_o_direction();
    // only meaningful when uploading only; kept off otherwise so it can't surprise later
    want(
        &mut plain,
        "no_remote_delete",
        (direction == 2 && ui.get_o_keep_online()).then(|| "true".into()),
    );
    // which way changes go: checked with a dry run first, as it can delete files here
    let mut direction_changes: Vec<(String, Option<String>)> = Vec::new();
    want(
        &mut direction_changes,
        "download_only",
        (direction == 1).then(|| "true".into()),
    );
    want(
        &mut direction_changes,
        "upload_only",
        (direction == 2).then(|| "true".into()),
    );
    want(
        &mut direction_changes,
        "cleanup_local_files",
        (direction == 1 && ui.get_o_cleanup()).then(|| "true".into()),
    );
    want(
        &mut resync,
        "skip_dotfiles",
        ui.get_o_skip_hidden().then(|| "true".into()),
    );
    if cfg.get_joined("skip_dir") != skip_dirs {
        resync.push(("skip_dir".into(), skip_dirs));
    }
    want(
        &mut resync,
        "skip_symlinks",
        ui.get_o_skip_symlinks().then(|| "true".into()),
    );
    want(
        &mut resync,
        "check_nosync",
        ui.get_o_nosync().then(|| "true".into()),
    );
    if acc.kind == "business" {
        want(
            &mut resync,
            "sync_business_shared_items",
            ui.get_o_shared().then(|| "true".into()),
        );
    }
    if let Some(v) = big_delete {
        plain.push(("classify_as_big_delete".into(), v));
    }
    let signin_changed = adv.iter().any(|c| c.effect == Effect::SignIn);
    for c in adv {
        let list = if c.effect == Effect::Resync {
            &mut resync
        } else {
            &mut plain
        };
        list.push((c.key.to_string(), c.value));
    }
    let skip = ui.get_o_skip_files().trim().to_string();
    want(
        &mut resync,
        "skip_file",
        (!skip.is_empty() && skip != DEFAULT_SKIP_FILE).then_some(skip),
    );
    // treat "absent" and "the default" as the same thing so we don't trigger needless resyncs
    let is_default = |(k, v): &(String, Option<String>)| match k.as_str() {
        "skip_file" => v.is_none() && cfg.get("skip_file").is_none_or(|x| x == DEFAULT_SKIP_FILE),
        "skip_dotfiles"
        | "skip_symlinks"
        | "check_nosync"
        | "sync_business_shared_items"
        | "download_only"
        | "upload_only"
        | "cleanup_local_files" => v.is_none() && !cfg.get_bool(k),
        _ => false,
    };
    resync.retain(|ch| !is_default(ch));
    direction_changes.retain(|ch| !is_default(ch));

    if !plain.is_empty() {
        let mut cfg = ClientConfig::load(&acc.confdir);
        for (k, v) in &plain {
            cfg.set(k, v.as_deref());
        }
        if let Err(e) = cfg.save() {
            ui.set_o_msg(ss(format!("Couldn't save: {e}")));
            return;
        }
        restart_if_running(&acc);
    }
    if resync.is_empty() && direction_changes.is_empty() {
        ui.set_o_msg(ss(if signin_changed {
            "Saved. Sign out and in again for the settings marked † to take effect."
        } else if plain.is_empty() {
            "Saved."
        } else {
            "Saved. The sync engine restarts to pick up the changes."
        }));
        refresh(ctx);
        return;
    }
    if !resync.is_empty() && !ui.get_o_allow_resync() {
        if direction_changes.is_empty() {
            ui.set_o_msg(ss("Saved, except the settings marked * — they need a full resync, which is turned off for this account."));
            return;
        }
        // the direction can still change; the * settings stay as they were
        ui.set_o_msg(ss("The settings marked * weren't saved: they need a full resync, which is turned off for this account. Check the preview for the new direction."));
        resync.clear();
    }
    if resync.is_empty() {
        let mut note = String::new();
        if direction_changes
            .iter()
            .any(|(k, v)| k == "cleanup_local_files" && v.is_some())
        {
            note = "Files here that aren't online are deleted, including new files you put in this folder from now on.".into();
        }
        start_preview(
            ctx,
            &acc,
            "Change sync direction",
            PreviewChange {
                sync_list: None,
                config: direction_changes,
                mode: PreviewMode::Check,
                note,
            },
        );
        return;
    }
    // a full resync's dry run covers a new direction too
    resync.extend(direction_changes);
    ui.set_o_msg(ss(
        "Saved. The settings marked * need a full resync — check the preview.",
    ));
    start_preview(
        ctx,
        &acc,
        "Apply file settings",
        PreviewChange {
            sync_list: None,
            config: resync,
            mode: PreviewMode::Resync,
            note: String::new(),
        },
    );
}

/// After a big delete: let the deletions go ahead online, or get the deleted items back. Both
/// start with a dry run, like any resync.
fn big_delete_choose(ctx: &Shared, delete_online: bool) {
    let Some(acc) = current(ctx) else { return };
    let big = {
        let c = ctx.borrow();
        c.accounts
            .iter()
            .position(|a| a.key() == acc.key())
            .and_then(|i| c.statuses.get(i))
            .filter(|s| s.phase == Phase::BigDelete)
            .and_then(|s| s.big_delete.clone())
    };
    let Some(big) = big else {
        // it was dealt with meanwhile (the status is about to change)
        return;
    };
    if delete_online {
        let mut note = match big.count {
            Some(n) => {
                format!("Counting everything inside the deleted folders, that's {n} items in all.")
            }
            None => String::new(),
        };
        if !ClientConfig::load(&acc.confdir).get_bool("permanent_delete") {
            note.push_str(" Items deleted online go to the OneDrive recycle bin, where they can be restored for a while.");
        }
        start_preview(
            ctx,
            &acc,
            "Delete them online too",
            PreviewChange {
                sync_list: None,
                config: Vec::new(),
                mode: PreviewMode::ForceDelete,
                note: note.trim().to_string(),
            },
        );
    } else {
        start_preview(
            ctx,
            &acc,
            "Download them again",
            PreviewChange {
                sync_list: None,
                config: Vec::new(),
                mode: PreviewMode::Resync,
                note: "The deleted items are downloaded again from OneDrive.".into(),
            },
        );
    }
}

fn restart_if_running(acc: &AccountInfo) {
    if let Some(u) = &acc.unit
        && core::unit_state(u).active
    {
        core::unit_action("restart", u);
    }
}

// ---------------------------------------------------------------- folders tab

fn load_folders(ctx: &Shared, acc: &AccountInfo, force: bool) {
    let key = acc.key();
    if !force && ctx.borrow().tree.loaded_for.as_deref() == Some(key.as_str()) {
        return;
    }
    let rules = match core::read_sync_list(&acc.confdir) {
        Ok(rules) => rules,
        Err(e) => {
            ctx.borrow()
                .ui()
                .set_f_msg(ss(format!("Couldn't read the folder-selection rules: {e}")));
            return;
        }
    };
    {
        let mut c = ctx.borrow_mut();
        let mut t = Tree {
            loaded_for: Some(key.clone()),
            ..Default::default()
        };
        for r in rules {
            match core::sync_list_folder(&r) {
                Some(f) if !r.contains('*') || r.ends_with("/*") => {
                    t.selected.insert(f);
                }
                _ => t.extra_rules.push(r),
            }
        }
        t.original = t.selected.clone();
        t.sync_all_original = t.selected.is_empty();
        t.root_files_original = ClientConfig::load(&acc.confdir).get_bool("sync_root_files");
        c.tree = t;
        let ui = c.ui();
        ui.set_f_sync_all(c.tree.sync_all_original);
        ui.set_f_root_files(c.tree.root_files_original);
        ui.set_f_allowed(c.settings.prefs(&acc.confdir).allow_resync.unwrap_or(true));
    }
    render_tree(ctx);
    if !acc.signed_in {
        ctx.borrow()
            .ui()
            .set_f_msg(ss("Sign in to choose folders."));
        return;
    }
    fetch_children(ctx, acc, "/".into());
}

fn fetch_children(ctx: &Shared, acc: &AccountInfo, path: String) {
    let (key, confdir, drive) = (acc.key(), acc.confdir.clone(), acc.drive_id.clone());
    let cached = ctx.borrow().token(&key);
    ctx.borrow().ui().set_f_loading(true);
    bg(
        {
            let path = path.clone();
            move || {
                let t = token_for(&confdir, cached)?;
                let f = graph::list_folders(&as_graph(&t), drive.as_deref(), &path)?;
                Ok::<_, String>((t, f))
            }
        },
        move |ctx, r| {
            {
                let mut c = ctx.borrow_mut();
                c.ui().set_f_loading(false);
                if c.tree.loaded_for.as_deref() != Some(key.as_str()) {
                    return; // user moved on to another account
                }
                match r {
                    Ok((t, folders)) => {
                        c.tokens.insert(key.clone(), t);
                        c.tree.cache.insert(path.clone(), folders.clone());
                        let tree = &mut c.tree;
                        if path == "/" {
                            tree.rows = folders
                                .iter()
                                .map(|f| Node {
                                    path: format!("/{}", f.name),
                                    name: f.name.clone(),
                                    depth: 0,
                                    expanded: false,
                                    has_children: f.has_children,
                                    loading: false,
                                })
                                .collect();
                        } else if let Some(i) = tree.rows.iter().position(|n| n.path == path) {
                            tree.rows[i].loading = false;
                            tree.rows[i].expanded = true;
                            tree.rows[i].has_children = !folders.is_empty();
                            let depth = tree.rows[i].depth + 1;
                            let kids: Vec<Node> = folders
                                .iter()
                                .map(|f| Node {
                                    path: format!("{path}/{}", f.name),
                                    name: f.name.clone(),
                                    depth,
                                    expanded: false,
                                    has_children: f.has_children,
                                    loading: false,
                                })
                                .collect();
                            tree.rows.splice(i + 1..i + 1, kids);
                        }
                        c.ui().set_f_msg(ss(""));
                    }
                    Err(e) => {
                        if let Some(n) = c.tree.rows.iter_mut().find(|n| n.path == path) {
                            n.loading = false;
                        }
                        c.ui().set_f_msg(ss(format!("Couldn't load folders: {e}")));
                    }
                }
            }
            render_tree(ctx);
        },
    );
}

fn render_tree(ctx: &Shared) {
    let c = ctx.borrow();
    let t = &c.tree;
    let rows: Vec<FolderRow> = t
        .rows
        .iter()
        .map(|n| FolderRow {
            name: ss(&n.name),
            depth: n.depth as i32,
            expanded: n.expanded,
            has_children: n.has_children,
            loading: n.loading,
            checked: t.covered(&n.path),
            inherited: t.inherited(&n.path),
        })
        .collect();
    c.rows.set_vec(rows);
    let ui = c.ui();
    let dirty = folder_changes(&c, &ui).is_some();
    ui.set_f_dirty(dirty);
    let msg = if !ui.get_f_allowed() {
        "Full resyncs are turned off for this account (Options), so folder choices are locked."
    } else if !ui.get_f_sync_all() && t.selected.is_empty() && !t.rows.is_empty() {
        "Tick at least one folder, or choose \"Sync everything\"."
    } else {
        ""
    };
    if !msg.is_empty() || dirty {
        ui.set_f_msg(ss(msg));
    }
}

/// What the Folders tab would write, or None when nothing changed.
fn folder_changes(c: &Ctx, ui: &AppWindow) -> Option<(Vec<String>, Option<bool>)> {
    let t = &c.tree;
    let sync_all = ui.get_f_sync_all();
    let root_files = ui.get_f_root_files();
    let list_changed = sync_all != t.sync_all_original || (!sync_all && t.selected != t.original);
    let root_changed = !sync_all && root_files != t.root_files_original;
    if !list_changed && !root_changed {
        return None;
    }
    if !sync_all && t.selected.is_empty() {
        return None;
    }
    // "Everything" means no sync_list at all: the client excludes everything a sync_list doesn't
    // include, so the user's own rules can't stay on their own (they're saved aside instead).
    if sync_all {
        return Some((Vec::new(), root_changed.then_some(root_files)));
    }
    // the client wants exclusions before inclusions
    let (exclusions, other): (Vec<&String>, Vec<&String>) = t
        .extra_rules
        .iter()
        .partition(|r| r.starts_with('!') || r.starts_with('-'));
    let mut lines: Vec<String> = exclusions.into_iter().cloned().collect();
    lines.extend(t.selected.iter().cloned());
    lines.extend(other.into_iter().cloned());
    Some((lines, root_changed.then_some(root_files)))
}

fn folder_expand(ctx: &Shared, i: usize) {
    let Some(acc) = current(ctx) else { return };
    let action = {
        let mut c = ctx.borrow_mut();
        let Some(n) = c.tree.rows.get(i).cloned() else {
            return;
        };
        if n.expanded {
            // collapse: drop everything deeper that follows
            let end = c.tree.rows[i + 1..]
                .iter()
                .position(|r| r.depth <= n.depth)
                .map(|p| i + 1 + p)
                .unwrap_or(c.tree.rows.len());
            c.tree.rows.drain(i + 1..end);
            c.tree.rows[i].expanded = false;
            None
        } else if let Some(kids) = c.tree.cache.get(&n.path).cloned() {
            let nodes: Vec<Node> = kids
                .iter()
                .map(|f| Node {
                    path: format!("{}/{}", n.path, f.name),
                    name: f.name.clone(),
                    depth: n.depth + 1,
                    expanded: false,
                    has_children: f.has_children,
                    loading: false,
                })
                .collect();
            c.tree.rows.splice(i + 1..i + 1, nodes);
            c.tree.rows[i].expanded = true;
            None
        } else {
            c.tree.rows[i].loading = true;
            Some(n.path)
        }
    };
    render_tree(ctx);
    if let Some(p) = action {
        fetch_children(ctx, &acc, p);
    }
}

fn folder_check(ctx: &Shared, i: usize) {
    {
        let mut c = ctx.borrow_mut();
        let Some(path) = c.tree.rows.get(i).map(|n| n.path.clone()) else {
            return;
        };
        if c.tree.inherited(&path) {
            return;
        }
        if !c.tree.selected.remove(&path) {
            // selecting a folder makes any ticked sub-folders redundant
            let prefix = format!("{path}/");
            c.tree.selected.retain(|s| !s.starts_with(&prefix));
            c.tree.selected.insert(path);
        }
    }
    render_tree(ctx);
}

fn folders_apply(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    let ui = ctx.borrow().ui();
    let Some((lines, root_files)) = folder_changes(&ctx.borrow(), &ui) else {
        return;
    };
    let config = root_files
        .map(|r| vec![("sync_root_files".to_string(), r.then(|| "true".to_string()))])
        .unwrap_or_default();
    start_preview(
        ctx,
        &acc,
        "Change synced folders",
        PreviewChange {
            sync_list: Some(lines),
            config,
            mode: PreviewMode::Resync,
            note: String::new(),
        },
    );
}

// ---------------------------------------------------------------- resync preview

struct PreviewChange {
    sync_list: Option<Vec<String>>,
    config: Vec<(String, Option<String>)>,
    mode: PreviewMode,
    /// Added to the dry run's summary
    note: String,
}

#[derive(Clone, Copy, PartialEq)]
enum PreviewMode {
    /// A full resync (changed settings or folders, or getting deleted items back)
    Resync,
    /// Letting a big delete go ahead online
    ForceDelete,
    /// An ordinary sync with changed settings (the direction)
    Check,
}

impl PreviewMode {
    fn client_args(self) -> &'static [&'static str] {
        match self {
            PreviewMode::Resync => &["--sync", "--resync", "--resync-auth"],
            PreviewMode::ForceDelete => &["--sync", "--force"],
            PreviewMode::Check => &["--sync"],
        }
    }
}

const SNAPSHOT_FILES: [&str; 5] = [
    "config",
    "sync_list",
    ".config.hash",
    ".sync_list.hash",
    ".config.backup",
];

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Starts `cmd`, appending everything it prints (stdout and stderr) to `out` as it arrives.
/// Reads raw chunks rather than lines so prompts without a newline show up too. The reader
/// threads come back as well: a caller that reads `out` once the program has ended lets them
/// finish first (`finish_reading`), or the last lines, often the error, may not be in yet.
fn spawn_capture(
    mut cmd: Command,
    out: Arc<Mutex<String>>,
    with_stdin: bool,
) -> std::io::Result<(Child, Option<ChildStdin>, Vec<std::thread::JoinHandle<()>>)> {
    let stdin = if with_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = cmd
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    let mut readers = Vec::new();
    for mut pipe in pipes.into_iter().flatten() {
        let out = out.clone();
        readers.push(std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }));
    }
    let stdin = child.stdin.take();
    Ok((child, stdin, readers))
}

/// Gives the reader threads up to two seconds to take in the last of the output. Not longer: a
/// program the command left running in the background can hold the pipes open for good.
fn finish_reading(readers: Vec<std::thread::JoinHandle<()>>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    for r in readers {
        while !r.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if r.is_finished() {
            let _ = r.join();
        }
    }
}

fn start_preview(ctx: &Shared, acc: &AccountInfo, title: &str, change: PreviewChange) {
    if ctx.borrow().preview.is_some() {
        return;
    }
    let unit = acc.unit.clone();
    let was_active = unit.as_deref().is_some_and(|u| core::unit_state(u).active);
    let out = Arc::new(Mutex::new(String::new()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let confdir = acc.confdir.clone();
    let busy = match core::BusyMarker::try_new(&confdir) {
        Ok(marker) => marker,
        Err(e) => {
            ctx.borrow().ui().set_s_msg(ss(format!(
                "Couldn't protect this account from a second sync process, so the preview was not started: {e}"
            )));
            return;
        }
    };
    let mut snapshot = Vec::with_capacity(SNAPSHOT_FILES.len());
    for name in SNAPSHOT_FILES {
        let path = confdir.join(name);
        let data = match fs::read(&path) {
            Ok(data) => Some(data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                ctx.borrow().ui().set_s_msg(ss(format!(
                    "Couldn't safely prepare the preview because {} couldn't be read: {e}",
                    path.display()
                )));
                return;
            }
        };
        snapshot.push((path, data));
    }
    {
        let ui = ctx.borrow().ui();
        ui.set_p_title(ss(title));
        ui.set_p_text(ss(match change.mode {
            PreviewMode::Resync => "Checking what a full resync would do. Nothing is changed while this runs — it's a dry run. Large accounts can take several minutes.",
            PreviewMode::ForceDelete => "Checking what syncing with the deletions would do. Nothing is changed while this runs — it's a dry run.",
            PreviewMode::Check => "Checking what syncing with the new settings would do. Nothing is changed while this runs — it's a dry run.",
        }));
        ui.set_p_warn(ss(""));
        ui.set_p_live(ss(if was_active {
            "Pausing background sync…"
        } else {
            "Starting…"
        }));
        ui.set_p_busy(true);
        ui.set_p_can_apply(false);
        ui.set_p_visible(true);
    }
    ctx.borrow_mut().preview = Some(Preview {
        key: acc.key(),
        title: title.into(),
        unit: unit.clone(),
        was_active,
        snapshot,
        out: out.clone(),
        cancelled: cancelled.clone(),
        running: true,
        folders_changed: change.sync_list.is_some(),
        mode: change.mode,
        note: change.note.clone(),
        _busy: busy,
    });
    let save_result = ctx
        .borrow()
        .preview
        .as_ref()
        .map(save_pending_preview)
        .unwrap_or_else(|| Err("Preview state disappeared before it could be saved.".into()));
    if let Err(e) = save_result {
        ctx.borrow_mut().preview.take();
        let ui = ctx.borrow().ui();
        ui.set_p_visible(false);
        ui.set_p_busy(false);
        ui.set_s_msg(ss(format!(
            "Couldn't safely start the preview because its recovery record couldn't be saved: {e}"
        )));
        return;
    }
    bg(
        move || -> Result<bool, String> {
            if let Some(u) = &unit {
                let (ok, err) = core::systemctl(&["stop", u]);
                if !ok {
                    return Err(format!(
                        "Couldn't pause the sync engine, so no preview was run: {}",
                        err.trim()
                    ));
                }
            }
            if cancelled.load(Ordering::SeqCst) {
                return Ok(false);
            }
            if let Some(list) = &change.sync_list {
                core::write_sync_list(&confdir, list)
                    .map_err(|e| format!("Couldn't write the folder list: {e}"))?;
            }
            if !change.config.is_empty() {
                let mut cfg = ClientConfig::load(&confdir);
                for (k, v) in &change.config {
                    cfg.set(k, v.as_deref());
                }
                cfg.save()
                    .map_err(|e| format!("Couldn't write the settings: {e}"))?;
            }
            let mut cmd = core::client_command();
            cmd.arg("--confdir")
                .arg(&confdir)
                .args(change.mode.client_args())
                .args(["--dry-run", "--verbose"]);
            let (mut child, _, readers) = spawn_capture(cmd, out, false)
                .map_err(|e| format!("Couldn't start the sync engine: {e}"))?;
            let child_pid = child.id();
            let raw_fd =
                unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid as libc::pid_t, 0) };
            let pidfd = if raw_fd >= 0 {
                // SAFETY: pidfd_open returned a new owned file descriptor.
                Some(unsafe { OwnedFd::from_raw_fd(raw_fd as i32) })
            } else {
                None
            };
            let mut cancel_sent = false;
            let ok = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status.success(),
                    Err(_) => {
                        let _ = child.wait();
                        break false;
                    }
                    Ok(None) => {}
                }
                if cancelled.load(Ordering::SeqCst) && !cancel_sent {
                    if let Some(fd) = &pidfd {
                        // SAFETY: this pidfd refers to this child, even if it has just exited.
                        unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                fd.as_raw_fd(),
                                libc::SIGTERM,
                                std::ptr::null::<libc::siginfo_t>(),
                                0,
                            );
                        }
                    } else {
                        // This worker is the only code that reaps the child. Since try_wait just
                        // reported it still running, its PID cannot have been reused.
                        unsafe {
                            libc::kill(child_pid as libc::pid_t, libc::SIGTERM);
                        }
                    }
                    cancel_sent = true;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            finish_reading(readers);
            Ok(ok)
        },
        |ctx, r| {
            let mut c = ctx.borrow_mut();
            let Some(p) = c.preview.as_mut() else { return };
            p.running = false;
            if p.cancelled.load(Ordering::SeqCst) {
                drop(c);
                finish_cancel(ctx);
                return;
            }
            let output = p.out.lock().unwrap().clone();
            let folders_changed = p.folders_changed;
            let note = p.note.clone();
            let ui = c.ui();
            ui.set_p_busy(false);
            match r {
                Ok(ok) => {
                    let mut s = summarize(&output, ok, folders_changed);
                    if s.can_apply && !note.is_empty() {
                        s.text = format!("{}\n\n{note}", s.text.trim_end());
                    }
                    ui.set_p_text(ss(s.text));
                    ui.set_p_warn(ss(s.warn));
                    ui.set_p_can_apply(s.can_apply);
                }
                Err(e) => {
                    ui.set_p_text(ss(e));
                    ui.set_p_can_apply(false);
                }
            }
        },
    );
}

struct Summary {
    text: String,
    warn: String,
    can_apply: bool,
}

fn summarize(output: &str, ok: bool, folders_changed: bool) -> Summary {
    let clean = |p: &str| {
        let p = p.split(" ... ").next().unwrap_or(p).trim();
        p.strip_prefix("./").unwrap_or(p).to_string()
    };
    let num_after = |line: &str, prefix: &str| -> Option<u64> {
        line.trim().strip_prefix(prefix)?.trim().parse().ok()
    };
    let (mut downloads, mut uploads) = (0u64, 0u64);
    let (mut online_del, mut local_del, mut errors): (Vec<String>, Vec<String>, Vec<String>) =
        (vec![], vec![], vec![]);
    for line in output.lines() {
        let l = line.trim();
        if let Some(n) = num_after(l, "Number of items to download from Microsoft OneDrive:") {
            downloads += n;
        } else if let Some(n) = num_after(l, "New items to upload to Microsoft OneDrive:") {
            uploads += n;
        } else if let Some(p) = l.strip_prefix("Deleting item from Microsoft OneDrive: ") {
            online_del.push(clean(p));
        } else if let Some(p) = l
            .strip_prefix("Deleting local file: ")
            .or_else(|| l.strip_prefix("Deleting local directory: "))
            // "download only" cleaning up what isn't online
            .or_else(|| l.strip_prefix("Attempting removal of local file: "))
            .or_else(|| l.strip_prefix("Attempting removal of local directory: "))
        {
            local_del.push(clean(p));
        } else if l.starts_with("ERROR") {
            errors.push(l.to_string());
        }
    }
    if !ok {
        let tail: Vec<&str> = output
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(6)
            .collect();
        let detail = if errors.is_empty() {
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        } else {
            errors.join("\n")
        };
        return Summary {
            text: "The dry run failed, so nothing was changed. Your previous settings will be restored when you close this.".into(),
            warn: detail,
            can_apply: false,
        };
    }
    let mut text = String::from(
        "Dry run finished — nothing has changed yet. If you apply this, the sync engine will:\n",
    );
    text.push_str(&format!(
        "  •  download {downloads} item(s) to this computer\n"
    ));
    text.push_str(&format!("  •  upload {uploads} item(s) to OneDrive\n"));
    text.push_str(&format!(
        "  •  delete {} item(s) online\n",
        online_del.len()
    ));
    text.push_str(&format!(
        "  •  delete {} item(s) on this computer\n",
        local_del.len()
    ));
    if folders_changed {
        text.push_str(
            "\nFolders you stop syncing stay on this computer; they just stop being kept in sync.",
        );
    }
    let mut warn = String::new();
    for (label, list) in [
        ("Online deletions", &online_del),
        ("Deletions on this computer", &local_del),
    ] {
        if !list.is_empty() {
            warn.push_str(&format!("{label}:\n"));
            for p in list.iter().take(8) {
                warn.push_str(&format!("  {p}\n"));
            }
            if list.len() > 8 {
                warn.push_str(&format!("  …and {} more\n", list.len() - 8));
            }
        }
    }
    if !errors.is_empty() {
        warn.push_str(&format!(
            "{} warning(s) during the check, e.g.: {}\n",
            errors.len(),
            errors[0]
        ));
    }
    Summary {
        text,
        warn: warn.trim_end().to_string(),
        can_apply: true,
    }
}

fn restore_snapshot(p: &Preview) -> Result<(), String> {
    restore_files(&p.snapshot)?;
    clear_pending_preview().map_err(|e| format!("Couldn't clear the recovery record: {e}"))
}

fn restore_files(files: &[(PathBuf, Option<Vec<u8>>)]) -> Result<(), String> {
    let mut errors = Vec::new();
    for (path, data) in files {
        match data {
            Some(d) => {
                if let Err(e) = core::atomic_write(path, d) {
                    errors.push(format!("{}: {e}", path.display()));
                }
            }
            None => {
                if let Err(e) = fs::remove_file(path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    errors.push(format!("{}: {e}", path.display()));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Couldn't restore preview settings: {}",
            errors.join("; ")
        ))
    }
}

/// A preview's trial settings, written down until it's applied or cancelled, so a crash or a
/// logout in the middle doesn't leave them in place (the client would then demand a resync).
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingPreview {
    window_pid: u32,
    #[serde(default)]
    window_start_time: u64,
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    unit: Option<String>,
    was_active: bool,
    #[serde(default)]
    committed: bool,
}

fn pending_preview_file() -> PathBuf {
    core::app_dir().join("preview-in-progress.json")
}

fn pending_for(p: &Preview, committed: bool) -> PendingPreview {
    PendingPreview {
        window_pid: std::process::id(),
        window_start_time: core::process_start_time(std::process::id()).unwrap_or(0),
        files: p.snapshot.clone(),
        unit: p.unit.clone(),
        was_active: p.was_active,
        committed,
    }
}

fn save_pending_preview(p: &Preview) -> Result<(), String> {
    let json = serde_json::to_vec(&pending_for(p, false)).map_err(|e| e.to_string())?;
    core::create_private_dir(&core::app_dir()).map_err(|e| e.to_string())?;
    core::atomic_write(&pending_preview_file(), &json).map_err(|e| e.to_string())
}

fn mark_pending_committed(p: &Preview) -> Result<(), String> {
    let json = serde_json::to_vec(&pending_for(p, true)).map_err(|e| e.to_string())?;
    core::atomic_write(&pending_preview_file(), &json).map_err(|e| e.to_string())
}

fn clear_pending_preview() -> std::io::Result<()> {
    match fs::remove_file(pending_preview_file()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Puts back the settings of a preview whose window went away without applying or cancelling
/// it. Does nothing while that window is still running.
pub fn recover_interrupted_preview() -> Result<(), String> {
    let data = match fs::read(pending_preview_file()) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Couldn't read the preview recovery record: {e}")),
    };
    let p: PendingPreview = serde_json::from_slice(&data)
        .map_err(|e| format!("The preview recovery record is damaged: {e}"))?;
    if p.window_pid != std::process::id()
        && let Some(current_start) = core::process_start_time(p.window_pid)
        && (p.window_start_time == 0 || current_start == p.window_start_time)
    {
        return Ok(());
    }
    if p.committed {
        return clear_pending_preview()
            .map_err(|e| format!("Couldn't clear the committed preview record: {e}"));
    }
    let confdir = p.files.first().and_then(|(path, _)| path.parent());
    if let Some(confdir) = confdir {
        let job = core::resync_unit(confdir);
        if core::unit_state(&job).active {
            let (ok, err) = core::systemctl(&["stop", &job]);
            if !ok {
                pin_recovery_busy(&p);
                return Err(format!(
                    "Couldn't stop the one-time sync job for recovery: {}",
                    err.trim()
                ));
            }
        }
    }
    if let Some(u) = &p.unit
        && core::unit_state(u).active
    {
        let (ok, err) = core::systemctl(&["stop", u]);
        if !ok {
            pin_recovery_busy(&p);
            return Err(format!(
                "Couldn't pause syncing to recover preview settings: {}",
                err.trim()
            ));
        }
    }
    if let Err(e) = restore_files(&p.files) {
        pin_recovery_busy(&p);
        return Err(e);
    }
    if let Err(e) = clear_pending_preview() {
        pin_recovery_busy(&p);
        return Err(format!(
            "Settings were restored, but the recovery record couldn't be cleared: {e}"
        ));
    }
    if p.was_active
        && let Some(u) = &p.unit
    {
        let (ok, err) = core::systemctl(&["start", u]);
        if !ok {
            return Err(format!(
                "Settings were restored, but syncing couldn't be restarted: {}",
                err.trim()
            ));
        }
    }
    Ok(())
}

fn pin_recovery_busy(p: &PendingPreview) {
    if let Some((path, _)) = p.files.first()
        && let Some(confdir) = path.parent()
        && let Ok(marker) = core::BusyMarker::try_new(confdir)
    {
        std::mem::forget(marker);
    }
}

fn finish_cancel(ctx: &Shared) {
    let Some(mut p) = ctx.borrow_mut().preview.take() else {
        return;
    };
    if let Err(e) = restore_snapshot(&p) {
        p.running = false;
        let ui = ctx.borrow().ui();
        ui.set_p_busy(false);
        ui.set_p_can_apply(false);
        ui.set_p_text(ss(format!(
            "Settings couldn't be fully restored. Syncing remains paused until recovery succeeds. {e}"
        )));
        ctx.borrow_mut().preview = Some(p);
        return;
    }
    if p.was_active
        && let Some(u) = &p.unit
    {
        let (ok, err) = core::systemctl(&["start", u]);
        if !ok {
            ctx.borrow().ui().set_s_msg(ss(format!(
                "Preview settings were restored, but syncing couldn't be restarted: {}",
                err.trim()
            )));
        }
    }
    let ui = ctx.borrow().ui();
    ui.set_p_visible(false);
    let key = p.key.clone();
    let acc = ctx.borrow().account(&key).cloned();
    if let Some(acc) = acc {
        load_options(ctx, &acc);
        load_folders(ctx, &acc, true);
    }
    refresh(ctx);
    if ctx.borrow().close_when_idle {
        window_hiding();
        let _ = ctx.borrow().ui().hide();
    }
}

fn preview_cancel(ctx: &Shared) {
    let running = {
        let c = ctx.borrow();
        let Some(p) = c.preview.as_ref() else { return };
        p.cancelled.store(true, Ordering::SeqCst);
        p.running
    };
    if running {
        ctx.borrow().ui().set_p_live(ss("Cancelling…"));
    } else {
        finish_cancel(ctx);
    }
}

fn preview_apply(ctx: &Shared) {
    let Some(p) = ctx.borrow_mut().preview.take() else {
        return;
    };
    let Some(acc) = ctx.borrow().account(&p.key).cloned() else {
        if let Err(e) = restore_snapshot(&p) {
            ctx.borrow_mut().preview = Some(p);
            ctx.borrow().ui().set_s_msg(ss(format!(
                "The account disappeared and preview settings couldn't be restored: {e}"
            )));
        }
        return;
    };
    let job = core::resync_unit(&acc.confdir);
    let client = core::client_binary().unwrap_or_else(|| PathBuf::from("onedrive"));
    let mut script = format!(
        "{} --confdir {} {}",
        sh_quote(&client.to_string_lossy()),
        sh_quote(&acc.confdir.to_string_lossy()),
        p.mode.client_args().join(" ")
    );
    if let (true, Some(u)) = (
        p.was_active
            || p.unit
                .as_deref()
                .is_some_and(|u| core::unit_state(u).enabled),
        &p.unit,
    ) {
        script.push_str(&format!("; systemctl --user start {}", sh_quote(u)));
    }
    if let Some(u) = &p.unit {
        // a big delete is being dealt with: schedules may start it again
        core::release_hold(u);
    }
    core::systemctl(&["reset-failed", &job]);
    let r = Command::new("systemd-run")
        .args([
            "--user",
            "--collect",
            "--quiet",
            &format!("--unit={job}"),
            &format!(
                "--description={} ({})",
                match p.mode {
                    PreviewMode::Resync => "OneDrive full resync",
                    PreviewMode::ForceDelete => "OneDrive sync with deletions",
                    PreviewMode::Check => "OneDrive sync with new settings",
                },
                core::unit_text(&acc.name)
            ),
            "/bin/sh",
            "-c",
            &script,
        ])
        .output();
    let ui = ctx.borrow().ui();
    ui.set_p_visible(false);
    match r {
        Ok(o) if o.status.success() => {
            if let Err(e) = mark_pending_committed(&p) {
                let (stopped, stop_error) = core::systemctl(&["stop", &job]);
                if stopped {
                    fail_preview_apply(
                        ctx,
                        p,
                        format!(
                            "The recovery record couldn't be committed: {e}; the sync job was stopped"
                        ),
                    );
                } else {
                    let ui = ctx.borrow().ui();
                    ui.set_p_visible(false);
                    ui.set_s_msg(ss(format!(
                        "The sync job started, but its recovery state couldn't be saved or stopped: {e}; {}. Restart this app to retry recovery. No other sync operation will start for this account in this session.",
                        stop_error.trim()
                    )));
                    // Keep the account blocked until process exit; startup recovery will stop the
                    // one-time job before it restores the saved files.
                    std::mem::forget(p);
                }
                return;
            }
            let cleanup = clear_pending_preview();
            let mut msg = format!(
                "{}: {} The status shows when it's done.",
                p.title,
                match p.mode {
                    PreviewMode::Resync => "full resync started.",
                    PreviewMode::ForceDelete | PreviewMode::Check => "started.",
                }
            );
            if let Err(e) = cleanup {
                msg.push_str(&format!(" The committed settings are safe, but the recovery record couldn't be cleared: {e}"));
            }
            ui.set_s_msg(ss(msg));
            ui.set_o_msg(ss(""));
            if p.mode == PreviewMode::Resync {
                ui.set_f_msg(ss("Resync started."));
            }
        }
        Ok(o) => {
            fail_preview_apply(
                ctx,
                p,
                format!(
                    "Couldn't start the sync engine: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            );
        }
        Err(e) => {
            fail_preview_apply(ctx, p, format!("Couldn't start the sync engine: {e}"));
        }
    }
    load_folders(ctx, &acc, true);
    refresh(ctx);
}

fn fail_preview_apply(ctx: &Shared, mut p: Preview, cause: String) {
    // The apply did not start; make sure recovery still describes an uncommitted preview.
    if let Err(e) = save_pending_preview(&p) {
        let ui = ctx.borrow().ui();
        ui.set_p_visible(true);
        ui.set_p_busy(false);
        ui.set_p_can_apply(false);
        ui.set_p_text(ss(format!(
            "{cause}. The recovery record couldn't be refreshed, so syncing remains paused: {e}"
        )));
        ctx.borrow_mut().preview = Some(p);
        return;
    }
    if let Err(e) = restore_snapshot(&p) {
        p.running = false;
        let ui = ctx.borrow().ui();
        ui.set_p_visible(true);
        ui.set_p_busy(false);
        ui.set_p_can_apply(false);
        ui.set_p_text(ss(format!(
            "{cause}. Preview settings couldn't be fully restored; syncing remains paused. {e}"
        )));
        ctx.borrow_mut().preview = Some(p);
        return;
    }
    let restart = if p.was_active {
        p.unit.as_deref().map(|u| core::systemctl(&["start", u]))
    } else {
        None
    };
    let msg = match restart {
        Some((false, e)) => format!(
            "{cause}. The previous sync service couldn't be restarted: {}",
            e.trim()
        ),
        _ => cause,
    };
    ctx.borrow().ui().set_s_msg(ss(msg));
}

fn update_preview_live(ctx: &Shared) {
    let c = ctx.borrow();
    let Some(p) = c.preview.as_ref().filter(|p| p.running) else {
        return;
    };
    let out = p.out.lock().unwrap();
    if let Some(last) = out.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        let last: String = last.chars().take(110).collect();
        c.ui().set_p_live(ss(last));
    }
}

// ---------------------------------------------------------------- activity tab

fn load_activity(ctx: &Shared, acc: &AccountInfo) {
    let ui = ctx.borrow().ui();
    ui.set_a_current(-1);
    let Some(unit) = acc.unit.clone() else {
        ui.set_a_rows(ModelRc::new(
            VecModel::<ModelRc<StandardListViewItem>>::default(),
        ));
        ctx.borrow_mut().activity.clear();
        return;
    };
    let key = acc.key();
    bg(
        move || {
            let mut rows: Vec<(String, &'static str, String)> = Vec::new();
            for line in core::journal_tail(&unit, 8000).iter().rev() {
                let Some((when, msg)) = core::split_journal_line(line) else {
                    continue;
                };
                if let Some(LogEvent::File { kind, path }) = core::parse_log(msg) {
                    let what = match kind {
                        "↑" => "Uploaded",
                        "↓" => "Downloaded",
                        "✕" => "Deleted",
                        _ => "New folder",
                    };
                    rows.push((when.format("%b %-d  %H:%M").to_string(), what, path));
                    if rows.len() >= 500 {
                        break;
                    }
                }
            }
            rows
        },
        move |ctx, rows| {
            let mut c = ctx.borrow_mut();
            if c.current.as_deref() != Some(key.as_str()) {
                return;
            }
            c.activity = rows.iter().map(|r| r.2.clone()).collect();
            let model: Vec<ModelRc<StandardListViewItem>> = rows
                .into_iter()
                .map(|(t, w, p)| {
                    ModelRc::new(VecModel::from(vec![
                        StandardListViewItem::from(ss(t)),
                        StandardListViewItem::from(ss(w)),
                        StandardListViewItem::from(ss(p)),
                    ]))
                })
                .collect();
            c.ui().set_a_rows(ModelRc::new(VecModel::from(model)));
        },
    );
}

// ---------------------------------------------------------------- sign in / out

fn sign_in(ctx: &Shared, acc: &AccountInfo, enable_after: bool) {
    if ctx.borrow().signin.is_some() {
        return;
    }
    if let Some(why) = client_blocker() {
        ctx.borrow().ui().set_s_msg(ss(why));
        check_client();
        return;
    }
    // Cover the whole transition so the tray cannot restart this account between stop and spawn.
    let busy = match core::BusyMarker::try_new(&acc.confdir) {
        Ok(marker) => marker,
        Err(e) => {
            ctx.borrow().ui().set_s_msg(ss(format!(
                "Couldn't protect this account from a second sync process, so sign-in was not started: {e}"
            )));
            return;
        }
    };
    let was_active = acc
        .unit
        .as_deref()
        .is_some_and(|u| core::unit_state(u).active);
    if let Some(u) = &acc.unit {
        let (ok, err) = core::systemctl(&["stop", u]);
        if !ok {
            ctx.borrow().ui().set_s_msg(ss(format!(
                "Couldn't stop the sync engine before signing in: {}",
                err.trim()
            )));
            return;
        }
    }
    let out = Arc::new(Mutex::new(String::new()));
    let mut cmd = core::client_command();
    cmd.arg("--confdir").arg(&acc.confdir);
    // a saved sign-in Microsoft no longer accepts (password changed, revoked) must be replaced,
    // not retried
    if acc.confdir.join("refresh_token").exists() {
        cmd.arg("--reauth");
    }
    let url_file = browser_shim(&mut cmd);
    core::activate(&mut cmd);
    let ui = ctx.borrow().ui();
    let (child, stdin, readers) = match spawn_capture(cmd, out.clone(), true) {
        Ok(c) => c,
        Err(e) => {
            let restore = if was_active {
                core::systemctl(&["start", acc.unit.as_deref().unwrap()])
            } else {
                (true, String::new())
            };
            ui.set_s_msg(ss(if restore.0 {
                format!("Couldn't start the sign-in process: {e}")
            } else {
                format!(
                    "Couldn't start the sign-in process: {e}. The previously running sync engine also couldn't be restarted: {}",
                    restore.1.trim()
                )
            }));
            return;
        }
    };
    ui.set_si_text(ss("Opening the Microsoft sign-in page in your browser…\n\nLog in and approve access. This window updates by itself when you're done."));
    ui.set_si_need_paste(false);
    ui.set_si_has_url(false);
    ui.set_si_done(false);
    ui.set_si_paste(ss(""));
    ui.set_si_visible(true);
    ctx.borrow_mut().signin = Some(SignIn {
        key: acc.key(),
        child,
        stdin,
        out,
        readers,
        exited_polls: 0,
        url: None,
        url_file,
        enable_after,
        was_active,
        finished: false,
        _busy: busy,
    });
}

/// Called on a timer while a sign-in is running.
fn poll_signin(ctx: &Shared) {
    let mut c = ctx.borrow_mut();
    let ui = c.ui();
    let Some(si) = c.signin.as_mut() else { return };
    if si.finished {
        return;
    }
    let text = si.out.lock().unwrap().clone();
    if si.url.is_none() {
        let from_shim = si
            .url_file
            .as_ref()
            .and_then(|f| fs::read_to_string(f).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("https://"));
        si.url = from_shim.or_else(|| {
            text.split_whitespace()
                .find(|w| w.starts_with("https://login.") && w.contains("authorize"))
                .map(String::from)
        });
        ui.set_si_has_url(si.url.is_some());
    }
    if !ui.get_si_need_paste() && text.contains("Paste redirect URI here") {
        ui.set_si_need_paste(true);
        ui.set_si_text(ss("Your browser couldn't hand the sign-in back automatically.\n\nAfter you log in you'll end up on a blank page. Copy that page's full address from the address bar and paste it below."));
    }
    let exited = si.child.try_wait().ok().flatten();
    if exited.is_none() {
        return;
    }
    // let the last of its output (often the error) come in first, for up to about 2 s
    si.exited_polls += 1;
    if si.exited_polls < 5 && !si.readers.iter().all(|r| r.is_finished()) {
        return;
    }
    let text = si.out.lock().unwrap().clone();
    let key = si.key.clone();
    let enable_after = si.enable_after;
    let was_active = si.was_active;
    // a quick sign-in can end before the account list (refreshed in the background) has the
    // account it just made: keep waiting for it rather than leave the dialog hanging
    let Some(acc) = c.account(&key).cloned() else {
        return;
    };
    if let Some(si) = c.signin.as_mut() {
        si.finished = true;
    }
    if acc.confdir.join("refresh_token").exists() {
        ui.set_si_text(ss(if enable_after {
            "Signed in. Your files will start syncing in a moment."
        } else {
            "Signed in."
        }));
        ui.set_si_done(true);
        ui.set_si_need_paste(false);
        c.tokens.remove(&key);
        drop(c);
        after_sign_in(acc, enable_after, was_active);
    } else {
        let detail = text
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("Paste"))
            .unwrap_or("The sign-in didn't finish.")
            .to_string();
        ui.set_si_text(ss(format!(
            "Sign-in didn't complete.\n\n{}",
            friendly_signin_error(&detail)
        )));
        if was_active && let Some(unit) = acc.unit.as_deref() {
            let (ok, err) = core::systemctl(&["start", unit]);
            if !ok {
                ui.set_s_msg(ss(format!(
                    "Sign-in failed, and the previously running sync engine couldn't be restarted: {}",
                    err.trim()
                )));
            }
        }
        ui.set_si_done(true);
        ui.set_si_need_paste(false);
    }
}

/// Turns the client's command-line oriented errors into something a person can act on.
fn friendly_signin_error(detail: &str) -> String {
    let d = detail.to_lowercase();
    if d.contains("--reauth") || d.contains("invalid_grant") || d.contains("authorisation code") {
        "Microsoft didn't accept the sign-in. Please try again, and make sure you finish logging in in the browser.".into()
    } else if d.contains("timed out") || d.contains("timeout") {
        "The sign-in timed out. Please try again.".into()
    } else if d.contains("aadsts") {
        format!("Microsoft reported a problem: {detail}")
    } else if d.contains("unable to reach")
        || d.contains("network")
        || d.contains("could not resolve")
    {
        "Couldn't reach Microsoft. Check your internet connection and try again.".into()
    } else {
        detail.to_string()
    }
}

/// Puts a small `xdg-open` wrapper first on the client's PATH that records the sign-in address
/// before opening the browser, so we can offer "open again" and "copy link".
fn browser_shim(cmd: &mut Command) -> Option<PathBuf> {
    let dir = core::runtime_dir().join("shim");
    fs::create_dir_all(&dir).ok()?;
    let url_file = dir.join("last-url");
    let _ = fs::remove_file(&url_file);
    let real = std::env::var_os("PATH")
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$1\" > {}\nPATH={}\nexec xdg-open \"$@\"\n",
        sh_quote(&url_file.to_string_lossy()),
        sh_quote(&real)
    );
    let shim = dir.join("xdg-open");
    fs::write(&shim, script).ok()?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).ok()?;
    cmd.env("PATH", format!("{}:{real}", dir.display()));
    Some(url_file)
}

fn after_sign_in(acc: AccountInfo, enable: bool, was_active: bool) {
    let mut acc = acc;
    acc.signed_in = true;
    bg(
        move || {
            // detect personal vs work/school, then switch on background sync
            if let Ok(t) = graph::token(&acc.confdir)
                && let Ok(kind) = graph::account_kind(&t)
            {
                let mut s = AppSettings::load();
                if s.prefs(&acc.confdir).kind.is_none() {
                    s.prefs_mut(&acc.confdir).kind = Some(kind);
                    let _ = s.save();
                }
            }
            let unit = core::ensure_unit(&acc).map_err(|e| e.to_string())?;
            // just signed in on purpose: a hold from an old big delete no longer applies
            core::release_hold(&unit);
            if enable || core::unit_state(&unit).enabled {
                let (ok, msg) = core::systemctl(&["enable", "--now", &unit]);
                if !ok {
                    return Err(msg);
                }
                let autostart = set_autostart(true);
                start_tray();
                autostart.map_err(|e| {
                    format!("Sync was enabled, but the tray couldn't be set to start at login: {e}")
                })?;
            } else if was_active {
                let (ok, msg) = core::systemctl(&["start", &unit]);
                if !ok {
                    return Err(msg);
                }
            }
            Ok(())
        },
        |ctx, r: Result<(), String>| {
            if let Err(e) = r {
                ctx.borrow().ui().set_s_msg(ss(format!(
                    "Sign-in finished, but setup wasn't fully completed: {e}"
                )));
            }
            refresh(ctx);
        },
    );
}

fn signin_submit(ctx: &Shared) {
    let mut c = ctx.borrow_mut();
    let ui = c.ui();
    let url = ui.get_si_paste().trim().to_string();
    if !url.contains("code=") {
        ui.set_si_text(ss("That doesn't look like the right address — it should contain \"code=\". Copy the whole address of the blank page you land on after logging in."));
        return;
    }
    if let Some(si) = c.signin.as_mut() {
        if let Some(stdin) = si.stdin.as_mut() {
            let _ = writeln!(stdin, "{url}");
            let _ = stdin.flush();
        }
        ui.set_si_text(ss("Finishing sign-in…"));
        ui.set_si_need_paste(false);
    }
}

fn signin_close(ctx: &Shared) {
    let si = ctx.borrow_mut().signin.take();
    if let Some(mut si) = si {
        let restart_unit = if !si.finished && si.was_active {
            ctx.borrow().account(&si.key).and_then(|a| a.unit.clone())
        } else {
            None
        };
        if !si.finished {
            let _ = si.child.kill();
        }
        let _ = si.child.wait();
        drop(si); // release the busy marker before restoring the previous service
        if let Some(unit) = restart_unit {
            let (ok, err) = core::systemctl(&["start", &unit]);
            if !ok {
                ctx.borrow().ui().set_s_msg(ss(format!(
                    "The sign-in window closed, but the previously running sync engine couldn't be restarted: {}",
                    err.trim()
                )));
            }
        }
    }
    ctx.borrow().ui().set_si_visible(false);
    refresh(ctx);
    let i = ctx.borrow().ui().get_current();
    if i >= 0 {
        select(ctx, i);
    }
}

fn sign_out(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    ctx.borrow().ui().set_s_busy(true);
    bg(
        move || -> Result<(String, Option<String>), String> {
            core::validate_confdir(&acc.confdir)
                .map_err(|e| format!("Couldn't find this account's settings folder: {e}"))?;
            let _busy = core::BusyMarker::try_new(&acc.confdir).map_err(|e| {
                format!("Couldn't protect this account from another sync operation: {e}")
            })?;
            if let Some(u) = &acc.unit {
                let (ok, err) = core::systemctl(&["disable", "--now", u]);
                if !ok {
                    return Err(format!(
                        "Couldn't stop background syncing, so you weren't signed out: {}",
                        err.trim()
                    ));
                }
            }
            // people often sign out because the sign-in is broken, so a client that can't
            // log out cleanly doesn't stop the saved sign-in from being removed
            let logout = core::client_command()
                .arg("--confdir")
                .arg(&acc.confdir)
                .arg("--logout")
                .stdin(Stdio::null())
                .output();
            let note = match logout {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Err(e) => Some(e.to_string()),
            };
            match fs::remove_file(acc.confdir.join("refresh_token")) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!("Couldn't remove the saved sign-in: {e}"));
                }
            }
            Ok((acc.key(), note))
        },
        |ctx, result| {
            let mut c = ctx.borrow_mut();
            let ui = c.ui();
            ui.set_s_busy(false);
            match result {
                Ok((key, note)) => {
                    c.tokens.remove(&key);
                    ui.set_s_msg(ss(match note {
                        None => "Signed out. Your files are still on this computer.".to_string(),
                        Some(n) => format!(
                            "Signed out. Your files are still on this computer. (The sync engine reported: {n})"
                        ),
                    }));
                }
                Err(e) => ui.set_s_msg(ss(e)),
            }
            drop(c);
            refresh(ctx);
        },
    );
}

// ---------------------------------------------------------------- remove

fn remove_confirm(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    let delete_config = ctx.borrow().ui().get_r_delete_config();
    let ui = ctx.borrow().ui();
    ui.set_r_visible(false);
    ui.set_s_busy(true);
    bg(
        move || -> Result<(), String> {
            core::validate_confdir(&acc.confdir)
                .map_err(|e| format!("Couldn't find this account's settings folder: {e}"))?;
            let _busy = core::BusyMarker::try_new(&acc.confdir).map_err(|e| {
                format!("Couldn't protect this account from another sync operation: {e}")
            })?;
            if let Some(u) = &acc.unit {
                let (ok, err) = core::systemctl(&["disable", "--now", u]);
                if !ok {
                    return Err(format!(
                        "The account was left intact because its sync service couldn't be stopped: {}",
                        err.trim()
                    ));
                }
                if acc.managed {
                    let unit_file = core::user_unit_dir().join(u);
                    match fs::remove_file(&unit_file) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(format!(
                                "The service stopped, but couldn't remove {}: {e}",
                                unit_file.display()
                            ));
                        }
                    }
                    let (ok, err) = core::systemctl(&["daemon-reload"]);
                    if !ok {
                        return Err(format!(
                            "The service stopped, but systemd couldn't reload its units: {}",
                            err.trim()
                        ));
                    }
                }
            }
            let mut s = AppSettings::load();
            if delete_config {
                let _ = core::client_command()
                    .arg("--confdir")
                    .arg(&acc.confdir)
                    .arg("--logout")
                    .stdin(Stdio::null())
                    .output();
                core::delete_confdir(&acc.confdir)
                    .map_err(|e| format!("Couldn't delete {}: {e}", acc.confdir.display()))?;
                s.accounts.remove(&acc.key());
            }
            // an unmanaged unit still points at this config dir, so hide it rather than have it reappear
            if (!delete_config || (acc.unit.is_some() && !acc.managed))
                && !s.hidden.contains(&acc.key())
            {
                s.hidden.push(acc.key());
            }
            s.save().map_err(|e| e.to_string())
        },
        |ctx, r| {
            {
                let mut c = ctx.borrow_mut();
                c.current = None;
                c.tree = Tree::default();
                let ui = c.ui();
                ui.set_s_busy(false);
                if let Err(e) = r {
                    ui.set_s_msg(ss(e));
                }
            }
            refresh(ctx);
            select(ctx, -1);
        },
    );
}

// ---------------------------------------------------------------- add account

fn prepare_add_page(ctx: &Shared) {
    let c = ctx.borrow();
    let ui = c.ui();
    ui.set_add_mode(0);
    ui.set_add_msg(ss(""));
    ui.set_add_busy(false);
    ui.set_add_lib_index(-1);
    ui.set_add_libraries(ModelRc::default());
    let business: Vec<SharedString> = c
        .accounts
        .iter()
        .filter(|a| a.signed_in && a.kind != "sharepoint" && a.kind != "personal")
        .map(|a| ss(&a.name))
        .collect();
    ui.set_add_from_index(if business.is_empty() { -1 } else { 0 });
    ui.set_add_from(ModelRc::new(VecModel::from(business)));
}

/// Whether another account (other than `except`) already goes by `name`. Names are how people
/// tell accounts apart in the window and the tray, so they have to be unique.
fn name_taken(c: &Ctx, name: &str, except: Option<&str>) -> bool {
    c.accounts
        .iter()
        .any(|a| Some(a.key().as_str()) != except && a.name.eq_ignore_ascii_case(name.trim()))
}

/// `base`, or "base 2", "base 3"… if that's taken.
fn unique_name(c: &Ctx, base: &str) -> String {
    std::iter::once(base.to_string())
        .chain((2..).map(|n| format!("{base} {n}")))
        .find(|n| !name_taken(c, n, None))
        .unwrap()
}

fn business_accounts(c: &Ctx) -> Vec<AccountInfo> {
    c.accounts
        .iter()
        .filter(|a| a.signed_in && a.kind != "sharepoint" && a.kind != "personal")
        .cloned()
        .collect()
}

/// Suggests a folder in the home folder that doesn't exist yet, so nothing already there gets
/// uploaded by accident: ~/OneDrive, then "~/OneDrive - Work", then numbered copies.
fn default_folder(home: &Path, name: &str, sharepoint: bool) -> PathBuf {
    let (dir, first) = if sharepoint {
        (home.join("SharePoint"), name.to_string())
    } else if name.is_empty() || name.eq_ignore_ascii_case("onedrive") {
        (home.to_path_buf(), "OneDrive".to_string())
    } else if !home.join("OneDrive").exists() {
        return home.join("OneDrive");
    } else {
        (home.to_path_buf(), format!("OneDrive - {name}"))
    };
    std::iter::once(first.clone())
        .chain((2..).map(|n| format!("{first} ({n})")))
        .map(|f| dir.join(f))
        .find(|p| !p.exists())
        .unwrap()
}

fn add_choose(ctx: &Shared, mode: i32) {
    let ui = ctx.borrow().ui();
    ui.set_add_mode(mode);
    ui.set_add_msg(ss(""));
    match mode {
        1 => {
            let first = ctx.borrow().accounts.is_empty();
            let name = unique_name(&ctx.borrow(), if first { "OneDrive" } else { "Work" });
            let name = name.as_str();
            ui.set_add_name(ss(name));
            ui.set_add_folder(ss(
                default_folder(&core::home(), name, false).to_string_lossy()
            ));
            ui.set_add_cloud(0);
            ui.set_add_tenant(ss(""));
        }
        2 => {
            ui.set_add_name(ss(""));
            ui.set_add_folder(ss(""));
            if business_accounts(&ctx.borrow()).is_empty() {
                ui.set_add_msg(ss("Add and sign in to a work or school OneDrive account first. Its sign-in is reused to reach SharePoint."));
            }
        }
        _ => {}
    }
}

fn add_browse(ctx: &Shared) {
    // open the chooser at the suggested folder, or the nearest parent that exists yet
    let typed = core::expand_tilde(ctx.borrow().ui().get_add_folder().trim());
    pick_folder("Choose a folder to sync into", &typed, |ctx, picked| {
        let ui = ctx.borrow().ui();
        match picked {
            Ok(Some(p)) => ui.set_add_folder(ss(p)),
            Ok(None) => {}
            Err(()) => ui.set_add_msg(ss(
                "No folder chooser is available on this desktop. Type the folder's full path instead.",
            )),
        }
    });
}

/// Asks for a folder with the desktop's chooser (or kdialog/zenity), opened at `near` or the
/// nearest parent that exists. `done` gets Ok(None) if the user cancelled, Err if there's no
/// chooser at all.
fn pick_folder(
    title: &'static str,
    near: &Path,
    done: impl FnOnce(&Shared, Result<Option<String>, ()>) + Send + 'static,
) {
    let start = near
        .ancestors()
        .find(|p| p.is_absolute() && p.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(core::home)
        .to_string_lossy()
        .to_string();
    bg(
        move || {
            if let Some(picked) = crate::dbus::choose_folder(title, Path::new(&start)) {
                return Ok(picked.map(|p| p.to_string_lossy().to_string()));
            }
            let tries: [(&str, Vec<String>); 2] = [
                (
                    "kdialog",
                    vec!["--getexistingdirectory".into(), start.clone()],
                ),
                (
                    "zenity",
                    vec![
                        "--file-selection".into(),
                        "--directory".into(),
                        format!("--filename={start}/"),
                    ],
                ),
            ];
            for (prog, args) in tries {
                if core::which(prog).is_none() {
                    continue;
                }
                if let Ok(o) = Command::new(prog).args(&args).output() {
                    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    return Ok((o.status.success() && !p.is_empty()).then_some(p));
                }
            }
            Err(())
        },
        done,
    );
}

// ---------------------------------------------------------------- conflicting copies

fn load_conflicts(ctx: &Shared, acc: &AccountInfo) {
    let (key, root) = (acc.key(), acc.sync_dir.clone());
    ctx.borrow().ui().set_c_rows(ModelRc::default());
    bg(
        move || crate::conflict::find(&root, 20_000),
        move |ctx, found| {
            if ctx.borrow().current.as_deref() != Some(key.as_str()) {
                return;
            }
            let (found, note) = match found {
                Ok(found) => found,
                Err(e) => {
                    ctx.borrow().ui().set_s_msg(ss(e));
                    return;
                }
            };
            if let Some(note) = note {
                ctx.borrow().ui().set_s_msg(ss(note));
            }
            let Some(acc) = current(ctx) else { return };
            let rows: Vec<ConflictRow> = found
                .iter()
                .map(|c| {
                    let name = c
                        .original
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().to_string());
                    let folder = c
                        .original
                        .parent()
                        .and_then(|p| p.strip_prefix(&acc.sync_dir).ok())
                        .map(|p| p.display().to_string())
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| "Top of OneDrive".into());
                    let when = c
                        .modified
                        .map(|t| {
                            chrono::DateTime::<Local>::from(t)
                                .format("%-d %b, %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default();
                    ConflictRow {
                        name: ss(name),
                        detail: ss(format!("{folder}  ·  this computer's copy from {when}")),
                    }
                })
                .collect();
            ctx.borrow_mut().conflicts = found;
            ctx.borrow()
                .ui()
                .set_c_rows(ModelRc::new(VecModel::from(rows)));
        },
    );
}

fn conflict_open(ctx: &Shared, i: usize) {
    if let Some(c) = ctx.borrow().conflicts.get(i) {
        if c.original.exists() {
            core::open_path(&c.original);
        }
        core::open_path(&c.copy);
    }
}

fn conflict_settle(ctx: &Shared, i: usize, choice: i32) {
    let Some(c) = ctx.borrow().conflicts.get(i).cloned() else {
        return;
    };
    let choice = match choice {
        0 => crate::conflict::Choice::KeepMine,
        1 => crate::conflict::Choice::KeepOnline,
        _ => crate::conflict::Choice::KeepBoth,
    };
    // no need to pause syncing: settle() rechecks both files and never replaces anything, and
    // restarting the client would make it rescan the whole OneDrive for every click
    bg(
        move || crate::conflict::settle(&c, choice),
        |ctx, r| {
            ctx.borrow().ui().set_s_msg(ss(match r {
                Ok(m) => m,
                Err(e) => e,
            }));
            if let Some(acc) = current(ctx) {
                load_conflicts(ctx, &acc);
            }
        },
    );
}

// ---------------------------------------------------------------- backing up desktop folders

fn load_backups(ctx: &Shared, acc: &AccountInfo) {
    use crate::backup;
    let c = ctx.borrow();
    let home = core::home();
    let rows: Vec<BackupRow> = backup::FOLDERS
        .iter()
        .map(|k| {
            let (state, detail) = match backup::backed_up_by(&c.settings, &c.accounts, k.key) {
                Some((a, b)) if a.key() == acc.key() => {
                    (1, format!("In OneDrive: {}", b.target.display()))
                }
                Some((a, _)) => (2, format!("Backed up to \"{}\"", a.name)),
                None => match backup::xdg_dir(k.key) {
                    None => (3, "Not set up on this computer".to_string()),
                    Some(p) if p == home || home.starts_with(&p) => (
                        3,
                        "Set to your whole home folder, so it can't be backed up".to_string(),
                    ),
                    Some(p) => (0, p.display().to_string()),
                },
            };
            BackupRow {
                label: ss(k.label),
                detail: ss(detail),
                state,
            }
        })
        .collect();
    c.ui().set_b_rows(ModelRc::new(VecModel::from(rows)));
}

fn backup_start(ctx: &Shared, i: usize) {
    let Some(acc) = current(ctx) else { return };
    let Some(known) = crate::backup::FOLDERS.get(i) else {
        return;
    };
    let key = known.key;
    let all = ctx.borrow().accounts.clone();
    let ui = ctx.borrow().ui();
    ui.set_b_busy(true);
    ui.set_s_msg(ss(format!("Looking at your {} folder…", known.label)));
    bg(
        move || crate::backup::plan(&acc, &all, key).map(|p| (acc, p)),
        |ctx, r| {
            let ui = ctx.borrow().ui();
            ui.set_b_busy(false);
            ui.set_s_msg(ss(""));
            let (acc, p) = match r {
                Ok(v) => v,
                Err(e) => return ui.set_s_msg(ss(e)),
            };
            let size = graph::human_size(p.bytes);
            let mut text = format!(
                "Your {} folder ({}) moves into OneDrive, at {}, and everything in it uploads: {} {}, {size}. Your apps keep finding it in the same place.",
                p.label,
                p.source.display(),
                p.target.display(),
                p.files,
                if p.files == 1 { "file" } else { "files" },
            );
            if p.same_drive {
                text.push_str(" OneDrive is on the same drive, so the move is instant.");
            }
            if p.key == "DESKTOP" {
                text.push_str(" The desktop shows the new place after you next log in.");
            }
            text.push_str(" To undo it later, press Stop: your files stay in OneDrive and the folder here starts empty again, as on Windows.");
            let mut warn = String::new();
            if !p.same_drive {
                warn.push_str(&format!(
                    "OneDrive is on another drive, so the files are copied. Nothing is deleted: your original folder is kept, renamed \"{} (copied to OneDrive)\". Once you've checked everything is in OneDrive, delete it yourself to free the space. ",
                    p.label
                ));
            }
            if p.target_has_files {
                warn.push_str("OneDrive already has this folder: your files join what's there, and any whose name is taken get \"(from this computer's name)\" added.");
            }
            ui.set_bk_title(ss(format!("Back up your {} folder", p.label)));
            ui.set_bk_text(ss(text));
            ui.set_bk_warn(ss(warn.trim()));
            ui.set_bk_action(ss(if p.same_drive {
                "Back up"
            } else {
                "Copy and back up"
            }));
            ctx.borrow_mut().backup_ask = Some(BackupAsk::Start(acc, p));
            ui.set_bk_visible(true);
        },
    );
}

fn backup_stop(ctx: &Shared, i: usize) {
    let Some(acc) = current(ctx) else { return };
    let Some(known) = crate::backup::FOLDERS.get(i) else {
        return;
    };
    let target = ctx
        .borrow()
        .settings
        .prefs(&acc.confdir)
        .backups
        .iter()
        .find(|b| b.key == known.key)
        .map(|b| b.target.display().to_string())
        .unwrap_or_default();
    let ui = ctx.borrow().ui();
    ui.set_bk_title(ss(format!("Stop backing up {}?", known.label)));
    ui.set_bk_text(ss(format!(
        "Your files stay in OneDrive, at {target}, and keep syncing there. Your {} folder on this computer starts empty again, and new files you put in it won't be backed up.",
        known.label
    )));
    ui.set_bk_warn(ss(format!(
        "Nothing is moved back or deleted. To have the files here again, move them from {target} yourself."
    )));
    ui.set_bk_action(ss("Stop backing up"));
    ctx.borrow_mut().backup_ask = Some(BackupAsk::Stop(acc, known.key.to_string()));
    ui.set_bk_visible(true);
}

fn backup_confirm(ctx: &Shared) {
    let Some(ask) = ctx.borrow_mut().backup_ask.take() else {
        return;
    };
    let ui = ctx.borrow().ui();
    ui.set_b_busy(true);
    bg(
        move || {
            match ask {
            BackupAsk::Start(acc, p) => crate::backup::start(&acc, &p).map(|d| {
                let mut msg = format!("{} is now backed up to OneDrive.", p.label);
                if d.renamed > 0 {
                    msg.push_str(&format!(
                        " {} {} already taken in OneDrive, so {} saved with \"(from …)\" added.",
                        d.renamed,
                        if d.renamed == 1 { "name was" } else { "names were" },
                        if d.renamed == 1 { "it was" } else { "they were" }
                    ));
                }
                if let Some(k) = d.kept_original {
                    msg.push_str(&format!(
                        " Your original folder is kept at {}: delete it yourself once you've checked your files in OneDrive.",
                        k.display()
                    ));
                }
                if let Some(e) = d.desktop_setting {
                    msg.push_str(&format!(
                        " The desktop's folder setting couldn't be updated ({e}); the link at the old place still works."
                    ));
                }
                msg
            }),
            BackupAsk::Stop(acc, key) => crate::backup::stop(&acc, &key).map(|b| {
                format!(
                    "Stopped. Your files are still in OneDrive at {}.",
                    b.target.display()
                )
            }),
        }
        },
        |ctx, r| {
            let ui = ctx.borrow().ui();
            ui.set_b_busy(false);
            ui.set_bk_visible(false);
            ui.set_s_msg(ss(match r {
                Ok(m) => m,
                Err(e) => e,
            }));
            ctx.borrow_mut().settings = AppSettings::load();
            if let Some(acc) = current(ctx) {
                load_backups(ctx, &acc);
            }
        },
    );
}

/// Where a moved OneDrive folder ends up: the picked folder itself when it's empty, otherwise
/// a folder with the same name as now inside it.
fn move_destination(picked: &Path, current: &Path) -> Result<PathBuf, String> {
    let mut entries = fs::read_dir(picked).map_err(|e| {
        format!(
            "Couldn't inspect the selected folder {}: {e}",
            picked.display()
        )
    })?;
    let has_entry = entries
        .next()
        .transpose()
        .map_err(|e| {
            format!(
                "Couldn't inspect the selected folder {}: {e}",
                picked.display()
            )
        })?
        .is_some();
    Ok(if has_entry {
        picked.join(current.file_name().unwrap_or_default())
    } else {
        picked.to_path_buf()
    })
}

/// The folder picker intentionally lets the user choose an existing empty directory as the new
/// sync root. Remove only that empty directory; if it changed since review, stop safely.
fn remove_empty_destination(dest: &Path) -> Result<(), String> {
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::remove_dir(dest)
            .map_err(|e| {
                format!(
                    "Couldn't clear the empty destination {}: {e}",
                    dest.display()
                )
            }),
        Ok(_) => Err(format!(
            "The destination {} changed after review.",
            dest.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Couldn't inspect {}: {e}", dest.display())),
    }
}

/// Copies a sync tree to a staging sibling and publishes it without overwriting a path created
/// after the destination check.
fn copy_tree_noreplace(source: &Path, dest: &Path) -> Result<(), String> {
    static NEXT_STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = dest.parent().unwrap_or(Path::new("/"));
    for _ in 0..1000 {
        let n = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let stage = parent.join(format!(".lodg-move-stage-{}-{n}", std::process::id()));
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(&stage) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't prepare a safe copy: {e}")),
        }
        let payload = stage.join("payload");
        let copied = Command::new("cp")
            .args(["-a", "--reflink=auto", "-T", "--"])
            .arg(source)
            .arg(&payload)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !copied {
            let _ = fs::remove_dir_all(&stage);
            return Err(format!(
                "Copying to {} failed (is there enough space?). The original remains in {}.",
                dest.display(),
                source.display()
            ));
        }
        if let Err(e) = remove_empty_destination(dest) {
            let _ = fs::remove_dir_all(&stage);
            return Err(e);
        }
        match core::rename_noreplace(&payload, dest) {
            Ok(()) => {
                fs::remove_dir(&stage).map_err(|e| {
                    format!(
                        "The copy is in {}, but couldn't clean up {}: {e}",
                        dest.display(),
                        stage.display()
                    )
                })?;
                return Ok(());
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(format!(
                    "Couldn't publish the copied folder at {}: {e}",
                    dest.display()
                ));
            }
        }
    }
    Err("Couldn't allocate a staging folder after 1,000 attempts.".into())
}

fn move_folder_start(ctx: &Shared) {
    let Some(acc) = current(ctx) else { return };
    if ctx.borrow().preview.is_some() {
        return;
    }
    let near = acc.sync_dir.parent().unwrap_or(&acc.sync_dir).to_path_buf();
    pick_folder(
        "Choose where to move the OneDrive folder",
        &near,
        move |ctx, picked| {
            match picked {
            Ok(Some(p)) => move_folder_to(ctx, &acc, &core::expand_tilde(&p)),
            Ok(None) => {}
            Err(()) => ctx.borrow().ui().set_s_msg(ss(
                "No folder chooser is available on this desktop, so the folder can't be moved from here.",
            )),
        }
        },
    );
}

/// What happened to the files when moving the OneDrive folder.
enum Moved {
    Renamed,
    /// Copied to another drive; the old folder is still there
    Copied,
}

fn move_folder_to(ctx: &Shared, acc: &AccountInfo, picked: &Path) {
    let ui = ctx.borrow().ui();
    let old = acc.sync_dir.clone();
    let dest = match move_destination(picked, &old).and_then(|path| {
        core::resolve_path(&path)
            .ok_or_else(|| "Couldn't safely resolve the selected destination folder.".to_string())
    }) {
        Ok(path) => path,
        Err(e) => {
            ui.set_s_msg(ss(e));
            return;
        }
    };
    let old_meta = match fs::symlink_metadata(&old) {
        Ok(meta) => meta,
        Err(e) => {
            ui.set_s_msg(ss(format!("Couldn't inspect {}: {e}", old.display())));
            return;
        }
    };
    if old_meta.file_type().is_symlink() {
        ui.set_s_msg(ss(format!(
            "{} is a symbolic link. Move the actual sync folder or choose a new folder in Options first.",
            old.display()
        )));
        return;
    }
    if !old_meta.is_dir() {
        ui.set_s_msg(ss(format!(
            "{} isn't there (is its drive connected?), so there's nothing to move.",
            old.display()
        )));
        return;
    }
    let destination_fs_path = if fs::symlink_metadata(&dest).is_ok() {
        dest.clone()
    } else {
        dest.parent().unwrap_or(Path::new("/")).to_path_buf()
    };
    let destination_dev = match fs::metadata(&destination_fs_path) {
        Ok(meta) => meta.dev(),
        Err(e) => {
            ui.set_s_msg(ss(format!(
                "Couldn't inspect {}: {e}",
                destination_fs_path.display()
            )));
            return;
        }
    };
    if let Some(why) = core::unsuitable_sync_folder(&dest) {
        ui.set_s_msg(ss(why));
        return;
    }
    if core::paths_overlap(&dest, &old) {
        ui.set_s_msg(ss(
            "Pick a folder outside the current OneDrive folder (and not one that holds it).",
        ));
        return;
    }
    if let Some(other) = ctx
        .borrow()
        .accounts
        .iter()
        .find(|a| a.key() != acc.key() && core::paths_overlap(&a.sync_dir, &dest))
    {
        ui.set_s_msg(ss(format!(
            "That folder overlaps with \"{}\" ({}). Pick a separate folder.",
            other.name,
            other.sync_dir.display()
        )));
        return;
    }
    match fs::read_dir(&dest) {
        Ok(mut entries) => match entries.next() {
            Some(Ok(_)) => {
                ui.set_s_msg(ss(format!(
                    "{} already exists and isn't empty. Pick an empty folder.",
                    dest.display()
                )));
                return;
            }
            Some(Err(e)) => {
                ui.set_s_msg(ss(format!("Couldn't inspect {}: {e}", dest.display())));
                return;
            }
            None => {}
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            ui.set_s_msg(ss(format!("Couldn't inspect {}: {e}", dest.display())));
            return;
        }
    }
    ui.set_s_msg(ss(format!(
        "Moving the folder to {}… The sync engine is paused meanwhile.",
        dest.display()
    )));
    let (unit, confdir) = (acc.unit.clone(), acc.confdir.clone());
    let (source_dev, source_ino) = (old_meta.dev(), old_meta.ino());
    let mut moved_acc = acc.clone();
    moved_acc.sync_dir = dest.clone();
    let old_shown = old.display().to_string();
    bg(
        move || -> Result<Moved, String> {
            let _busy = core::BusyMarker::try_new(&confdir).map_err(|e| {
                format!("Couldn't protect this account from another sync operation: {e}")
            })?;
            let was_active = unit.as_deref().is_some_and(|u| core::unit_state(u).active);
            // stopped even when idle, so nothing starts it mid-move; if stopping fails it is
            // still in the state it was, so there is nothing to put back
            if let Some(u) = unit.as_deref() {
                let (ok, err) = core::systemctl(&["stop", u]);
                if !ok {
                    return Err(format!("Couldn't stop the sync engine: {}", err.trim()));
                }
            }
            let operation = (|| {
                let current_source = fs::symlink_metadata(&old)
                    .map_err(|e| format!("Couldn't recheck {}: {e}", old.display()))?;
                if current_source.file_type().is_symlink()
                    || !current_source.is_dir()
                    || current_source.dev() != source_dev
                    || current_source.ino() != source_ino
                {
                    return Err("The current sync folder changed while the destination was being reviewed. Refresh and try again.".into());
                }
                let current_destination = fs::metadata(&destination_fs_path).map_err(|e| {
                    format!("Couldn't recheck {}: {e}", destination_fs_path.display())
                })?;
                if current_destination.dev() != destination_dev {
                    return Err("The destination drive changed while the folder move was being reviewed. Connect the intended drive and try again.".into());
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("Couldn't create {}: {e}", parent.display()))?;
                }
                let move_result = core::rename_noreplace(&old, &dest);
                let move_result = match move_result {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        remove_empty_destination(&dest)?;
                        core::rename_noreplace(&old, &dest)
                    }
                    other => other,
                };
                let how = match move_result {
                    Ok(()) => Moved::Renamed,
                    // another drive: copy everything, keeping dates and permissions
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                        copy_tree_noreplace(&old, &dest)?;
                        Moved::Copied
                    }
                    Err(e) => return Err(format!("Couldn't move the folder: {e}")),
                };
                let mut cfg = ClientConfig::load(&confdir);
                cfg.set("sync_dir", Some(&dest.to_string_lossy()));
                if core::needs_disable_permission_set(&dest) {
                    cfg.set("disable_permission_set", Some("true"));
                }
                if let Err(e) = cfg.save() {
                    let rollback = match how {
                        Moved::Renamed => fs::rename(&dest, &old),
                        // Keep the copied data: the destination may have existed before the
                        // operation, and deleting it would make rollback less safe than recovery.
                        Moved::Copied => Ok(()),
                    };
                    return Err(match rollback {
                        Ok(()) => format!("Couldn't save the new folder in the settings: {e}"),
                        Err(rollback) => format!(
                            "Couldn't save the new folder in the settings: {e}; rollback also failed: {rollback}"
                        ),
                    });
                }
                Ok(how)
            })();
            let restarted = if was_active {
                let u = unit.as_deref().unwrap();
                let (ok, err) = core::systemctl(&["start", u]);
                Some(if ok {
                    Ok(())
                } else {
                    Err(err.trim().to_string())
                })
            } else {
                None
            };
            match (operation, restarted) {
                (Ok(done), Some(Ok(()))) | (Ok(done), None) => Ok(done),
                (Err(e), Some(Ok(()))) | (Err(e), None) => Err(e),
                (Ok(_), Some(Err(restart))) => Err(format!(
                    "The folder operation finished, but the sync engine couldn't be restarted: {restart}"
                )),
                (Err(operation), Some(Err(restart))) => Err(format!(
                    "{operation}; the sync engine couldn't be restarted: {restart}"
                )),
            }
        },
        move |ctx, r| {
            let ui = ctx.borrow().ui();
            refresh(ctx);
            match r {
                Err(e) => ui.set_s_msg(ss(e)),
                Ok(how) => {
                    ui.set_s_folder(ss(moved_acc.sync_dir.to_string_lossy()));
                    ui.set_mv_from(ss(moved_acc.sync_dir.to_string_lossy()));
                    ui.set_s_msg(ss(match how {
                        Moved::Renamed => "Folder moved. The sync engine needs a full resync to carry on there — check the preview.".to_string(),
                        Moved::Copied => format!(
                            "Folder copied. Your old folder ({}) was left untouched and is no longer synced: once you've checked your files in the new place, delete it yourself to free the space. Next, check the preview to carry on syncing.",
                            old_shown
                        ),
                    }));
                    start_preview(
                        ctx,
                        &moved_acc,
                        "Finish moving the folder",
                        PreviewChange {
                            sync_list: None,
                            config: Vec::new(),
                            mode: PreviewMode::Resync,
                            note: "Your files are already in the new folder, so little or nothing should need to transfer.".into(),
                        },
                    );
                }
            }
        },
    );
}

/// Finds libraries from what was typed: a pasted SharePoint address opens that site; anything
/// else searches the sites the chosen account belongs to (empty lists them all).
fn add_search(ctx: &Shared) {
    let c = ctx.borrow();
    let ui = c.ui();
    let accs = business_accounts(&c);
    let Some(from) = accs.get(ui.get_add_from_index().max(0) as usize).cloned() else {
        ui.set_add_msg(ss("Add and sign in to a work or school account first."));
        return;
    };
    let query = ui.get_add_query().trim().to_string();
    let cached = c.token(&from.key());
    ui.set_add_busy(true);
    ui.set_add_msg(ss(if query.starts_with("http") {
        "Opening that site…"
    } else {
        "Looking through your sites…"
    }));
    drop(c);
    let key = from.key();
    bg(
        move || {
            let t = token_for(&from.confdir, cached)?;
            let found = if query.starts_with("http") {
                graph::libraries_at_url(&as_graph(&t), &query)?
            } else {
                (graph::find_libraries(&as_graph(&t), &query)?, None)
            };
            Ok::<_, String>((t, found))
        },
        move |ctx, r| {
            let mut c = ctx.borrow_mut();
            let ui = c.ui();
            ui.set_add_busy(false);
            match r {
                Ok((t, (libs, pick))) => {
                    c.tokens.insert(key, t);
                    ui.set_add_msg(ss(if libs.is_empty() {
                        "No libraries found. Paste the site's address from your browser, or type part of its name."
                    } else {
                        ""
                    }));
                    let labels: Vec<SharedString> = libs
                        .iter()
                        .map(|l| ss(format!("{}  —  {}", l.site, l.name)))
                        .collect();
                    ui.set_add_libraries(ModelRc::new(VecModel::from(labels)));
                    ui.set_add_lib_index(-1);
                    c.libraries = libs;
                    // a single library, or an address pointing into one, picks itself
                    let auto = pick.or((c.libraries.len() == 1).then_some(0));
                    drop(c);
                    if let Some(i) = auto {
                        add_lib_picked(ctx, i);
                    }
                }
                Err(e) => ui.set_add_msg(ss(e)),
            }
        },
    );
}

fn add_lib_picked(ctx: &Shared, i: usize) {
    let c = ctx.borrow();
    let ui = c.ui();
    ui.set_add_lib_index(i as i32);
    if let Some(l) = c.libraries.get(i) {
        let name = unique_name(
            &c,
            &if l.name == "Documents" || l.name == "Shared Documents" {
                l.site.clone()
            } else {
                format!("{} - {}", l.site, l.name)
            },
        );
        ui.set_add_name(ss(&name));
        ui.set_add_folder(ss(
            default_folder(&core::home(), &name, true).to_string_lossy()
        ));
    }
}

fn new_confdir(name: &str, accounts: &[AccountInfo]) -> PathBuf {
    let default = core::config_home().join("onedrive");
    if !default.exists() && accounts.iter().all(|a| a.confdir != default) {
        return default;
    }
    let base = core::slug(name);
    (1..)
        .map(|n| {
            core::config_home().join(if n == 1 {
                format!("onedrive-{base}")
            } else {
                format!("onedrive-{base}-{n}")
            })
        })
        .find(|p| !p.exists())
        .unwrap()
}

fn add_create(ctx: &Shared) {
    let ui = ctx.borrow().ui();
    ui.set_client_done(ss(""));
    let mode = ui.get_add_mode();
    let name = ui.get_add_name().trim().to_string();
    let folder = ui.get_add_folder().trim().to_string();
    if let Some(why) = client_blocker() {
        ui.set_add_msg(ss(why));
        check_client();
        return;
    }
    if name.is_empty() || folder.is_empty() {
        ui.set_add_msg(ss("Give the account a name and pick a local folder."));
        return;
    }
    // a national cloud has to be set before the first sign-in, and needs the tenant
    let cloud = if mode == 1 {
        advanced::CLOUDS
            .get(ui.get_add_cloud().max(0) as usize)
            .map_or("", |(v, _)| *v)
    } else {
        ""
    };
    let tenant = ui.get_add_tenant().trim().to_string();
    if !cloud.is_empty() {
        if tenant.is_empty() {
            ui.set_add_msg(ss(
                "A national cloud account needs your organisation's tenant (ask its IT team).",
            ));
            return;
        }
        if let Err(e) = advanced::check_tenant(&tenant) {
            ui.set_add_msg(ss(format!("The tenant {e}.")));
            return;
        }
    }
    if name_taken(&ctx.borrow(), &name, None) {
        ui.set_add_msg(ss(format!(
            "You already have an account called \"{name}\". Pick a different name."
        )));
        return;
    }
    let folder = core::expand_tilde(&folder);
    if let Some(why) = core::unsuitable_sync_folder(&folder) {
        ui.set_add_msg(ss(why));
        return;
    }
    let c = ctx.borrow();
    if let Some(other) = c
        .accounts
        .iter()
        .find(|a| core::paths_overlap(&a.sync_dir, &folder))
    {
        ui.set_add_msg(ss(format!(
            "That folder overlaps with \"{}\" ({}). Pick a separate folder.",
            other.name,
            other.sync_dir.display()
        )));
        return;
    }
    let library = if mode == 2 {
        let Some(l) = c
            .libraries
            .get(ui.get_add_lib_index().max(-1) as usize)
            .cloned()
        else {
            ui.set_add_msg(ss("Search for and pick a library first."));
            return;
        };
        let Some(from) = business_accounts(&c)
            .get(ui.get_add_from_index().max(0) as usize)
            .cloned()
        else {
            return;
        };
        Some((l, from))
    } else {
        None
    };
    let confdir = new_confdir(&name, &c.accounts);
    drop(c);

    let setup = (|| -> Result<(), String> {
        fs::create_dir_all(&folder)
            .map_err(|e| format!("Couldn't create {}: {e}", folder.display()))?;
        core::create_private_dir(&confdir).map_err(|e| e.to_string())?;
        let mut cfg = ClientConfig::load(&confdir);
        cfg.set("sync_dir", Some(&folder.to_string_lossy()));
        if core::needs_disable_permission_set(&folder) {
            cfg.set("disable_permission_set", Some("true"));
        }
        if !cloud.is_empty() {
            cfg.set("azure_ad_endpoint", Some(cloud));
            cfg.set("azure_tenant_id", Some(&tenant));
        }
        if let Some((lib, from)) = &library {
            cfg.set("drive_id", Some(&lib.drive_id));
            let fcfg = ClientConfig::load(&from.confdir);
            for k in ["application_id", "azure_ad_endpoint", "azure_tenant_id"] {
                if let Some(v) = fcfg.get(k) {
                    cfg.set(k, Some(&v));
                }
            }
            fs::copy(
                from.confdir.join("refresh_token"),
                confdir.join("refresh_token"),
            )
            .map_err(|e| format!("Couldn't copy the sign-in: {e}"))?;
        }
        cfg.save().map_err(|e| e.to_string())?;
        let mut s = AppSettings::load();
        let p = s.prefs_mut(&confdir);
        p.name = Some(name.clone());
        if library.is_some() {
            p.kind = Some("sharepoint".into());
        }
        s.hidden.retain(|h| *h != confdir.to_string_lossy());
        s.save().map_err(|e| e.to_string())
    })();
    if let Err(e) = setup {
        ui.set_add_msg(ss(e));
        return;
    }
    let s = AppSettings::load();
    let acc = core::discover(&s)
        .into_iter()
        .find(|a| a.confdir == confdir);
    let Some(acc) = acc else {
        ui.set_add_msg(ss(
            "Created the account but couldn't find it again — try reopening the app.",
        ));
        return;
    };
    let index = {
        let mut c = ctx.borrow_mut();
        c.accounts.push(acc.clone());
        // a placeholder until the next refresh reads its real status
        c.statuses.push(Status {
            phase: if acc.signed_in {
                Phase::Unknown
            } else {
                Phase::SignedOut
            },
            last_complete: None,
            last_error: None,
            blocked: Vec::new(),
            big_delete: None,
            unit: Default::default(),
        });
        c.settings = s;
        c.accounts.len() - 1
    };
    // through select(), so nothing from the previously shown account lingers on the page
    select(ctx, index as i32);
    if library.is_some() {
        after_sign_in(acc, true, false);
    } else {
        sign_in(ctx, &acc, true);
    }
    refresh(ctx);
}

// ---------------------------------------------------------------- app page

pub fn autostart_file() -> PathBuf {
    core::config_home()
        .join("autostart")
        .join(format!("{}.desktop", core::APP_ID))
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    let f = autostart_file();
    if on {
        fs::create_dir_all(f.parent().unwrap())
            .map_err(|e| format!("Couldn't create the autostart folder: {e}"))?;
        let body = format!(
            "[Desktop Entry]\nType=Application\nName={}\nComment=OneDrive sync status\nExec={} --tray\nIcon={}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            core::APP_NAME,
            core::desktop_exec(),
            core::APP_ID
        );
        core::atomic_write(&f, body.as_bytes())
            .map_err(|e| format!("Couldn't save the autostart setting: {e}"))?;
    } else {
        match fs::remove_file(&f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Couldn't remove {}: {e}", f.display())),
        }
    }
    Ok(())
}

fn start_tray() {
    let me = core::self_command();
    if let Ok(mut c) = Command::new(me)
        .arg("--tray")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = c.wait();
        });
    }
}

fn prepare_app_page(ctx: &Shared) {
    let ui = ctx.borrow().ui();
    let settings = AppSettings::load();
    ui.set_app_autostart(autostart_file().exists());
    ui.set_app_stop_on_quit(settings.stop_sync_on_quit);
    ui.set_app_pause_metered(settings.pause_on_metered.unwrap_or(true));
    ui.set_app_pause_saver(settings.pause_on_power_saver.unwrap_or(true));
    ui.set_app_badges(settings.show_badges.unwrap_or(true));
    ui.set_app_dolphin_offer(crate::badges::dolphin_offer());
    let check = settings.check_updates.unwrap_or(true);
    ui.set_app_check_updates(check);
    if check {
        bg(crate::update::newer_version, |ctx, v| {
            ctx.borrow().ui().set_app_update(ss(v.unwrap_or_default()));
        });
    }
    ui.set_app_version(ss(env!("CARGO_PKG_VERSION")));
    ui.set_app_is_appimage(std::env::var("APPIMAGE").is_ok());
    ui.set_app_msg(ss(""));
    ui.set_app_client(ss("Checking the sync engine…"));
    bg(core::check_client, |ctx, check| {
        let client = match check {
            core::ClientCheck::Ok { path, version } => format!(
                "Sync engine: OneDrive Client for Linux {version} ({})",
                path.display()
            ),
            core::ClientCheck::TooOld { path, version } => format!(
                "Sync engine: OneDrive Client for Linux {version} ({}) — too old, please update it.",
                path.display()
            ),
            core::ClientCheck::Missing => {
                "Sync engine: the OneDrive Client for Linux isn't installed.".into()
            }
        };
        ctx.borrow().ui().set_app_client(ss(client));
    });
}

/// For AppImages: add a menu entry, icon and Dolphin actions for the current user.
fn add_to_menu(ctx: &Shared) {
    let r = crate::share::install_menu_entry();
    let ui = ctx.borrow().ui();
    ui.set_app_msg(ss(match r {
        Ok(()) => "Added. Look for \"DriveDeck for OneDrive\" in your applications menu. Right-clicking a file in Dolphin now also shows a OneDrive menu with \"Copy view-only link\", \"Copy editing link\" and \"View on the OneDrive website\".".to_string(),
        Err(e) => format!("Couldn't add it: {e}"),
    }));
}

// ---------------------------------------------------------------- sync client check

/// Looks for the OneDrive Client for Linux and shows the banner while it's missing or too old.
fn check_client() {
    bg(core::check_client, |ctx, check| {
        let ui = ctx.borrow().ui();
        let was_missing = !ui.get_client_problem().is_empty() && !ui.get_client_update();
        show_client_check(ctx, &check);
        if was_missing && let core::ClientCheck::Ok { version, .. } = &check {
            // installed by hand while the window was open
            show_client_done(
                ctx,
                format!("Found the OneDrive client ({version}). You're ready to sync."),
            );
            put_client_to_work(false);
        }
    });
}

/// Once the client is in place: points the sync units at it, restarts running ones when it was
/// replaced (they'd keep using the old program), and starts the ones that were waiting for it.
/// systemctl takes a while per unit, so not on the window's thread.
fn put_client_to_work(replaced: bool) {
    bg(
        move || {
            let restarted = core::repair_managed_units();
            let settings = AppSettings::load();
            if replaced {
                core::restart_active_units(&settings, &restarted);
            }
            core::start_idle_units(&settings);
        },
        |ctx, ()| refresh(ctx),
    );
}

// ---------------------------------------------------------------- sync started outside this app

/// What the outside-start banner says, or "" for nothing to show.
fn outside_start_text(found: &[core::OutsideStart], kept: &[String]) -> String {
    if found.iter().all(|o| kept.contains(&o.unit)) {
        return String::new();
    }
    let names = |system: bool| {
        found
            .iter()
            .filter(|o| o.system == system)
            .map(|o| o.unit.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (everyone, boot) = (names(false), names(true));
    let mut parts = Vec::new();
    if !everyone.is_empty() {
        parts.push(format!(
            "A setting for the whole computer starts {everyone} at login for everyone who uses it."
        ));
    }
    if !boot.is_empty() {
        parts.push(format!(
            "A system service, {boot}, starts syncing for you when the computer starts."
        ));
    }
    parts.push(
        "Whoever looks after this computer may have set this up on purpose, or an older \
         onedrive package left it behind. While it's there, OneDrive keeps syncing even \
         when the account is turned off here."
            .into(),
    );
    parts.join(" ")
}

fn show_outside_starts() {
    bg(core::outside_starts, |ctx, found| {
        let kept = AppSettings::load().outside_start_kept;
        let ui = ctx.borrow().ui();
        ui.set_outside_start(ss(outside_start_text(&found, &kept)));
        ui.set_outside_start_hint(ss(
            "Turning it off asks for your password and changes nothing else. It can be switched \
             back on with systemctl.",
        ));
    });
}

/// Switches off the outside starts after the user asked to: one password prompt for both kinds.
fn turn_off_outside_starts(ctx: &Shared) {
    let ui = ctx.borrow().ui();
    ui.set_outside_start_busy(true);
    bg(
        || {
            let found = core::outside_starts();
            let everyone: Vec<&str> = found
                .iter()
                .filter(|o| !o.system)
                .map(|o| o.unit.as_str())
                .collect();
            let boot: Vec<&str> = found
                .iter()
                .filter(|o| o.system)
                .map(|o| o.unit.as_str())
                .collect();
            if found.is_empty() {
                return Ok(());
            }
            // the first argument says how many of the rest are for-everyone units
            let script = r#"n=$1; shift; i=0
for u; do
    if [ "$i" -lt "$n" ]; then systemctl --global disable -- "$u"
    else systemctl disable --now -- "$u"; fi || exit
    i=$((i + 1))
done"#;
            let out = Command::new(clientinstall::elevate())
                .args(["/bin/sh", "-c", script, "sh", &everyone.len().to_string()])
                .args(&everyone)
                .args(&boot)
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("Couldn't ask for the password: {e}"))?;
            match out.status.code() {
                Some(0) => Ok(()),
                Some(126) => {
                    Err("Nothing was changed (the password window was closed).".to_string())
                }
                Some(127) => Err("Nothing was changed: your computer didn't allow it.".to_string()),
                _ => Err(format!(
                    "That didn't work: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
            }
        },
        |ctx, r: Result<(), String>| {
            let ui = ctx.borrow().ui();
            ui.set_outside_start_busy(false);
            match r {
                Ok(()) => {
                    show_outside_starts();
                    refresh(ctx);
                }
                Err(e) => ui.set_outside_start_hint(ss(e)),
            }
        },
    );
}

fn show_client_check(ctx: &Shared, check: &core::ClientCheck) {
    let ui = ctx.borrow().ui();
    let problem = match check {
        core::ClientCheck::Ok { .. } => String::new(),
        core::ClientCheck::Missing => "The OneDrive Client for Linux isn't installed".into(),
        core::ClientCheck::TooOld { version, .. } => {
            format!("Your OneDrive Client for Linux ({version}) is too old")
        }
    };
    let plan = clientinstall::plan();
    ui.set_client_update(false);
    ui.set_client_problem(ss(problem));
    ui.set_client_hint(ss(clientinstall::hint(plan.as_ref())));
    ui.set_client_can_install(plan.is_some());
    if check.is_ok() && !ctx.borrow().update_dismissed {
        check_client_update();
    }
}

/// On Arch, offers to update a client the AUR has a newer version of (pacman doesn't).
fn check_client_update() {
    bg(clientinstall::aur_update, |ctx, newer| {
        let ui = ctx.borrow().ui();
        let Some((installed, newest)) = newer else {
            return;
        };
        if !ui.get_client_problem().is_empty() || ui.get_client_installing() {
            return;
        }
        ui.set_client_update(true);
        ui.set_client_problem(ss(format!(
            "A newer OneDrive Client for Linux is available ({newest}; you have {installed})"
        )));
        ui.set_client_hint(ss(
            "It came from the AUR, which your normal software updates don't cover.",
        ));
        ui.set_client_can_install(true);
    });
}

/// The reason not to start something that needs the client, if there is one.
fn client_blocker() -> Option<String> {
    match core::check_client() {
        core::ClientCheck::Ok { .. } => None,
        core::ClientCheck::Missing => Some(
            "Install the OneDrive Client for Linux first — see the note at the top of the window."
                .into(),
        ),
        core::ClientCheck::TooOld { version, .. } => Some(format!(
            "Your OneDrive Client for Linux ({version}) is too old — update it first, see the note at the top of the window."
        )),
    }
}

/// "Install it for me": says what will happen, and offers doing it by hand, before the password.
fn install_client(ctx: &Shared) {
    let Some(plan) = clientinstall::plan() else {
        return;
    };
    let ui = ctx.borrow().ui();
    let update = ui.get_client_update();
    ui.set_ci_title(ss(if update {
        "Update the OneDrive client"
    } else {
        "Install the OneDrive client"
    }));
    ui.set_ci_action(ss(if update { "Update" } else { "Install" }));
    ui.set_ci_text(ss(plan.confirmation()));
    ui.set_ci_visible(true);
}

/// Progress lines the installer marks for people; the rest (apt, makepkg…) is only for errors.
const STEP: &str = "STEP ";

fn run_client_install(ctx: &Shared, plan: clientinstall::Plan) {
    let ui = ctx.borrow().ui();
    ui.set_ci_visible(false);
    ui.set_client_installing(true);
    ui.set_client_install_msg(ss("Your computer will ask for your password…"));
    let out = Arc::new(Mutex::new(String::new()));
    ctx.borrow_mut().install_out = Some(out.clone());
    ctx.borrow_mut().install_seen = 0;
    let was_update = ui.get_client_update();
    bg(
        move || {
            let (mut child, _, readers) = spawn_capture(plan.command(), out.clone(), false)?;
            let status = child.wait()?;
            finish_reading(readers);
            let text = out.lock().unwrap().clone();
            Ok::<_, std::io::Error>((status.code(), text))
        },
        move |ctx, r| {
            ctx.borrow_mut().install_out = None;
            let ui = ctx.borrow().ui();
            ui.set_client_installing(false);
            let check = core::check_client();
            let last = |t: &str| {
                t.lines()
                    .rev()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with(STEP))
                    .unwrap_or("unknown error")
                    .chars()
                    .take(200)
                    .collect::<String>()
            };
            let failed = match &r {
                Ok((Some(0), _)) => None,
                // the installer itself never started: pkexec's 126 = the password window was
                // closed, 127 = not allowed (wrong password, or no password window available)
                Ok((Some(126), text)) if !text.contains(STEP) => {
                    Some("Nothing was changed (the password window was closed).".to_string())
                }
                Ok((Some(127), text)) if !text.contains(STEP) => Some(
                    "Nothing was changed: your computer didn't allow it (the password wasn't \
                     accepted, or no password window could be shown)."
                        .to_string(),
                ),
                Ok((_, text)) => Some(format!("That didn't work: {}", last(text))),
                Err(e) => Some(format!("Couldn't start the installer: {e}")),
            };
            match failed {
                // the client still works: keep offering the update, with the reason
                Some(msg) if was_update && check.is_ok() => {
                    ui.set_client_update(true);
                    ui.set_client_can_install(true);
                    ui.set_client_install_msg(ss(msg));
                }
                Some(msg) => {
                    show_client_check(ctx, &check);
                    ui.set_client_install_msg(ss(msg));
                }
                None => {
                    show_client_check(ctx, &check);
                    ui.set_client_install_msg(ss(""));
                    if let core::ClientCheck::Ok { version, .. } = &check {
                        show_client_done(
                            ctx,
                            if was_update {
                                format!("The OneDrive client is updated to {version}.")
                            } else {
                                "The OneDrive client is installed. You're ready to sync.".into()
                            },
                        );
                        put_client_to_work(true);
                    } else {
                        ui.set_client_install_msg(ss(
                            "It finished, but the OneDrive client still isn't working. The instructions show how to install it by hand.",
                        ));
                    }
                }
            }
        },
    );
}

/// Shows the installer's latest step while it runs (building on Arch takes minutes).
fn update_install_live(ctx: &Shared) {
    let mut c = ctx.borrow_mut();
    let Some(out) = c.install_out.clone() else {
        return;
    };
    let out = out.lock().unwrap();
    // only what came in since the last look, up to its last full line (a line still arriving is
    // read whole next time); makepkg's output runs to many thousands of lines
    let new = &out[c.install_seen.min(out.len())..];
    let complete = new.rfind('\n').map_or(0, |i| i + 1);
    if let Some(step) = new[..complete]
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(STEP))
    {
        c.ui().set_client_install_msg(ss(step.trim()));
    }
    c.install_seen += complete;
}

/// A short "it worked" note where the banner was. It stays until OK or adding an account: going
/// away by itself would move the page under the pointer.
fn show_client_done(ctx: &Shared, msg: String) {
    let ui = ctx.borrow().ui();
    ui.set_client_install_msg(ss(""));
    ui.set_client_done(ss(msg));
}

// ---------------------------------------------------------------- uninstall

fn uninstall(ctx: &Shared) {
    let ui = ctx.borrow().ui();
    let keep_setup = ui.get_u_keep_setup();
    ui.set_u_busy(true);
    bg(
        move || {
            let how = crate::uninstall::installed();
            let notes = crate::uninstall::remove_user_files(keep_setup);
            let program = crate::uninstall::remove_program(&how);
            (notes, program, how)
        },
        |ctx, (notes, program, how)| {
            let ui = ctx.borrow().ui();
            ui.set_u_busy(false);
            let mut text = match (&program, how) {
                (Ok(()), crate::uninstall::Installed::BuildDirectory) => {
                    "This app's settings have been removed.".to_string()
                }
                (Ok(()), _) => "The app has been uninstalled.".to_string(),
                (Err(_), _) => "Your settings have been cleaned up.".to_string(),
            };
            for n in notes {
                text.push_str("\n\n• ");
                text.push_str(&n);
            }
            if let Err(e) = program {
                text.push_str("\n\n");
                text.push_str(&e);
            }
            ui.set_u_result(ss(text));
        },
    );
}

// ---------------------------------------------------------------- wiring

fn instance_socket() -> PathBuf {
    core::runtime_dir().join("window.sock")
}

/// If a window is already open, asks it to show `account` and returns true.
/// Asks the open window to show (on `account`), passing on our token to come to the front.
fn hand_off(account: Option<&str>) -> bool {
    match std::os::unix::net::UnixStream::connect(instance_socket()) {
        Ok(mut s) => {
            // the account alone, as before, when there's no token: a window of an older
            // version still open after an update reads it right
            let msg = match core::startup_token() {
                Some(token) => format!("{}\n{token}", account.unwrap_or("")),
                None => account.unwrap_or("").to_string(),
            };
            s.write_all(msg.as_bytes()).is_ok()
        }
        Err(_) => false,
    }
}

/// Decides whether this launch opens the window. Returns false when an open window took the
/// request instead. Windows started at the same moment race here, so the one that holds
/// `window.lock` (for as long as it runs) is the only one that may listen; the others keep
/// trying to hand off to it while it starts up.
fn claim_window(account: Option<&str>) -> bool {
    static LOCK: std::sync::OnceLock<fs::File> = std::sync::OnceLock::new();
    let path = instance_socket().with_file_name("window.lock");
    let _ = fs::create_dir_all(path.parent().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if hand_off(account) {
            return false;
        }
        let Ok(file) = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        else {
            return true; // no runtime dir to coordinate in: just open
        };
        if file.try_lock().is_ok() {
            let _ = LOCK.set(file);
            return true;
        }
        if std::time::Instant::now() > deadline {
            // the other window never started listening; better a second window than none
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Lets later launches (e.g. "Settings…" in the tray) reuse this window instead of opening another.
fn listen_for_hand_offs() {
    let path = instance_socket();
    let _ = fs::create_dir_all(path.parent().unwrap());
    let _ = fs::remove_file(&path); // a stale socket from a crashed instance
    let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) else {
        return;
    };
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut msg = String::new();
            let _ = conn.read_to_string(&mut msg);
            // "account\ntoken" (older versions sent the account alone)
            let (key, token) = match msg.split_once('\n') {
                Some((k, t)) => (k.to_string(), t.trim().to_string()),
                None => (msg, String::new()),
            };
            if std::env::var_os("LODG_POPUP_DEBUG").is_some() {
                eprintln!("window: handed off (token: {})", !token.is_empty());
            }
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|ctx| {
                    let busy = {
                        let c = ctx.borrow();
                        c.preview.is_some() || c.signin.is_some()
                    };
                    // an update replaced us: reopen as the new version, unless that would
                    // interrupt a preview or a sign-in
                    if !busy && core::program_file_replaced() {
                        restart_into_new_version(&key);
                        return;
                    }
                    let ui = ctx.borrow().ui();
                    let _ = ui.show();
                    ui.window().set_minimized(false);
                    {
                        // in front of what the user clicked to get here
                        use slint::winit_030::WinitWindowAccessor;
                        ui.window()
                            .with_winit_window(|w| crate::activation::activate(w, &token));
                    }
                    let idx = ctx.borrow().accounts.iter().position(|a| a.key() == key);
                    if let Some(i) = idx {
                        select(ctx, i as i32);
                    }
                })
            });
        }
    });
}

/// An update replaced our program while this window was open: open the new version in its
/// place (on the account that was asked for) and close this one.
fn restart_into_new_version(account: &str) {
    let _ = fs::remove_file(instance_socket()); // so the new window doesn't hand back to us
    let mut c = Command::new(core::self_command());
    if !account.is_empty() {
        c.args(["--account", account]);
    }
    if c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
    {
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    }
}

/// Started from a click elsewhere (the tray's icon or panel, a menu): the first window this
/// process opens uses the token that came with it, so it opens in front and gets focus, not
/// behind. Call before the window is made.
pub(crate) fn use_startup_token() {
    let Some(token) = core::startup_token() else {
        return;
    };
    use slint::winit_030::winit::platform::startup_notify::WindowAttributesExtStartupNotify;
    use slint::winit_030::winit::window::ActivationToken;
    // a token works once: the first window gets it, not one recreated later (on Wayland
    // hiding the window and showing it again makes a new one)
    let token = Mutex::new(Some(token));
    let _ = slint::BackendSelector::new()
        .with_winit_window_attributes_hook(move |a| match token.lock().unwrap().take() {
            Some(t) => a.with_activation_token(ActivationToken::from_raw(t)),
            None => a,
        })
        .select();
}

/// The settings window's title (as in app.slint), by which the panel finds it on KDE Plasma.
pub const WINDOW_TITLE: &str = "DriveDeck for OneDrive";

pub fn run(account: Option<String>) {
    if !claim_window(account.as_deref()) {
        return;
    }
    let recovery_error = recover_interrupted_preview().err();
    if let Some(e) = &recovery_error {
        crate::share::notify(
            "Preview recovery needs attention",
            &format!(
                "Sync services were not started because preview settings could not be recovered: {e}"
            ),
        );
    }
    core::repair_managed_units();
    if recovery_error.is_none() {
        core::start_idle_units(&AppSettings::load());
    }
    use_startup_token();
    let ui = match AppWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            // started from a menu, a message on the terminal alone would go unseen
            eprintln!("The window couldn't open: {e}");
            let first = e.to_string().lines().next().unwrap_or_default().to_string();
            crate::share::notify(
                "Couldn't open OneDrive settings",
                &format!("The window couldn't open ({first})."),
            );
            std::process::exit(1);
        }
    };
    // Matches the .desktop file name, so desktops show our icon and group the window correctly.
    // Slint only accepts this once its backend exists (after AppWindow::new) and before the
    // window is first shown.
    if let Err(e) = slint::set_xdg_app_id(core::APP_ID) {
        eprintln!("couldn't set the application id: {e}");
    }
    // the look follows the desktop, now and whenever it changes (dark at sunset, say)
    let look = read_look(&ui, Duration::from_millis(1500));
    apply_look(&ui, AppSettings::load().glass, look);
    {
        let weak = ui.as_weak();
        crate::dbus::watch_appearance(move |appearance| {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    APPEARANCE_CHANGED.store(true, Ordering::Relaxed);
                    let look = Look {
                        appearance,
                        ..*LOOK.lock().unwrap()
                    };
                    apply_look(&ui, AppSettings::load().glass, look);
                }
            });
        });
    }
    let rows = Rc::new(VecModel::<FolderRow>::default());
    ui.set_f_rows(ModelRc::from(rows.clone()));
    let ctx: Shared = Rc::new(RefCell::new(Ctx {
        win: ui.as_weak(),
        settings: AppSettings::load(),
        accounts: Vec::new(),
        statuses: Vec::new(),
        current: None,
        tree: Tree::default(),
        tokens: HashMap::new(),
        activity: Vec::new(),
        libraries: Vec::new(),
        signin: None,
        preview: None,
        install_out: None,
        install_seen: 0,
        update_dismissed: false,
        // The first status snapshot is read off the UI thread before the window appears.
        refreshing: true,
        refresh_again: false,
        close_when_idle: false,
        rows,
        backup_ask: None,
        conflicts: Vec::new(),
    }));
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    listen_for_hand_offs();
    show_outside_starts();
    check_client();
    let initial_account = account.clone();
    let start_tray_after_load = recovery_error.is_none();
    bg(take_snapshot, move |ctx, snap| {
        let reread = {
            let mut c = ctx.borrow_mut();
            c.refreshing = false;
            std::mem::take(&mut c.refresh_again)
        };
        apply_snapshot(ctx, snap);
        let start = initial_account
            .and_then(|k| {
                ctx.borrow()
                    .accounts
                    .iter()
                    .position(|a| a.key() == k || a.name == k)
            })
            .map(|i| i as i32)
            .unwrap_or(if ctx.borrow().accounts.is_empty() {
                -1
            } else {
                0
            });
        select(ctx, start);
        if start_tray_after_load && !ctx.borrow().accounts.is_empty() {
            start_tray();
        }
        if reread {
            refresh(ctx);
        }
    });

    macro_rules! on {
        ($setter:ident, |$c:ident $(, $arg:ident : $t:ty)*| $body:expr) => {{
            let w = ctx.clone();
            ui.$setter(move |$($arg: $t),*| {
                let $c = &w;
                $body
            });
        }};
    }
    on!(on_select, |c, i: i32| select(c, i));
    on!(on_rename, |c| {
        let Some(acc) = current(c) else { return };
        let name = c.borrow().ui().get_s_name().trim().to_string();
        if name.is_empty() {
            return;
        }
        if name_taken(&c.borrow(), &name, Some(&acc.key())) {
            c.borrow().ui().set_s_msg(ss(format!(
                "You already have an account called \"{name}\". Pick a different name."
            )));
            return;
        }
        let mut s = AppSettings::load();
        s.prefs_mut(&acc.confdir).name = Some(name.clone());
        if let Err(e) = s.save() {
            c.borrow()
                .ui()
                .set_s_msg(ss(format!("Couldn't save the account name: {e}")));
            return;
        }
        c.borrow_mut().settings = s;
        if acc.managed
            && let Some(u) = &acc.unit
            && let Err(e) = core::write_managed_unit(u, &name, &acc.confdir)
        {
            c.borrow().ui().set_s_msg(ss(format!(
                "The account name was saved, but its service description couldn't be updated: {e}"
            )));
            refresh(c);
            return;
        }
        c.borrow().ui().set_s_msg(ss("Renamed."));
        refresh(c);
    });
    on!(on_open_folder, |c| {
        if let Some(a) = current(c) {
            core::open_path(&a.sync_dir);
        }
    });
    on!(on_sync_now, |c| {
        let Some(acc) = current(c) else { return };
        if let Some(why) = client_blocker() {
            c.borrow().ui().set_s_msg(ss(why));
            check_client();
            return;
        }
        match core::sync_now(&acc) {
            Ok(_) => {
                c.borrow().ui().set_s_msg(ss("Syncing now…"));
            }
            Err(e) => c.borrow().ui().set_s_msg(ss(e)),
        }
        refresh(c);
    });
    on!(on_set_background, |c, on: bool| {
        let Some(acc) = current(c) else { return };
        c.borrow().ui().set_s_busy(true);
        bg(
            move || -> Result<(), String> {
                let u = core::ensure_unit(&acc).map_err(|e| e.to_string())?;
                if on {
                    core::release_hold(&u);
                }
                let (ok, msg) =
                    core::systemctl(&[if on { "enable" } else { "disable" }, "--now", &u]);
                if !ok {
                    return Err(msg);
                }
                if on {
                    start_tray();
                    set_autostart(true).map_err(|e| {
                        format!(
                            "Sync is enabled, but the tray couldn't be set to start at login: {e}"
                        )
                    })?;
                }
                Ok(())
            },
            |c, r| {
                let ui = c.borrow().ui();
                ui.set_s_busy(false);
                ui.set_s_msg(ss(r.err().unwrap_or_default()));
                refresh(c);
            },
        );
    });
    on!(on_sign_in, |c| {
        if let Some(acc) = current(c) {
            let enable = acc
                .unit
                .as_deref()
                .is_none_or(|u| !core::unit_state(u).enabled);
            sign_in(c, &acc, enable);
        }
    });
    on!(on_sign_out, |c| sign_out(c));
    on!(on_remove_account, |c| {
        let Some(acc) = current(c) else { return };
        let ui = c.borrow().ui();
        ui.set_r_text(ss(format!(
            "Stop syncing \"{}\" on this computer?\n\nYour files in {} stay where they are, and nothing is deleted online.",
            acc.name,
            acc.sync_dir.display()
        )));
        ui.set_r_delete_config(true);
        ui.set_r_visible(true);
    });
    on!(on_r_confirm, |c| remove_confirm(c));
    on!(on_r_cancel, |c| c.borrow().ui().set_r_visible(false));
    on!(on_si_open_url, |c| {
        let url = c.borrow().signin.as_ref().and_then(|s| s.url.clone());
        if let Some(u) = url {
            core::run_detached("xdg-open", &[&u]);
        }
    });
    on!(on_si_copy_url, |c| {
        let url = c.borrow().signin.as_ref().and_then(|s| s.url.clone());
        if let Some(u) = url {
            let ok = crate::share::copy_to_clipboard(&u);
            c.borrow().ui().set_si_text(ss(if ok {
                "Sign-in link copied. Paste it into any browser, log in, and this window will update by itself."
            } else {
                "Couldn't reach the clipboard. Use \"Open sign-in page again\" instead."
            }));
        }
    });
    on!(on_si_submit, |c| signin_submit(c));
    on!(on_si_close, |c| signin_close(c));
    on!(on_f_sync_all_toggled, |c| render_tree(c));
    on!(on_f_root_toggled, |c| render_tree(c));
    on!(on_f_expand, |c, i: i32| folder_expand(c, i as usize));
    on!(on_f_check, |c, i: i32| folder_check(c, i as usize));
    on!(on_f_reload, |c| {
        if let Some(acc) = current(c) {
            load_folders(c, &acc, true);
        }
    });
    on!(on_f_apply, |c| folders_apply(c));
    on!(on_o_save, |c| save_options(c));
    on!(on_move_folder, |c| move_folder_start(c));
    on!(on_b_start, |c, i: i32| backup_start(c, i.max(0) as usize));
    on!(on_b_stop, |c, i: i32| backup_stop(c, i.max(0) as usize));
    on!(on_bk_confirm, |c| backup_confirm(c));
    on!(on_c_open, |c, i: i32| conflict_open(c, i.max(0) as usize));
    on!(on_c_settle, |c, i: i32, choice: i32| conflict_settle(
        c,
        i.max(0) as usize,
        choice
    ));
    on!(on_o_show_config, |c| show_client_config(c));
    on!(on_o_open_logs, |c| open_log_folder(c));
    on!(on_o_adv_search, |c, q: SharedString| adv_search(c, &q));
    on!(on_a_refresh, |c| {
        if let Some(acc) = current(c) {
            load_activity(c, &acc);
        }
    });
    on!(on_a_show, |c, i: i32| {
        let b = c.borrow();
        if let (Some(acc), Some(p)) = (b.current_account(), b.activity.get(i.max(0) as usize)) {
            core::show_in_folder(&acc.sync_dir.join(p));
        }
    });
    on!(on_p_apply, |c| preview_apply(c));
    on!(on_big_delete_online, |c| big_delete_choose(c, true));
    on!(on_big_delete_restore, |c| big_delete_choose(c, false));
    on!(on_p_cancel, |c| preview_cancel(c));
    on!(on_add_choose, |c, m: i32| add_choose(c, m));
    on!(on_add_browse, |c| add_browse(c));
    on!(on_add_search, |c| add_search(c));
    on!(on_add_lib_picked, |c, i: i32| add_lib_picked(c, i as usize));
    on!(on_add_create, |c| add_create(c));
    on!(on_app_autostart_toggled, |c| {
        let on = c.borrow().ui().get_app_autostart();
        match set_autostart(on) {
            Ok(()) => {
                c.borrow().ui().set_app_msg(ss(""));
                if on {
                    start_tray();
                }
            }
            Err(e) => {
                c.borrow().ui().set_app_autostart(!on);
                c.borrow().ui().set_app_msg(ss(e));
            }
        }
    });
    on!(on_app_add_to_menu, |c| add_to_menu(c));
    on!(on_app_pause_toggled, |c| {
        let ui = c.borrow().ui();
        let mut s = AppSettings::load();
        s.pause_on_metered = (!ui.get_app_pause_metered()).then_some(false);
        s.pause_on_power_saver = (!ui.get_app_pause_saver()).then_some(false);
        if let Err(e) = s.save() {
            ui.set_app_msg(ss(format!("Couldn't save the setting: {e}")));
        }
    });
    on!(on_app_badges_toggled, |c| {
        let ui = c.borrow().ui();
        let on = ui.get_app_badges();
        let mut s = AppSettings::load();
        s.show_badges = (!on).then_some(false);
        if let Err(e) = s.save() {
            ui.set_app_badges_msg(ss(format!("Couldn't save the setting: {e}")));
            return;
        }
        // the tray picks the setting up within a few seconds; the Nemo / Caja extension and the
        // badge pictures come and go with it
        let errors = if on {
            crate::badges::install_user_files();
            Vec::new()
        } else {
            crate::badges::remove_user_files()
        };
        ui.set_app_badges_msg(ss(if errors.is_empty() {
            String::new()
        } else {
            format!("Couldn't remove {}", errors.join(", "))
        }));
    });
    on!(on_app_dolphin_install, |c| {
        let ui = c.borrow().ui();
        ui.set_app_dolphin_busy(true);
        ui.set_app_badges_msg(ss("Your computer will ask for your password…"));
        bg(crate::badges::install_dolphin_plugin, |ctx, r| {
            let ui = ctx.borrow().ui();
            ui.set_app_dolphin_busy(false);
            ui.set_app_dolphin_offer(crate::badges::dolphin_offer());
            ui.set_app_badges_msg(ss(match r {
                Ok(()) => "Added. Close every Dolphin window and open it again to see the badges."
                    .to_string(),
                Err(e) => e,
            }));
        });
    });
    on!(on_app_check_updates_toggled, |c| {
        let on = c.borrow().ui().get_app_check_updates();
        let mut s = AppSettings::load();
        s.check_updates = (!on).then_some(false);
        if !on {
            c.borrow().ui().set_app_update(ss(""));
        }
        if let Err(e) = s.save() {
            c.borrow()
                .ui()
                .set_app_msg(ss(format!("Couldn't save the setting: {e}")));
        }
    });
    on!(on_app_get_update, |_c| core::run_detached(
        "xdg-open",
        &[crate::update::RELEASES_URL]
    ));
    on!(on_app_stop_on_quit_toggled, |c| {
        let on = c.borrow().ui().get_app_stop_on_quit();
        let mut s = AppSettings::load();
        s.stop_sync_on_quit = on;
        if let Err(e) = s.save() {
            c.borrow()
                .ui()
                .set_app_msg(ss(format!("Couldn't save the setting: {e}")));
        }
    });
    on!(on_app_glass_changed, |c| {
        let ui = c.borrow().ui();
        let glass = match ui.get_app_glass() {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
        let mut settings = AppSettings::load();
        settings.glass = glass;
        if let Err(e) = settings.save() {
            ui.set_app_msg(ss(format!("Couldn't save the setting: {e}")));
        }
        c.borrow_mut().settings.glass = glass;
        // copied out first: apply_look locks LOOK itself, and a guard in the call would still hold it
        let look = *LOOK.lock().unwrap();
        apply_look(&ui, glass, look);
    });
    on!(on_client_recheck, |_c| check_client());
    on!(on_client_install, |c| install_client(c));
    on!(on_ci_confirm, |c| {
        if let Some(plan) = clientinstall::plan() {
            run_client_install(c, plan);
        }
    });
    on!(on_ci_cancel, |c| c.borrow().ui().set_ci_visible(false));
    // the banner notices the client by itself once it's installed (see the timer below)
    on!(on_ci_myself, |c| {
        let ui = c.borrow().ui();
        ui.set_ci_visible(false);
        ui.set_client_install_msg(ss("Once it's installed, this note goes away by itself."));
        core::run_detached("xdg-open", &[clientinstall::instructions_url()]);
    });
    on!(on_client_done_ok, |c| c
        .borrow()
        .ui()
        .set_client_done(ss("")));
    on!(on_client_not_now, |c| {
        c.borrow_mut().update_dismissed = true;
        let ui = c.borrow().ui();
        ui.set_client_update(false);
        ui.set_client_problem(ss(""));
        ui.set_client_install_msg(ss(""));
    });
    on!(on_outside_start_off, |c| turn_off_outside_starts(c));
    on!(on_outside_start_keep, |c| {
        let mut s = AppSettings::load();
        let ui = c.borrow().ui();
        for o in core::outside_starts() {
            if !s.outside_start_kept.contains(&o.unit) {
                s.outside_start_kept.push(o.unit);
            }
        }
        match s.save() {
            Ok(()) => ui.set_outside_start(ss("")),
            Err(e) => ui.set_outside_start_hint(ss(format!("Couldn't save that: {e}"))),
        }
    });
    on!(on_client_instructions, |_c| core::run_detached(
        "xdg-open",
        &[clientinstall::instructions_url()]
    ));
    on!(on_app_uninstall, |c| {
        let ui = c.borrow().ui();
        ui.set_u_remove_app_label(ss(crate::uninstall::removal_summary(
            &crate::uninstall::installed(),
        )));
        ui.set_u_keep_setup(true);
        ui.set_u_result(ss(""));
        ui.set_u_visible(true);
    });
    on!(on_u_confirm, |c| uninstall(c));
    on!(on_u_cancel, |c| {
        let ui = c.borrow().ui();
        if ui.get_u_result().is_empty() {
            ui.set_u_visible(false);
        } else {
            // everything is gone; nothing left for this window to show
            let _ = slint::quit_event_loop();
        }
    });
    on!(on_open_url, |_c, u: SharedString| core::run_detached(
        "xdg-open",
        &[u.as_str()]
    ));
    on!(on_open_online_page, |c, page: i32| {
        let Some(acc) = current(c) else { return };
        let cached = c.borrow().token(&acc.key());
        let page = match page {
            1 => graph::WebPage::RecycleBin,
            2 => graph::WebPage::Restore,
            _ => graph::WebPage::Home,
        };
        bg(
            move || {
                let t = token_for(&acc.confdir, cached)?;
                graph::web_page(&as_graph(&t), acc.drive_id.as_deref(), page)
            },
            |c, r| match r {
                Ok(u) => core::run_detached("xdg-open", &[&u]),
                Err(e) => c.borrow().ui().set_s_msg(ss(e)),
            },
        );
    });

    // closing the window mid-preview puts everything back as it was
    {
        let w = ctx.clone();
        ui.window().on_close_requested(move || {
            if w.borrow().preview.is_some() {
                preview_cancel(&w);
                let still = w.borrow().preview.as_ref().is_some_and(|p| p.running);
                if still {
                    w.borrow_mut().close_when_idle = true;
                    return slint::CloseRequestResponse::KeepWindowShown;
                }
            }
            let signin = w.borrow_mut().signin.take();
            if let Some(mut si) = signin {
                let _ = si.child.kill();
                let _ = si.child.wait();
            }
            window_hiding();
            slint::CloseRequestResponse::HideWindow
        });
    }

    // asks the desktop, on every click and key press, for permission to bring the next program
    // the window starts to the front (see core::activate)
    {
        use slint::winit_030::winit::event::{ElementState, WindowEvent};
        use slint::winit_030::winit::platform::startup_notify::WindowExtStartupNotify;
        use slint::winit_030::{EventResult, WinitWindowAccessor};
        ui.window().on_winit_window_event(|w, ev| {
            // the window only exists once the event loop runs, so its blur is asked for here
            if !BLUR_SENT.swap(true, Ordering::Relaxed) {
                w.with_winit_window(|ww| crate::glass::set(ww, GLASS.load(Ordering::Relaxed)));
            }
            // notes clicks, so a token can say which click asked (see activation.rs)
            w.with_winit_window(crate::activation::track);
            match ev {
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    ..
                } => {
                    // with the click's serial where the desktop gives one, else winit's
                    match w
                        .with_winit_window(crate::activation::request_token)
                        .flatten()
                    {
                        Some(token) => core::set_activation_token(token),
                        None => {
                            w.with_winit_window(|ww| ww.request_activation_token());
                        }
                    }
                }
                // (typing: winit's, which doesn't wait for the desktop's answer)
                WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
                    w.with_winit_window(|ww| ww.request_activation_token());
                }
                WindowEvent::ActivationTokenDone { token, .. } => {
                    core::set_activation_token(token.clone().into_raw());
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    let timer = slint::Timer::default();
    {
        let w = ctx.clone();
        let mut ticks = 0u32;
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(400),
            move || {
                poll_signin(&w);
                update_preview_live(&w);
                update_install_live(&w);
                ticks += 1;
                if ticks.is_multiple_of(8) {
                    refresh_tick(&w);
                    // installed some other way while the window is open
                    let ui = w.borrow().ui();
                    if !ui.get_client_problem().is_empty()
                        && !ui.get_client_update()
                        && !ui.get_client_installing()
                    {
                        check_client();
                    }
                }
                // a cancelled preview whose dry run has now stopped can close the window
                if w.borrow()
                    .preview
                    .as_ref()
                    .is_some_and(|p| p.cancelled.load(Ordering::SeqCst) && !p.running)
                {
                    finish_cancel(&w);
                }
            },
        );
    }
    #[cfg(feature = "test-driver")]
    let _driver = crate::testdrive::start(&ui);
    ui.run().expect("event loop failed");
    let _ = fs::remove_file(instance_socket());
    CTX.with(|c| *c.borrow_mut() = None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_folders() {
        let home = std::env::temp_dir().join(format!("lodg-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let f = |name, sp| default_folder(&home, name, sp);
        assert_eq!(f("OneDrive", false), home.join("OneDrive"));
        assert_eq!(f("Work", false), home.join("OneDrive"));
        fs::create_dir(home.join("OneDrive")).unwrap();
        assert_eq!(f("OneDrive", false), home.join("OneDrive (2)"));
        assert_eq!(f("Work", false), home.join("OneDrive - Work"));
        fs::create_dir(home.join("OneDrive - Work")).unwrap();
        assert_eq!(f("Work", false), home.join("OneDrive - Work (2)"));
        assert_eq!(f("Team", true), home.join("SharePoint/Team"));
        fs::create_dir_all(home.join("SharePoint/Team")).unwrap();
        assert_eq!(f("Team", true), home.join("SharePoint/Team (2)"));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn summary_counts() {
        let out = "Number of items to download from Microsoft OneDrive: 3\nNew items to upload to Microsoft OneDrive: 2\nDeleting item from Microsoft OneDrive: ./a/b.txt\nDeleting local file: c.txt ... done\nDeleting the saved application sync status ...\n";
        let s = summarize(out, true, true);
        assert!(s.text.contains("download 3"));
        assert!(s.text.contains("upload 2"));
        assert!(s.text.contains("delete 1 item(s) online"));
        assert!(s.warn.contains("  a/b.txt") && s.warn.contains("  c.txt"));
        assert!(s.text.contains("delete 1 item(s) on this computer"));
        assert!(s.can_apply);
        assert!(!summarize("ERROR: boom", false, false).can_apply);
        assert!(!summarize("", true, false).text.contains("Folders you stop"));
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn window_title_matches_the_ui() {
        // the panel finds the settings window by this title (popup::focus_settings_with_kwin)
        let slint = include_str!("../ui/app.slint");
        assert!(slint.contains(&format!("title: \"{WINDOW_TITLE}\";")));
    }

    #[test]
    fn skip_dirs_are_checked() {
        assert_eq!(clean_skip_dirs(""), Ok(None));
        assert_eq!(clean_skip_dirs("  |  "), Ok(None));
        assert_eq!(
            clean_skip_dirs(" node_modules | Projects/Old "),
            Ok(Some("node_modules|Projects/Old".into()))
        );
        assert!(clean_skip_dirs("a||b").is_err());
        assert!(clean_skip_dirs("a|").is_err());
        assert!(clean_skip_dirs("Temp|.*").is_err());
    }

    #[test]
    fn dry_run_counts_download_only_cleanup() {
        let out = "Attempting removal of local file: ./Notes/mine.txt\nAttempting removal of local directory: ./Scratch\nDeleting local file: Old/notes.txt ... done\n";
        let s = summarize(out, true, false);
        assert!(
            s.text.contains("delete 3 item(s) on this computer"),
            "{}",
            s.text
        );
        assert!(s.warn.contains("Notes/mine.txt"));
    }
}
