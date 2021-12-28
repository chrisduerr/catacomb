//! Drawing utilities.

use std::rc::Rc;

use smithay::backend::renderer::gles2::{ffi, Gles2Error, Gles2Frame, Gles2Renderer, Gles2Texture};
use smithay::backend::renderer::{Frame, Transform};
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::geometry::CatacombVector;
use crate::output::Output;

/// Cached texture.
///
/// Includes all information necessary to render a surface's texture even after
/// the surface itself has already died.
#[derive(Clone, Debug)]
pub struct Texture {
    dimensions: Size<i32, Logical>,
    location: Point<i32, Logical>,
    texture: Rc<Gles2Texture>,
    scale: i32,
}

impl Texture {
    pub fn new(
        texture: Rc<Gles2Texture>,
        dimensions: impl Into<Size<i32, Logical>>,
        location: impl Into<Point<i32, Logical>>,
        scale: i32,
    ) -> Self {
        Self { location: location.into(), dimensions: dimensions.into(), texture, scale }
    }

    /// Create a texture from an RGBA buffer.
    pub fn from_buffer(
        renderer: &mut Gles2Renderer,
        buffer: &[u8],
        width: i32,
        height: i32,
    ) -> Result<Self, Gles2Error> {
        assert!(buffer.len() as i32 >= width * height * 4);

        let texture = renderer.with_context(|renderer, gl| unsafe {
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
        })?;

        Ok(Texture::new(Rc::new(texture), (width, height), (0, 0), 1))
    }

    /// Render the texture at the specified location.
    ///
    /// Using the `window_bounds` and `window_scale` parameters, it is possible to scale the
    /// texture and truncate it to be within the specified window bounds. The scaling will always
    /// take part **before** the truncation.
    pub fn draw_at(
        &self,
        frame: &mut Gles2Frame,
        output: &Output,
        window_bounds: Rectangle<i32, Logical>,
        window_scale: f64,
    ) {
        // Skip textures completely outside of the window bounds.
        let scaled_window_bounds = window_bounds.size.scale(1. / window_scale).max((1, 1));
        if self.location.x >= scaled_window_bounds.w || self.location.y >= scaled_window_bounds.h {
            return;
        }

        // Truncate source size based on window bounds.
        let src_size = (self.dimensions + self.location).min(scaled_window_bounds);
        let src = Rectangle::from_loc_and_size((0, 0), src_size);

        // Scale output size based on window scale.
        let location = window_bounds.loc + self.location.scale(window_scale);
        let dest_size = src_size.scale(window_scale).min(window_bounds.size);
        let dest = Rectangle::from_loc_and_size(location, dest_size);

        let _ = frame.render_texture_from_to(
            &self.texture,
            src.to_buffer(self.scale),
            dest.to_f64().to_physical(output.scale),
            Transform::Normal,
            1.,
        );
    }
}
