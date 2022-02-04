//! Wayland shells.

use std::cell::RefCell;
use std::rc::Rc;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::shell::wlr_layer::{self, LayerShellRequest};
use smithay::wayland::shell::xdg::{self as xdg_shell, XdgRequest};
use smithay::wayland::{compositor, SERIAL_COUNTER};
use wayland_commons::filter::DispatchData;

use crate::catacomb::Catacomb;
use crate::window::Windows;

/// Wayland shells.
pub struct Shells {
    pub windows: Rc<RefCell<Windows>>,
}

impl Shells {
    /// Initialize all available shells.
    pub fn new<B: 'static>(display: &mut Display) -> Self {
        // Create the compositor and register a surface commit handler.
        compositor::compositor_init(display, surface_commit::<B>, None);

        let windows = Rc::new(RefCell::new(Windows::new()));

        // XDG Shell.
        let xdg_windows = windows.clone();
        let _ = xdg_shell::xdg_shell_init(
            display,
            move |event, mut data| match event {
                XdgRequest::NewToplevel { surface } => {
                    // Automatically focus new windows.
                    let catacomb = data.get::<Catacomb<B>>().unwrap();
                    if let Some(wl_surface) = surface.get_surface() {
                        catacomb.keyboard.set_focus(Some(wl_surface), SERIAL_COUNTER.next_serial());
                    }

                    xdg_windows.borrow_mut().add(surface, &catacomb.output);
                },
                XdgRequest::AckConfigure { surface, .. } => {
                    // Request new frames after each resize.
                    let runtime = xdg_windows.borrow().runtime();
                    if let Some(mut window) = xdg_windows.borrow_mut().find_xdg(&surface) {
                        window.request_frame(runtime);
                    }
                },
                XdgRequest::NewClient { .. } => (),
                _ => eprintln!("UNHANDLED EVENT: {event:?}"),
            },
            None,
        );

        // Layer shell.
        let layer_windows = windows.clone();
        wlr_layer::wlr_layer_shell_init(
            display,
            move |event, _data| match event {
                LayerShellRequest::NewLayerSurface { surface, layer, .. } => {
                    layer_windows.borrow_mut().add_layer(layer, surface);
                },
                LayerShellRequest::AckConfigure { .. } => (),
            },
            None,
        );

        Self { windows }
    }
}

/// Handle a new surface commit.
fn surface_commit<B: 'static>(surface: WlSurface, mut data: DispatchData) {
    if compositor::is_sync_subsurface(&surface) {
        return;
    }

    let catacomb = data.get::<Catacomb<B>>().unwrap();
    catacomb.windows.borrow_mut().surface_commit(&surface, &mut catacomb.output);
}
