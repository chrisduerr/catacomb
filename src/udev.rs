use std::error::Error as StdError;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::time::Duration;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::{DrmDevice, DrmEvent, GbmBufferedSurface};
use smithay::backend::egl::context::EGLContext;
use smithay::backend::egl::display::EGLDisplay;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles2::Gles2Renderer;
use smithay::backend::renderer::Bind;
use smithay::backend::session::auto::{AutoSession, AutoSessionNotifier};
use smithay::backend::session::{Session, Signal};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::reexports::calloop::{Dispatcher, EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::connector::State as ConnectorState;
use smithay::reexports::drm::control::crtc::Handle as CrtcHandle;
use smithay::reexports::drm::control::Device;
use smithay::reexports::input::Libinput;
use smithay::reexports::nix::fcntl::OFlag;
use smithay::reexports::nix::sys::stat::dev_t as DeviceId;
use smithay::reexports::wayland_server::protocol::wl_output::Subpixel;
use smithay::reexports::wayland_server::Display;
use smithay::utils::signaling::{Linkable, SignalToken, Signaler};
use smithay::wayland::output::{Mode, PhysicalProperties};

use crate::catacomb::{Backend, Catacomb};
use crate::output::Output;

mod catacomb;
mod drawing;
mod geometry;
mod input;
mod layer;
mod output;
mod overview;
mod shell;
mod window;

struct Udev {
    handle: LoopHandle<'static, Catacomb<Udev>>,
    output_device: Option<OutputDevice>,
    signaler: Signaler<Signal>,
    session: AutoSession,
}

impl Udev {
    fn new(
        event_loop: &EventLoop<Catacomb<Udev>>,
        handle: LoopHandle<'static, Catacomb<Udev>>,
    ) -> Self {
        let (session, notifier) = AutoSession::new(None).expect("init session");
        let signaler = notifier.signaler();

        // Register session with the event loop for objects linking to the signaler.
        event_loop.handle().insert_source(notifier, |_, _, _| {}).expect("insert notifier source");

        Self { handle, signaler, session, output_device: None }
    }
}

impl Backend for Udev {
    fn seat_name(&self) -> String {
        self.session.seat()
    }
}

struct OutputDevice {
    gbm_buffer: GbmBufferedSurface<RawFd>,
    gbm: GbmDevice<RawFd>,
    renderer: Gles2Renderer,
    device_id: DeviceId,

    dispatcher: Dispatcher<'static, DrmDevice<RawFd>, Catacomb<Udev>>,
    _restart_token: SignalToken,
    token: RegistrationToken,
}

fn main() {
    let mut event_loop = EventLoop::try_new().expect("event loop");
    let mut display = Display::new();

    // TODO: Having to create a dummy output is stupid.
    let mode = Mode { size: (0, 0).into(), refresh: 200_000 };
    let output = Output::new(&mut display, "output-0", mode, PhysicalProperties {
        subpixel: Subpixel::Unknown,
        model: "model-0".into(),
        make: "make-0".into(),
        size: (0, 0).into(),
    });

    let udev = Udev::new(&event_loop, event_loop.handle());

    let mut catacomb = Catacomb::new(display, output, udev, &mut event_loop);

    let backend = UdevBackend::new(&catacomb.seat_name, None).expect("init udev");

    let session = catacomb.backend.session.clone();
    let mut context = Libinput::new_with_udev::<LibinputSessionInterface<_>>(session.into());
    context.udev_assign_seat(&catacomb.seat_name).expect("assign seat");

    let mut input_backend = LibinputInputBackend::new(context, None);
    input_backend.link(catacomb.backend.signaler.clone());
    event_loop
        .handle()
        .insert_source(input_backend, |event, _, catacomb| catacomb.handle_input(event))
        .expect("insert input source");

    for (_, path) in backend.device_list() {
        let _ = catacomb.add_device(path.into());
    }

    event_loop
        .handle()
        .insert_source(backend, move |event, _, catacomb| match event {
            UdevEvent::Added { device_id, path } => {
                let _ = catacomb.add_device(path);
            },
            UdevEvent::Changed { device_id } => catacomb.change_device(device_id),
            UdevEvent::Removed { device_id } => catacomb.remove_device(device_id),
        })
        .expect("insert udev source");

    let display = catacomb.display.clone();
    loop {
        if event_loop.dispatch(Some(Duration::from_millis(5)), &mut catacomb).is_err() {
            eprintln!("event loop error");
            break;
        }

        // TODO: Refresh windows?
        display.borrow_mut().flush_clients(&mut catacomb);
    }
}

impl Catacomb<Udev> {
    fn add_device(&mut self, path: PathBuf) -> Result<(), Box<dyn StdError>> {
        let open_flags = OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOCTTY | OFlag::O_NONBLOCK;
        let device_fd = self.backend.session.open(&path, open_flags)?;

        let mut drm = DrmDevice::new(device_fd, true, None)?;
        let gbm = GbmDevice::new(device_fd)?;

        let display = EGLDisplay::new(&gbm, None)?;
        let context = EGLContext::new(&display, None)?;

        let renderer = unsafe { Gles2Renderer::new(context, None)? };

        let gbm_buffer = self.xxx(&renderer, &drm, &gbm).ok_or("could not create gbm buffer")?;

        // TODO: What the fuck is this?
        let device_id = drm.device_id();
        let mut handle = self.backend.handle.clone();
        let restart_token = self.backend.signaler.register(move |signal| match signal {
            Signal::ActivateSession | Signal::ActivateDevice { .. } => {
                handle.insert_idle(move |catacomb| catacomb.render(device_id));
            },
            _ => {},
        });

        // TODO: Formatting here sucks.
        drm.link(self.backend.signaler.clone());
        let dispatcher =
            Dispatcher::new(drm, move |event, _, catacomb: &mut Catacomb<_>| match event {
                DrmEvent::VBlank(crtc) => catacomb.render(device_id),
                DrmEvent::Error(error) => eprintln!("DRM error: {}", error),
            });
        let token = self.backend.handle.register_dispatcher(dispatcher.clone())?;

        // TODO: Render once?

        self.backend.output_device = Some(OutputDevice {
            _restart_token: restart_token,
            gbm_buffer,
            dispatcher,
            device_id,
            renderer,
            token,
            gbm,
        });

        Ok(())
    }

    fn remove_device(&mut self, device_id: DeviceId) {
        let output_device = self.backend.output_device.take();
        if let Some(output_device) = output_device.filter(|device| device.device_id == device_id) {
            self.backend.handle.remove(output_device.token);
        }
    }

    fn change_device(&mut self, device_id: DeviceId) {
        self.remove_device(device_id);
        self.add_device();
    }

    // TODO: Biggus cleanupus.
    fn xxx(
        &mut self,
        renderer: &Gles2Renderer,
        drm: &DrmDevice<RawFd>,
        gbm: &GbmDevice<RawFd>,
    ) -> Option<GbmBufferedSurface<RawFd>> {
        let formats = Bind::<Dmabuf>::supported_formats(renderer)?;
        let resources = drm.resource_handles().ok()?;

        // Find the first connected output port.
        let connector = resources.connectors().iter().find_map(|conn| {
            drm.get_connector(*conn).ok().filter(|conn| conn.state() != ConnectorState::Connected)
        })?;
        let connector_mode = connector.modes()[0];

        let surface = connector
            // Get all available encoders.
            .encoders()
            .iter()
            .flatten()
            .flat_map(|handle| drm.get_encoder(*handle))
            // Get all CRTCs compatible with the encoder.
            .map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
            .flatten()
            // Try to create a DRM surface.
            .flat_map(|crtc| drm.create_surface(crtc, connector_mode, &[connector.handle()]))
            // Yield the first successful GBM buffer creation.
            .find_map(|mut surface| {
                surface.link(self.backend.signaler.clone());
                GbmBufferedSurface::new(surface, gbm.clone(), formats.clone(), None).ok()
            })?;

        let (width, height) = connector_mode.size();
        let mode = Mode {
            size: (width as i32, height as i32).into(),
            refresh: (connector_mode.vrefresh() * 1000) as i32,
        };

        let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
        let output_name = format!("{:?}", connector.interface());
        let mut display = self.display.borrow_mut();

        self.output = Output::new(&mut display, output_name, mode, PhysicalProperties {
            size: (physical_width as i32, physical_height as i32).into(),
            subpixel: Subpixel::Unknown,
            model: "Generic DRM".into(),
            make: "Catacomb".into(),
        });

        Some(surface)
    }

    fn render(&self, device_id: DeviceId) {
        // TODO
    }
}
