//! The settings window: accounts, sign-in, folders, options, activity and the resync preview.
//!
//! All slow work (systemctl, the client, Microsoft Graph) runs on worker threads via [`bg`];
//! results come back to the UI thread, where the shared [`Ctx`] lives.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use slint::{ComponentHandle, ModelRc, SharedString, StandardListViewItem, VecModel};

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
    pid: Arc<AtomicU32>,
    cancelled: Arc<AtomicBool>,
    running: bool,
    folders_changed: bool,
    _busy: core::BusyMarker,
}

struct Snapshot {
    settings: AppSettings,
    infos: Vec<AccountInfo>,
    statuses: Vec<Status>,
}

#[derive(Clone)]
struct CachedToken {
    access: String,
    expires: Instant,
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
    /// The user closed the window while a dry run was still stopping; close once it has.
    close_when_idle: bool,
    rows: Rc<VecModel<FolderRow>>,
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
    })
}

fn as_graph(t: &CachedToken) -> graph::Token {
    graph::Token {
        access: t.access.clone(),
        expires: t.expires,
    }
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

// ---------------------------------------------------------------- status refresh

fn log_state(unit: Option<&str>) -> (Phase, Option<chrono::DateTime<Local>>, Option<String>) {
    let Some(unit) = unit else {
        return (Phase::Unknown, None, None);
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
            Some(LogEvent::NeedsLogin) => {
                phase = Phase::SignedOut;
                err = Some("Microsoft needs you to sign in again.".into());
            }
            _ => {}
        }
    }
    (phase, last, err)
}

