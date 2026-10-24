//! The panel a click on the tray icon opens: each account's status, files syncing now and
//! recent files. It runs as its own short-lived process (`--popup [x y]`), so the tray itself
//! stays small; it closes when it loses focus, on Esc, or when the tray icon is clicked again.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::core::{self, AccountInfo, AppSettings, FileActivity, Phase, Status, Transfer};
use crate::ui::{PanelAccount, PanelFile, PopupPanel, Theme};

/// Recent files kept per account, and in the list
const RECENT_MAX: usize = 200;

/// The OneDrive client is installed and new enough (checked once, when the panel opens)
static CLIENT_OK: AtomicBool = AtomicBool::new(true);

/// One read of everything the panel shows.
#[derive(Clone)]
struct Data {
    infos: Vec<AccountInfo>,
    statuses: Vec<Status>,
    settings: AppSettings,
    /// per account, as in `infos`
    files: Vec<(Vec<FileActivity>, Vec<Transfer>)>,
}

fn read_data() -> Data {
    let snap = crate::ui::take_snapshot();
    let files = snap
        .infos
        .iter()
        .map(|i| match &i.unit {
            Some(u) => core::account_files(u, RECENT_MAX),
            None => (Vec::new(), Vec::new()),
        })
        .collect();
    Data {
        infos: snap.infos,
        statuses: snap.statuses,
        settings: snap.settings,
        files,
    }
}

/// A row in one of the file lists, with what the UI doesn't show but acting on it needs.
#[derive(Clone, PartialEq)]
struct Row {
    account: usize,
    path: String,
    folder: bool,
    deleted: bool,
}

