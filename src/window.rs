//! Window management.

use std::borrow::Cow;
use std::cell::{RefCell, RefMut};
use std::cmp::{self, Ordering};
use std::mem;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use smithay::backend::renderer::gles2::{Gles2Error, Gles2Frame, Gles2Renderer};
use smithay::backend::renderer::{self, BufferType, ImportAll};
use smithay::reexports::wayland_protocols::unstable::xdg_decoration;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{
    self, Damage, SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction,
};
use smithay::wayland::shell::xdg::{SurfaceCachedState, ToplevelSurface};
use wayland_protocols::xdg_shell::server::xdg_toplevel::State;
use xdg_decoration::v1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

use crate::drawing::Texture;
use crate::geometry::CatacombVector;
use crate::input::HOLD_DURATION;
use crate::output::Output;
use crate::shell::SurfaceBuffer;

/// Maximum time before a transaction is cancelled.
const MAX_TRANSACTION_DURATION: Duration = Duration::from_millis(200);

/// Horizontal sensitivity of the application overview.
const OVERVIEW_HORIZONTAL_SENSITIVITY: f64 = 250.;

/// Percentage of output width reserved for the main window in the application overview.
const FG_OVERVIEW_PERCENTAGE: f64 = 0.75;

/// Percentage of remaining space reserved for background windows in the application overview.
const BG_OVERVIEW_PERCENTAGE: f64 = 0.5;

/// Percentage of the output height a window can be moved before closing it in the overview.
const OVERVIEW_CLOSE_DISTANCE: f64 = 0.5;

/// Color of the hovered overview tiling location highlight.
const ACTIVE_DROP_TARGET_RGBA: [u8; 4] = [0, 0, 0, 64];

/// Color of the overview tiling location highlight.
const DROP_TARGET_RGBA: [u8; 4] = [0, 0, 0, 128];

/// Animation speed for the return from close, lower means faster.
const CLOSE_CANCEL_ANIMATION_SPEED: f64 = 0.3;

/// Animation speed for the return from overdrag, lower means faster.
const OVERDRAG_ANIMATION_SPEED: f64 = 25.;

/// Maximum amount of overdrag before inputs are ignored.
const OVERDRAG_LIMIT: f64 = 3.;

/// Container tracking all known clients.
#[derive(Debug)]
pub struct Windows {
    windows: Vec<Rc<RefCell<Window>>>,
    primary: Weak<RefCell<Window>>,
    secondary: Weak<RefCell<Window>>,
    transaction: Option<Transaction>,
    start_time: Instant,
    graphics: Graphics,
    view: View,
}

impl Windows {
    pub fn new(renderer: &mut Gles2Renderer) -> Self {
        Self {
            graphics: Graphics::new(renderer).expect("texture creation error"),
            start_time: Instant::now(),
            transaction: Default::default(),
            secondary: Default::default(),
            windows: Default::default(),
            primary: Default::default(),
            view: Default::default(),
        }
    }

    /// Add a new window.
    pub fn add(&mut self, surface: ToplevelSurface, output: &Output) {
        self.windows.push(Rc::new(RefCell::new(Window::new(surface))));
        self.set_primary(output, self.windows.len() - 1);
        self.set_secondary(output, None);
    }

    /// Find the window responsible for a specific surface.
    pub fn find(&mut self, wl_surface: &WlSurface) -> Option<RefMut<Window>> {
        // Get root surface.
        let mut wl_surface = Cow::Borrowed(wl_surface);
        while let Some(surface) = compositor::get_parent(&wl_surface) {
            wl_surface = Cow::Owned(surface);
        }

        self.windows.iter_mut().map(|window| window.borrow_mut()).find(|window| {
            window.surface.get_surface().map_or(false, |surface| surface.eq(&wl_surface))
        })
    }

    /// Execute a function for all visible windows.
    pub fn with_visible<F: FnMut(&mut Window)>(&mut self, mut fun: F) {
        for window in self.primary.upgrade().iter_mut().chain(&mut self.secondary.upgrade()) {
            fun(&mut window.borrow_mut());
        }
    }

