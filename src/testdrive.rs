//! Scripted UI driver for automated testing (built only with `--features test-driver`).
//!
//! Set `LODG_TEST_SCRIPT=<file>` and `LODG_TEST_OUT=<dir>`. Each script line is one step:
//!   wait <seconds>                 pause
//!   shot <name>                    save a screenshot as <dir>/<name>.ppm
//!   call <callback> [int]          invoke a UI callback, e.g. `call select 2`
//!   set <property> <value>         set a property (strings, ints, true/false)
//!   until <property> <text> <secs> wait until a string property contains text (fails on timeout)
//!   dump <property>                print a property's value
//!   selectname <text>              open the first account whose name contains text
//!   demo                           fill the window with made-up accounts (run with LODG_DEMO=1)
//!   key <name>                     press and release a key: tab, backtab, space, return, left,
//!                                  right, up, down, home, end, escape
//!   quit                           close the window
//! Results go to stdout as `STEP …`, `DUMP …` and `FAIL …` lines.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, SharedString};

use crate::ui::{AppWindow, FileWindow, PanelAccount, PanelFile, PopupPanel, VersionRow};

/// A window the script can drive.
pub trait Driven: ComponentHandle + 'static {
    fn get(&self, k: &str) -> Option<String>;
    fn set(&self, k: &str, v: &str) -> bool;
    fn call(&self, name: &str, n: Option<i32>) -> bool;
    /// `selectname`: the settings window only
    fn select_name(&self, _name: &str) -> Option<i32> {
        None
    }
    fn demo(&self) {
        println!("FAIL demo isn't available here");
    }
}

impl Driven for AppWindow {
    fn get(&self, k: &str) -> Option<String> {
        get_str(self, k)
    }
    fn set(&self, k: &str, v: &str) -> bool {
        set(self, k, v)
    }
    fn call(&self, name: &str, n: Option<i32>) -> bool {
        call(self, name, n)
    }
    fn select_name(&self, name: &str) -> Option<i32> {
        use slint::Model;
        let i = self
            .get_accounts()
            .iter()
            .position(|a| a.name.contains(name))? as i32;
        self.invoke_select(i);
        Some(i)
    }
    fn demo(&self) {
        fill_demo(self)
    }
}

impl Driven for FileWindow {
    fn get(&self, k: &str) -> Option<String> {
        Some(match k {
            "msg" => self.get_msg().to_string(),
            "mode" => self.get_mode().to_string(),
            _ => return None,
        })
    }
    fn set(&self, k: &str, v: &str) -> bool {
        match k {
            "dark" => self.set_dark(v == "true"),
            "mode" => self.set_mode(v.parse().unwrap_or(0)),
            "emails" => self.set_emails(v.into()),
            "message" => self.set_message(v.into()),
            "confirm" => self.set_confirm(v.parse().unwrap_or(-1)),
            _ => return false,
        }
        true
    }
    fn call(&self, _name: &str, _n: Option<i32>) -> bool {
        false
    }
    /// A made-up file's history, for README screenshots.
    fn demo(&self) {
        use slint::{ModelRc, VecModel};
        self.set_file_name("Budget 2027.ods".into());
        self.set_account("Personal".into());
        self.set_is_work(false);
        self.set_busy(false);
        self.set_msg("".into());
        let v = |when: &str, detail: &str| VersionRow {
            when: when.into(),
            detail: detail.into(),
        };
        self.set_versions(ModelRc::new(VecModel::from(vec![
            v("Today, 09:41", "48 KB  ·  saved by Alex Rivera"),
            v("Today, 08:15", "47 KB  ·  saved by Alex Rivera"),
            v("Yesterday, 17:02", "45 KB  ·  saved by Sam Lee"),
            v("21 Oct 2026, 11:30", "44 KB  ·  saved by Alex Rivera"),
            v("14 Oct 2026, 16:48", "39 KB  ·  saved by Alex Rivera"),
            v("2 Oct 2026, 10:05", "31 KB  ·  saved by Alex Rivera"),
        ])));
        println!("STEP demo");
    }
}

