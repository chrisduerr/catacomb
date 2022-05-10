//! Drawing utilities.

use std::ops::Deref;
use std::rc::Rc;

use smithay::backend::renderer::gles2::{ffi, Gles2Frame, Gles2Renderer, Gles2Texture};
use smithay::backend::renderer::{self, Frame};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::utils::{Buffer as BufferSpace, Logical, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{BufferAssignment, Damage as SurfaceDamage, SurfaceAttributes};

use crate::geometry::Vector;
use crate::output::Output;
use crate::overview::FG_OVERVIEW_PERCENTAGE;

/// Maximum buffer age before damage information is discarded.
pub const MAX_DAMAGE_AGE: usize = 2;

/// Color of the hovered overview tiling location highlight.
const ACTIVE_DROP_TARGET_RGBA: [u8; 4] = [128, 128, 128, 128];

/// Color of the overview tiling location highlight.
const DROP_TARGET_RGBA: [u8; 4] = [128, 128, 128, 64];

/// Background color behind half-size windows in the overview.
const BACKGROUND_RGBA: [u8; 4] = [0, 0, 0, 255];

/// Decoration titlebar color in the overview.
const TITLE_RGBA: [u8; 4] = [64, 64, 64, 255];

/// Decoration border color in the overview.
const BORDER_RGBA: [u8; 4] = [32, 32, 32, 255];

/// Height of the window decoration title in the application overview with a DPR of 1.
const OVERVIEW_TITLE_HEIGHT: i32 = 30;

/// Width of the window decoration border in the application overview with a DPR of 1.
const OVERVIEW_BORDER_WIDTH: i32 = 1;

/// Size of the debug touch rectangle.
const TOUCH_DEBUG_SIZE: usize = 50;

/// Cached texture.
///
/// Includes all information necessary to render a surface's texture even after
/// the surface itself has already died.
#[derive(Clone, Debug)]
pub struct Texture {
    damage: [Rectangle<f64, Physical>; MAX_DAMAGE_AGE],
    location: Point<i32, Logical>,
    texture: Rc<Gles2Texture>,
    size: Size<i32, Logical>,
    transform: Transform,
    scale: i32,
}

impl Texture {
    pub fn new(
        texture: Rc<Gles2Texture>,
        size: impl Into<Size<i32, Logical>>,
        output_scale: f64,
    ) -> Self {
        let size = size.into();
        let physical_size = size.to_f64().to_physical(output_scale);
        let damage = [Rectangle::from_loc_and_size((0., 0.), physical_size); MAX_DAMAGE_AGE];
        Self {
            scale: 1,
            texture,
            damage,
            size,
            transform: Default::default(),
            location: Default::default(),
        }
    }

    pub fn from_surface(
        texture: Rc<Gles2Texture>,
        location: impl Into<Point<i32, Logical>>,
        buffer: &SurfaceBuffer,
    ) -> Self {
        Self {
            damage: buffer.damage.physical,
            transform: buffer.transform,
            location: location.into(),
            scale: buffer.scale,
            size: buffer.size,
            texture,
        }
    }

    /// Create a texture from an RGBA buffer.
    pub fn from_buffer(
        renderer: &mut Gles2Renderer,
        buffer: &[u8],
        width: i32,
        height: i32,
        output_scale: f64,
    ) -> Self {
        assert!(buffer.len() as i32 >= width * height * 4);

        let texture = renderer
            .with_context(|renderer, gl| unsafe {
                let mut tex = 0;
                gl.GenTextures(1, &mut tex);
                gl.BindTexture(ffi::TEXTURE_2D, tex);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);
                gl.TexImage2D(
                    ffi::TEXTURE_2D,
                    0,
                    ffi::RGBA as i32,
                    width,
                    height,
                    0,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE as u32,
                    buffer.as_ptr() as *const _,
                );
                gl.BindTexture(ffi::TEXTURE_2D, 0);

                Gles2Texture::from_raw(renderer, tex, (width, height).into())
            })
            .expect("create texture");

        Texture::new(Rc::new(texture), (width, height), output_scale)
    }

    /// Render the texture at the specified location.
    ///
    /// Using the `window_bounds` and `window_scale` parameters, it is possible to scale the
    /// texture and truncate it to be within the specified window bounds. The scaling will always
    /// take part **before** the truncation.
    pub fn draw_at(
        &mut self,
        frame: &mut Gles2Frame,
        output: &Output,
        window_bounds: Rectangle<i32, Logical>,
        window_scale: f64,
        buffer_age: u8,
    ) {
        // Skip textures completely outside of the window bounds.
        let scaled_window_bounds = window_bounds.size.scale(1. / window_scale).max((1, 1));
        if self.location.x >= scaled_window_bounds.w || self.location.y >= scaled_window_bounds.h {
            return;
        }

        // Truncate source size based on window bounds.
        let src_size = (self.size + self.location).min(scaled_window_bounds);
        let src = Rectangle::from_loc_and_size((0, 0), src_size);
        let src_buffer = src.to_buffer(self.scale, self.transform, &self.size);

        // Scale output size based on window scale.
        let location = window_bounds.loc + self.location.scale(window_scale);
        let dst_size = src_size.scale(window_scale).min(window_bounds.size);
        let dst = Rectangle::from_loc_and_size(location, dst_size);
        let dst_physical = dst.to_f64().to_physical(output.scale());

        // Calculate buffer damage.
        let buffer_age = buffer_age as usize;
        let full_damage = [Rectangle::from_loc_and_size((0., 0.), dst_physical.size)];
        let damage = (buffer_age != 0).then(|| &self.damage[..buffer_age]).unwrap_or(&full_damage);

        // Skip rendering surfaces without damage.
        if damage.iter().all(|rect| rect == &Rectangle::default()) {
            return;
        }

        let _ = frame.render_texture_from_to(
            &self.texture,
            src_buffer,
            dst_physical,
            damage,
            self.transform,
            1.,
        );
    }

    /// Texture dimensions.
    pub fn size(&self) -> Size<i32, Logical> {
        self.size
    }
}

