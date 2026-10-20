//! The few session-bus calls we make to desktop services. Every call is best-effort: on a desktop
//! without the service it quietly returns false/None and the caller falls back.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

fn session() -> Option<&'static Connection> {
    static CONN: OnceLock<Option<Connection>> = OnceLock::new();
    CONN.get_or_init(|| {
        // A desktop service that's installed but stuck (seen with xdg-desktop-portal outside a
        // full desktop session) would otherwise block the caller forever.
        zbus::blocking::connection::Builder::session()
            .ok()?
            .method_timeout(Duration::from_secs(10))
            .build()
            .ok()
    })
    .as_ref()
}

/// Shows a desktop notification (org.freedesktop.Notifications).
pub fn notify(app_name: &str, icon: &str, summary: &str, body: &str) -> bool {
    let Some(c) = session() else { return false };
    let hints: HashMap<&str, Value> = HashMap::new();
    // where the server reads the body as markup, a file name like "<b>x</b>" or "a&b" must
    // show as written
    let body = if body_markup(c) {
        markup_escape(body)
    } else {
        body.to_string()
    };
    let body = body.as_str();
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

/// Whether the notification server reads bodies as markup (KDE and GNOME do).
fn body_markup(c: &Connection) -> bool {
    static MARKUP: OnceLock<bool> = OnceLock::new();
    *MARKUP.get_or_init(|| {
        c.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "GetCapabilities",
            &(),
        )
        .ok()
        .and_then(|m| m.body().deserialize::<Vec<String>>().ok())
        .is_some_and(|caps| caps.iter().any(|c| c == "body-markup"))
    })
}

fn markup_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// What the desktop asks of how apps look, from the settings portal (every desktop with
/// xdg-desktop-portal: KDE, GNOME, Cinnamon, COSMIC, …). Anything it doesn't say stays at the default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Appearance {
    /// None when the desktop has no preference
    pub dark: Option<bool>,
    pub high_contrast: bool,
    pub reduce_motion: bool,
}

pub fn appearance() -> Appearance {
    let scheme = portal_setting("org.freedesktop.appearance", "color-scheme").and_then(as_u32);
    let contrast = portal_setting("org.freedesktop.appearance", "contrast").and_then(as_u32);
    // GNOME and its relatives switch animations off here, KDE by setting their speed to 0
    let gnome_animations =
        portal_setting("org.gnome.desktop.interface", "enable-animations").and_then(as_bool);
    let kde_speed =
        portal_setting("org.kde.kdeglobals.KDE", "AnimationDurationFactor").and_then(as_f64);
    Appearance {
        dark: match scheme {
            Some(1) => Some(true),
            Some(2) => Some(false),
            _ => None,
        },
        high_contrast: contrast == Some(1),
        reduce_motion: gnome_animations == Some(false) || kde_speed == Some(0.0),
    }
}

/// Calls `changed` with the new appearance whenever the desktop changes one of those settings
/// (switching to dark at sunset, say). Runs for as long as the app does.
pub fn watch_appearance(changed: impl Fn(Appearance) + Send + 'static) {
    std::thread::spawn(move || {
        let Some(c) = session() else { return };
        let Ok(proxy) = zbus::blocking::Proxy::new(
            c,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
        ) else {
            return;
        };
        let Ok(signals) = proxy.receive_signal("SettingChanged") else {
            return;
        };
        for msg in signals {
            let Ok((namespace, key, _)) = msg.body().deserialize::<(String, String, OwnedValue)>()
            else {
                continue;
            };
            if matches!(
                (namespace.as_str(), key.as_str()),
                ("org.freedesktop.appearance", "color-scheme" | "contrast")
                    | ("org.gnome.desktop.interface", "enable-animations")
                    | ("org.kde.kdeglobals.KDE", "AnimationDurationFactor")
            ) {
                changed(appearance());
            }
        }
    });
}