struct State {
    data: Data,
    transfers: Vec<Row>,
    recent: Vec<Row>,
    blocked: Vec<Row>,
    blocked_model: Rc<VecModel<PanelFile>>,
    transfer_model: Rc<VecModel<PanelFile>>,
    recent_model: Rc<VecModel<PanelFile>>,
    account_model: Rc<VecModel<PanelAccount>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn ss(s: impl AsRef<str>) -> SharedString {
    s.as_ref().into()
}

/// "Documents/Reports" for a file in it, or "" at the top of the sync folder.
fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn eta_words(eta: &str) -> Option<String> {
    let mut parts = eta.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, s) = (parts.next()??, parts.next()??, parts.next()??);
    let secs = h * 3600 + m * 60 + s;
    if secs < 60 {
        return Some("less than a minute left".into());
    }
    // rounded up to whole minutes, so 59:59 is "1 h left", not "60 min left"
    let mins = secs.div_ceil(60);
    Some(match (mins / 60, mins % 60) {
        (0, m) => format!("{m} min left"),
        (h, 0) => format!("{h} h left"),
        (h, m) => format!("{h} h {m} min left"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_left() {
        assert_eq!(
            eta_words("00:00:30").as_deref(),
            Some("less than a minute left")
        );
        assert_eq!(eta_words("00:01:20").as_deref(), Some("2 min left"));
        assert_eq!(eta_words("00:59:59").as_deref(), Some("1 h left"));
        assert_eq!(eta_words("01:30:00").as_deref(), Some("1 h 30 min left"));
        assert_eq!(eta_words("--:--:--"), None);
    }
}

/// Fills the panel from fresh data, keeping the lists' scroll position and selection.
fn apply(ui: &PopupPanel, data: Data) {
    STATE.with(|st| {
        let mut st = st.borrow_mut();
        let Some(st) = st.as_mut() else { return };
        let many = data.infos.len() > 1;

        // accounts
        let accounts: Vec<PanelAccount> = data
            .infos
            .iter()
            .zip(&data.statuses)
            .map(|(i, s)| PanelAccount {
                name: ss(&i.name),
                status: ss(s.text(&data.settings.prefs(&i.confdir))),
                tone: crate::ui::tone(s),
                personal: i.kind == "personal",
            })
            .collect();
        update_model(&st.account_model, accounts);

        // the summary under the title
        let problems = data
            .statuses
            .iter()
            .filter(|s| crate::ui::tone(s) == 2)
            .count();
        let transfers_now: usize = data.files.iter().map(|f| f.1.len()).sum();
        let syncing = data
            .statuses
            .iter()
            .any(|s| matches!(s.phase, Phase::Syncing | Phase::Resyncing));
        let (summary, tone) = if data.infos.is_empty() {
            ("No accounts yet".to_string(), 3)
        } else if problems == 1 && !many {
            (accounts_status(&data, 0), 2)
        } else if problems > 0 {
            (
                if problems == 1 {
                    "1 account needs attention".to_string()
                } else {
                    format!("{problems} accounts need attention")
                },
                2,
            )
        } else if transfers_now > 0 {
            (
                if transfers_now == 1 {
                    "Syncing 1 large file".to_string()
                } else {
                    format!("Syncing {transfers_now} large files")
                },
                1,
            )
        } else if syncing {
            ("Syncing…".to_string(), 1)
        } else if data
            .statuses
            .iter()
            .all(|s| matches!(s.phase, Phase::Paused | Phase::Off | Phase::Stopped))
        {
            ("Not syncing right now".to_string(), 3)
        } else {
            ("Up to date".to_string(), 0)
        };
        ui.set_summary(ss(summary));
        ui.set_summary_tone(tone);
        // as the tray's "Sync now": signed in, no resync running, and the client there
        ui.set_can_sync(
            CLIENT_OK.load(Ordering::Relaxed)
                && data
                    .infos
                    .iter()
                    .zip(&data.statuses)
                    .any(|(i, s)| i.signed_in && s.phase != Phase::Resyncing),
        );
        // a newer version the tray found (it notes the one it told the user about)
        let update = data
            .settings
            .update_notified
            .clone()
            .filter(|v| {
                data.settings.check_updates.unwrap_or(true)
                    && crate::update::is_newer(v, env!("CARGO_PKG_VERSION"))
            })
            .unwrap_or_default();
        ui.set_update(ss(update));
        ui.set_any_paused(
            data.infos
                .iter()
                .any(|i| data.settings.prefs(&i.confdir).paused()),
        );

        // the account picker for recent files: "All accounts", then each one
        let mut filters = vec![ss("All accounts")];
        filters.extend(data.infos.iter().map(|i| ss(&i.name)));
        let old_filters: Vec<SharedString> = ui.get_filters().iter().collect();
        if old_filters != filters {
            // keep the chosen account if it's still there
            let chosen = old_filters.get(ui.get_filter().max(0) as usize).cloned();
            let idx = chosen
                .and_then(|c| filters.iter().position(|f| *f == c))
                .unwrap_or(0);
            ui.set_filters(ModelRc::new(VecModel::from(filters)));
            ui.set_filter(idx as i32);
        }
        let only = match ui.get_filter() {
            f if f > 0 => Some(f as usize - 1),
            _ => None,
        };

        // files that didn't sync (every account)
        let mut blocked: Vec<(Row, PanelFile)> = Vec::new();
        for (ai, s) in data.statuses.iter().enumerate() {
            for b in &s.blocked {
                let mut detail = vec![b.reason.short().to_string()];
                if many {
                    detail.push(data.infos[ai].name.clone());
                }
                blocked.push((
                    Row {
                        account: ai,
                        path: b.path.clone(),
                        folder: false,
                        deleted: false,
                    },
                    PanelFile {
                        name: ss(core::shown_name(&b.path)),
                        detail: ss(detail.join(" · ")),
                        kind: core::file_kind(&b.path, false),
                        percent: -1,
                        can_open: true,
                    },
                ));
            }
        }
        let keep_bl = selected_path(&st.blocked, ui.get_bl_current());
        st.blocked = blocked.iter().map(|b| b.0.clone()).collect();
        update_model(
            &st.blocked_model,
            blocked.into_iter().map(|b| b.1).collect(),
        );
        ui.set_bl_current(reselect(&st.blocked, keep_bl, ui.get_bl_current()));

        // syncing now (every account)
        let mut transfers: Vec<(Row, PanelFile)> = Vec::new();
        for (ai, (_, list)) in data.files.iter().enumerate() {
            for t in list {
                let mut detail = vec![format!(
                    "{} · {}%",
                    if t.up { "Uploading" } else { "Downloading" },
                    t.percent
                )];
                if let Some(e) = t.eta.as_deref().and_then(eta_words) {
                    detail.push(e);
                }
                if many {
                    detail.push(data.infos[ai].name.clone());
                }
                transfers.push((
                    Row {
                        account: ai,
                        path: t.path.clone(),
                        folder: false,
                        deleted: false,
                    },
                    PanelFile {
                        name: ss(core::shown_name(&t.path)),
                        detail: ss(detail.join(" · ")),
                        kind: core::file_kind(&t.path, false),
                        percent: t.percent as i32,
                        can_open: false,
                    },
                ));
            }
        }
        let keep_tr = selected_path(&st.transfers, ui.get_tr_current());
        st.transfers = transfers.iter().map(|t| t.0.clone()).collect();
        update_model(
            &st.transfer_model,
            transfers.into_iter().map(|t| t.1).collect(),
        );
        ui.set_tr_current(reselect(&st.transfers, keep_tr, ui.get_tr_current()));

        // recent files, newest first across the chosen accounts
        let mut recent: Vec<(&FileActivity, usize)> = data
            .files
            .iter()
            .enumerate()
            .filter(|(ai, _)| only.is_none_or(|o| o == *ai))
            .flat_map(|(ai, f)| f.0.iter().map(move |r| (r, ai)))
            .collect();
        recent.sort_by_key(|r| std::cmp::Reverse(r.0.when));
        recent.truncate(RECENT_MAX);
        let rows: Vec<(Row, PanelFile)> = recent
            .into_iter()
            .map(|(r, ai)| {
                let folder = r.kind == "+";
                let deleted = r.kind == "✕";
                let what = match r.kind {
                    "↑" => "Uploaded",
                    "↓" => "Downloaded",
                    "✕" => "Deleted",
                    _ => "New folder",
                };
                let mut detail = vec![format!("{what} {}", core::short_time(r.when))];
                let dir = folder_of(&r.path);
                if !dir.is_empty() {
                    detail.push(format!("in {}", core::shown_path(dir)));
                }
                if many && only.is_none() {
                    detail.push(data.infos[ai].name.clone());
                }
                (
                    Row {
                        account: ai,
                        path: r.path.clone(),
                        folder,
                        deleted,
                    },
                    PanelFile {
                        name: ss(core::shown_name(&r.path)),
                        detail: ss(detail.join(" · ")),
                        kind: core::file_kind(&r.path, folder),
                        percent: -1,
                        can_open: !deleted,
                    },
                )
            })
            .collect();
        let keep = selected_path(&st.recent, ui.get_rec_current());
        st.recent = rows.iter().map(|r| r.0.clone()).collect();
        update_model(&st.recent_model, rows.into_iter().map(|r| r.1).collect());
        ui.set_rec_current(reselect(&st.recent, keep, ui.get_rec_current()));
        st.data = data;
    });
}

fn accounts_status(data: &Data, i: usize) -> String {
    data.statuses[i].text(&data.settings.prefs(&data.infos[i].confdir))
}

fn selected_path(rows: &[Row], current: i32) -> Option<(usize, String)> {
    let r = rows.get(usize::try_from(current).ok()?)?;
    Some((r.account, r.path.clone()))
}

/// The selection follows its file when the list changes; gone, it stays put (or clears).
fn reselect(rows: &[Row], keep: Option<(usize, String)>, current: i32) -> i32 {
    if let Some((a, p)) = keep
        && let Some(i) = rows.iter().position(|r| r.account == a && r.path == p)
    {
        return i as i32;
    }
    if current >= rows.len() as i32 {
        rows.len() as i32 - 1
    } else {
        current
    }
}

/// Replaces a model's rows, changing only what differs, so the list doesn't jump.
fn update_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, new: Vec<T>) {
    if model.row_count() == new.len() {
        for (i, row) in new.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        model.set_vec(new);
    }
}

/// The file a row is about, if its path stays inside the account's sync folder. Paths come from
/// the log, and other people can name the files shared with this account.
fn file_path(info: &AccountInfo, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if !rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    Some(info.sync_dir.join(rel))
}

/// Opening these would run something (or install it) rather than show it: they are shown in
/// their folder instead.
fn runs_when_opened(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    const RISKY: [&str; 18] = [
        "desktop",
        "appimage",
        "exe",
        "msi",
        "bat",
        "cmd",
        "com",
        "scr",
        "jar",
        "sh",
        "run",
        "bin",
        "deb",
        "rpm",
        "flatpakref",
        "flatpakrepo",
        "apk",
        "lnk",
    ];
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if RISKY.contains(&ext.as_str()) {
        return true;
    }
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Opens a file from a list, or shows it in its folder when opening isn't right for it. Says
/// what was opened, so its window can be brought forward (see `focus_opened_with_kwin`).
fn open_row(row: &Row, info: &AccountInfo, reveal: bool) -> Opened {
    let Some(p) = file_path(info, &row.path) else {
        return Opened::default();
    };
    // still inside the sync folder once links are followed
    let inside = match (p.canonicalize(), info.sync_dir.canonicalize()) {
        (Ok(real), Ok(root)) => real.starts_with(root),
        _ => false,
    };
    if reveal || row.deleted || !inside || runs_when_opened(&p) {
        core::show_in_folder(&p);
        // its folder, or the nearest one still there
        Opened::folder(
            p.parent()
                .and_then(|d| d.ancestors().find(|a| a.is_dir()))
                .and_then(folder_name),
        )
    } else if p.is_dir() {
        core::open_path(&p);
        Opened::folder(folder_name(&p))
    } else {
        // a file in its program
        let app = default_app(&p);
        core::open_path(&p);
        Opened { folder: None, app }
    }
}

/// What the panel opened, so its window can be found and brought forward.
#[derive(Default)]
struct Opened {
    /// the folder a file manager shows
    folder: Option<String>,
    /// the desktop file (without ".desktop") of the program a file opens in
    app: Option<String>,
}

impl Opened {
    /// A folder, shown in the file manager (whichever is the default for folders).
    fn folder(folder: Option<String>) -> Self {
        Opened {
            folder,
            app: default_app_for("inode/directory"),
        }
    }
}

/// The program that opens `p` by default, as its desktop file name ("org.kde.okular").
fn default_app(p: &Path) -> Option<String> {
    let run = |args: &[&std::ffi::OsStr]| -> Option<String> {
        let o = Command::new("xdg-mime")
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (o.status.success() && !s.is_empty()).then_some(s)
    };
    let mime = run(&["query".as_ref(), "filetype".as_ref(), p.as_os_str()])?;
    default_app_for(&mime)
}

/// The program that opens files of type `mime` by default, as its desktop file name.
fn default_app_for(mime: &str) -> Option<String> {
    let o = Command::new("xdg-mime")
        .args(["query", "default", mime])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let desktop = String::from_utf8_lossy(&o.stdout).trim().to_string();
    // several may be listed; the first is the default
    let first = desktop.split(';').next()?.trim();
    Some(first.strip_suffix(".desktop").unwrap_or(first).to_string()).filter(|d| !d.is_empty())
}

fn folder_name(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Brings what the panel just opened to the front on KDE Plasma, where the click's token isn't
/// always enough. For a few seconds it raises the first window of the program that opened it
/// (the default file manager or the file's program, when known) that: is new; asks for attention
/// (what KWin does when a program in the background is refused the front: Dolphin opening the
/// folder in a tab, a program reusing its window); or is a Dolphin window that shows, or switches
/// to, the folder. Under a fixed name, so the next panel unloads one still waiting.
fn focus_opened_with_kwin(opened: &Opened) {
    const NAME: &str = "drivedeck-focus-opened";
    crate::dbus::kwin_unload_script(NAME);
    // a JSON string is a JavaScript string; the two separators JSON leaves alone are escaped too
    let js = |s: Option<&str>| {
        serde_json::to_string(s.unwrap_or(""))
            .unwrap_or_else(|_| "\"\"".into())
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029")
    };
    let folder = js(opened.folder.as_deref());
    let app = js(opened.app.as_deref());
    let script = format!(
        r#"(function () {{
    const folder = {folder};
    // the program a file opens in, when known: only its windows are brought forward
    const app = {app};
    const panel = {pid};
    // the file manager or program that opened it, when known; otherwise any window
    function ours(w) {{
        return app === "" || w.desktopFileName == app;
    }}
    const start = Date.now();
    const watched = [];
    let finished = false;
    function finish() {{
        if (finished) return;
        finished = true;
        workspace.windowAdded.disconnect(added);
        for (const [w, attention, caption] of watched) {{
            w.demandsAttentionChanged.disconnect(attention);
            w.captionChanged.disconnect(caption);
        }}
    }}
    function late() {{
        if (Date.now() - start > 6000) {{
            finish();
            return true;
        }}
        return false;
    }}
    function bring(w) {{
        finish();
        w.minimized = false;
        workspace.activeWindow = w;
    }}
    function showsFolder(w) {{
        return folder !== "" && w.resourceClass == "org.kde.dolphin"
            && (w.caption == folder || w.caption.startsWith(folder + " "));
    }}
    // a new window (a file manager window, the file's program)
    function added(w) {{
        if (late() || w.pid === panel || !w.normalWindow || !ours(w)) return;
        bring(w);
    }}
    workspace.windowAdded.connect(added);
    for (const w of workspace.windowList()) {{
        if (w.pid === panel || !w.normalWindow) continue;
        // an open window that was asked to come forward and wasn't let (Dolphin opening the
        // folder in a tab of its window, a program reusing its window); only the program that
        // opened it, so an unrelated one flashing meanwhile isn't raised
        const attention = function () {{
            if (!late() && w.demandsAttention && ours(w)) bring(w);
        }};
        // Dolphin switching an open window to the folder
        const caption = function () {{
            if (!late() && showsFolder(w)) bring(w);
        }};
        w.demandsAttentionChanged.connect(attention);
        w.captionChanged.connect(caption);
        watched.push([w, attention, caption]);
    }}
    // already showing it
    for (const [w] of watched) {{
        if (showsFolder(w)) {{
            bring(w);
            return;
        }}
    }}
}})();
"#,
        pid = std::process::id(),
    );
    let file = core::runtime_dir().join(format!("{NAME}.js"));
    if std::fs::write(&file, script).is_ok() {
        let ok = crate::dbus::kwin_run_script(&file, NAME);
        debug(&format!("focus-opened script: {ok}"));
        let _ = std::fs::remove_file(&file);
    }
}

/// After opening something: on Plasma, see that it comes to the front.
fn bring_forward(opened: Opened) {
    if crate::dbus::kwin_present() {
        focus_opened_with_kwin(&opened);
    }
}

/// A token from the user's latest click or key press here has arrived (see `launch`).
static TOKEN_FRESH: AtomicBool = AtomicBool::new(false);
/// Something is about to be opened (see `launch`): losing focus meanwhile doesn't cancel it.
static LAUNCHING: AtomicBool = AtomicBool::new(false);

thread_local! {
    static WAITING: RefCell<Vec<slint::Timer>> = const { RefCell::new(Vec::new()) };
}

/// Asks for a token for the click or key press just made: our own request, with the press's
/// serial (accepted more widely), else winit's (answered by an ActivationTokenDone event).
fn ask_for_token(w: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::platform::startup_notify::WindowExtStartupNotify;
    let ours = w
        .with_winit_window(crate::activation::request_token)
        .flatten();
    match ours {
        Some(token) => {
            debug(&format!("token (with the press's serial): {token}"));
            core::set_activation_token(token);
            TOKEN_FRESH.store(true, Ordering::Relaxed);
        }
        None => {
            w.with_winit_window(|ww| ww.request_activation_token());
        }
    }
}

/// Runs `f` (which opens something: a window, a file, a folder) once the desktop has handed us a
/// token for the click that asked for it, so what opens comes to the front instead of hiding
/// behind; then closes the panel. Waits half a second at most: without a token it still opens.
fn launch(f: impl FnOnce() + 'static) {
    LAUNCHING.store(true, Ordering::Relaxed);
    let f = RefCell::new(Some(f));
    let started = std::time::Instant::now();
    let t = slint::Timer::default();
    t.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(20),
        move || {
            if !TOKEN_FRESH.load(Ordering::Relaxed)
                && started.elapsed() < Duration::from_millis(500)
            {
                return;
            }
            if let Some(f) = f.borrow_mut().take() {
                f();
                close();
            }
        },
    );
    WAITING.with_borrow_mut(|w| w.push(t));
}

