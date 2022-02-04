use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::{Frame, ImportDma, ImportEgl, Renderer, TextureFilter};
use smithay::backend::winit;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::protocol::wl_output::Subpixel;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Rectangle, Transform};
use smithay::wayland::dmabuf;
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

struct Winit;

impl Backend for Winit {
    fn seat_name(&self) -> String {
        String::from("seat-0")
    }
}

fn main() {
    let mut display = Display::new();

    let (graphics, mut input) = winit::init(None).expect("init winit");
    let graphics = Rc::new(RefCell::new(graphics));
    graphics.borrow_mut().bind().expect("binding renderer");
    let _ = graphics.borrow_mut().renderer().downscale_filter(TextureFilter::Linear);

    // Setup hardware acceleration.
    if graphics.borrow_mut().renderer().bind_wl_display(&display).is_ok() {
        let formats: Vec<_> = graphics.borrow_mut().renderer().dmabuf_formats().cloned().collect();
        let graphics = graphics.clone();
        dmabuf::init_dmabuf_global(
            &mut display,
            formats,
            move |buffer, _| graphics.borrow_mut().renderer().import_dmabuf(buffer).is_ok(),
            None,
        );
    }

    let mode = Mode { size: graphics.borrow().window_size().physical_size, refresh: 200_000 };
    let output = Output::new(&mut display, "output-0", mode, PhysicalProperties {
        subpixel: Subpixel::Unknown,
        model: "model-0".into(),
        make: "make-0".into(),
        size: (0, 0).into(),
    });

    let mut event_loop = EventLoop::try_new().expect("event loop");
    let mut catacomb = Catacomb::new(display, output, Winit, &mut event_loop);

    let display = catacomb.display.clone();
    loop {
        if input.dispatch_new_events(|event| catacomb.handle_winit_input(event)).is_err() {
            eprintln!("input error");
            break;
        }

        let logical_size = catacomb.output.screen_size().to_f64();
        let output_size = logical_size.to_physical(catacomb.output.scale).to_i32_round();
        graphics
            .borrow_mut()
            .renderer()
            .render(output_size, Transform::Flipped180, |renderer, frame| {
                let full_rect = Rectangle::from_loc_and_size((0, 0), output_size);
                let _ = frame.clear([1., 0., 1., 1.], &[full_rect]);
                catacomb.draw(renderer, frame);
            })
            .expect("render");
        graphics.borrow_mut().submit(None, 1.0).expect("submit");

        // Handle window liveliness changes.
        catacomb.windows.borrow_mut().refresh(&mut catacomb.output);

        catacomb.windows.borrow_mut().request_frames();
        display.borrow_mut().flush_clients(&mut catacomb);

        // NOTE: The timeout picked here is 5ms to allow for up to 200 FPS. Increasing it would
        // reduce the framerate, while decreasing it would mean that most of the vblank interval is
        // spent not doing anything, rather than handling events.
        if event_loop.dispatch(Some(Duration::from_millis(5)), &mut catacomb).is_err() {
            eprintln!("event loop error");
            break;
        }

        display.borrow_mut().flush_clients(&mut catacomb);
    }
}
