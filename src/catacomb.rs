//! Catacomb compositor state.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::{env, io};

use smithay::backend::renderer::gles2::{Gles2Frame, Gles2Renderer};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode as TriggerMode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::seat::{KeyboardHandle, Seat, XkbConfig};
use smithay::wayland::shell::legacy::decoration as kde_decoration;
use smithay::wayland::shell::legacy::decoration::KdeDecorationRequest;
use smithay::wayland::shell::xdg::decoration;
use smithay::wayland::{data_device, shm};
use wayland_protocols::misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode;

use crate::input::TouchState;
use crate::output::Output;
use crate::shell::Shells;
use crate::window::Windows;

/// Shared compositor state.
pub struct Catacomb {
    pub windows: Rc<RefCell<Windows>>,
    pub keyboard: KeyboardHandle,
    pub touch_state: TouchState,
    pub terminated: bool,
    pub output: Output,

    // NOTE: Must be last field to ensure it's dropped after any global.
    pub display: Rc<RefCell<Display>>,
}

impl Catacomb {
    /// Initialize the compositor.
    pub fn new(
        mut display: Display,
        output: Output,
        event_loop: &mut EventLoop<Self>,
        renderer: &mut Gles2Renderer,
    ) -> Self {
        // Create our Wayland socket.
        let socket_name = &mut display
            .add_socket_auto()
            .expect("wayland socket")
            .into_string()
            .expect("wayland socket name");
        env::set_var("WAYLAND_DISPLAY", &socket_name);
        println!("Wayland socket: {}", socket_name);

        // Subscribe to Wayland socket events.
        event_loop
            .handle()
            .insert_source(
                Generic::from_fd(display.get_poll_fd(), Interest::READ, TriggerMode::Level),
                |_, _, catacomb| catacomb.handle_socket_readiness(),
            )
            .expect("register wayland socket source");

        // Advertise support for rendering from CPU-based shared memory buffers.
        shm::init_shm_global(&mut display, Vec::new(), None);

        // Force server-side decorations.
        decoration::init_xdg_decoration_manager(&mut display, |_, _| {}, None);
        kde_decoration::init_kde_decoration_manager(
            &mut display,
            |request| match request {
                KdeDecorationRequest::Setup { manager } => manager.default_mode(Mode::Server),
                KdeDecorationRequest::RequestMode { decoration, mode, .. } => decoration.mode(mode),
                _ => (),
            },
            None,
        );

        // Initialize input.
        let (mut seat, _) = Seat::new(&mut display, String::from("seat-0"), None);
        data_device::init_data_device(
            &mut display,
            |_| {},
            data_device::default_action_chooser,
            None,
        );
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 200, 25, |seat, focused_surface| {
                data_device::set_data_device_focus(
                    seat,
                    focused_surface.and_then(|surface| surface.as_ref().client()),
                )
            })
            .expect("adding keyboard");

        // Initialize all available shells.
        let shells = Shells::new(&mut display, renderer);

        Self {
            display: Rc::new(RefCell::new(display)),
            windows: shells.windows,
            keyboard,
            output,
            touch_state: Default::default(),
            terminated: Default::default(),
        }
    }

    /// Handle Wayland event socket read readiness.
    fn handle_socket_readiness(&mut self) -> io::Result<PostAction> {
        let display = self.display.clone();
        let mut display = display.borrow_mut();
        match display.dispatch(Duration::from_millis(0), self) {
            Ok(_) => Ok(PostAction::Continue),
            Err(e) => {
                eprintln!("I/O error on the Wayland display: {}", e);
                self.terminated = true;
                Err(e)
            },
        }
    }

    /// Render the current compositor state.
    pub fn draw(&mut self, renderer: &mut Gles2Renderer, frame: &mut Gles2Frame) {
        self.windows.borrow_mut().draw(renderer, frame, &self.output);
    }
}
