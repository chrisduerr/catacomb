//! Application overview.

use std::cell::RefCell;
use std::cmp::{self, Ordering};
use std::rc::Rc;
use std::time::Instant;

use smithay::backend::renderer::gles2::{ffi, Gles2Frame, Gles2Renderer};
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::drawing::Graphics;
use crate::geometry::Vector;
use crate::output::{Orientation, Output};
use crate::window::Window;

/// Percentage of output width reserved for the main window in the application overview.
pub const FG_OVERVIEW_PERCENTAGE: f64 = 0.75;

/// Percentage of remaining space reserved for background windows in the application overview.
const BG_OVERVIEW_PERCENTAGE: f64 = 0.5;

/// Percentage of the screen for the drop highlight areas.
const DRAG_AND_DROP_PERCENTAGE: f64 = 0.3;

/// Animation speed for the return from close, lower means faster.
const CLOSE_CANCEL_ANIMATION_SPEED: f64 = 0.3;

/// Animation speed for the return from overdrag, lower means faster.
const OVERDRAG_ANIMATION_SPEED: f64 = 25.;

/// Maximum amount of overdrag before inputs are ignored.
const OVERDRAG_LIMIT: f64 = 3.;

/// Overview view state.
#[derive(Default, Copy, Clone, PartialEq, Debug)]
pub struct Overview {
    pub x_offset: f64,
    pub y_offset: f64,
    pub last_drag_point: Point<f64, Logical>,
    pub last_overdrag_step: Option<Instant>,
    pub drag_direction: Option<Direction>,
    pub hold_start: Option<Instant>,
}

impl Overview {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index of the focused window.
    pub fn focused_index(&self, window_count: usize) -> usize {
        (self.x_offset.min(0.).abs().round() as usize).min(window_count - 1)
    }

    /// Focused window bounds.
    pub fn focused_bounds(&self, output: &Output, window_count: usize) -> Rectangle<i32, Logical> {
        let available = output.available();
        let window_size = available.size.scale(FG_OVERVIEW_PERCENTAGE);
        let x = overview_x_position(
            FG_OVERVIEW_PERCENTAGE,
            BG_OVERVIEW_PERCENTAGE,
            available.size.w,
            window_size.w,
            self.focused_index(window_count) as f64 + self.x_offset,
        ) + available.loc.x;
        let y = (available.size.h - window_size.h) / 2 + available.loc.y;
        Rectangle::from_loc_and_size((x, y), window_size)
    }

    /// Clamp the X/Y offsets.
    ///
    /// This takes overdrag into account and will animate the bounce-back.
    fn clamp_offset(&mut self, window_count: i32) {
        // Limit maximum overdrag.
        let min_offset = -window_count as f64 + 1.;
        self.x_offset = self.x_offset.clamp(min_offset - OVERDRAG_LIMIT, OVERDRAG_LIMIT);

        let last_overdrag_step = match &mut self.last_overdrag_step {
            Some(last_overdrag_step) => last_overdrag_step,
            None => return,
        };

        // Handle bounce-back from overdrag/cancelled application close.

        // Compute framerate-independent delta.
        let delta = last_overdrag_step.elapsed().as_millis() as f64;
        let overdrag_delta = delta / OVERDRAG_ANIMATION_SPEED;
        let close_delta = delta / CLOSE_CANCEL_ANIMATION_SPEED;

        // Overdrag bounce-back.
        if self.x_offset > 0. {
            self.x_offset -= overdrag_delta.min(self.x_offset);
        } else if self.x_offset < min_offset {
            self.x_offset = (self.x_offset + overdrag_delta).min(min_offset);
        }

        // Close window bounce-back.
        self.y_offset -= close_delta.min(self.y_offset.abs()).copysign(self.y_offset);

        *last_overdrag_step = Instant::now();
    }

