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
//!   quit                           close the window
//! Results go to stdout as `STEP …`, `DUMP …` and `FAIL …` lines.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, SharedString};

use crate::ui::AppWindow;

enum Step {
    Wait(f64),
    Shot(String),
    Call(String, Option<i32>),
    Set(String, String),
    Until(String, String, f64),
    Dump(String),
    SelectName(String),
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
        "p_live" => get_p_live, "p_title" => get_p_title, "add_msg" => get_add_msg, "app_client" => get_app_client, "app_msg" => get_app_msg,
        "r_text" => get_r_text, "o_skip_files" => get_o_skip_files, "o_from" => get_o_from, "o_to" => get_o_to,
        "add_name" => get_add_name, "add_folder" => get_add_folder
    );
    r.or_else(|| {
        Some(match k {
            "current" => ui.get_current().to_string(),
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
            "r_visible" => ui.get_r_visible().to_string(),
            "accounts" => {
                use slint::Model;
                ui.get_accounts().iter().map(|a| format!("{} [{}]", a.name, a.status)).collect::<Vec<_>>().join(" | ")
            }
            "f_rows" => {
                use slint::Model;
                ui.get_f_rows()
                    .iter()
                    .map(|r| format!("{}{}{}", "  ".repeat(r.depth as usize), if r.checked { "[x] " } else { "[ ] " }, r.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "add_libraries" => {
                use slint::Model;
                ui.get_add_libraries().iter().map(|s| s.to_string()).collect::<Vec<_>>().join(" | ")
            }
            "a_rows" => {
                use slint::Model;
                let rows = ui.get_a_rows();
                format!("{} rows; first: {}", rows.row_count(), rows.row_data(0).map(|r| r.iter().map(|c| c.text.to_string()).collect::<Vec<_>>().join(" / ")).unwrap_or_default())
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
        "o_skip_files" => ui.set_o_skip_files(s),
        "o_allow_resync" => ui.set_o_allow_resync(b),
        "f_sync_all" => ui.set_f_sync_all(b),
        "f_root_files" => ui.set_f_root_files(b),
        "r_delete_config" => ui.set_r_delete_config(b),
        "dark" => ui.set_dark(b),
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
        "sync_now" => ui.invoke_sync_now(),
        "set_background" => ui.invoke_set_background(n != 0),
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
        "add_search" => ui.invoke_add_search(),
        "add_lib_picked" => ui.invoke_add_lib_picked(n),
        "add_create" => ui.invoke_add_create(),
        "app_autostart_toggled" => ui.invoke_app_autostart_toggled(),
        _ => return false,
    }
    true
}

fn shot(ui: &AppWindow, dir: &PathBuf, name: &str) {
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

/// Starts the script if `LODG_TEST_SCRIPT` is set. The returned timer must be kept alive.
pub fn start(ui: &AppWindow) -> Option<slint::Timer> {
    let script = fs::read_to_string(std::env::var("LODG_TEST_SCRIPT").ok()?).ok()?;
    let out = PathBuf::from(std::env::var("LODG_TEST_OUT").unwrap_or_else(|_| ".".into()));
    let _ = fs::create_dir_all(&out);
    let steps: Rc<std::cell::RefCell<std::collections::VecDeque<Step>>> = Rc::new(std::cell::RefCell::new(script.lines().filter_map(parse).collect()));
    let next_at = Rc::new(std::cell::Cell::new(Instant::now() + Duration::from_millis(800)));
    let until_deadline: Rc<std::cell::Cell<Option<Instant>>> = Rc::new(std::cell::Cell::new(None));
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
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
                if call(&ui, name, *n) {
                    println!("STEP call {name} {}", n.map(|n| n.to_string()).unwrap_or_default());
                } else {
                    println!("FAIL unknown callback {name}");
                }
                next_at.set(Instant::now() + Duration::from_millis(300));
            }
            Step::Set(k, v) => {
                if !set(&ui, k, v) {
                    println!("FAIL unknown property {k}");
                }
            }
            Step::Until(k, text, secs) => {
                let deadline = until_deadline.get().unwrap_or_else(|| {
                    let d = Instant::now() + Duration::from_secs_f64(*secs);
                    until_deadline.set(Some(d));
                    d
                });
                let val = get_str(&ui, k).unwrap_or_default();
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
            Step::Dump(k) => match get_str(&ui, k) {
                Some(v) => println!("DUMP {k} = {}", v.replace('\n', " ⏎ ")),
                None => println!("FAIL unknown property {k}"),
            },
            Step::SelectName(n) => {
                use slint::Model;
                match ui.get_accounts().iter().position(|a| a.name.contains(n.as_str())) {
                    Some(i) => {
                        ui.invoke_select(i as i32);
                        println!("STEP selectname {n} -> {i}");
                    }
                    None => println!("FAIL selectname {n}: no such account"),
                }
                next_at.set(Instant::now() + Duration::from_millis(300));
            }
            Step::Quit => {
                let _ = std::io::stdout().flush();
                let _ = slint::quit_event_loop();
            }
        }
        if advance {
            q.pop_front();
        }
        let _ = std::io::stdout().flush();
    });
    Some(timer)
}