/// Brings the settings window to the front on KDE Plasma, whether it's opening now or was already
/// open behind other windows. The click's activation token alone isn't always enough for KWin,
/// which then only flags the window for attention. Under a fixed name, so the next panel unloads
/// one left waiting (for a window that never opened).
fn focus_settings_with_kwin() {
    const NAME: &str = "drivedeck-focus-settings";
    crate::dbus::kwin_unload_script(NAME);
    let script = format!(
        r#"(function () {{
    function bring(w) {{
        if (w.resourceClass != "{app}" || w.caption != "{title}") return false;
        w.minimized = false;
        workspace.activeWindow = w;
        return true;
    }}
    for (const w of workspace.windowList()) {{
        if (bring(w)) return;
    }}
    const added = function (w) {{
        if (bring(w)) workspace.windowAdded.disconnect(added);
    }};
    workspace.windowAdded.connect(added);
}})();
"#,
        app = core::APP_ID,
        title = crate::ui::WINDOW_TITLE,
    );
    let file = core::runtime_dir().join(format!("{NAME}.js"));
    if std::fs::write(&file, script).is_ok() {
        let ok = crate::dbus::kwin_run_script(&file, NAME);
        debug(&format!("focus script: {ok}"));
        let _ = std::fs::remove_file(&file);
    }
}