    /// Render the overview.
    pub fn draw(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        windows: &[Rc<RefCell<Window>>],
        graphics: &mut Graphics,
    ) {
        let window_count = windows.len() as i32;
        self.clamp_offset(window_count);

        // Create an iterator over all windows in the overview.
        //
        // We start by going over all negative index windows from lowest to highest index and then
        // proceed from highest to lowest index with the positive windows. This ensures that outer
        // windows are rendered below the ones toward the center.
        let min_inc = self.x_offset.round() as i32;
        let max_exc = window_count + self.x_offset.round() as i32;
        let neg_iter = (min_inc..0).zip(0..window_count);
        let pos_iter = (min_inc.max(0)..max_exc).zip(-min_inc.min(0)..window_count).rev();

        // Maximum window size. Bigger windows will be truncated.
        let available = output.available();
        let max_size = available.size.scale(FG_OVERVIEW_PERCENTAGE);

        // Window decoration.
        let decoration = graphics.decoration(renderer, output);
        let border_width = Graphics::border_width(output);
        let title_height = Graphics::title_height(output);

        // Render each window at the desired location in the overview.
        for (position, i) in neg_iter.chain(pos_iter) {
            let mut window = windows[i as usize].borrow_mut();

            // Window scale.
            let window_geometry = window.geometry();
            let scale = (max_size.w as f64 / window_geometry.size.w as f64).min(1.);

            // Window boundaries.
            let mut bounds = Rectangle::from_loc_and_size(available.loc, max_size);
            bounds.loc.x += overview_x_position(
                FG_OVERVIEW_PERCENTAGE,
                BG_OVERVIEW_PERCENTAGE,
                available.size.w,
                max_size.w,
                position as f64 - self.x_offset.fract().round() + self.x_offset.fract(),
            ) - border_width;
            bounds.loc.y += (available.size.h - max_size.h + title_height + border_width) / 2;

            // Offset windows in the process of being closed.
            if position == min_inc.max(0) {
                bounds.loc.y += self.y_offset.round() as i32;
            }

            // Draw decoration.
            let decoration_bounds = Rectangle::from_loc_and_size(
                (bounds.loc.x - border_width, bounds.loc.y - title_height),
                decoration.size(),
            );
            decoration.draw_at(frame, output, decoration_bounds, 1.);

            window.draw(renderer, frame, output, scale, bounds);
        }
    }
}

/// Drag and drop windows into tiling position.
#[derive(Default, Copy, Clone, PartialEq, Debug)]
pub struct DragAndDrop {
    pub window_position: Point<f64, Logical>,
    pub touch_position: Point<f64, Logical>,
    pub overview_x_offset: f64,
    pub window_index: usize,
}

impl DragAndDrop {
    pub fn new(
        touch_position: Point<f64, Logical>,
        overview_x_offset: f64,
        window_index: usize,
    ) -> Self {
        Self {
            overview_x_offset,
            touch_position,
            window_index,
            window_position: Default::default(),
        }
    }

    /// Draw the tiling location picker.
    pub fn draw(
        &self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        windows: &[Rc<RefCell<Window>>],
        graphics: &mut Graphics,
    ) {
        let available = output.available();
        let border_width = Graphics::border_width(output);
        let title_height = Graphics::title_height(output);

        // Calculate window bounds.
        let max_size = available.size.scale(FG_OVERVIEW_PERCENTAGE);
        let mut bounds = Rectangle::from_loc_and_size(available.loc, max_size);
        bounds.loc.x += overview_x_position(
            FG_OVERVIEW_PERCENTAGE,
            BG_OVERVIEW_PERCENTAGE,
            available.size.w,
            max_size.w,
            self.overview_x_offset.fract() - self.overview_x_offset.fract().round(),
        ) - border_width;
        bounds.loc.y += (available.size.h - max_size.h + title_height + border_width) / 2;

        // Offset by dragged distance.
        bounds.loc += self.window_position.to_i32_round();

        // Render decoration for the window.
        let decoration = graphics.decoration(renderer, output);
        let decoration_bounds = Rectangle::from_loc_and_size(
            (bounds.loc.x - border_width, bounds.loc.y - title_height),
            decoration.size(),
        );
        decoration.draw_at(frame, output, decoration_bounds, 1.);

        // Render the window being drag-and-dropped.
        let mut window = windows[self.window_index].borrow_mut();
        window.draw(renderer, frame, output, FG_OVERVIEW_PERCENTAGE, bounds);

        // Set custom OpenGL blending function.
        let _ = renderer.with_context(|_, gl| unsafe {
            gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);
        });