    /// Draw the current window state.
    pub fn draw(&mut self, renderer: &mut Gles2Renderer, frame: &mut Gles2Frame, output: &Output) {
        self.update_transaction();

        match self.view {
            View::Workspace => {
                self.with_visible(|window| window.draw(renderer, frame, output, 1., None));
            },
            View::Overview(mut overview @ Overview { floating_position: Some(_), .. }) => {
                self.with_visible(|window| window.draw(renderer, frame, output, 1., None));
                overview.draw_drag_and_drop(renderer, frame, output, &self.windows, &self.graphics);
            },
            View::Overview(mut overview) => overview.draw(renderer, frame, output, &self.windows),
        }
    }

    /// Request new frames for all visible windows.
    pub fn request_frames(&mut self) {
        if self.view == View::Workspace {
            let runtime = self.runtime();
            self.with_visible(|window| window.request_frame(runtime));
        }
    }

    /// Update window manager state.
    pub fn refresh(&mut self, output: &Output) {
        if self.windows.iter().any(|window| !window.borrow().surface.alive()) {
            self.refresh_visible(output);
        }

        // Open as secondary on long touch in overview.
        if let View::Overview(overview) = &mut self.view {
            if overview.hold_start.map_or(false, |start| start.elapsed() >= HOLD_DURATION) {
                overview.floating_position = Some(Point::default());
                overview.hold_start = None;
            }
        }
    }

    /// Reap dead visible windows.
    ///
    /// This will reorder and resize visible windows when any of them has died.
    fn refresh_visible(&mut self, output: &Output) {
        let transaction = self.start_transaction();

        // Remove dead primary/secondary windows.
        if transaction.secondary.upgrade().map_or(true, |window| !window.borrow().surface.alive()) {
            transaction.secondary = Weak::new();
        }
        if transaction.primary.upgrade().map_or(true, |window| !window.borrow().surface.alive()) {
            transaction.primary = mem::take(&mut transaction.secondary);
        }

        transaction.update_dimensions(output);
    }

    /// Create a new transaction, or access the active one.
    pub fn start_transaction(&mut self) -> &mut Transaction {
        self.transaction.get_or_insert(Transaction::new(self))
    }

    /// Attempt to execute pending transactions.
    fn update_transaction(&mut self) {
        let transaction = match &mut self.transaction {
            Some(start) => start,
            None => return,
        };

        // Check if the transaction requires updating.
        if Instant::now().duration_since(transaction.start) <= MAX_TRANSACTION_DURATION {
            // Check if all participants are ready.
            let finished = self.windows.iter().map(|window| window.borrow()).all(|window| {
                window
                    .transaction
                    .as_ref()
                    .map_or(true, |transaction| window.acked_size == transaction.rectangle.size)
            });

            // Abort if the transaction is still pending.
            if !finished {
                return;
            }
        }

        let secondary_index = self.primary.strong_count().max(1);
        let mut i = self.windows.len();
        while i > 0 {
            i -= 1;

            // Remove dead windows.
            if !self.windows[i].borrow().surface.alive() {
                self.windows.remove(i);
                continue;
            }

            // Apply transaction changes.
            self.windows[i].borrow_mut().apply_transaction();

            // Ensure primary/secondary are always first/second window.
            let weak = Rc::downgrade(&self.windows[i]);
            if i > 0 && transaction.primary.ptr_eq(&weak) {
                self.windows.swap(0, i);
                i += 1;
            } else if i > secondary_index && transaction.secondary.ptr_eq(&weak) {
                self.windows.swap(secondary_index, i);
                i += 1;
            }
        }

        // Apply window management changes.
        let transaction = self.transaction.take().unwrap();
        self.view = transaction.view.unwrap_or(self.view);
        self.secondary = transaction.secondary;
        self.primary = transaction.primary;
    }

    /// Toggle the active view.
    pub fn toggle_view(&mut self) {
        let new_view = match self.view {
            View::Workspace => Some(View::Overview(Overview::default())),
            View::Overview { .. } => Some(View::Workspace),
        };
        self.start_transaction().view = new_view;
    }