/// Grahpics texture cache.
#[derive(Debug, Default)]
pub struct Graphics {
    active_drop_target: Option<Texture>,
    drop_target: Option<Texture>,
    decoration: Option<Texture>,
    touch_debug: Option<Texture>,
}

impl Graphics {
    /// Get the window decoration texture corresponding to the active output size.
    pub fn decoration(&mut self, renderer: &mut Gles2Renderer, output: &Output) -> &mut Texture {
        let expected_size = Self::decoration_size(output);
        if self.decoration.as_ref().map(|decoration| decoration.size) != Some(expected_size) {
            self.decoration = None;
        }

        self.decoration.get_or_insert_with(|| Self::create_decoration(renderer, output))
    }

    /// Get the texture for the hovered overview drop target area.
    pub fn active_drop_target(
        &mut self,
        renderer: &mut Gles2Renderer,
        output_scale: f64,
    ) -> &mut Texture {
        self.active_drop_target.get_or_insert_with(|| {
            Texture::from_buffer(renderer, &ACTIVE_DROP_TARGET_RGBA, 1, 1, output_scale)
        })
    }

    /// Get the texture for the unfocused overview drop target area.
    pub fn drop_target(&mut self, renderer: &mut Gles2Renderer, output_scale: f64) -> &mut Texture {
        self.drop_target.get_or_insert_with(|| {
            Texture::from_buffer(renderer, &DROP_TARGET_RGBA, 1, 1, output_scale)
        })
    }

    pub fn touch_debug(&mut self, renderer: &mut Gles2Renderer, output_scale: f64) -> &mut Texture {
        self.touch_debug.get_or_insert_with(|| {
            Texture::from_buffer(
                renderer,
                &[255; TOUCH_DEBUG_SIZE * TOUCH_DEBUG_SIZE * 4],
                TOUCH_DEBUG_SIZE as i32,
                TOUCH_DEBUG_SIZE as i32,
                output_scale,
            )
        })
    }

    /// Decoration title bar height.
    pub fn title_height(output: &Output) -> i32 {
        (OVERVIEW_TITLE_HEIGHT as f64 / output.scale()).round() as i32
    }

    /// Decoration border width.
    pub fn border_width(output: &Output) -> i32 {
        (OVERVIEW_BORDER_WIDTH as f64 / output.scale()).round() as i32
    }