fn open_settings(account: Option<&AccountInfo>) {
    let mut c = Command::new(core::self_command());
    core::activate(&mut c);
    debug(&format!(
        "opening settings (token: {})",
        core::current_activation_token().is_some()
    ));
    c.arg("--settings");
    if let Some(a) = account {
        c.arg("--account").arg(a.key());
    }
    if let Ok(mut ch) = c.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || {
            let _ = ch.wait();
        });
        if crate::dbus::kwin_present() {
            focus_settings_with_kwin();
        }
    }
}

fn view_online(info: AccountInfo, page: crate::graph::WebPage) {
    std::thread::spawn(move || {
        let url = crate::graph::token(&info.confdir)
            .and_then(|t| crate::graph::web_page(&t, info.drive_id.as_deref(), page));
        match url {
            Ok(u) => core::run_detached("xdg-open", &[&u]),
            Err(e) => crate::share::notify("Couldn't open OneDrive online", &e),
        }
    });
}

fn with_state<R>(f: impl FnOnce(&State) -> R) -> Option<R> {
    STATE.with(|st| st.borrow().as_ref().map(f))
}

fn close() {
    debug("closing");
    let _ = slint::quit_event_loop();
}

/// With LODG_POPUP_DEBUG set, says what the panel is doing (for testing placement and focus).
fn debug(what: &str) {
    if std::env::var_os("LODG_POPUP_DEBUG").is_some() {
        eprintln!("popup: {what}");
    }
}