    /// Handle start of touch input.
    pub fn on_touch_start(&mut self, output: &Output, point: Point<f64, Logical>) {
        if let View::Overview(overview) = &mut self.view {
            // Click inside focused window stages it for opening as secondary.
            let window_bounds = overview.focused_bounds(output.size(), self.windows.len());
            if window_bounds.contains(point.to_i32_round()) {
                overview.hold_start = Some(Instant::now());
            }

            overview.last_drag_point = point;
        }
    }

    /// Hand quick touch input.
    pub fn on_tap(&mut self, output: &Output, point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::Workspace => return,
        };

        overview.hold_start = None;

        // Click inside focused window opens it as primary.
        let window_bounds = overview.focused_bounds(output.size(), self.windows.len());
        if window_bounds.contains(point.to_i32_round()) {
            let index = overview.focused_index(self.windows.len());

            // Clear secondary unless *only* primary is empty.
            self.set_primary(output, index);
            if self.primary.strong_count() > 0 {
                self.set_secondary(output, None);
            }

            self.toggle_view();
        }
    }

    // TODO: Cleanup big time.
    //
    /// Handle a touch drag.
    pub fn on_drag(&mut self, output: &Output, point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            _ => return,
        };

        let delta = point - mem::replace(&mut overview.last_drag_point, point);

        if let Some(floating_position) = &mut overview.floating_position {
            *floating_position += delta;
            return;
        }

        let drag_direction =
            overview.drag_direction.get_or_insert(if delta.x.abs() >= delta.y.abs() {
                Direction::Horizontal
            } else {
                Direction::Vertical
            });

        match drag_direction {
            Direction::Horizontal => {
                overview.x_offset += delta.x / OVERVIEW_HORIZONTAL_SENSITIVITY;
                overview.last_overdrag_step = None;
                overview.hold_start = None;
                overview.y_offset = 0.;
            },
            Direction::Vertical if !overview.close_release_pending => {
                overview.last_overdrag_step = None;
                overview.hold_start = None;
                overview.y_offset += delta.y;

                // Close window once offset surpassed the threshold.
                let close_distance = output.size().h as f64 * OVERVIEW_CLOSE_DISTANCE;
                if overview.y_offset.abs() >= close_distance && !self.windows.is_empty() {
                    let index = overview.focused_index(self.windows.len());
                    self.windows[index].borrow_mut().surface.send_close();
                    self.windows.remove(index);

                    overview.last_overdrag_step = Some(Instant::now());
                    overview.close_release_pending = true;
                    overview.y_offset = 0.;

                    self.refresh_visible(output);
                }
            },
            _ => (),
        }
    }

    /// Handle touch release.
    pub fn on_drag_release(&mut self, output: &Output) {
        // TODO: Cleanup
        if let View::Overview(overview) = &self.view {
            if overview.floating_position.is_some() {
                let output_size = output.size();
                if overview.last_drag_point.y < output_size.h as f64 / 3. {
                    let index = overview.focused_index(self.windows.len());
                    self.set_primary(output, index);
                    self.toggle_view();
                    return;
                } else if overview.last_drag_point.y >= output_size.h as f64 / 1.5 {
                    let index = overview.focused_index(self.windows.len());
                    self.set_secondary(output, index);
                    self.toggle_view();
                    return;
                }
            }
        }

        if let View::Overview(overview) = &mut self.view {
            overview.last_overdrag_step = Some(Instant::now());
            overview.close_release_pending = false;
            overview.floating_position = None;
        }
    }

    /// Application runtime.
    pub fn runtime(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// Change the primary window.
    fn set_primary(&mut self, output: &Output, index: impl Into<Option<usize>>) {
        let transaction = self.transaction.get_or_insert(Transaction::new(self));
        let window = index.into().map(|index| &self.windows[index]);

        // TODO: Formatting, best way to do it?
        let weak_window =
            window.map(Rc::downgrade).unwrap_or_else(|| mem::take(&mut transaction.secondary));
        if weak_window.ptr_eq(&transaction.primary) {
            return;
        }

        // Update output's visible windows.
        if let Some(primary) = transaction.primary.upgrade() {
            primary.borrow_mut().leave(transaction, output);
        }
        if let Some(window) = &window {
            window.borrow_mut().enter(output);
        }

        // Clear secondary if it's the new primary.
        if weak_window.ptr_eq(&transaction.secondary) {
            transaction.secondary = Weak::new();
        }

        // Set primary and move old one to secondary if it is empty.
        let old_primary = mem::replace(&mut transaction.primary, weak_window);
        if transaction.secondary.strong_count() == 0 {
            transaction.secondary = old_primary;
        }

        transaction.update_dimensions(output);
    }

    /// Change the secondary window.
    fn set_secondary(&mut self, output: &Output, index: impl Into<Option<usize>>) {
        let transaction = self.transaction.get_or_insert(Transaction::new(self));
        let window = index.into().map(|i| &self.windows[i]);

        // Update output's visible windows.
        if let Some(secondary) = transaction.secondary.upgrade() {
            secondary.borrow_mut().leave(transaction, output);
        }
        if let Some(window) = &window {
            window.borrow_mut().enter(output);
        }

        // Clear primary if it's the new secondary.
        let weak_window = window.map(Rc::downgrade);
        if weak_window.as_ref().map_or(false, |window| window.ptr_eq(&transaction.primary)) {
            transaction.primary = Weak::new();
        }

        // Set primary and recompute window dimensions.
        transaction.secondary = weak_window.unwrap_or_default();
        transaction.update_dimensions(output);
    }
}