fn take_snapshot() -> Snapshot {
    let settings = AppSettings::load();
    let infos = core::discover(&settings);
    let statuses = infos
        .iter()
        .map(|i| {
            let unit = i.unit.as_deref().map(core::unit_state).unwrap_or_default();
            let resyncing = core::unit_state(&core::resync_unit(&i.confdir)).active;
            let (log_phase, last_complete, last_error) = log_state(i.unit.as_deref());
            let prefs = settings.prefs(&i.confdir);
            Status {
                phase: core::compose_phase(i, &prefs, &unit, resyncing, log_phase),
                last_complete,
                last_error,
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

fn tone(p: Phase) -> i32 {
    match p {
        Phase::UpToDate => 0,
        Phase::Syncing | Phase::Resyncing | Phase::Unknown => 1,
        Phase::Error | Phase::NeedsResync | Phase::SignedOut => 2,
        Phase::Off | Phase::Stopped | Phase::Paused => 3,
    }
}

fn refresh(ctx: &Shared) {
    // demo screenshots fill the window with made-up data that live refreshes would overwrite
    #[cfg(feature = "test-driver")]
    if std::env::var_os("LODG_DEMO").is_some() {
        return;
    }
    if std::mem::replace(&mut ctx.borrow_mut().refreshing, true) {
        return;
    }
    bg(take_snapshot, |ctx, snap| {
        ctx.borrow_mut().refreshing = false;
        apply_snapshot(ctx, snap);
    });
}

fn apply_snapshot(ctx: &Shared, snap: Snapshot) {
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
            kind: ss(a.kind_label()),
            status: ss(s.text(&c.settings.prefs(&a.confdir))),
            tone: tone(s.phase),
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
    ui.set_s_status(ss(s.text(&prefs)));
    ui.set_s_tone(tone(s.phase));
    ui.set_s_last(ss(s
        .last_complete
        .map(core::short_time)
        .unwrap_or_else(|| "—".into())));
    let err = match s.phase {
        Phase::Error => s.last_error.clone().unwrap_or_else(|| "The sync service stopped with an error. Try \"Sync now\".".into()),
        Phase::NeedsResync => "The sync engine needs a full resync before it can continue. Open the Folders tab and press \"Apply changes…\", or turn on \"Allow full resyncs\" in Options and save.".into(),
        Phase::SignedOut if a.signed_in => "Microsoft no longer accepts the saved sign-in (this happens after a password change, or when access is revoked). Sign in again to continue.".into(),
        _ => String::new(),
    };
    ui.set_s_error(ss(err));
    ui.set_s_signed_in(a.signed_in && s.phase != Phase::SignedOut);
    ui.set_s_bg_enabled(s.unit.enabled);
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

fn load_options(ctx: &Shared, acc: &AccountInfo) {
    let c = ctx.borrow();
    let ui = c.ui();
    let cfg = ClientConfig::load(&acc.confdir);
    let prefs = c.settings.prefs(&acc.confdir);
    let interval: u32 = cfg
        .get("monitor_interval")
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    ui.set_o_interval(
        INTERVALS
            .iter()
            .position(|v| *v >= interval)
            .unwrap_or(INTERVALS.len() - 1) as i32,
    );
    let rate: u64 = cfg
        .get("rate_limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rate_idx = if rate == 0 {
        0
    } else {
        RATES_MB
            .iter()
            .position(|m| *m * 1_048_576 >= rate)
            .unwrap_or(RATES_MB.len() - 1)
    };
    ui.set_o_rate(rate_idx as i32);
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
    ui.set_o_msg(ss(""));
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

    let cfg = ClientConfig::load(&acc.confdir);
    let mut plain: Vec<(String, Option<String>)> = Vec::new();
    let mut resync: Vec<(String, Option<String>)> = Vec::new();
    let want = |list: &mut Vec<(String, Option<String>)>, key: &str, val: Option<String>| {
        if cfg.get(key) != val {
            list.push((key.to_string(), val));
        }
    };
    let interval = INTERVALS[ui.get_o_interval().clamp(0, 5) as usize];
    want(
        &mut plain,
        "monitor_interval",
        (interval != 300)
            .then(|| interval.to_string())
            .or_else(|| cfg.get("monitor_interval").map(|_| "300".into())),
    );
    let rate = RATES_MB[ui.get_o_rate().clamp(0, 6) as usize] * 1_048_576;
    want(
        &mut plain,
        "rate_limit",
        (rate > 0).then(|| rate.to_string()),
    );
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
    want(
        &mut resync,
        "skip_dotfiles",
        ui.get_o_skip_hidden().then(|| "true".into()),
    );
    let skip = ui.get_o_skip_files().trim().to_string();
    want(
        &mut resync,
        "skip_file",
        (!skip.is_empty() && skip != DEFAULT_SKIP_FILE).then_some(skip),
    );
    // treat "absent" and "the default" as the same thing so we don't trigger needless resyncs
    let is_default = |(k, v): &(String, Option<String>)| match k.as_str() {
        "skip_dotfiles" => v.is_none() && !cfg.get_bool("skip_dotfiles"),
        "skip_file" => v.is_none() && cfg.get("skip_file").is_none_or(|x| x == DEFAULT_SKIP_FILE),
        _ => false,
    };
    resync.retain(|ch| !is_default(ch));

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
    if resync.is_empty() {
        ui.set_o_msg(ss(if plain.is_empty() {
            "Saved."
        } else {
            "Saved. The sync engine restarts to pick up the changes."
        }));
        refresh(ctx);
        return;
    }
    if !ui.get_o_allow_resync() {
        ui.set_o_msg(ss("Saved, except the settings marked * — they need a full resync, which is turned off for this account."));
        return;
    }
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
        },
    );
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
    {
        let mut c = ctx.borrow_mut();
        if !force && c.tree.loaded_for.as_deref() == Some(key.as_str()) {
            return;
        }
        let rules = core::read_sync_list(&acc.confdir);
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
        },
    );
}

// ---------------------------------------------------------------- resync preview

struct PreviewChange {
    sync_list: Option<Vec<String>>,
    config: Vec<(String, Option<String>)>,
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
    let pid = Arc::new(AtomicU32::new(0));
    let cancelled = Arc::new(AtomicBool::new(false));
    let confdir = acc.confdir.clone();
    let snapshot: Vec<(PathBuf, Option<Vec<u8>>)> = SNAPSHOT_FILES
        .iter()
        .map(|f| (confdir.join(f), fs::read(confdir.join(f)).ok()))
        .collect();
    {
        let ui = ctx.borrow().ui();
        ui.set_p_title(ss(title));
        ui.set_p_text(ss("Checking what a full resync would do. Nothing is changed while this runs — it's a dry run. Large accounts can take several minutes."));
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
        pid: pid.clone(),
        cancelled: cancelled.clone(),
        running: true,
        folders_changed: change.sync_list.is_some(),
        _busy: core::BusyMarker::new(&acc.confdir),
    });
    if let Some(p) = ctx.borrow().preview.as_ref() {
        save_pending_preview(p);
    }
    bg(
        move || -> Result<bool, String> {
            if let Some(u) = &unit {
                core::systemctl(&["stop", u]);
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
            cmd.arg("--confdir").arg(&confdir).args([
                "--sync",
                "--resync",
                "--resync-auth",
                "--dry-run",
                "--verbose",
            ]);
            let (mut child, _, readers) = spawn_capture(cmd, out, false)
                .map_err(|e| format!("Couldn't start the sync engine: {e}"))?;
            pid.store(child.id(), Ordering::SeqCst);
            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            finish_reading(readers);
            pid.store(0, Ordering::SeqCst);
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
            let ui = c.ui();
            ui.set_p_busy(false);
            match r {
                Ok(ok) => {
                    let s = summarize(&output, ok, folders_changed);
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

fn restore_snapshot(p: &Preview) {
    restore_files(&p.snapshot);
    clear_pending_preview();
}

fn restore_files(files: &[(PathBuf, Option<Vec<u8>>)]) {
    for (path, data) in files {
        match data {
            Some(d) => {
                let _ = core::atomic_write(path, d);
            }
            None => {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// A preview's trial settings, written down until it's applied or cancelled, so a crash or a
/// logout in the middle doesn't leave them in place (the client would then demand a resync).
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingPreview {
    window_pid: u32,
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    unit: Option<String>,
    was_active: bool,
}

fn pending_preview_file() -> PathBuf {
    core::app_dir().join("preview-in-progress.json")
}

fn save_pending_preview(p: &Preview) {
    let pending = PendingPreview {
        window_pid: std::process::id(),
        files: p.snapshot.clone(),
        unit: p.unit.clone(),
        was_active: p.was_active,
    };
    if let Ok(json) = serde_json::to_vec(&pending) {
        let _ = core::create_private_dir(&core::app_dir());
        let _ = core::atomic_write(&pending_preview_file(), &json);
    }
}

fn clear_pending_preview() {
    let _ = fs::remove_file(pending_preview_file());
}

/// Puts back the settings of a preview whose window went away without applying or cancelling
/// it. Does nothing while that window is still running.
pub fn recover_interrupted_preview() {
    let Ok(data) = fs::read(pending_preview_file()) else {
        return;
    };
    let Ok(p) = serde_json::from_slice::<PendingPreview>(&data) else {
        clear_pending_preview();
        return;
    };
    if p.window_pid != std::process::id() && Path::new(&format!("/proc/{}", p.window_pid)).exists()
    {
        return;
    }
    restore_files(&p.files);
    clear_pending_preview();
    if p.was_active
        && let Some(u) = &p.unit
    {
        core::systemctl(&["start", u]);
    }
}

fn finish_cancel(ctx: &Shared) {
    let Some(p) = ctx.borrow_mut().preview.take() else {
        return;
    };
    restore_snapshot(&p);
    if p.was_active
        && let Some(u) = &p.unit
    {
        core::unit_action("start", u);
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
        let _ = ctx.borrow().ui().hide();
    }
}

fn preview_cancel(ctx: &Shared) {
    let running = {
        let c = ctx.borrow();
        let Some(p) = c.preview.as_ref() else { return };
        p.cancelled.store(true, Ordering::SeqCst);
        let pid = p.pid.load(Ordering::SeqCst);
        if pid != 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
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
        return;
    };
    let job = core::resync_unit(&acc.confdir);
    let client = core::client_binary().unwrap_or_else(|| PathBuf::from("onedrive"));
    let mut script = format!(
        "{} --confdir {} --sync --resync --resync-auth",
        sh_quote(&client.to_string_lossy()),
        sh_quote(&acc.confdir.to_string_lossy())
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
    core::systemctl(&["reset-failed", &job]);
    let r = Command::new("systemd-run")
        .args([
            "--user",
            "--collect",
            "--quiet",
            &format!("--unit={job}"),
            &format!(
                "--description=OneDrive full resync ({})",
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
            clear_pending_preview(); // the new settings are now meant to stay
            ui.set_s_msg(ss(format!(
                "{}: full resync started. The status shows when it's done.",
                p.title
            )));
            ui.set_o_msg(ss(""));
            ui.set_f_msg(ss("Resync started."));
        }
        Ok(o) => {
            restore_snapshot(&p);
            ui.set_s_msg(ss(format!(
                "Couldn't start the resync: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )));
        }
        Err(e) => {
            restore_snapshot(&p);
            ui.set_s_msg(ss(format!("Couldn't start the resync: {e}")));
        }
    }
    load_folders(ctx, &acc, true);
    refresh(ctx);
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
    if let Some(u) = &acc.unit {
        core::systemctl(&["stop", u]);
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
            ui.set_s_msg(ss(format!("Couldn't start the sync engine: {e}")));
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
        finished: false,
        _busy: core::BusyMarker::new(&acc.confdir),
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
            .filter(|s| !s.is_empty());
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
    si.finished = true;
    let key = si.key.clone();
    let enable_after = si.enable_after;
    let acc = c.account(&key).cloned();
    let Some(acc) = acc else { return };
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
        after_sign_in(acc, enable_after);
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
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(core::APP_ID).join("shim");
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

fn after_sign_in(acc: AccountInfo, enable: bool) {
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
            if enable || core::unit_state(&unit).enabled {
                let (ok, msg) = core::systemctl(&["enable", "--now", &unit]);
                if !ok {
                    return Err(msg);
                }
                set_autostart(true);
                start_tray();
            }
            Ok(())
        },
        |ctx, r: Result<(), String>| {
            if let Err(e) = r {
                ctx.borrow().ui().set_s_msg(ss(format!(
                    "Signed in, but background sync couldn't start: {e}"
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
        if !si.finished {
            let _ = si.child.kill();
        }
        let _ = si.child.wait();
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
        move || {
            if let Some(u) = &acc.unit {
                core::systemctl(&["disable", "--now", u]);
            }
            let _ = core::client_command()
                .arg("--confdir")
                .arg(&acc.confdir)
                .arg("--logout")
                .stdin(Stdio::null())
                .output();
            let _ = fs::remove_file(acc.confdir.join("refresh_token"));
            acc.key()
        },
        |ctx, key| {
            {
                let mut c = ctx.borrow_mut();
                c.tokens.remove(&key);
                let ui = c.ui();
                ui.set_s_busy(false);
                ui.set_s_msg(ss("Signed out. Your files are still on this computer."));
            }
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
            if let Some(u) = &acc.unit {
                core::systemctl(&["disable", "--now", u]);
                if acc.managed {
                    let _ = fs::remove_file(core::user_unit_dir().join(u));
                    core::systemctl(&["daemon-reload"]);
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
                fs::remove_dir_all(&acc.confdir)
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
    let start = typed
        .ancestors()
        .find(|p| p.is_absolute() && p.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(core::home)
        .to_string_lossy()
        .to_string();
    bg(
        move || {
            // Err = no folder chooser at all, Ok(None) = the user cancelled
            if let Some(picked) =
                crate::dbus::choose_folder("Choose a folder to sync into", Path::new(&start))
            {
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
        |ctx, picked| {
            let ui = ctx.borrow().ui();
            match picked {
                Ok(Some(p)) => ui.set_add_folder(ss(p)),
                Ok(None) => {}
                Err(()) => ui.set_add_msg(ss(
                    "No folder chooser is available on this desktop. Type the folder's full path instead.",
                )),
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
    if let Some(other) = c.accounts.iter().find(|a| {
        a.sync_dir == folder || folder.starts_with(&a.sync_dir) || a.sync_dir.starts_with(&folder)
    }) {
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
            unit: Default::default(),
        });
        c.settings = s;
        c.accounts.len() - 1
    };
    // through select(), so nothing from the previously shown account lingers on the page
    select(ctx, index as i32);
    if library.is_some() {
        after_sign_in(acc, true);
    } else {
        sign_in(ctx, &acc, true);
    }
    refresh(ctx);
}

// ---------------------------------------------------------------- app page

fn autostart_file() -> PathBuf {
    core::config_home()
        .join("autostart")
        .join(format!("{}.desktop", core::APP_ID))
}

fn set_autostart(on: bool) {
    let f = autostart_file();
    if on {
        let _ = fs::create_dir_all(f.parent().unwrap());
        let body = format!(
            "[Desktop Entry]\nType=Application\nName={}\nComment=OneDrive sync status\nExec={} --tray\nIcon={}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            core::APP_NAME,
            core::desktop_exec(),
            core::APP_ID
        );
        let _ = core::atomic_write(&f, body.as_bytes());
    } else {
        let _ = fs::remove_file(f);
    }
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
    ui.set_app_autostart(autostart_file().exists());
    ui.set_app_version(ss(env!("CARGO_PKG_VERSION")));
    ui.set_app_is_appimage(std::env::var("APPIMAGE").is_ok());
    ui.set_app_msg(ss(""));
    let client = match core::check_client() {
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
    ui.set_app_client(ss(client));
}

/// For AppImages: add a menu entry, icon and Dolphin actions for the current user.
fn add_to_menu(ctx: &Shared) {
    let r = (|| -> std::io::Result<()> {
        let data = core::data_home();
        let icon_dir = data.join("icons/hicolor/scalable/apps");
        fs::create_dir_all(&icon_dir)?;
        fs::write(
            icon_dir.join(format!("{}.svg", core::APP_ID)),
            include_bytes!("../assets/linux-onedrive-gui.svg"),
        )?;
        let apps = data.join("applications");
        fs::create_dir_all(&apps)?;
        let desktop = include_str!("../packaging/linux-onedrive-gui.desktop").replace(
            "Exec=linux-onedrive-gui",
            &format!("Exec={}", core::desktop_exec()),
        );
        fs::write(apps.join(format!("{}.desktop", core::APP_ID)), desktop)?;
        crate::share::install_service_menu(&data)?;
        Ok(())
    })();
    let ui = ctx.borrow().ui();
    ui.set_app_msg(ss(match r {
        Ok(()) => "Added. Look for \"A GUI that works with OneDrive on Linux (unofficial)\" in your applications menu. Right-clicking a file in Dolphin now also shows a OneDrive menu with \"Copy sharing link\" and \"View on the OneDrive website\".".to_string(),
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
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join(core::APP_ID).join("window.sock")
}

/// If a window is already open, asks it to show `account` and returns true.
fn hand_off(account: Option<&str>) -> bool {
    match std::os::unix::net::UnixStream::connect(instance_socket()) {
        Ok(mut s) => s.write_all(account.unwrap_or("").as_bytes()).is_ok(),
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
            let mut key = String::new();
            let _ = conn.read_to_string(&mut key);
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

pub fn run(account: Option<String>) {
    if !claim_window(account.as_deref()) {
        return;
    }
    recover_interrupted_preview();
    core::repair_managed_units();
    core::start_idle_units(&AppSettings::load());
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
    if let Some(dark) = crate::dbus::prefers_dark(std::time::Duration::from_millis(1500)) {
        ui.set_dark(dark);
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
        refreshing: false,
        close_when_idle: false,
        rows,
    }));
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    listen_for_hand_offs();

    // the first load is synchronous so the window opens fully populated, on the right page
    apply_snapshot(&ctx, take_snapshot());
    show_client_check(&ctx, &core::check_client());
    let start = account
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
    select(&ctx, start);
    if !ctx.borrow().accounts.is_empty() {
        start_tray();
    }

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
        let _ = s.save();
        if acc.managed
            && let Some(u) = &acc.unit
        {
            let _ = core::write_managed_unit(u, &name, &acc.confdir);
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
        let mut s = AppSettings::load();
        s.prefs_mut(&acc.confdir).override_until = Local::now().timestamp() + 3600;
        let _ = s.save();
        let unit = acc
            .unit
            .clone()
            .map(Ok)
            .unwrap_or_else(|| core::ensure_unit(&acc));
        match unit {
            Ok(u) => {
                let verb = if core::unit_state(&u).active {
                    "restart"
                } else {
                    "start"
                };
                core::unit_action(verb, &u);
                c.borrow().ui().set_s_msg(ss("Syncing now…"));
            }
            Err(e) => c.borrow().ui().set_s_msg(ss(e.to_string())),
        }
        refresh(c);
    });
    on!(on_set_background, |c, on: bool| {
        let Some(acc) = current(c) else { return };
        c.borrow().ui().set_s_busy(true);
        bg(
            move || -> Result<(), String> {
                let u = core::ensure_unit(&acc).map_err(|e| e.to_string())?;
                let (ok, msg) =
                    core::systemctl(&[if on { "enable" } else { "disable" }, "--now", &u]);
                if on {
                    set_autostart(true);
                    start_tray();
                }
                if ok { Ok(()) } else { Err(msg) }
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
    on!(on_p_cancel, |c| preview_cancel(c));
    on!(on_add_choose, |c, m: i32| add_choose(c, m));
    on!(on_add_browse, |c| add_browse(c));
    on!(on_add_search, |c| add_search(c));
    on!(on_add_lib_picked, |c, i: i32| add_lib_picked(c, i as usize));
    on!(on_add_create, |c| add_create(c));
    on!(on_app_autostart_toggled, |c| {
        let on = c.borrow().ui().get_app_autostart();
        set_autostart(on);
        if on {
            start_tray();
        }
    });
    on!(on_app_add_to_menu, |c| add_to_menu(c));
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
    on!(on_open_online, |c| {
        let Some(acc) = current(c) else { return };
        let cached = c.borrow().token(&acc.key());
        bg(
            move || {
                let t = token_for(&acc.confdir, cached)?;
                graph::web_url(&as_graph(&t), acc.drive_id.as_deref())
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
            match ev {
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    ..
                } => {
                    w.with_winit_window(|ww| ww.request_activation_token());
                }
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
                    refresh(&w);
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
}