impl Driven for PopupPanel {
    fn get(&self, k: &str) -> Option<String> {
        use slint::Model;
        let files = |m: slint::ModelRc<PanelFile>| {
            m.iter()
                .map(|f| format!("[{}|{}|kind {}|{}%]", f.name, f.detail, f.kind, f.percent))
                .collect::<Vec<_>>()
                .join(" ")
        };
        Some(match k {
            "summary" => self.get_summary().to_string(),
            "summary_tone" => self.get_summary_tone().to_string(),
            "accounts" => self
                .get_accounts()
                .iter()
                .map(|a: PanelAccount| format!("[{}|{}|tone {}]", a.name, a.status, a.tone))
                .collect::<Vec<_>>()
                .join(" "),
            "transfers" => files(self.get_transfers()),
            "blocked" => files(self.get_blocked()),
            "recent" => files(self.get_recent()),
            "recent_count" => self.get_recent().row_count().to_string(),
            "filters" => self
                .get_filters()
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join("|"),
            "filter" => self.get_filter().to_string(),
            "any_paused" => self.get_any_paused().to_string(),
            "update" => self.get_update().to_string(),
            "can_sync" => self.get_can_sync().to_string(),
            "rec_current" => self.get_rec_current().to_string(),
            "tr_current" => self.get_tr_current().to_string(),
            "acc_current" => self.get_acc_current().to_string(),
            _ => return None,
        })
    }
    fn set(&self, k: &str, v: &str) -> bool {
        let i = v.parse::<i32>().unwrap_or(0);
        match k {
            "filter" => self.set_filter(i),
            "rec_current" => self.set_rec_current(i),
            "dark" => self.set_dark(v == "true"),
            _ => return false,
        }
        true
    }
    fn call(&self, name: &str, n: Option<i32>) -> bool {
        let n = n.unwrap_or(0);
        match name {
            "sync_now" => self.invoke_sync_now(),
            "settings" => self.invoke_settings(n),
            "pause" => self.invoke_pause(n),
            "open_recent" => self.invoke_open_file(false, n),
            "reveal_recent" => self.invoke_reveal_file(false, n),
            "open_transfer" => self.invoke_open_file(true, n),
            "open_folder" => self.invoke_open_folder(n),
            "reveal_blocked" => self.invoke_reveal_blocked(n),
            "filter_changed" => self.invoke_filter_changed(),
            "close_panel" => self.invoke_close_panel(),
            _ => return false,
        }
        true
    }
    /// Made-up accounts and files for README screenshots.
    fn demo(&self) {
        use slint::{ModelRc, VecModel};
        let acc = |name: &str, status: &str, tone| PanelAccount {
            name: name.into(),
            status: status.into(),
            tone,
            personal: name == "Personal",
        };
        self.set_accounts(ModelRc::new(VecModel::from(vec![
            acc("Personal", "Up to date (09:41)", 0),
            acc("Contoso", "Syncing…", 1),
            acc("Marketing team", "Up to date (09:38)", 0),
        ])));
        self.set_summary("Syncing 2 large files".into());
        self.set_summary_tone(1);
        let file = |name: &str, detail: &str, kind, percent| PanelFile {
            name: name.into(),
            detail: detail.into(),
            kind,
            percent,
            can_open: percent < 0,
        };
        self.set_transfers(ModelRc::new(VecModel::from(vec![
            file(
                "Lisbon trip.mp4",
                "Uploading · 62% · 2 min left · Personal",
                7,
                62,
            ),
            file(
                "Q3 forecast.xlsx",
                "Downloading · 35% · less than a minute left · Contoso",
                3,
                35,
            ),
        ])));
        self.set_recent(ModelRc::new(VecModel::from(vec![
            file(
                "Budget 2027.ods",
                "Uploaded 09:41 · in Documents/Finances · Personal",
                3,
                -1,
            ),
            file(
                "IMG_2041.jpg",
                "Downloaded 09:40 · in Pictures/Camera Roll · Personal",
                6,
                -1,
            ),
            file(
                "Launch plan.docx",
                "Uploaded 09:38 · in Campaigns/Spring · Marketing team",
                2,
                -1,
            ),
            file(
                "Brand guide.pdf",
                "Downloaded 09:31 · in Brand · Marketing team",
                5,
                -1,
            ),
            file(
                "Pitch deck.pptx",
                "Uploaded 09:12 · in Sales · Contoso",
                4,
                -1,
            ),
            file(
                "notes.md",
                "Uploaded 08:55 · in Projects/garden-planner · Personal",
                11,
                -1,
            ),
            file(
                "Essay - final.odt",
                "Uploaded Oct 17, 22:03 · in Documents/School · Personal",
                2,
                -1,
            ),
        ])));
        self.set_filters(ModelRc::new(VecModel::from(vec![
            "All accounts".into(),
            "Personal".into(),
            "Contoso".into(),
            "Marketing team".into(),
        ])));
        self.set_filter(0);
    }
}