/// Grahpics texture cache.
#[derive(Debug)]
struct Graphics {
    active_drop_target: Texture,
    drop_target: Texture,
}

impl Graphics {
    fn new(renderer: &mut Gles2Renderer) -> Result<Self, Gles2Error> {
        Ok(Self {
            active_drop_target: Texture::from_buffer(renderer, &ACTIVE_DROP_TARGET_RGBA, 1, 1)?,
            drop_target: Texture::from_buffer(renderer, &DROP_TARGET_RGBA, 1, 1)?,
        })
    }
}

/// Compositor window arrangements.
#[derive(Copy, Clone, PartialEq, Debug)]
enum View {
    /// List of all open windows.
    Overview(Overview),
    /// Currently active windows.
    Workspace,
}

impl Default for View {
    fn default() -> Self {
        View::Workspace
    }
}

/// Overview view state.
#[derive(Default, Copy, Clone, PartialEq, Debug)]
struct Overview {
    x_offset: f64,
    y_offset: f64,
    floating_position: Option<Point<f64, Logical>>,
    last_drag_point: Point<f64, Logical>,
    last_overdrag_step: Option<Instant>,
    drag_direction: Option<Direction>,
    close_release_pending: bool,
    hold_start: Option<Instant>,
}

impl Overview {
    /// Index of the focused window.
    fn focused_index(&self, window_count: usize) -> usize {
        (self.x_offset.min(0.).abs().round() as usize).min(window_count - 1)
    }