// ---------------------------------------------------------------- placement

/// KWin (KDE Plasma) places windows itself, also on Wayland where an app can't: a one-off
/// script puts the panel next to the tray icon (or the mouse), keeps it above other windows and
/// out of the task bar, and gives it focus. Returns the script's name, to unload it later.
fn place_with_kwin(at: Option<(i32, i32)>) -> Option<String> {
    let pid = std::process::id();
    let name = format!("drivedeck-popup-{pid}");
    // numbers only go into the script
    let (px, py) = match at {
        Some((x, y)) => (x.to_string(), y.to_string()),
        None => (
            "workspace.cursorPos.x".into(),
            "workspace.cursorPos.y".into(),
        ),
    };
    let debug_print = if std::env::var_os("LODG_POPUP_DEBUG").is_some() {
        r#"print("lodg-popup placed " + x + "," + y + " " + g.width + "x" + g.height + " in " + area.x + "," + area.y + " " + area.width + "x" + area.height);"#
    } else {
        ""
    };
    let script = format!(
        r#"(function () {{
    const pid = {pid};
    function place(w) {{
        if (w.pid !== pid) return false;
        const area = workspace.clientArea(KWin.MaximizeArea, w);
        const g = w.frameGeometry;
        const m = 8;
        const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
        const x = clamp({px} - g.width / 2, area.x + m, area.x + area.width - g.width - m);
        const y = clamp({py} - g.height / 2, area.y + m, area.y + area.height - g.height - m);
        w.frameGeometry = {{ x: x, y: y, width: g.width, height: g.height }};
        {debug_print}
        w.keepAbove = true;
        w.skipTaskbar = true;
        w.skipSwitcher = true;
        w.skipPager = true;
        workspace.activeWindow = w;
        return true;
    }}
    for (const w of workspace.windowList()) {{
        if (place(w)) return;
    }}
    const added = function (w) {{
        if (place(w)) workspace.windowAdded.disconnect(added);
    }};
    workspace.windowAdded.connect(added);
}})();
"#
    );
    let file = core::runtime_dir().join(format!("{name}.js"));
    std::fs::write(&file, script).ok()?;
    let ok = crate::dbus::kwin_run_script(&file, &name);
    debug(&format!("kwin script {name}: {ok}"));
    let _ = std::fs::remove_file(&file);
    ok.then_some(name)
}