enum Step {
    Wait(f64),
    Shot(String),
    Call(String, Option<i32>),
    Set(String, String),
    Until(String, String, f64),
    Dump(String),
    SelectName(String),
    Demo,
    Key(String),
    Quit,
}

fn parse(line: &str) -> Option<Step> {
    let mut w = line.trim().splitn(2, ' ');
    let cmd = w.next()?;
    let rest = w.next().unwrap_or("").trim().to_string();
    Some(match cmd {
        "" | "#" => return None,
        c if c.starts_with('#') => return None,
        "wait" => Step::Wait(rest.parse().ok()?),
        "shot" => Step::Shot(rest),
        "call" => {
            let mut p = rest.split_whitespace();
            Step::Call(p.next()?.to_string(), p.next().and_then(|n| n.parse().ok()))
        }
        "set" => {
            let (k, v) = rest.split_once(' ').unwrap_or((&rest, ""));
            Step::Set(k.to_string(), v.to_string())
        }
        "until" => {
            let (k, r) = rest.split_once(' ')?;
            let (text, secs) = r.rsplit_once(' ')?;
            Step::Until(k.to_string(), text.to_string(), secs.parse().ok()?)
        }
        "dump" => Step::Dump(rest),
        "selectname" => Step::SelectName(rest),
        "demo" => Step::Demo,
        "key" => Step::Key(rest),
        "quit" => Step::Quit,
        _ => {
            println!("FAIL unknown step: {line}");
            return None;
        }
    })
}

