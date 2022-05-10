use std::error::Error as StdError;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::time::Duration;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::{DevPath, DrmDevice, DrmEvent, GbmBufferedSurface};
use smithay::backend::egl::context::EGLContext;
use smithay::backend::egl::display::EGLDisplay;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles2::{Gles2Frame, Gles2Renderer};
use smithay::backend::renderer::{Bind, ImportDma, ImportEgl, Renderer};
use smithay::backend::session::auto::AutoSession;
use smithay::backend::session::{Session, Signal};
use smithay::backend::udev;
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::reexports::calloop::timer::{Timer, TimerHandle};
use smithay::reexports::calloop::{Dispatcher, EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::connector::State as ConnectorState;
use smithay::reexports::drm::control::Device;
use smithay::reexports::input::Libinput;
use smithay::reexports::nix::fcntl::OFlag;
use smithay::reexports::nix::sys::stat::dev_t as DeviceId;
use smithay::reexports::wayland_server::protocol::wl_output::Subpixel;
use smithay::utils::signaling::{Linkable, SignalToken, Signaler};
use smithay::wayland::dmabuf;
use smithay::wayland::output::{Mode, PhysicalProperties};

use crate::catacomb::{Backend, Catacomb, Render};
use crate::output::Output;

pub fn run() {
    let mut event_loop = EventLoop::try_new().expect("event loop");
    let udev = Udev::new(event_loop.handle());
    let mut catacomb = Catacomb::new(&mut event_loop, udev);

    // Create backend and add presently connected devices.
    let backend = UdevBackend::new(&catacomb.seat_name, None).expect("init udev");
    for (_, path) in backend.device_list() {
        let _ = catacomb.add_device(path.into());
    }

    // Setup hardware acceleration.
    let output_device = catacomb.backend.output_device.as_ref();
    let formats = output_device.map(|device| device.renderer.dmabuf_formats().cloned().collect());
    dmabuf::init_dmabuf_global(
        &mut catacomb.display.borrow_mut(),
        formats.unwrap_or_default(),
        |buffer, mut data| {
            let catacomb = data.get::<Catacomb<Udev>>().unwrap();
            let output_device = catacomb.backend.output_device.as_mut();
            output_device
                .and_then(|device| device.renderer.import_dmabuf(buffer, None).ok())
                .is_some()
        },
        None,
    );

    // Setup input handling.

    let session = catacomb.backend.session.clone();
    let mut context = Libinput::new_with_udev::<LibinputSessionInterface<_>>(session.into());
    context.udev_assign_seat(&catacomb.seat_name).expect("assign seat");

    let mut input_backend = LibinputInputBackend::new(context, None);
    input_backend.link(catacomb.backend.signaler.clone());
    event_loop
        .handle()
        .insert_source(input_backend, |event, _, catacomb| catacomb.handle_input(event))
        .expect("insert input source");

    // Handle device events.
    event_loop
        .handle()
        .insert_source(backend, move |event, _, catacomb| match event {
            UdevEvent::Added { path, .. } => {
                let _ = catacomb.add_device(path);
            },
            UdevEvent::Changed { device_id } => catacomb.change_device(device_id),
            UdevEvent::Removed { device_id } => catacomb.remove_device(device_id),
        })
        .expect("insert udev source");

    // Continously dispatch event loop.
    let display = catacomb.display.clone();
    loop {
        if event_loop.dispatch(Some(Duration::from_millis(10)), &mut catacomb).is_err() {
            eprintln!("event loop error");
            break;
        }
        display.borrow_mut().flush_clients(&mut catacomb);
    }
}

/// Udev backend shared state.
pub struct Udev {
    handle: LoopHandle<'static, Catacomb<Udev>>,
    output_device: Option<OutputDevice>,
    render_timer: TimerHandle<DeviceId>,
    signaler: Signaler<Signal>,
    session: AutoSession,
    gpu: Option<PathBuf>,
}

impl Udev {
    fn new(handle: LoopHandle<'static, Catacomb<Udev>>) -> Self {
        // Initialize the VT session.
        let (session, notifier) = AutoSession::new(None).expect("init session");
        let signaler = notifier.signaler();

        // Register session with the event loop so objects can link to the signaler.
        handle.insert_source(notifier, |_, _, _| {}).expect("insert notifier source");

        // Find active GPUs for hardware acceleration.
        let gpu = udev::primary_gpu(session.seat()).ok().flatten();

        // Create timer for internally-triggerd redraws.
        let render_timer = Timer::new().expect("setup render timer");
        let timer_handle = render_timer.handle();
        handle
            .insert_source(render_timer, |device_id, _, catacomb| catacomb.render(device_id))
            .expect("setting up render timer callback");

        Self { handle, signaler, session, gpu, render_timer: timer_handle, output_device: None }
    }
}

impl Backend for Udev {
    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn change_vt(&mut self, vt: i32) {
        let _ = self.session.change_vt(vt);
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

        let mut renderer = unsafe { Gles2Renderer::new(context, None).expect("create renderer") };

        // Initialize GPU for EGL rendering.
        if Some(path) == self.backend.gpu {
            let _ = renderer.bind_wl_display(&self.display.borrow());
        }

        // Create the surface we will render to.
        let gbm_surface = self.create_gbm_surface(&renderer, &drm, &gbm).ok_or("gbm surface")?;

        // Redraw when VT is focused.
        let device_id = drm.device_id();
        let handle = self.backend.handle.clone();
        let restart_token = self.backend.signaler.register(move |signal| match signal {
            Signal::ActivateSession | Signal::ActivateDevice { .. } => {
                handle.insert_idle(move |catacomb| catacomb.render(device_id));
            },
            _ => {},
        });

        // Listen for VBlanks.
        drm.link(self.backend.signaler.clone());
        let dispatcher = Dispatcher::new(drm, move |event, _, catacomb: &mut Catacomb<_>| {
            match event {
                DrmEvent::VBlank(_crtc) => catacomb.render(device_id),
                DrmEvent::Error(error) => eprintln!("DRM error: {}", error),
            };
        });
        let token = self.backend.handle.register_dispatcher(dispatcher)?;

        self.backend.output_device = Some(OutputDevice {
            frame_interval: Duration::from_millis(self.output.frame_interval()),
            timer: self.backend.render_timer.clone(),
            _restart_token: restart_token,
            id: device_id,
            gbm_surface,
            renderer,
            token,
            gbm,
        });

        // Kick-off rendering.
        self.render(device_id);

        Ok(())
    }

    fn remove_device(&mut self, device_id: DeviceId) {
        let output_device = self.backend.output_device.take();
        if let Some(mut output_device) = output_device.filter(|device| device.id == device_id) {
            self.backend.handle.remove(output_device.token);

            // Disable hardware acceleration when the GPU is removed.
            if output_device.gbm.dev_path() == self.backend.gpu {
                output_device.renderer.unbind_wl_display();
            }
        }
    }

    fn change_device(&mut self, device_id: DeviceId) {
        let device = self.backend.output_device.as_ref().filter(|dev| dev.id == device_id);
        let path = device.and_then(|device| device.gbm.dev_path());
        if let Some(path) = path {
            self.remove_device(device_id);
            let _ = self.add_device(path);
        }
    }

    /// Create a new GBM surface for a device.
    fn create_gbm_surface(
        &mut self,
        renderer: &Gles2Renderer,
        drm: &DrmDevice<RawFd>,
        gbm: &GbmDevice<RawFd>,
    ) -> Option<GbmBufferedSurface<GbmDevice<RawFd>, RawFd>> {
        let formats = Bind::<Dmabuf>::supported_formats(renderer)?;
        let resources = drm.resource_handles().ok()?;

        // Find the first connected output port.
        let connector = resources.connectors().iter().find_map(|conn| {
            drm.get_connector(*conn).ok().filter(|conn| conn.state() == ConnectorState::Connected)
        })?;
        let connector_mode = *connector.modes().get(0)?;

        let surface = connector
            // Get all available encoders.
            .encoders()
            .iter()
            .flatten()
            .flat_map(|handle| drm.get_encoder(*handle))
            // Get all CRTCs compatible with the encoder.
            .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
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
            refresh: connector_mode.vrefresh() as i32 * 1000,
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

    /// Render a specific device.
    fn render(&mut self, device_id: DeviceId) {
        let mut device = self.backend.output_device.take();
        if let Some(device) = device.as_mut().filter(|device| device.id == device_id) {
            self.create_frame(device);
        }
        self.backend.output_device = device;
    }
}

/// Target device for rendering.
struct OutputDevice {
    gbm_surface: GbmBufferedSurface<GbmDevice<RawFd>, RawFd>,
    timer: TimerHandle<DeviceId>,
    gbm: GbmDevice<RawFd>,
    renderer: Gles2Renderer,
    id: DeviceId,

    _restart_token: SignalToken,
    frame_interval: Duration,
    token: RegistrationToken,
}

impl Render for &mut OutputDevice {
    fn render<B, F>(
        &mut self,
        catacomb: &mut Catacomb<B>,
        draw_fun: F,
    ) -> Result<(), Box<dyn StdError>>
    where
        F: FnOnce(&mut Catacomb<B>, &mut Gles2Renderer, &mut Gles2Frame, u8),
    {
        // Mark the current frame as submitted.
        self.gbm_surface.frame_submitted()?;

        // Bind the next buffer to render into.
        let (dmabuf, age) = self.gbm_surface.next_buffer()?;
        self.renderer.bind(dmabuf)?;

        // Draw the current frame into the buffer.
        let transform = catacomb.windows.orientation().transform();
        let output_size = catacomb.output.physical_resolution();
        self.renderer.render(output_size, transform, |renderer, frame| {
            draw_fun(catacomb, renderer, frame, age)
        })?;

        // Queue buffer for rendering.
        self.gbm_surface.queue_buffer()?;

        Ok(())
    }

    fn reschedule(&mut self) {
        self.timer.add_timeout(self.frame_interval, self.id);
    }
}