    /// Focused window bounds.
    fn focused_bounds(
        &self,
        output_size: Size<i32, Logical>,
        window_count: usize,
    ) -> Rectangle<i32, Logical> {
        let window_size = output_size.scale(FG_OVERVIEW_PERCENTAGE);
        let x = overview_x_position(
            FG_OVERVIEW_PERCENTAGE,
            BG_OVERVIEW_PERCENTAGE,
            output_size.w,
            window_size.w,
            self.focused_index(window_count) as f64 + self.x_offset,
        );
        let y = (output_size.h - window_size.h) / 2;
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
    fn draw(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        windows: &[Rc<RefCell<Window>>],
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
        let output_size = output.size();
        let max_size = output_size.scale(FG_OVERVIEW_PERCENTAGE);

        // Render each window at the desired location in the overview.
        for (position, i) in neg_iter.chain(pos_iter) {
            let mut window = windows[i as usize].borrow_mut();

            // Window scale.
            let window_geometry = window.geometry();
            let scale = (max_size.w as f64 / window_geometry.size.w as f64).min(1.);

            // Window boundaries.
            let x_position = overview_x_position(
                FG_OVERVIEW_PERCENTAGE,
                BG_OVERVIEW_PERCENTAGE,
                output_size.w,
                max_size.w,
                position as f64 - self.x_offset.fract().round() + self.x_offset.fract(),
            );
            let y_position = (output_size.h - max_size.h) / 2;
            let mut bounds = Rectangle::from_loc_and_size((x_position, y_position), max_size);

            // Offset windows in the process of being closed.
            if position == min_inc.max(0) {
                bounds.loc.y += self.y_offset.round() as i32;
            }

            window.draw(renderer, frame, output, scale, Some(bounds));
        }
    }

    // TODO: Cleanup big time.
    //
    /// Draw the tiling location picker.
    fn draw_drag_and_drop(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        windows: &[Rc<RefCell<Window>>],
        graphics: &Graphics,
    ) {
        let output_size = output.size();

        let scale = 0.8;
        let size = output_size.scale(scale);
        let loc = Point::from((
            output_size.w / 2 - size.w / 2 + self.floating_position.unwrap().x as i32,
            output_size.h / 2 - size.h / 2 + self.floating_position.unwrap().y as i32,
        ));
        let bounds = Rectangle::from_loc_and_size(loc, size);
        let index = self.focused_index(windows.len());
        let mut window = windows[index].borrow_mut();
        window.draw(renderer, frame, output, scale, Some(bounds));

        // TODO
        let size = Size::from((output_size.w, output_size.h / 3));
        let bounds = Rectangle::from_loc_and_size((0, 0), size);
        let scale = cmp::max(output_size.w, output_size.h) as f64;
        if self.last_drag_point.y < size.h as f64 {
            graphics.active_drop_target.draw_at(frame, output, bounds, scale);
        } else {
            graphics.drop_target.draw_at(frame, output, bounds, scale);
        }

        // TODO
        let size = Size::from((output_size.w, output_size.h / 3));
        let bounds = Rectangle::from_loc_and_size((0, output_size.h - output_size.h / 3), size);
        let scale = cmp::max(output_size.w, output_size.h) as f64;
        if self.last_drag_point.y >= bounds.loc.y as f64 {
            graphics.active_drop_target.draw_at(frame, output, bounds, scale);
        } else {
            graphics.drop_target.draw_at(frame, output, bounds, scale);
        }
    }
}

/// Directional plane.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Direction {
    Horizontal,
    Vertical,
}

/// Atomic changes to [`Windows`].
#[derive(Clone, Debug)]
pub struct Transaction {
    primary: Weak<RefCell<Window>>,
    secondary: Weak<RefCell<Window>>,
    view: Option<View>,
    start: Instant,
}

impl Transaction {
    fn new(current_state: &Windows) -> Self {
        Self {
            primary: current_state.primary.clone(),
            secondary: current_state.secondary.clone(),
            start: Instant::now(),
            view: None,
        }
    }

    /// Update window dimensions.
    pub fn update_dimensions(&mut self, output: &Output) {
        if let Some(mut primary) = self.primary.upgrade().as_ref().map(|s| s.borrow_mut()) {
            let secondary_visible = self.secondary.strong_count() > 0;
            let rectangle = output.primary_rectangle(secondary_visible);
            primary.update_dimensions(self, rectangle);
        }

        if let Some(mut secondary) = self.secondary.upgrade().as_ref().map(|s| s.borrow_mut()) {
            let rectangle = output.secondary_rectangle();
            secondary.update_dimensions(self, rectangle);
        }
    }
}

/// Atomic changes to [`Window`].
#[derive(Debug)]
struct WindowTransaction {
    rectangle: Rectangle<i32, Logical>,
}

impl WindowTransaction {
    fn new(current_state: &Window) -> Self {
        Self { rectangle: current_state.rectangle }
    }
}

/// Cached window textures.
#[derive(Default, Debug)]
struct TextureCache {
    /// Geometry of all textures combined.
    geometry: Size<i32, Logical>,
    textures: Vec<Texture>,
}

impl TextureCache {
    /// Reset the texture cache.
    fn reset(&mut self, geometry: Size<i32, Logical>) {
        self.geometry = geometry;
        self.textures.clear();
    }