        // Get bounds of the drop areas.
        let (primary_bounds, secondary_bounds) = self.drop_bounds(output);

        // Render the drop areas.
        let scale = cmp::max(available.size.w, available.size.h) as f64;
        for bounds in [primary_bounds, secondary_bounds] {
            if bounds.to_f64().contains(self.touch_position) {
                graphics.active_drop_target(renderer).draw_at(frame, output, bounds, scale);
            } else {
                graphics.drop_target(renderer).draw_at(frame, output, bounds, scale);
            }
        }

        // Reset OpenGL blending function.
        let _ = renderer.with_context(|_, gl| unsafe {
            gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
        });
    }

    /// Bounds for the drop preview areas of the D&D action.
    pub fn drop_bounds(
        &self,
        output: &Output,
    ) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
        let available = output.available();
        match output.orientation {
            Orientation::Landscape => {
                let dnd_width = (available.size.w as f64 * DRAG_AND_DROP_PERCENTAGE).round() as i32;
                let size = Size::from((dnd_width, available.size.h));
                let primary = Rectangle::from_loc_and_size(available.loc, size);

                let mut secondary = primary;
                secondary.loc.x += available.size.w - dnd_width;

                (primary, secondary)
            },
            Orientation::Portrait => {
                let dnd_height =
                    (available.size.h as f64 * DRAG_AND_DROP_PERCENTAGE).round() as i32;
                let size = Size::from((available.size.w, dnd_height));
                let primary = Rectangle::from_loc_and_size(available.loc, size);

                let mut secondary = primary;
                secondary.loc.y += available.size.h - dnd_height;

                (primary, secondary)
            },
        }
    }
}

/// Directional plane.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Direction {
    Horizontal,
    Vertical,
}

/// Calculate the X coordinate of a window in the application overview based on its position.
fn overview_x_position(
    fg_percentage: f64,
    bg_percentage: f64,
    output_width: i32,
    window_width: i32,
    position: f64,
) -> i32 {
    let bg_space_size = output_width as f64 * (1. - fg_percentage) * 0.5;
    let next_space_size = bg_space_size * (1. - bg_percentage).powf(position.abs());
    let next_space_size = next_space_size.round() as i32;

    match position.partial_cmp(&0.) {
        Some(Ordering::Less) => next_space_size,
        Some(Ordering::Greater) => output_width - window_width - next_space_size,
        _ => bg_space_size.round() as i32,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn overview_position() {
        assert_eq!(overview_x_position(0.5, 0.5, 100, 50, -2.), 6);
        assert_eq!(overview_x_position(0.5, 0.5, 100, 50, -1.), 13);
        assert_eq!(overview_x_position(0.5, 0.5, 100, 50, 0.), 25);
        assert_eq!(overview_x_position(0.5, 0.5, 100, 50, 1.), 37);
        assert_eq!(overview_x_position(0.5, 0.5, 100, 50, 2.), 44);

        assert_eq!(overview_x_position(0.5, 0.75, 100, 50, -2.), 2);
        assert_eq!(overview_x_position(0.5, 0.75, 100, 50, -1.), 6);
        assert_eq!(overview_x_position(0.5, 0.75, 100, 50, 0.), 25);
        assert_eq!(overview_x_position(0.5, 0.75, 100, 50, 1.), 44);
        assert_eq!(overview_x_position(0.5, 0.75, 100, 50, 2.), 48);

        assert_eq!(overview_x_position(0.75, 0.75, 100, 50, -2.), 1);
        assert_eq!(overview_x_position(0.75, 0.75, 100, 50, -1.), 3);
        assert_eq!(overview_x_position(0.75, 0.75, 100, 50, 0.), 13);
        assert_eq!(overview_x_position(0.75, 0.75, 100, 50, 1.), 47);
        assert_eq!(overview_x_position(0.75, 0.75, 100, 50, 2.), 49);
    }
}
