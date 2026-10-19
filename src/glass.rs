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

/// What the window's own Wayland connection offers for blur, set up once and kept for as long as
/// the app runs: dropping its queue with objects still on it would leave them dangling.
enum Blur {
    /// the desktop has no ext-background-effect (KWin before 6.6, GNOME): winit's KWin blur only
    Unavailable,
    Ready(Box<WindowBlur>),
}

struct WindowBlur {
    conn: Connection,
    queue: EventQueue<State>,
    manager: ExtBackgroundEffectManagerV1,
    compositor: wl_compositor::WlCompositor,
    /// the blur request for the open window's surface, and that surface's address
    effect: Option<(usize, ExtBackgroundEffectSurfaceV1)>,
}

thread_local! {
    static WINDOW_BLUR: RefCell<Option<Blur>> = const { RefCell::new(None) };
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
        // SAFETY: the display pointer comes from the live winit window, and winit's display lives
        // as long as the event loop; the requests go out on a queue of our own, next to winit's
        let blur = slot.get_or_insert_with(|| unsafe { attach(d.display.as_ptr()) });
        let Blur::Ready(b) = blur else { return };
        let qh = b.queue.handle();
        if b.effect.as_ref().is_none_or(|(p, _)| *p != surface_ptr) {
            if let Some((_, old)) = b.effect.take() {
                old.destroy();
            }
            // SAFETY: the surface pointer comes from the live winit window
            let Ok(id) = (unsafe {
                ObjectId::from_ptr(
                    wl_surface::WlSurface::interface(),
                    w.surface.as_ptr().cast(),
                )
            }) else {
                return;
            };
            let Ok(surface) = wl_surface::WlSurface::from_id(&b.conn, id) else {
                return;
            };
            b.effect = Some((
                surface_ptr,
                b.manager.get_background_effect(&surface, &qh, ()),
            ));
        }
        let Some((_, effect)) = &b.effect else { return };
        if on {
            let region = b.compositor.create_region(&qh, ());
            region.add(0, 0, i32::MAX, i32::MAX);
            effect.set_blur_region(Some(&region));
            region.destroy();
        } else {
            effect.set_blur_region(None);
        }
        let _ = b.queue.dispatch_pending(&mut State::default());
        let _ = b.conn.flush();
    });
    window.request_redraw(); // the region applies with the surface's next commit
}

/// Drops the blur request while the window still exists. Call it before the window is hidden:
/// on Wayland hiding destroys the window, and a request left on a destroyed surface is an error
/// the desktop answers by disconnecting the app (a new window can reuse the old one's address).
pub fn forget() {
    WINDOW_BLUR.with_borrow_mut(|slot| {
        if let Some(Blur::Ready(b)) = slot
            && let Some((_, effect)) = b.effect.take()
        {
            effect.destroy();
            let _ = b.conn.flush();
        }
    });
}

unsafe fn attach(display: *mut std::ffi::c_void) -> Blur {
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let Ok((globals, queue)) = registry_queue_init::<State>(&conn) else {
        return Blur::Unavailable;
    };
    let qh = queue.handle();
    let (Ok(manager), Ok(compositor)) = (
        globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ()),
        globals.bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=4, ()),
    ) else {
        // the queue stays alive with the registry on it (see Blur)
        std::mem::forget(queue);
        return Blur::Unavailable;
    };
    Blur::Ready(Box::new(WindowBlur {
        conn,
        queue,
        manager,
        compositor,
        effect: None,
    }))
}
