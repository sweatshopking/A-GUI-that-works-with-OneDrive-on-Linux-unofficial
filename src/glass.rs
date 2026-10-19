//! Asking the desktop to blur what's behind the see-through window, through ext-background-effect,
//! the standard Wayland way (KWin 6.6 and later, and other desktops as they adopt it). Older KWin
//! only has its own protocol, which winit speaks (`set_blur`), so the window asks both ways.

use std::cell::RefCell;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_region, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
};

const KDE_BLUR: &str = "org_kde_kwin_blur_manager";

#[derive(Default)]
struct State {
    blur_capable: bool,
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

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            state.blur_capable = flags
                .into_result()
                .is_ok_and(|f| f.contains(ext_background_effect_manager_v1::Capability::Blur));
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
no_events!(
    ExtBackgroundEffectSurfaceV1,
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_surface::WlSurface
);

/// Whether the desktop blurs what's behind see-through windows when asked to. Asks the desktop
/// itself over a short Wayland connection of its own, so it works before the window exists.
pub fn desktop_blurs() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false; // not Wayland: there's no way to ask for blur
    };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else {
        return false;
    };
    let qh = queue.handle();
    let mut state = State::default();
    match globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ()) {
        Ok(_) => {
            // the capabilities come in reply to binding
            let _ = queue.roundtrip(&mut state);
            state.blur_capable
        }
        Err(_) => globals
            .contents()
            .with_list(|list| list.iter().any(|g| g.interface == KDE_BLUR)),
    }
}

/// The blur request for the open window, on the window's own Wayland connection.
struct WindowBlur {
    surface_ptr: usize,
    conn: Connection,
    queue: EventQueue<State>,
    compositor: wl_compositor::WlCompositor,
    effect: ExtBackgroundEffectSurfaceV1,
}

thread_local! {
    static WINDOW_BLUR: RefCell<Option<WindowBlur>> = const { RefCell::new(None) };
}

/// Turns the blur behind `window` on or off. The change shows with the window's next frame.
pub fn set(window: &slint::winit_030::winit::window::Window, on: bool) {
    window.set_blur(on); // KWin's own protocol, where that's what the desktop has
    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else {
        return;
    };
    let (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) =
        (display.as_raw(), handle.as_raw())
    else {
        return; // X11 has no standard way to ask
    };
    let surface_ptr = w.surface.as_ptr() as usize;
    WINDOW_BLUR.with_borrow_mut(|slot| {
        // the window's surface is new after it was hidden and shown again
        if slot.as_ref().is_none_or(|b| b.surface_ptr != surface_ptr) {
            // SAFETY: both pointers come from the live winit window and stay valid while it
            // exists; the requests go out on a queue of our own, next to winit's
            *slot = unsafe { attach(d.display.as_ptr(), w.surface.as_ptr(), surface_ptr) };
        }
        let Some(b) = slot.as_mut() else { return };
        let qh = b.queue.handle();
        if on {
            let region = b.compositor.create_region(&qh, ());
            region.add(0, 0, i32::MAX, i32::MAX);
            b.effect.set_blur_region(Some(&region));
            region.destroy();
        } else {
            b.effect.set_blur_region(None);
        }
        let _ = b.conn.flush();
    });
    window.request_redraw(); // the region applies with the surface's next commit
}

unsafe fn attach(
    display: *mut std::ffi::c_void,
    surface: *mut std::ffi::c_void,
    surface_ptr: usize,
) -> Option<WindowBlur> {
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();
    let manager = globals
        .bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ())
        .ok()?;
    let compositor = globals
        .bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=4, ())
        .ok()?;
    let id =
        unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()) }.ok()?;
    let surface = wl_surface::WlSurface::from_id(&conn, id).ok()?;
    let effect = manager.get_background_effect(&surface, &qh, ());
    let _ = queue.roundtrip(&mut State::default());
    Some(WindowBlur {
        surface_ptr,
        conn,
        queue,
        compositor,
        effect,
    })
}