/// Elsewhere on X11 the window can be moved by itself: next to the click, inside its screen.
/// Called with the real window, which only exists once the event loop runs.
fn place_on_x11(w: &slint::winit_030::winit::window::Window, at: (i32, i32)) {
    use slint::winit_030::winit::dpi::PhysicalPosition;
    let (x, y) = at;
    let size = w.outer_size();
    let Some(mon) = w.available_monitors().find(|m| {
        let p = m.position();
        let s = m.size();
        x >= p.x && y >= p.y && x < p.x + s.width as i32 && y < p.y + s.height as i32
    }) else {
        debug("no screen at the click");
        return;
    };
    let (mp, ms) = (mon.position(), mon.size());
    let m = (8.0 * mon.scale_factor()) as i32;
    let (w_, h_) = (size.width as i32, size.height as i32);
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
    // above a click in the lower half (a bottom panel), below one in the upper half
    let ny = if y > mp.y + ms.height as i32 / 2 {
        y - h_ - m
    } else {
        y + m
    };
    let nx = clamp(x - w_ / 2, mp.x + m, mp.x + ms.width as i32 - w_ - m);
    let ny = clamp(ny, mp.y + m, mp.y + ms.height as i32 - h_ - m);
    debug(&format!("placed at {nx},{ny}"));
    w.set_outer_position(PhysicalPosition::new(nx, ny));
}

// ---------------------------------------------------------------- run