    /// Add a new texture.
    fn push(&mut self, texture: Texture) {
        self.textures.push(texture);
    }
}

/// Wayland client window state.
#[derive(Debug)]
pub struct Window {
    /// Initial size configure status.
    pub initial_configure_sent: bool,

    /// Buffers pending to be imported.
    pub buffers_pending: bool,

    /// Last configure size acked by the client.
    pub acked_size: Size<i32, Logical>,

    /// Desired window dimensions.
    rectangle: Rectangle<i32, Logical>,

    /// Attached surface.
    surface: ToplevelSurface,

    /// Texture cache, storing last window state.
    texture_cache: TextureCache,

    /// Window is currently visible on the output.
    visible: bool,

    /// Transaction for atomic upgrades.
    transaction: Option<WindowTransaction>,
}

impl Window {
    pub fn new(surface: ToplevelSurface) -> Self {
        Window {
            surface,
            initial_configure_sent: Default::default(),
            buffers_pending: Default::default(),
            texture_cache: Default::default(),
            transaction: Default::default(),
            acked_size: Default::default(),
            rectangle: Default::default(),
            visible: Default::default(),
        }
    }

    /// Check if window is visible on the output.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Send a frame request to the window.
    pub fn request_frame(&mut self, runtime: u32) {
        self.with_surfaces(|_, surface_data| {
            let mut attributes = surface_data.cached_state.current::<SurfaceAttributes>();
            for callback in attributes.frame_callbacks.drain(..) {
                callback.done(runtime);
            }
        });
    }

    /// Send a configure for the latest window properties.
    pub fn reconfigure(&mut self) {
        let result = self.surface.with_pending_state(|state| {
            state.size = Some(match &self.transaction {
                Some(transaction) => transaction.rectangle.size,
                None => self.rectangle.size,
            });

            // Mark window as tiled, using maximized fallback if tiling is unsupported.
            if self.surface.version() >= 2 {
                state.states.set(State::TiledBottom);
                state.states.set(State::TiledRight);
                state.states.set(State::TiledLeft);
                state.states.set(State::TiledTop);
            } else {
                state.states.set(State::Maximized);
            }

            // Always use server-side decorations.
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });

        if result.is_ok() {
            self.surface.send_configure();
        }
    }

    /// Change the window dimensions.
    fn update_dimensions(&mut self, transaction: &Transaction, rectangle: Rectangle<i32, Logical>) {
        // Prevent redundant configure events.
        let transaction = self.start_transaction(transaction);
        let old_ractangle = mem::replace(&mut transaction.rectangle, rectangle);
        if transaction.rectangle != old_ractangle && self.initial_configure_sent {
            self.reconfigure();
        }
    }

    /// Send output enter event to this window's surfaces.
    fn enter(&mut self, output: &Output) {
        self.with_surfaces(|surface, _| output.enter(surface));
        self.visible = true;
    }

    /// Send output leave event to this window's surfaces.
    fn leave(&mut self, transaction: &Transaction, output: &Output) {
        self.with_surfaces(|surface, _| output.leave(surface));
        self.visible = false;

        // Resize to fullscreen for app overview.
        let mut rectangle = self.start_transaction(transaction).rectangle;
        rectangle.size = output.size();
        self.update_dimensions(transaction, rectangle);
    }

    /// Render this window's buffers.
    ///
    /// If no location is specified, the textures cached location will be used.
    fn draw(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        scale: f64,
        bounds: Option<Rectangle<i32, Logical>>,
    ) {
        // Skip updating windows during transactions.
        if self.transaction.is_none() && self.buffers_pending {
            self.import_buffers(renderer);
        }

        let bounds = bounds.unwrap_or_else(|| {
            // Center window inside its space.
            let x_offset = ((self.rectangle.size.w - self.texture_cache.geometry.w) / 2).max(0);
            let y_offset = ((self.rectangle.size.h - self.texture_cache.geometry.h) / 2).max(0);
            let loc = self.rectangle.loc + Size::from((x_offset, y_offset));
            Rectangle::from_loc_and_size(loc, output.size())
        });

        for texture in &self.texture_cache.textures {
            texture.draw_at(frame, output, bounds, scale);
        }
    }

