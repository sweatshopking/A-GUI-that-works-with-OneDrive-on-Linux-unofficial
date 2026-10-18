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

/// The desktop's light/dark preference from the settings portal: Some(true) = dark. Gives up
/// after `wait`, since it's asked while the window is opening.
pub fn prefers_dark(wait: Duration) -> Option<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_color_scheme());
    });
    rx.recv_timeout(wait).ok().flatten()
}

fn read_color_scheme() -> Option<bool> {
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
