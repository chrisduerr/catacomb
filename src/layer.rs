//! Layer shell windows.

use smithay::backend::renderer::gles2::{Gles2Frame, Gles2Renderer};
use smithay::utils::{Logical, Point};
use smithay::wayland::shell::wlr_layer::Layer;

use crate::output::Output;
use crate::window::{CatacombLayerSurface, Window};

type LayerWindow = Window<CatacombLayerSurface>;

/// Layer shell windows.
#[derive(Debug, Default)]
pub struct Layers {
    background: Vec<LayerWindow>,
    bottom: Vec<LayerWindow>,
    top: Vec<LayerWindow>,
    overlay: Vec<LayerWindow>,
}

impl Layers {
    /// Add a new layer shell window.
    pub fn add(&mut self, layer: Layer, surface: CatacombLayerSurface) {
        let window = Window::new(surface);
        match layer {
            Layer::Background => self.background.push(window),
            Layer::Bottom => self.bottom.push(window),
            Layer::Top => self.top.push(window),
            Layer::Overlay => self.overlay.push(window),
        }
    }

    /// Request new frames for all layer windows.
    pub fn iter(&self) -> impl Iterator<Item = &LayerWindow> {
        self.background.iter().chain(&self.bottom).chain(&self.top).chain(&self.overlay)
    }

    /// Request new frames for all layer windows.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut LayerWindow> {
        self.background
            .iter_mut()
            .chain(&mut self.bottom)
            .chain(&mut self.top)
            .chain(&mut self.overlay)
    }

    /// Draw background/bottom layer windows.
    pub fn draw_background(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        buffer_age: u8,
    ) {
        for window in &mut self.background {
            window.draw(renderer, frame, output, 1., None, buffer_age);
        }

        for window in &mut self.bottom {
            window.draw(renderer, frame, output, 1., None, buffer_age);
        }
    }

    /// Draw top/overlay layer windows.
    pub fn draw_foreground(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        buffer_age: u8,
    ) {
        for window in &mut self.top {
            window.draw(renderer, frame, output, 1., None, buffer_age);
        }

        for window in &mut self.overlay {
            window.draw(renderer, frame, output, 1., None, buffer_age);
        }
    }

    /// Request new frames from all layer shell windows.
    pub fn request_frames(&mut self, runtime: u32) {
        for window in self.iter_mut() {
            window.request_frame(runtime);
        }
    }

    /// Foreground window at the specified position.
    pub fn foreground_window_at(&self, position: Point<f64, Logical>) -> Option<&LayerWindow> {
        self.overlay
            .iter()
            .find(|window| window.contains(position))
            .or_else(|| self.top.iter().find(|window| window.contains(position)))
    }

    /// Background window at the specified position.
    pub fn background_window_at(&self, position: Point<f64, Logical>) -> Option<&LayerWindow> {
        self.bottom
            .iter()
            .find(|window| window.contains(position))
            .or_else(|| self.background.iter().find(|window| window.contains(position)))
    }

    /// Apply all pending transactional updates.
    pub fn apply_transaction(&mut self) {
        Self::apply_window_transactions(&mut self.background);
        Self::apply_window_transactions(&mut self.bottom);
        Self::apply_window_transactions(&mut self.top);
        Self::apply_window_transactions(&mut self.overlay);
    }

    /// Apply transactions to all windows and remove dead ones.
    fn apply_window_transactions(windows: &mut Vec<LayerWindow>) {
        for i in (0..windows.len()).rev() {
            if windows[i].alive() {
                windows[i].apply_transaction();
            } else {
                windows.remove(i);
            }
        }
    }
}