    /// Create overview window decoration.
    fn create_decoration(renderer: &mut Gles2Renderer, output: &Output) -> Texture {
        let size = Self::decoration_size(output);
        let title_height = Self::title_height(output) as usize;
        let border_width = Self::border_width(output) as usize;

        let width = size.w as usize;
        let height = size.h as usize;

        let mut buffer = vec![0; width * height * 4];

        // Helper to fill rectangles of the buffer.
        let mut fill = |x_start, x_end, y_start, y_end, rgba: [u8; 4]| {
            for x in x_start..x_end {
                for y in y_start..y_end {
                    let start = y * width * 4 + x * 4;
                    buffer[start..start + 4].copy_from_slice(&rgba);
                }
            }
        };

        // Background.
        let right_border = width - border_width;
        let bottom_border = height - border_width;
        fill(border_width, right_border, title_height, bottom_border, BACKGROUND_RGBA);

        // Titlebar.
        fill(border_width, width, border_width, title_height - border_width, TITLE_RGBA);

        // Titlebar top border.
        fill(border_width, right_border, 0, border_width, BORDER_RGBA);

        // Titlebar bottom border.
        let title_border = title_height - border_width;
        fill(border_width, right_border, title_border, title_height, BORDER_RGBA);

        // Left border.
        fill(0, border_width, 0, height, BORDER_RGBA);

        // Right border.
        fill(right_border, width, 0, height, BORDER_RGBA);

        // Bottom border.
        fill(border_width, right_border, bottom_border, height, BORDER_RGBA);

        Texture::from_buffer(renderer, &buffer, size.w, size.h, output.scale())
    }

    /// Total window decoration size.
    fn decoration_size(output: &Output) -> Size<i32, Logical> {
        let title_height = Self::title_height(output);
        let border_width = Self::border_width(output);

        let window_size = output.available().size.scale(FG_OVERVIEW_PERCENTAGE);
        let width = window_size.w + border_width * 2;
        let height = window_size.h + title_height + border_width;

        Size::from((width, height))
    }
}

/// Surface buffer cache.
pub struct SurfaceBuffer {
    pub texture: Option<Texture>,
    pub size: Size<i32, Logical>,
    pub buffer: Option<Buffer>,
    pub transform: Transform,
    pub damage: Damage,
    pub scale: i32,
}

impl Default for SurfaceBuffer {
    fn default() -> Self {
        Self {
            scale: 1,
            transform: Default::default(),
            texture: Default::default(),
            buffer: Default::default(),
            damage: Default::default(),
            size: Default::default(),
        }
    }
}

impl SurfaceBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle buffer creation/removal.
    pub fn update_buffer(&mut self, attributes: &SurfaceAttributes, assignment: BufferAssignment) {
        match assignment {
            BufferAssignment::NewBuffer { buffer, .. } => {
                self.size = renderer::buffer_dimensions(&buffer)
                    .unwrap_or_default()
                    .to_logical(self.scale, self.transform);
                self.transform = attributes.buffer_transform.into();
                self.scale = attributes.buffer_scale;
                self.buffer = Some(Buffer(buffer));
                self.texture = None;
            },
            BufferAssignment::Removed => *self = Self::default(),
        }
    }

    /// Add new surface damage.
    pub fn add_damage(&mut self, output_scale: f64, damage: impl Iterator<Item = SurfaceDamage>) {
        for damage in damage {
            let (buffer, physical) = match damage {
                SurfaceDamage::Buffer(buffer) => {
                    let buffer_size = self.size.to_buffer(self.scale, self.transform);
                    let logical = buffer.to_logical(self.scale, self.transform, &buffer_size);
                    let physical = logical.to_f64().to_physical(output_scale);
                    (buffer, physical)
                },
                SurfaceDamage::Surface(logical) => {
                    let buffer = logical.to_buffer(self.scale, self.transform, &self.size);
                    let physical = logical.to_f64().to_physical(output_scale);
                    (buffer, physical)
                },
            };

            self.damage.physical[0] = self.damage.physical[0].merge(physical);
            self.damage.buffer.push(buffer);
        }
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

/// Surface damage history.
#[derive(Default)]
pub struct Damage {
    /// Damage history in physical coordinates.
    physical: [Rectangle<f64, Physical>; MAX_DAMAGE_AGE],
    /// Buffer damage since last import.
    buffer: Vec<Rectangle<i32, BufferSpace>>,
}

impl Damage {
    /// Clear the surface's damage slot of unimported changes.
    ///
    /// This function should be called exactly once after importing the damage
    /// into the texture cache.
    pub fn clear(&mut self) {
        self.physical.rotate_right(1);
        self.physical[0] = Rectangle::default();
        self.buffer.clear();
    }

    /// Retrieve buffer damage since last import.
    pub fn buffer(&self) -> &[Rectangle<i32, BufferSpace>] {
        self.buffer.as_slice()
    }

    /// Retrieve physical damage history.
    pub fn physical(&self) -> &[Rectangle<f64, Physical>] {
        &self.physical
    }

    /// Check if there has been damage since the last draw.
    pub fn damaged(&self) -> bool {
        !self.buffer.is_empty()
    }
}