    /// Import the buffers of all surfaces into the renderer.
    fn import_buffers(&mut self, renderer: &mut Gles2Renderer) {
        // Ensure there is a drawable surface present.
        let wl_surface = match self.surface.get_surface() {
            Some(surface) => surface,
            None => return,
        };

        let geometry = self.geometry();
        self.texture_cache.reset(geometry.size);
        self.buffers_pending = false;

        compositor::with_surface_tree_upward(
            wl_surface,
            Point::from((0, 0)) - geometry.loc,
            |_, surface_data, location| {
                let data = match surface_data.data_map.get::<RefCell<SurfaceBuffer>>() {
                    Some(data) => data,
                    None => return TraversalAction::SkipChildren,
                };
                let mut data = data.borrow_mut();

                // Use the subsurface's location as the origin for its children.
                let mut location = *location;
                if surface_data.role == Some("subsurface") {
                    let subsurface = surface_data.cached_state.current::<SubsurfaceCachedState>();
                    location += subsurface.location;
                }

                // Skip surface if buffer was already imported.
                if let Some(texture) = &data.texture {
                    let texture = Texture::new(texture.clone(), data.size(), location, data.scale);
                    self.texture_cache.push(texture);
                    return TraversalAction::DoChildren(location);
                }

                // Import and cache the buffer.

                let buffer = match &data.buffer {
                    Some(buffer) => buffer,
                    None => return TraversalAction::SkipChildren,
                };

                let damage: Vec<_> = surface_data
                    .cached_state
                    .current::<SurfaceAttributes>()
                    .damage
                    .iter()
                    .map(|damage| match damage {
                        Damage::Buffer(rect) => *rect,
                        Damage::Surface(rect) => rect.to_buffer(data.scale),
                    })
                    .collect();

                match renderer.import_buffer(buffer, Some(surface_data), &damage) {
                    Some(Ok(texture)) => {
                        // Release SHM buffers after import.
                        if let Some(BufferType::Shm) = renderer::buffer_type(buffer) {
                            data.buffer = None;
                        }

                        // Update and cache the texture.
                        let texture = Rc::new(texture);
                        data.texture = Some(texture.clone());
                        let texture = Texture::new(texture, data.size(), location, data.scale);
                        self.texture_cache.push(texture);

                        TraversalAction::DoChildren(location)
                    },
                    _ => {
                        eprintln!("unable to import buffer");
                        data.buffer = None;

                        TraversalAction::SkipChildren
                    },
                }
            },
            |_, _, _| (),
            |_, _, _| true,
        );
    }

    /// Geometry of the window's visible bounds.
    fn geometry(&self) -> Rectangle<i32, Logical> {
        self.surface
            .get_surface()
            .and_then(|surface| {
                compositor::with_states(surface, |states| {
                    states.cached_state.current::<SurfaceCachedState>().geometry
                })
                .ok()
                .flatten()
            })
            .unwrap_or_default()
    }

    /// Execute a function for all surfaces of this window.
    fn with_surfaces<F: FnMut(&WlSurface, &SurfaceData)>(&mut self, mut fun: F) {
        let wl_surface = match self.surface.get_surface() {
            Some(surface) => surface,
            None => return,
        };

        compositor::with_surface_tree_upward(
            wl_surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, surface_data, _| fun(surface, surface_data),
            |_, _, _| true,
        );
    }

    /// Create a new transaction, or access the active one.
    ///
    /// Takes in a transaction as parameter to ensure the window will not get stuck in the frozen
    /// state indefinitely.
    fn start_transaction(&mut self, _transaction: &Transaction) -> &mut WindowTransaction {
        self.transaction.get_or_insert(WindowTransaction::new(self))
    }

    /// Apply all staged changes if there is a transaction.
    fn apply_transaction(&mut self) {
        let transaction = match self.transaction.take() {
            Some(transaction) => transaction,
            None => return,
        };

        self.rectangle = transaction.rectangle;
    }
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
