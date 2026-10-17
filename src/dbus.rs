//! The few session-bus calls we make to desktop services. Every call is best-effort: on a desktop
//! without the service it quietly returns false/None and the caller falls back.

use std::collections::HashMap;
use std::sync::OnceLock;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

fn session() -> Option<&'static Connection> {
    static CONN: OnceLock<Option<Connection>> = OnceLock::new();
    CONN.get_or_init(|| Connection::session().ok()).as_ref()
}

/// Shows a desktop notification (org.freedesktop.Notifications).
pub fn notify(app_name: &str, icon: &str, summary: &str, body: &str) -> bool {
    let Some(c) = session() else { return false };
    let hints: HashMap<&str, Value> = HashMap::new();
    c.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            app_name,
            0u32,
            icon,
            summary,
            body,
            Vec::<&str>::new(),
            hints,
            8000i32,
        ),
    )
    .is_ok()
}

/// The desktop's light/dark preference from the settings portal: Some(true) = dark.
pub fn prefers_dark() -> Option<bool> {
    let c = session()?;
    let call = |method: &str| {
        c.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            method,
            &("org.freedesktop.appearance", "color-scheme"),
        )
        .ok()
        .and_then(|m| m.body().deserialize::<OwnedValue>().ok())
    };
    // ReadOne returns the value directly; the older Read wraps it in one more variant
    let v = call("ReadOne").or_else(|| call("Read"))?;
    let n = match &*v {
        Value::U32(n) => *n,
        Value::Value(inner) => match **inner {
            Value::U32(n) => n,
            _ => return None,
        },
        _ => return None,
    };
    match n {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Opens the file manager with `uri` selected (org.freedesktop.FileManager1).
pub fn show_items(uri: &str) -> bool {
    let Some(c) = session() else { return false };
    c.call_method(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        Some("org.freedesktop.FileManager1"),
        "ShowItems",
        &(vec![uri], ""),
    )
    .is_ok()
}

/// Puts text on the clipboard through KDE's Klipper, when it's running.
pub fn klipper_set(text: &str) -> bool {
    let Some(c) = session() else { return false };
    c.call_method(
        Some("org.kde.klipper"),
        "/klipper",
        Some("org.kde.klipper.klipper"),
        "setClipboardContents",
        &(text,),
    )
    .is_ok()
}
