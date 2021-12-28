//! Wayland shells.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Mutex;

use smithay::backend::renderer;
use smithay::backend::renderer::gles2::{Gles2Renderer, Gles2Texture};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Logical, Physical, Size};
use smithay::wayland::compositor::{self, BufferAssignment, SurfaceAttributes, TraversalAction};
use smithay::wayland::shell::xdg::{
    self as xdg_shell, XdgRequest, XdgToplevelSurfaceRoleAttributes,
};
use smithay::wayland::SERIAL_COUNTER;
use wayland_commons::filter::DispatchData;

use crate::catacomb::Catacomb;
use crate::window::Windows;

/// Wayland shells.
pub struct Shells {
    pub windows: Rc<RefCell<Windows>>,
}

impl Shells {
    /// Initialize all available shells.
    pub fn new(display: &mut Display, renderer: &mut Gles2Renderer) -> Self {
        // Create the compositor and register a surface commit handler.
        compositor::compositor_init(display, surface_commit, None);

        let windows = Rc::new(RefCell::new(Windows::new(renderer)));

        let xdg_windows = windows.clone();
        let _ = xdg_shell::xdg_shell_init(
            display,
            move |event, mut data| match event {
                XdgRequest::NewToplevel { surface } => {
                    // Automatically focus new windows.
                    let catacomb = data.get::<Catacomb>().unwrap();
                    if let Some(wl_surface) = surface.get_surface() {
                        catacomb.keyboard.set_focus(Some(wl_surface), SERIAL_COUNTER.next_serial());
                    }

                    xdg_windows.borrow_mut().add(surface, &catacomb.output);
                },
                XdgRequest::AckConfigure { surface, .. } => {
                    // Request new frames after each resize.
                    let runtime = xdg_windows.borrow().runtime();
                    if let Some(mut window) = xdg_windows.borrow_mut().find(&surface) {
                        window.request_frame(runtime);
                    }
                },
                XdgRequest::NewClient { .. } => (),
                _ => eprintln!("UNHANDLED EVENT: {:?}", event),
            },
            None,
        );

        Self { windows }
    }
}

/// Handle a new surface commit.
fn surface_commit(surface: WlSurface, mut data: DispatchData) {
    if compositor::is_sync_subsurface(&surface) {
        return;
    }

    // Find the window responsible for the surface.
    let catacomb = data.get::<Catacomb>().unwrap();
    let mut windows = catacomb.windows.borrow_mut();
    let mut window = match windows.find(&surface) {
        Some(window) => window,
        None => return,
    };

    // Cancel transactions on the commit after the configure was acked.
    let _ = compositor::with_states(&surface, |states| {
        let attributes = states
            .data_map
            .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
            .and_then(|attributes| attributes.lock().ok());

        if let Some(attributes) = attributes {
            window.acked_size = attributes.current.size.unwrap_or_default();
        }
    });

    // Handle surface buffer changes.
    compositor::with_surface_tree_upward(
        &surface,
        (),
        |_, data, _| {
            let mut attributes = data.cached_state.current::<SurfaceAttributes>();
            if let Some(assignment) = attributes.buffer.take() {
                data.data_map.insert_if_missing(|| RefCell::new(SurfaceBuffer::new()));
                let buffer = data.data_map.get::<RefCell<SurfaceBuffer>>().unwrap();
                buffer.borrow_mut().update_buffer(&attributes, assignment);
            }
            TraversalAction::DoChildren(())
        },
        |_, _, _| (),
        |_, _, _| true,
    );
    window.buffers_pending = true;

    // Send initial configure after the first commit.
    if !window.initial_configure_sent {
        window.initial_configure_sent = true;
        window.reconfigure();
    }

    // Advertise current output to visible surfaces.
    if window.visible() {
        catacomb.output.enter(&surface);
    }
}

/// Surface buffer cache.
pub struct SurfaceBuffer {
    pub texture: Option<Rc<Gles2Texture>>,
    pub buffer: Option<Buffer>,
    pub scale: i32,

    dimensions: Size<i32, Physical>,
}

impl Default for SurfaceBuffer {
    fn default() -> Self {
        Self {
            scale: 1,
            dimensions: Default::default(),
            texture: Default::default(),
            buffer: Default::default(),
        }
    }
}

impl SurfaceBuffer {
    fn new() -> Self {
        Self::default()
    }

    /// Handle buffer creation/removal.
    fn update_buffer(&mut self, attributes: &SurfaceAttributes, assignment: BufferAssignment) {
        match assignment {
            BufferAssignment::NewBuffer { buffer, .. } => {
                self.dimensions = renderer::buffer_dimensions(&buffer).unwrap_or_default();
                self.scale = attributes.buffer_scale;
                self.buffer = Some(Buffer(buffer));
                self.texture = None;
            },
            BufferAssignment::Removed => *self = Self::default(),
        }
    }

    /// Surface size.
    pub fn size(&self) -> Size<i32, Logical> {
        self.dimensions.to_logical(self.scale)
    }
}

/// Container for automatically releasing a buffer on drop.
pub struct Buffer(WlBuffer);

impl Drop for Buffer {
    fn drop(&mut self) {
        self.0.release();
    }
}

impl Deref for Buffer {
    type Target = WlBuffer;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
