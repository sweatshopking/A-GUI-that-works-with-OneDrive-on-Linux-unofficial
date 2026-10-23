//! Bringing a window to the front with a token from the user's click, through xdg-activation,
//! the standard Wayland way. Without a token, Wayland desktops leave a window where it is rather
//! than let it jump in front of what the user is doing.
//!
//! A token only counts as coming from a click when it's asked for with that click's serial
//! (KWin and Sway, for instance, don't raise a window for one without). winit asks without, so
//! tokens are asked for here, with the serial of the latest click or key press, which our own
//! pointer and keyboard on the window's Wayland connection see as winit's do. GTK does the same.

use std::cell::RefCell;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

#[derive(Default)]
struct State {
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// the serial of the latest button or key press
    serial: Option<u32>,
    /// the answer to the latest token request
    token: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            serial,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            state.serial = Some(serial);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // (the keymap's file descriptor closes as the event drops)
        if let wl_keyboard::Event::Key {
            serial,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            state.serial = Some(serial);
        }
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

macro_rules! no_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
no_events!(XdgActivationV1, wl_surface::WlSurface);

/// The window's own Wayland connection, with a queue of ours on it, kept for as long as the app
/// runs (as in glass.rs: a queue dropped with objects on it would leave them dangling).
enum Activation {
    Unavailable,
    Ready(Box<Ready>),
}

struct Ready {
    conn: Connection,
    queue: EventQueue<State>,
    manager: XdgActivationV1,
    seat: Option<wl_seat::WlSeat>,
    state: State,
}

thread_local! {
    static ACTIVATION: RefCell<Option<Activation>> = const { RefCell::new(None) };
}

/// Runs `f` with our setup on `window`'s Wayland connection and the window's surface; nothing on
/// X11 (or without xdg-activation).
fn with_ready<R>(
    window: &slint::winit_030::winit::window::Window,
    f: impl FnOnce(&mut Ready, wl_surface::WlSurface) -> Option<R>,
) -> Option<R> {
    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else {
        return None;
    };
    let (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) =
        (display.as_raw(), handle.as_raw())
    else {
        return None;
    };
    ACTIVATION.with_borrow_mut(|slot| {
        // SAFETY: the display pointer comes from the live winit window, whose display lives as
        // long as the event loop; our requests go out on a queue of our own, next to winit's
        let a = slot.get_or_insert_with(|| unsafe { attach(d.display.as_ptr()) });
        let Activation::Ready(r) = a else {
            return None;
        };
        // SAFETY: the surface pointer comes from the live winit window
        let id = unsafe {
            ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                w.surface.as_ptr().cast(),
            )
        }
        .ok()?;
        let surface = wl_surface::WlSurface::from_id(&r.conn, id).ok()?;
        f(r, surface)
    })
}

/// Notes clicks and key presses on `window` (and a keyboard or mouse plugged in meanwhile), so a
/// later `request_token` can say which one asked. Call as soon as the window exists, and with
/// each of its events.
pub fn track(window: &slint::winit_030::winit::window::Window) {
    let _ = with_ready(window, |r, _| {
        let _ = r.queue.dispatch_pending(&mut r.state);
        Some(())
    });
}

/// A token for the click or key press just made on `window`, to open something in front with;
/// None on X11 or where the desktop doesn't answer.
pub fn request_token(window: &slint::winit_030::winit::window::Window) -> Option<String> {
    with_ready(window, |r, surface| {
        // take in the input that came with the click winit just reported
        let _ = r.queue.dispatch_pending(&mut r.state);
        let qh = r.queue.handle();
        let token = r.manager.get_activation_token(&qh, ());
        if let (Some(serial), Some(seat)) = (r.state.serial, &r.seat) {
            token.set_serial(serial, seat);
        }
        token.set_app_id(crate::core::APP_ID.to_string());
        token.set_surface(&surface);
        token.commit();
        r.state.token = None;
        // the answer comes before the round trip ends
        let _ = r.queue.roundtrip(&mut r.state);
        token.destroy();
        r.state.token.take()
    })
}

/// Brings `window` to the front with `token`. On X11 an ordinary focus request does it.
pub fn activate(window: &slint::winit_030::winit::window::Window, token: &str) {
    if let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle())
        && !matches!(
            (display.as_raw(), handle.as_raw()),
            (RawDisplayHandle::Wayland(_), RawWindowHandle::Wayland(_))
        )
    {
        window.focus_window();
        return;
    }
    if token.is_empty() {
        return;
    }
    let _ = with_ready(window, |r, surface| {
        r.manager.activate(token.to_string(), &surface);
        let _ = r.queue.dispatch_pending(&mut r.state);
        let _ = r.conn.flush();
        Some(())
    });
}

unsafe fn attach(display: *mut std::ffi::c_void) -> Activation {
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else {
        return Activation::Unavailable;
    };
    let qh = queue.handle();
    let Ok(manager) = globals.bind::<XdgActivationV1, _, _>(&qh, 1..=1, ()) else {
        // the queue stays alive with the registry on it (see Activation)
        std::mem::forget(queue);
        return Activation::Unavailable;
    };
    let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=7, ()).ok();
    let mut state = State::default();
    // the seat says what it has; then our pointer and keyboard exist
    let _ = queue.roundtrip(&mut state);
    Activation::Ready(Box::new(Ready {
        conn,
        queue,
        manager,
        seat,
        state,
    }))
}