pub fn run(at: Option<(i32, i32)>) {
    static READING: AtomicBool = AtomicBool::new(false);
    // The tray closes the panel with SIGTERM (a second click on its icon): close the way Esc
    // does, so the KWin script is unloaded. First thing, so an early click can't leave it loaded.
    static TERM: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_term(_: libc::c_int) {
        TERM.store(true, Ordering::Relaxed);
    }
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
    let ui = match PopupPanel::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("The panel couldn't open: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = slint::set_xdg_app_id(core::APP_ID) {
        eprintln!("couldn't set the application id: {e}");
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    // on Wayland there's always a compositor, so rounded corners can be see-through
    ui.set_rounded(wayland);
    ui.set_summary(ss("Checking OneDrive…"));
    ui.set_summary_tone(1);
    {
        // the desktop's settings service can be slow or stuck: don't hold the panel up for it
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::dbus::appearance());
        });
        let a = rx
            .recv_timeout(Duration::from_millis(250))
            .unwrap_or_default();
        if let Some(dark) = a.dark {
            ui.set_dark(dark);
        }
        let theme = ui.global::<Theme>();
        theme.set_contrast(a.high_contrast);
        theme.set_reduce_motion(a.reduce_motion);
    }

    let account_model = Rc::new(VecModel::<PanelAccount>::default());
    let blocked_model = Rc::new(VecModel::<PanelFile>::default());
    ui.set_blocked(ModelRc::from(blocked_model.clone()));
    let transfer_model = Rc::new(VecModel::<PanelFile>::default());
    let recent_model = Rc::new(VecModel::<PanelFile>::default());
    ui.set_accounts(ModelRc::from(account_model.clone()));
    ui.set_transfers(ModelRc::from(transfer_model.clone()));
    ui.set_recent(ModelRc::from(recent_model.clone()));
    STATE.with(|st| {
        *st.borrow_mut() = Some(State {
            data: Data {
                infos: Vec::new(),
                statuses: Vec::new(),
                settings: AppSettings::default(),
                files: Vec::new(),
            },
            transfers: Vec::new(),
            recent: Vec::new(),
            blocked: Vec::new(),
            blocked_model,
            transfer_model,
            recent_model,
            account_model,
        })
    });

    ui.on_close_panel(close);
    {
        let weak = ui.as_weak();
        ui.on_sync_now(move || {
            let infos: Vec<AccountInfo> = with_state(|s| {
                s.data
                    .infos
                    .iter()
                    .zip(&s.data.statuses)
                    .filter(|(i, st)| i.signed_in && st.phase != Phase::Resyncing)
                    .map(|(i, _)| i.clone())
                    .collect()
            })
            .unwrap_or_default();
            let mut started = 0;
            let mut errors = Vec::new();
            for i in &infos {
                match core::sync_now(i) {
                    Ok(_) => started += 1,
                    Err(e) => errors.push(format!("{}: {e}", i.name)),
                }
            }
            // at once, rather than at the next refresh
            if let Some(ui) = weak.upgrade()
                && !infos.is_empty()
            {
                if errors.is_empty() {
                    ui.set_summary(ss("Starting a sync…"));
                    ui.set_summary_tone(1);
                } else {
                    ui.set_summary(ss(if started == 0 {
                        format!("Couldn't start syncing: {}", errors.join("; "))
                    } else {
                        format!(
                            "Started {started} account(s); some couldn't start: {}",
                            errors.join("; ")
                        )
                    }));
                    ui.set_summary_tone(2);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_pause(move |secs| {
            // as in the tray's menu: the accounts syncing in the background
            let infos: Vec<AccountInfo> = with_state(|s| {
                s.data
                    .infos
                    .iter()
                    .zip(&s.data.statuses)
                    .filter(|(i, st)| i.signed_in && st.unit.enabled)
                    .map(|(i, _)| i.clone())
                    .collect()
            })
            .unwrap_or_default();
            let mut errors = Vec::new();
            for i in &infos {
                if let Err(e) = core::pause(i, secs as i64) {
                    errors.push(format!("{}: {e}", i.name));
                }
            }
            if !errors.is_empty()
                && let Some(ui) = weak.upgrade()
            {
                ui.set_summary(ss(format!(
                    "Couldn't update every pause setting: {}",
                    errors.join("; ")
                )));
                ui.set_summary_tone(2);
            }
        });
    }
    ui.on_settings(|i| {
        let info = with_state(|s| s.data.infos.get(i.max(0) as usize).cloned()).flatten();
        launch(move || open_settings(if i >= 0 { info.as_ref() } else { None }));
    });
    ui.on_open_folder(|i| {
        if let Some(info) = with_state(|s| s.data.infos.get(i.max(0) as usize).cloned()).flatten() {
            launch(move || {
                core::open_path(&info.sync_dir);
                bring_forward(Opened::folder(folder_name(&info.sync_dir)));
            });
        }
    });
    ui.on_get_update(|| {
        launch(|| core::run_detached("xdg-open", &[crate::update::RELEASES_URL]));
    });
    ui.on_view_online(|i, page| {
        let page = match page {
            1 => crate::graph::WebPage::RecycleBin,
            2 => crate::graph::WebPage::Restore,
            _ => crate::graph::WebPage::Home,
        };
        if let Some(info) = with_state(|s| s.data.infos.get(i.max(0) as usize).cloned()).flatten() {
            launch(move || view_online(info, page));
        }
    });
    let act = |transfer: bool, i: i32, reveal: bool| {
        let found = with_state(|s| {
            let rows = if transfer { &s.transfers } else { &s.recent };
            let row = rows.get(usize::try_from(i).ok()?)?.clone();
            let info = s.data.infos.get(row.account)?.clone();
            Some((row, info))
        })
        .flatten();
        if let Some((row, info)) = found {
            // a file still on its way isn't complete yet: show where it's going
            launch(move || bring_forward(open_row(&row, &info, reveal || transfer)));
        }
    };
    ui.on_open_file(move |t, i| act(t, i, false));
    ui.on_reveal_file(move |t, i| act(t, i, true));
    // a file that didn't sync: shown in its folder (it may be the one being edited elsewhere)
    ui.on_reveal_blocked(|i| {
        let found = with_state(|s| {
            let row = s.blocked.get(usize::try_from(i).ok()?)?.clone();
            let info = s.data.infos.get(row.account)?.clone();
            Some((row, info))
        })
        .flatten();
        if let Some((row, info)) = found {
            launch(move || bring_forward(open_row(&row, &info, true)));
        }
    });
    {
        let weak = ui.as_weak();
        ui.on_filter_changed(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_rec_current(-1);
            // the same data, filtered again
            if let Some(d) = with_state(|s| s.data.clone()) {
                apply(&ui, d);
            }
        });
    }

    // fresh data every two seconds, read off the event loop (one read at a time)
    READING.store(true, Ordering::Relaxed);
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let client_ok = matches!(core::check_client(), core::ClientCheck::Ok { .. });
        let data = read_data();
        if slint::invoke_from_event_loop(move || {
            READING.store(false, Ordering::Relaxed);
            CLIENT_OK.store(client_ok, Ordering::Relaxed);
            if let Some(ui) = weak.upgrade() {
                apply(&ui, data);
            }
        })
        .is_err()
        {
            READING.store(false, Ordering::Relaxed);
        }
    });
    let refresh = slint::Timer::default();
    {
        let weak = ui.as_weak();
        refresh.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            move || {
                // README screenshots: the made-up data stays put
                if std::env::var_os("LODG_DEMO").is_some() || READING.swap(true, Ordering::Relaxed)
                {
                    return;
                }
                let weak = weak.clone();
                std::thread::spawn(move || {
                    let data = read_data();
                    let _ = slint::invoke_from_event_loop(move || {
                        READING.store(false, Ordering::Relaxed);
                        if let Some(ui) = weak.upgrade() {
                            apply(&ui, data);
                        }
                    });
                });
            },
        );
    }

    let kwin_script = if crate::dbus::kwin_present() {
        place_with_kwin(at)
    } else {
        None
    };
    // X11 without KWin: placed by us, once the window exists
    let mut x11_at = if kwin_script.is_none() && !wayland {
        at
    } else {
        None
    };

    // closes once it has had focus and loses it (a click elsewhere, another window opening)
    static FOCUSED: AtomicBool = AtomicBool::new(false);
    let placed_script = kwin_script.clone();
    {
        use slint::winit_030::winit::event::WindowEvent;
        use slint::winit_030::{EventResult, WinitWindowAccessor};
        ui.window().on_winit_window_event(move |w, ev| {
            if let Some(at) = x11_at.take() {
                w.with_winit_window(|ww| place_on_x11(ww, at));
            }
            w.with_winit_window(crate::activation::track);
            // each click or key press asks the desktop for a token to bring what it opens to
            // the front (see `launch`)
            use slint::winit_030::winit::event::ElementState;
            use slint::winit_030::winit::keyboard::{Key, NamedKey};
            match ev {
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    ..
                } => {
                    TOKEN_FRESH.store(false, Ordering::Relaxed);
                    ask_for_token(w);
                }
                // only the keys that open things (not every arrow key on a list)
                WindowEvent::KeyboardInput { event, .. }
                    if event.state.is_pressed()
                        && matches!(
                            event.logical_key,
                            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space)
                        ) =>
                {
                    TOKEN_FRESH.store(false, Ordering::Relaxed);
                    ask_for_token(w);
                }
                WindowEvent::ActivationTokenDone { token, .. } => {
                    debug("activation token arrived");
                    core::set_activation_token(token.clone().into_raw());
                    TOKEN_FRESH.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
            if let WindowEvent::Focused(f) = ev {
                debug(&format!("focused {f}"));
                if *f {
                    // placed and in front by now: KWin needn't keep the script
                    if !FOCUSED.swap(true, Ordering::Relaxed)
                        && let Some(name) = &placed_script
                    {
                        crate::dbus::kwin_unload_script(name);
                    }
                } else if FOCUSED.load(Ordering::Relaxed) && !LAUNCHING.load(Ordering::Relaxed) {
                    // (an action still waiting for its token closes the panel itself)
                    close();
                }
            }
            EventResult::Propagate
        });
    }

    let term_watch = slint::Timer::default();
    term_watch.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        || {
            if TERM.load(Ordering::Relaxed) {
                close();
            }
        },
    );

    #[cfg(feature = "test-driver")]
    let _driver = crate::testdrive::start(&ui);
    if let Err(e) = ui.show() {
        eprintln!("The panel couldn't open: {e}");
        std::process::exit(1);
    }
    // the script has done its work once the window is up
    let unload = kwin_script.clone().map(|name| {
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(3),
            move || {
                crate::dbus::kwin_unload_script(&name);
            },
        );
        t
    });
    // The token the tray got with the click on its icon brings the panel to the front with focus
    // (GNOME needs it, or the panel opens behind and never knows a click elsewhere happened).
    // Used once the window is on screen: given while it's being made, GNOME ignores it.
    let startup = core::startup_token();
    let activate_once = slint::Timer::default();
    if let Some(token) = startup {
        let weak = ui.as_weak();
        activate_once.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(150),
            move || {
                if let Some(ui) = weak.upgrade() {
                    use slint::winit_030::WinitWindowAccessor;
                    debug("activating with the tray's token");
                    ui.window()
                        .with_winit_window(|w| crate::activation::activate(w, &token));
                }
            },
        );
    }
    let _ = slint::run_event_loop();
    drop(unload);
    let _ = ui.hide();
    // closed within those seconds: unload it now (unloading twice does nothing)
    if let Some(name) = kwin_script {
        crate::dbus::kwin_unload_script(&name);
    }
}