fn portal_setting(namespace: &str, key: &str) -> Option<OwnedValue> {
    let c = session()?;
    let call = |method: &str| {
        c.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            method,
            &(namespace, key),
        )
        .ok()
        .and_then(|m| m.body().deserialize::<OwnedValue>().ok())
    };
    // ReadOne returns the value directly; the older Read wraps it in one more variant
    call("ReadOne").or_else(|| call("Read"))
}

/// The value inside a portal reply, unwrapping the extra variant the older Read adds.
fn unwrapped<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => inner,
        v => v,
    }
}

fn as_u32(v: OwnedValue) -> Option<u32> {
    match unwrapped(&v) {
        Value::U32(n) => Some(*n),
        _ => None,
    }
}

fn as_bool(v: OwnedValue) -> Option<bool> {
    match unwrapped(&v) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn as_f64(v: OwnedValue) -> Option<f64> {
    match unwrapped(&v) {
        Value::F64(n) => Some(*n),
        // kdeglobals values come through as text
        Value::Str(s) => s.parse().ok(),
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

/// Asks the desktop's own folder chooser (the XDG file chooser portal) for a folder, starting in
/// `start`. Blocks until the dialog closes. `None` means the portal isn't available, so the caller
/// should try something else; `Some(None)` means the user cancelled.
pub fn choose_folder(title: &str, start: &std::path::Path) -> Option<Option<std::path::PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    use zbus::proxy::CacheProperties;

    let c = session()?;
    // The portal answers with a Response signal on a request object whose path we can predict
    // from our bus name and a token, so subscribe to it before asking to avoid missing it.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token = format!("lodg{}_{n}", std::process::id());
    let sender = c.unique_name()?.trim_start_matches(':').replace('.', "_");
    let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let request = zbus::blocking::proxy::Builder::<zbus::blocking::Proxy>::new(c)
        .destination("org.freedesktop.portal.Desktop")
        .ok()?
        .path(request_path)
        .ok()?
        .interface("org.freedesktop.portal.Request")
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .ok()?;
    let mut responses = request.receive_signal("Response").ok()?;

    let mut folder = start.as_os_str().as_bytes().to_vec();
    folder.push(0); // the portal wants a NUL-terminated byte string
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", token.as_str().into());
    options.insert("directory", true.into());
    options.insert("modal", true.into());
    options.insert("current_folder", folder.into());
    c.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.FileChooser"),
        "OpenFile",
        &("", title, options),
    )
    .ok()?;

    let reply = responses.next()?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = reply.body().deserialize().ok()?;
    if code != 0 {
        return Some(None);
    }
    let uris: Vec<String> = results.get("uris")?.try_clone().ok()?.try_into().ok()?;
    Some(uris.first().and_then(|u| crate::core::path_from_uri(u)))
}

/// The process id of whoever owns a well-known bus name, if anyone does.
pub fn name_owner_pid(name: &str) -> Option<u32> {
    let c = session()?;
    let reply = c
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(name,),
        )
        .ok()?;
    reply.body().deserialize::<u32>().ok()
}

/// Result of [`claim_name`].
pub enum Claim {
    Owned(Connection),
    TakenByAnother,
    /// No session bus; callers should just carry on.
    Unavailable,
}

/// Claims a well-known bus name for as long as the returned connection is kept alive. Only one
/// process can hold it, so this is a reliable "already running" check however we were started
/// (package, AppImage, or a build directory).
pub fn claim_name(name: &str) -> Claim {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    let Ok(c) = Connection::session() else {
        return Claim::Unavailable;
    };
    match c.request_name_with_flags(name, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Claim::Owned(c),
        // zbus reports a name held elsewhere as an error rather than a reply
        Ok(_) | Err(zbus::Error::NameTaken) => Claim::TakenByAnother,
        Err(_) => Claim::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_markup() {
        assert_eq!(
            markup_escape("<b>a</b> & <img src=x>"),
            "&lt;b&gt;a&lt;/b&gt; &amp; &lt;img src=x&gt;"
        );
        assert_eq!(markup_escape("plain"), "plain");
    }
}