fn get_str(ui: &AppWindow, k: &str) -> Option<String> {
    macro_rules! s {
        ($($name:literal => $getter:ident),*) => {
            match k { $($name => Some(ui.$getter().to_string()),)* _ => None }
        };
    }
    let r = s!(
        "s_status" => get_s_status, "s_msg" => get_s_msg, "s_error" => get_s_error, "s_quota" => get_s_quota,
        "s_kind" => get_s_kind, "s_owner" => get_s_owner, "s_last" => get_s_last, "s_folder" => get_s_folder, "s_name" => get_s_name,
        "si_text" => get_si_text, "f_msg" => get_f_msg, "o_msg" => get_o_msg, "p_text" => get_p_text, "p_warn" => get_p_warn,
        "p_live" => get_p_live, "p_title" => get_p_title, "add_msg" => get_add_msg, "app_client" => get_app_client, "app_msg" => get_app_msg, "app_glass_note" => get_app_glass_note,
        "r_text" => get_r_text, "o_skip_files" => get_o_skip_files, "o_from" => get_o_from, "o_to" => get_o_to,
        "o_skip_dirs" => get_o_skip_dirs,
        "add_name" => get_add_name, "add_folder" => get_add_folder,
        "u_result" => get_u_result, "u_remove_app_label" => get_u_remove_app_label,
        "client_problem" => get_client_problem, "client_hint" => get_client_hint, "client_install_msg" => get_client_install_msg,
        "ci_text" => get_ci_text, "ci_title" => get_ci_title, "ci_action" => get_ci_action,
        "client_done" => get_client_done
    );
    r.or_else(|| {
        Some(match k {
            "current" => ui.get_current().to_string(),
            "dark" => ui.get_dark().to_string(),
            "f_dirty" => ui.get_f_dirty().to_string(),
            "f_loading" => ui.get_f_loading().to_string(),
            "f_sync_all" => ui.get_f_sync_all().to_string(),
            "f_allowed" => ui.get_f_allowed().to_string(),
            "p_visible" => ui.get_p_visible().to_string(),
            "p_busy" => ui.get_p_busy().to_string(),
            "p_can_apply" => ui.get_p_can_apply().to_string(),
            "si_visible" => ui.get_si_visible().to_string(),
            "si_done" => ui.get_si_done().to_string(),
            "si_need_paste" => ui.get_si_need_paste().to_string(),
            "si_has_url" => ui.get_si_has_url().to_string(),
            "s_signed_in" => ui.get_s_signed_in().to_string(),
            "s_bg_enabled" => ui.get_s_bg_enabled().to_string(),
            "s_busy" => ui.get_s_busy().to_string(),
            "add_busy" => ui.get_add_busy().to_string(),
            "o_interval" => ui.get_o_interval().to_string(),
            "o_rate" => ui.get_o_rate().to_string(),
            "o_allow_resync" => ui.get_o_allow_resync().to_string(),
            "o_skip_hidden" => ui.get_o_skip_hidden().to_string(),
            "o_direction" => ui.get_o_direction().to_string(),
            "o_cleanup" => ui.get_o_cleanup().to_string(),
            "o_keep_online" => ui.get_o_keep_online().to_string(),
            "o_skip_symlinks" => ui.get_o_skip_symlinks().to_string(),
            "o_nosync" => ui.get_o_nosync().to_string(),
            "o_shared" => ui.get_o_shared().to_string(),
            "o_is_business" => ui.get_o_is_business().to_string(),
            "o_ipv4" => ui.get_o_ipv4().to_string(),
            "r_visible" => ui.get_r_visible().to_string(),
            "tab" => ui.get_tab().to_string(),
            "u_visible" => ui.get_u_visible().to_string(),
            "client_can_install" => ui.get_client_can_install().to_string(),
            "client_update" => ui.get_client_update().to_string(),
            "client_installing" => ui.get_client_installing().to_string(),
            "ci_visible" => ui.get_ci_visible().to_string(),
            "u_busy" => ui.get_u_busy().to_string(),
            "a_current" => ui.get_a_current().to_string(),
            "app_autostart" => ui.get_app_autostart().to_string(),
            "app_stop_on_quit" => ui.get_app_stop_on_quit().to_string(),
            "app_check_updates" => ui.get_app_check_updates().to_string(),
            "app_update" => ui.get_app_update().to_string(),
            "app_version" => ui.get_app_version().to_string(),
            "o_sched_on" => ui.get_o_sched_on().to_string(),
            "o_recycle" => ui.get_o_recycle().to_string(),
            "o_notify" => ui.get_o_notify().to_string(),
            "s_tone" => ui.get_s_tone().to_string(),
            "s_big_delete" => ui.get_s_big_delete().to_string(),
            "accounts" => {
                use slint::Model;
                ui.get_accounts()
                    .iter()
                    .map(|a| format!("{} [{}]", a.name, a.status))
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
            "f_rows" => {
                use slint::Model;
                ui.get_f_rows()
                    .iter()
                    .map(|r| {
                        format!(
                            "{}{}{}",
                            "  ".repeat(r.depth as usize),
                            if r.checked { "[x] " } else { "[ ] " },
                            r.name
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "add_libraries" => {
                use slint::Model;
                ui.get_add_libraries()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
            "a_rows" => {
                use slint::Model;
                let rows = ui.get_a_rows();
                format!(
                    "{} rows; first: {}",
                    rows.row_count(),
                    rows.row_data(0)
                        .map(|r| r
                            .iter()
                            .map(|c| c.text.to_string())
                            .collect::<Vec<_>>()
                            .join(" / "))
                        .unwrap_or_default()
                )
            }
            _ => return None,
        })
    })
}

fn set(ui: &AppWindow, k: &str, v: &str) -> bool {
    let b = v == "true";
    let i: i32 = v.parse().unwrap_or(0);
    let s = SharedString::from(v);
    match k {
        "add_name" => ui.set_add_name(s),
        "add_folder" => ui.set_add_folder(s),
        "add_query" => ui.set_add_query(s),
        "add_from_index" => ui.set_add_from_index(i),
        "s_name" => ui.set_s_name(s),
        "si_paste" => ui.set_si_paste(s),
        "o_interval" => ui.set_o_interval(i),
        "o_rate" => ui.set_o_rate(i),
        "o_sched_on" => ui.set_o_sched_on(b),
        "o_from" => ui.set_o_from(s),
        "o_to" => ui.set_o_to(s),
        "o_recycle" => ui.set_o_recycle(b),
        "o_notify" => ui.set_o_notify(b),
        "o_skip_hidden" => ui.set_o_skip_hidden(b),
        "o_direction" => ui.set_o_direction(i),
        "o_cleanup" => ui.set_o_cleanup(b),
        "o_keep_online" => ui.set_o_keep_online(b),
        "o_skip_dirs" => ui.set_o_skip_dirs(s),
        "o_skip_symlinks" => ui.set_o_skip_symlinks(b),
        "o_nosync" => ui.set_o_nosync(b),
        "o_shared" => ui.set_o_shared(b),
        "o_ipv4" => ui.set_o_ipv4(b),
        "o_skip_files" => ui.set_o_skip_files(s),
        "o_allow_resync" => ui.set_o_allow_resync(b),
        "f_sync_all" => ui.set_f_sync_all(b),
        "f_root_files" => ui.set_f_root_files(b),
        "r_delete_config" => ui.set_r_delete_config(b),
        "u_keep_setup" => ui.set_u_keep_setup(b),
        "app_autostart" => ui.set_app_autostart(b),
        "app_stop_on_quit" => ui.set_app_stop_on_quit(b),
        "app_glass" => ui.set_app_glass(i),
        "dark" => ui.set_dark(b),
        "glass" => ui.global::<crate::ui::Theme>().set_glass(b),
        "contrast" => ui.global::<crate::ui::Theme>().set_contrast(b),
        "tab" => ui.set_tab(i),
        _ => return false,
    }
    true
}

fn call(ui: &AppWindow, name: &str, n: Option<i32>) -> bool {
    let n = n.unwrap_or(0);
    match name {
        "select" => ui.invoke_select(n),
        "rename" => ui.invoke_rename(),
        "open_folder" => ui.invoke_open_folder(),
        "open_online" => ui.invoke_open_online_page(0),
        "si_open_url" => ui.invoke_si_open_url(),
        "a_show" => ui.invoke_a_show(n),
        "open_url_project" => ui.invoke_open_url(
            "https://github.com/sweatshopking/A-GUI-that-works-with-OneDrive-on-Linux-unofficial"
                .into(),
        ),
        "sync_now" => ui.invoke_sync_now(),
        "big_delete_online" => ui.invoke_big_delete_online(),
        "big_delete_restore" => ui.invoke_big_delete_restore(),
        "set_background" => ui.invoke_set_background(n != 0),
        "app_glass_changed" => ui.invoke_app_glass_changed(),
        "sign_in" => ui.invoke_sign_in(),
        "sign_out" => ui.invoke_sign_out(),
        "remove_account" => ui.invoke_remove_account(),
        "r_confirm" => ui.invoke_r_confirm(),
        "r_cancel" => ui.invoke_r_cancel(),
        "si_submit" => ui.invoke_si_submit(),
        "si_copy_url" => ui.invoke_si_copy_url(),
        "si_close" => ui.invoke_si_close(),
        "f_sync_all_toggled" => ui.invoke_f_sync_all_toggled(),
        "f_root_toggled" => ui.invoke_f_root_toggled(),
        "f_expand" => ui.invoke_f_expand(n),
        "f_check" => ui.invoke_f_check(n),
        "f_reload" => ui.invoke_f_reload(),
        "f_apply" => ui.invoke_f_apply(),
        "o_save" => ui.invoke_o_save(),
        "a_refresh" => ui.invoke_a_refresh(),
        "p_apply" => ui.invoke_p_apply(),
        "p_cancel" => ui.invoke_p_cancel(),
        "add_choose" => ui.invoke_add_choose(n),
        "add_browse" => ui.invoke_add_browse(),
        "add_search" => ui.invoke_add_search(),
        "add_lib_picked" => ui.invoke_add_lib_picked(n),
        "add_create" => ui.invoke_add_create(),
        "app_autostart_toggled" => ui.invoke_app_autostart_toggled(),
        "app_stop_on_quit_toggled" => ui.invoke_app_stop_on_quit_toggled(),
        "app_add_to_menu" => ui.invoke_app_add_to_menu(),
        "app_uninstall" => ui.invoke_app_uninstall(),
        "client_recheck" => ui.invoke_client_recheck(),
        "client_instructions" => ui.invoke_client_instructions(),
        "client_install" => ui.invoke_client_install(),
        "client_not_now" => ui.invoke_client_not_now(),
        "ci_confirm" => ui.invoke_ci_confirm(),
        "ci_cancel" => ui.invoke_ci_cancel(),
        "ci_myself" => ui.invoke_ci_myself(),
        "client_done_ok" => ui.invoke_client_done_ok(),
        "u_confirm" => ui.invoke_u_confirm(),
        "u_cancel" => ui.invoke_u_cancel(),
        _ => return false,
    }
    true
}

fn shot<T: Driven>(ui: &T, dir: &Path, name: &str) {
    match ui.window().take_snapshot() {
        Ok(buf) => {
            let (w, h) = (buf.width(), buf.height());
            let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in buf.as_slice() {
                data.extend_from_slice(&[px.r, px.g, px.b]);
            }
            let path = dir.join(format!("{name}.ppm"));
            match fs::write(&path, data) {
                Ok(()) => println!("STEP shot {}", path.display()),
                Err(e) => println!("FAIL shot {name}: {e}"),
            }
        }
        Err(e) => println!("FAIL shot {name}: {e}"),
    }
}

static SETTINGS: std::sync::OnceLock<(Option<String>, Option<String>)> = std::sync::OnceLock::new();

/// Reads `LODG_TEST_SCRIPT`/`LODG_TEST_OUT` and removes them from the environment, so the
/// processes the app starts (the tray, a window the tray opens) don't run the script again.
/// Call first thing in main, while the program is still single-threaded.
pub fn take_env() {
    SETTINGS.get_or_init(|| {
        let v = (
            std::env::var("LODG_TEST_SCRIPT").ok(),
            std::env::var("LODG_TEST_OUT").ok(),
        );
        // SAFETY: called at the top of main, before any other thread exists
        unsafe {
            std::env::remove_var("LODG_TEST_SCRIPT");
            std::env::remove_var("LODG_TEST_OUT");
        }
        v
    });
}

/// Starts the script if `LODG_TEST_SCRIPT` was set. The returned timer must be kept alive.
pub fn start<T: Driven>(ui: &T) -> Option<slint::Timer> {
    let (script, out) = SETTINGS.get()?;
    let script = fs::read_to_string(script.as_ref()?).ok()?;
    let out = PathBuf::from(out.clone().unwrap_or_else(|| ".".into()));
    let _ = fs::create_dir_all(&out);
    let steps: Rc<std::cell::RefCell<std::collections::VecDeque<Step>>> = Rc::new(
        std::cell::RefCell::new(script.lines().filter_map(parse).collect()),
    );
    let next_at = Rc::new(std::cell::Cell::new(
        Instant::now() + Duration::from_millis(800),
    ));
    let until_deadline: Rc<std::cell::Cell<Option<Instant>>> = Rc::new(std::cell::Cell::new(None));
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if Instant::now() < next_at.get() {
                return;
            }
            let mut q = steps.borrow_mut();
            let Some(step) = q.front() else { return };
            let mut advance = true;
            match step {
                Step::Wait(s) => next_at.set(Instant::now() + Duration::from_secs_f64(*s)),
                Step::Shot(n) => shot(&ui, &out, n),
                Step::Call(name, n) => {
                    if ui.call(name, *n) {
                        println!(
                            "STEP call {name} {}",
                            n.map(|n| n.to_string()).unwrap_or_default()
                        );
                    } else {
                        println!("FAIL unknown callback {name}");
                    }
                    next_at.set(Instant::now() + Duration::from_millis(300));
                }
                Step::Set(k, v) => {
                    if !ui.set(k, v) {
                        println!("FAIL unknown property {k}");
                    }
                }
                Step::Until(k, text, secs) => {
                    let deadline = until_deadline.get().unwrap_or_else(|| {
                        let d = Instant::now() + Duration::from_secs_f64(*secs);
                        until_deadline.set(Some(d));
                        d
                    });
                    let val = ui.get(k).unwrap_or_default();
                    if val.contains(text.as_str()) {
                        println!("STEP until {k} ~ {text:?} ok");
                        until_deadline.set(None);
                    } else if Instant::now() > deadline {
                        println!("FAIL until {k} ~ {text:?} timed out; value: {val:?}");
                        until_deadline.set(None);
                    } else {
                        advance = false;
                    }
                }
                Step::Dump(k) => match ui.get(k) {
                    Some(v) => println!("DUMP {k} = {}", v.replace('\n', " ⏎ ")),
                    None => println!("FAIL unknown property {k}"),
                },
                Step::SelectName(n) => {
                    match ui.select_name(n) {
                        Some(i) => println!("STEP selectname {n} -> {i}"),
                        None => println!("FAIL selectname {n}: no such account"),
                    }
                    next_at.set(Instant::now() + Duration::from_millis(300));
                }
                Step::Demo => ui.demo(),
                Step::Key(name) => {
                    use slint::platform::{Key, WindowEvent};
                    let key: Option<SharedString> = match name.as_str() {
                        "tab" => Some(Key::Tab.into()),
                        "backtab" => Some(Key::Backtab.into()),
                        "space" => Some(Key::Space.into()),
                        "return" => Some(Key::Return.into()),
                        "left" => Some(Key::LeftArrow.into()),
                        "right" => Some(Key::RightArrow.into()),
                        "home" => Some(Key::Home.into()),
                        "end" => Some(Key::End.into()),
                        "up" => Some(Key::UpArrow.into()),
                        "down" => Some(Key::DownArrow.into()),
                        "escape" => Some(Key::Escape.into()),
                        _ => None,
                    };
                    match key {
                        Some(text) => {
                            ui.window()
                                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                            ui.window()
                                .dispatch_event(WindowEvent::KeyReleased { text });
                        }
                        None => println!("FAIL unknown key {name}"),
                    }
                }
                Step::Quit => {
                    let _ = std::io::stdout().flush();
                    // close the window the way a user does, so its close handling runs
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::CloseRequested);
                    let _ = slint::quit_event_loop();
                }
            }
            if advance {
                q.pop_front();
            }
            let _ = std::io::stdout().flush();
        },
    );
    Some(timer)
}

/// Made-up accounts and files for README screenshots, so nobody's real data gets published.
fn fill_demo(ui: &AppWindow) {
    use crate::ui::{AccountRow, FolderRow};
    use slint::{ModelRc, StandardListViewItem, VecModel};
    let row = |name: &str, kind: &str, status: &str, tone: i32| AccountRow {
        name: name.into(),
        initial: crate::ui::initial(name).into(),
        kind: kind.into(),
        status: status.into(),
        tone,
    };
    ui.set_accounts(ModelRc::new(VecModel::from(vec![
        row("Personal", "Personal", "Up to date (09:41)", 0),
        row("Contoso", "Work or school", "Syncing…", 1),
        row(
            "Marketing team",
            "SharePoint library",
            "Up to date (09:38)",
            0,
        ),
        row("Family photos", "Personal", "Paused until 18:00", 3),
    ])));
    ui.set_current(0);
    ui.set_s_name("Personal".into());
    ui.set_s_kind("Personal".into());
    ui.set_s_owner("alex@outlook.com".into());
    ui.set_s_folder("/home/alex/OneDrive".into());
    ui.set_s_status("Up to date (09:41)".into());
    ui.set_s_tone(0);
    ui.set_s_last("09:41".into());
    ui.set_s_error("".into());
    ui.set_s_msg("".into());
    ui.set_s_quota("312.4 GB of 1.0 TB used".into());
    ui.set_s_quota_frac(0.305);
    ui.set_s_signed_in(true);
    ui.set_s_bg_enabled(true);
    let folder =
        |name: &str, depth: i32, expanded: bool, kids: bool, checked: bool, inherited: bool| {
            FolderRow {
                name: name.into(),
                depth,
                expanded,
                has_children: kids,
                loading: false,
                checked,
                inherited,
            }
        };
    ui.set_f_rows(ModelRc::new(VecModel::from(vec![
        folder("Desktop", 0, false, true, false, false),
        folder("Documents", 0, true, true, true, false),
        folder("Finances", 1, false, true, true, true),
        folder("School", 1, false, true, true, true),
        folder("Travel", 1, false, false, true, true),
        folder("Music", 0, false, true, false, false),
        folder("Pictures", 0, true, true, false, false),
        folder("Camera Roll", 1, false, true, true, false),
        folder("Screenshots", 1, false, false, false, false),
        folder("Wallpapers", 1, false, false, true, false),
        folder("Projects", 0, false, true, true, false),
        folder("Videos", 0, false, true, false, false),
    ])));
    ui.set_f_sync_all(false);
    ui.set_f_root_files(true);
    ui.set_f_dirty(true);
    ui.set_f_allowed(true);
    ui.set_f_msg("".into());
    let activity = [
        (
            "Oct 18  09:41",
            "Uploaded",
            "Documents/Finances/Budget 2027.ods",
        ),
        (
            "Oct 18  09:40",
            "Downloaded",
            "Pictures/Camera Roll/IMG_2041.jpg",
        ),
        (
            "Oct 18  09:40",
            "Downloaded",
            "Pictures/Camera Roll/IMG_2040.jpg",
        ),
        (
            "Oct 18  09:12",
            "Uploaded",
            "Projects/garden-planner/notes.md",
        ),
        ("Oct 18  08:55", "Deleted", "Documents/old draft.docx"),
        (
            "Oct 17  22:03",
            "Uploaded",
            "Documents/School/Essay - final.odt",
        ),
        (
            "Oct 17  21:47",
            "Downloaded",
            "Documents/Travel/Lisbon itinerary.pdf",
        ),
        ("Oct 17  19:30", "New folder", "Pictures/Wallpapers/Autumn"),
    ];
    let rows: Vec<ModelRc<StandardListViewItem>> = activity
        .iter()
        .map(|(t, w, p)| ModelRc::new(VecModel::from(vec![(*t).into(), (*w).into(), (*p).into()])))
        .collect();
    ui.set_a_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_p_title("Change synced folders".into());
    ui.set_p_text("Dry run finished — nothing has changed yet. If you apply this, the sync engine will:\n  •  download 214 item(s) to this computer\n  •  upload 0 item(s) to OneDrive\n  •  delete 0 item(s) online\n  •  delete 0 item(s) on this computer\n\nFolders you stop syncing stay on this computer; they just stop being kept in sync.".into());
    ui.set_p_warn("".into());
    ui.set_p_busy(false);
    ui.set_p_can_apply(true);
    use crate::ui::{BackupRow, ConflictRow};
    ui.set_c_rows(ModelRc::new(VecModel::from(vec![
        ConflictRow {
            name: "Trip budget.xlsx".into(),
            detail: "Documents/Travel  ·  this computer's copy from 22 Oct, 18:40".into(),
        },
        ConflictRow {
            name: "Reading list.docx".into(),
            detail: "Documents/School  ·  this computer's copy from 21 Oct, 09:12".into(),
        },
    ])));
    let backup = |label: &str, detail: &str, state| BackupRow {
        label: label.into(),
        detail: detail.into(),
        state,
    };
    ui.set_b_rows(ModelRc::new(VecModel::from(vec![
        backup("Desktop", "/home/alex/Desktop", 0),
        backup("Documents", "Backed up to OneDrive/Documents", 1),
        backup("Pictures", "Backed up to OneDrive/Pictures", 1),
        backup("Music", "/home/alex/Music", 0),
        backup("Videos", "/home/alex/Videos", 0),
    ])));
    println!("STEP demo");
}
