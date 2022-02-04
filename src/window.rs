//! Window management.

use std::borrow::Cow;
use std::cell::{RefCell, RefMut};
use std::mem;
use std::rc::{Rc, Weak};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use smithay::backend::renderer::gles2::{Gles2Frame, Gles2Renderer};
use smithay::backend::renderer::{self, BufferType, ImportAll};
use smithay::reexports::wayland_protocols::unstable::xdg_decoration;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{
    self, Damage, SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction,
};
use smithay::wayland::shell::wlr_layer::{
    Anchor, ExclusiveZone, Layer, LayerSurface, LayerSurfaceAttributes, LayerSurfaceCachedState,
};
use smithay::wayland::shell::xdg::{
    SurfaceCachedState, ToplevelSurface, XdgToplevelSurfaceRoleAttributes,
};
use wayland_protocols::xdg_shell::server::xdg_toplevel::State;
use xdg_decoration::v1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

use crate::drawing::{Graphics, SurfaceBuffer, Texture};
use crate::input::HOLD_DURATION;
use crate::layer::Layers;
use crate::output::{ExclusiveSpace, Output};
use crate::overview::{Direction, DragAndDrop, Overview};

/// Horizontal sensitivity of the application overview.
const OVERVIEW_HORIZONTAL_SENSITIVITY: f64 = 250.;

/// Percentage of the output height a window can be moved before closing it in the overview.
const OVERVIEW_CLOSE_DISTANCE: f64 = 0.5;

/// Maximum time before a transaction is cancelled.
const MAX_TRANSACTION_DURATION: Duration = Duration::from_millis(200);

/// Container tracking all known clients.
#[derive(Debug)]
pub struct Windows {
    primary: Weak<RefCell<Window>>,
    secondary: Weak<RefCell<Window>>,
    view: View,

    windows: Vec<Rc<RefCell<Window>>>,
    layers: Layers,

    transaction: Option<Transaction>,
    start_time: Instant,
    graphics: Graphics,
}

impl Windows {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            transaction: Default::default(),
            secondary: Default::default(),
            graphics: Default::default(),
            windows: Default::default(),
            primary: Default::default(),
            layers: Default::default(),
            view: Default::default(),
        }
    }

    /// Add a new window.
    pub fn add(&mut self, surface: ToplevelSurface, output: &Output) {
        self.windows.push(Rc::new(RefCell::new(Window::new(surface))));
        self.set_primary(output, self.windows.len() - 1);
        self.set_secondary(output, None);
    }

    /// Add a new layer shell window.
    pub fn add_layer(&mut self, layer: Layer, surface: impl Into<CatacombLayerSurface>) {
        self.layers.add(layer, surface.into());
    }

    /// Find the XDG shell window responsible for a specific surface.
    pub fn find_xdg(&mut self, wl_surface: &WlSurface) -> Option<RefMut<Window>> {
        // Get root surface.
        let mut wl_surface = Cow::Borrowed(wl_surface);
        while let Some(surface) = compositor::get_parent(&wl_surface) {
            wl_surface = Cow::Owned(surface);
        }

        self.windows.iter_mut().map(|window| window.borrow_mut()).find(|window| {
            window.surface.get_surface().map_or(false, |surface| surface.eq(&wl_surface))
        })
    }

    /// Handle a surface commit for any window.
    pub fn surface_commit(&mut self, surface: &WlSurface, output: &mut Output) {
        // Get the topmost surface for window comparison.
        let mut root_surface = Cow::Borrowed(surface);
        while let Some(parent) = compositor::get_parent(surface) {
            root_surface = Cow::Owned(parent);
        }

        // Handle XDG surface commits.
        for mut window in self.windows.iter().map(|window| window.borrow_mut()) {
            if window.surface.get_surface() == Some(&root_surface) {
                window.surface_commit(surface, output);
                return;
            }
        }

        // Handle layer shell surface commits.
        let old_exclusive = output.exclusive;
        let transaction = self.transaction.get_or_insert(Transaction::new(self));
        for mut window in self.layers.iter_mut() {
            if window.surface.get_surface() == Some(&root_surface) {
                window.surface_commit(surface, output, transaction);
                break;
            }
        }

        // Resize windows after exclusive zone change.
        if output.exclusive != old_exclusive {
            self.resize_all(output);
        }
    }

    /// Draw the current window state.
    pub fn draw(&mut self, renderer: &mut Gles2Renderer, frame: &mut Gles2Frame, output: &Output) {
        self.update_transaction();

        self.layers.draw_background(renderer, frame, output);

        match self.view {
            View::Workspace => {
                self.with_visible(|window| window.draw(renderer, frame, output, 1., None));
            },
            View::DragAndDrop(ref dnd) => {
                self.with_visible(|window| window.draw(renderer, frame, output, 1., None));
                dnd.draw(renderer, frame, output, &self.windows, &mut self.graphics);
            },
            View::Overview(ref mut overview) => {
                overview.draw(renderer, frame, output, &self.windows, &mut self.graphics);
            },
        }

        self.layers.draw_foreground(renderer, frame, output);
    }

    /// Request new frames for all visible windows.
    pub fn request_frames(&mut self) {
        if self.view == View::Workspace {
            let runtime = self.runtime();
            self.layers.request_frames(runtime);
            self.with_visible(|window| window.request_frame(runtime));
        }
    }

    /// Update window manager state.
    pub fn refresh(&mut self, output: &mut Output) {
        // Handle layer shell death.
        let old_exclusive = output.exclusive;
        for window in self.layers.iter() {
            if !window.alive() {
                self.transaction.get_or_insert(Transaction::new(self));
                output.exclusive.reset(window.surface.anchor, window.surface.exclusive_zone);
            }
        }

        // Resize windows on demand.
        if output.exclusive != old_exclusive {
            self.resize_all(output);
        } else if self.windows.iter().any(|window| !window.borrow().alive()) {
            self.refresh_visible(output);
        }

        // Start D&D on long touch in overview.
        if let View::Overview(overview) = &mut self.view {
            if overview.hold_start.map_or(false, |start| start.elapsed() >= HOLD_DURATION) {
                let index = overview.focused_index(self.windows.len());
                let dnd = DragAndDrop::new(overview.last_drag_point, overview.x_offset, index);
                self.view = View::DragAndDrop(dnd);
            }
        }
    }

    /// Reap dead visible windows.
    ///
    /// This will reorder and resize visible windows when any of them has died.
    fn refresh_visible(&mut self, output: &Output) {
        let transaction = self.start_transaction();

        // Remove dead primary/secondary windows.
        if transaction.secondary.upgrade().map_or(true, |window| !window.borrow().alive()) {
            transaction.secondary = Weak::new();
        }
        if transaction.primary.upgrade().map_or(true, |window| !window.borrow().alive()) {
            transaction.primary = mem::take(&mut transaction.secondary);
        }

        transaction.update_visible_dimensions(output);
    }

    /// Create a new transaction, or access the active one.
    fn start_transaction(&mut self) -> &mut Transaction {
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
            let finished = self.windows.iter().all(|window| window.borrow().transaction_done())
                && self.layers.iter().all(|window| window.transaction_done());

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
            if !self.windows[i].borrow().alive() {
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

        // Update layer shell windows.
        self.layers.apply_transaction();

        // Apply window management changes.
        let transaction = self.transaction.take().unwrap();
        self.view = transaction.view.unwrap_or(self.view);
        self.secondary = transaction.secondary;
        self.primary = transaction.primary;
    }

    /// Resize all windows to their expected size.
    pub fn resize_all(&mut self, output: &mut Output) {
        let transaction = self.transaction.get_or_insert(Transaction::new(self));

        // Resize invisible windows.
        for window in &self.windows {
            let mut window = window.borrow_mut();
            let rectangle = Rectangle::from_loc_and_size((0, 0), output.available().size);
            window.set_dimensions(transaction, rectangle);
        }

        // Resize primary/secondary.
        transaction.update_visible_dimensions(output);

        // Resize layer shell windows.
        for mut window in self.layers.iter_mut() {
            window.update_dimensions(output, transaction);
        }
    }

    /// Handle start of touch input.
    pub fn on_touch_start(&mut self, output: &Output, point: Point<f64, Logical>) {
        if let View::Overview(overview) = &mut self.view {
            // Click inside focused window stages it for opening as secondary.
            let window_bounds = overview.focused_bounds(output, self.windows.len());
            if window_bounds.contains(point.to_i32_round()) {
                overview.hold_start = Some(Instant::now());
            }

            overview.last_drag_point = point;
            overview.drag_direction = None;
        }
    }

    /// Hand quick touch input.
    pub fn on_tap(&mut self, output: &Output, point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::DragAndDrop(_) | View::Workspace => return,
        };

        overview.hold_start = None;

        // Click inside focused window opens it as primary.
        let window_bounds = overview.focused_bounds(output, self.windows.len());
        if window_bounds.contains(point.to_i32_round()) {
            let index = overview.focused_index(self.windows.len());

            // Clear secondary unless *only* primary is empty.
            self.set_primary(output, index);
            if self.primary.strong_count() > 0 {
                self.set_secondary(output, None);
            }

            self.set_view(View::Workspace);
        }
    }

    /// Handle a touch drag.
    pub fn on_drag(&mut self, point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::DragAndDrop(dnd) => {
                let delta = point - mem::replace(&mut dnd.touch_position, point);
                dnd.window_position += delta;
                return;
            },
            _ => return,
        };

        let delta = point - mem::replace(&mut overview.last_drag_point, point);

        let drag_direction = overview.drag_direction.get_or_insert_with(|| {
            if delta.x.abs() >= delta.y.abs() {
                Direction::Horizontal
            } else {
                Direction::Vertical
            }
        });

        match drag_direction {
            Direction::Horizontal => overview.x_offset += delta.x / OVERVIEW_HORIZONTAL_SENSITIVITY,
            Direction::Vertical => overview.y_offset += delta.y,
        }

        overview.last_overdrag_step = None;
        overview.hold_start = None;
    }

    /// Handle touch release.
    pub fn on_drag_release(&mut self, output: &Output) {
        match self.view {
            View::Overview(ref mut overview) => {
                let close_distance = output.available().size.h as f64 * OVERVIEW_CLOSE_DISTANCE;
                let should_close = overview.y_offset.abs() >= close_distance;

                overview.last_overdrag_step = Some(Instant::now());
                overview.y_offset = 0.;

                // Close window if y offset exceeds the threshold.
                if should_close && !self.windows.is_empty() {
                    let index = overview.focused_index(self.windows.len());
                    self.windows[index].borrow_mut().surface.send_close();
                    self.windows.remove(index);
                    self.refresh_visible(output);
                }
            },
            View::DragAndDrop(dnd) => {
                let (primary_bounds, secondary_bounds) = dnd.drop_bounds(output);
                if primary_bounds.to_f64().contains(dnd.touch_position) {
                    self.set_primary(output, dnd.window_index);
                    self.set_view(View::Workspace);
                } else if secondary_bounds.to_f64().contains(dnd.touch_position) {
                    self.set_secondary(output, dnd.window_index);
                    self.set_view(View::Workspace);
                } else {
                    let overview = Overview { x_offset: dnd.overview_x_offset, ..Overview::new() };
                    self.set_view(View::Overview(overview));
                }
            },
            View::Workspace => (),
        }
    }

    /// Application runtime.
    pub fn runtime(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// Show the application overview.
    pub fn toggle_overview(&mut self) {
        let current_view = self.view;
        let transaction = self.start_transaction();
        transaction.view = match transaction.view.unwrap_or(current_view) {
            View::Overview(_) | View::DragAndDrop(_) => Some(View::Workspace),
            View::Workspace => Some(View::Overview(Overview::default())),
        };
    }

    /// Change the active view.
    fn set_view(&mut self, view: View) {
        self.start_transaction().view = Some(view);
    }

    /// Execute a function for all visible windows.
    fn with_visible<F: FnMut(&mut Window)>(&self, mut fun: F) {
        for window in self.primary.upgrade().iter_mut().chain(&mut self.secondary.upgrade()) {
            fun(&mut window.borrow_mut());
        }
    }

    /// Change the primary window.
    fn set_primary(&mut self, output: &Output, index: impl Into<Option<usize>>) {
        let transaction = self.transaction.get_or_insert(Transaction::new(self));
        let window = index.into().map(|index| &self.windows[index]);

        // Ignore no-ops.
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

        transaction.update_visible_dimensions(output);
    }

    /// Change the secondary window.
    fn set_secondary(&mut self, output: &Output, index: impl Into<Option<usize>>) {
        let transaction = self.transaction.get_or_insert(Transaction::new(self));
        let window = index.into().map(|i| &self.windows[i]);

        // Ignore no-ops.
        let weak_window = window.map(Rc::downgrade).unwrap_or_default();
        if weak_window.ptr_eq(&transaction.secondary) {
            return;
        }

        // Update output's visible windows.
        if let Some(secondary) = transaction.secondary.upgrade() {
            secondary.borrow_mut().leave(transaction, output);
        }
        if let Some(window) = &window {
            window.borrow_mut().enter(output);
        }

        // Clear primary if it's the new secondary.
        if weak_window.ptr_eq(&transaction.primary) {
            transaction.primary = Weak::new();
        }

        // Set secondary and move old one to primary if it is empty.
        let old_secondary = mem::replace(&mut transaction.secondary, weak_window);
        if transaction.primary.strong_count() == 0 {
            transaction.primary = old_secondary;
        }

        transaction.update_visible_dimensions(output);
    }
}

/// Compositor window arrangements.
#[derive(Copy, Clone, PartialEq, Debug)]
enum View {
    /// List of all open windows.
    Overview(Overview),
    /// Drag and drop for tiling windows.
    DragAndDrop(DragAndDrop),
    /// Currently active windows.
    Workspace,
}

impl Default for View {
    fn default() -> Self {
        View::Workspace
    }
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

    /// Update visible window dimensions.
    pub fn update_visible_dimensions(&mut self, output: &Output) {
        if let Some(mut primary) = self.primary.upgrade().as_ref().map(|s| s.borrow_mut()) {
            let secondary_visible = self.secondary.strong_count() > 0;
            let rectangle = output.primary_rectangle(secondary_visible);
            primary.set_dimensions(self, rectangle);
        }

        if let Some(mut secondary) = self.secondary.upgrade().as_ref().map(|s| s.borrow_mut()) {
            let rectangle = output.secondary_rectangle();
            secondary.set_dimensions(self, rectangle);
        }
    }
}

/// Atomic changes to [`Window`].
#[derive(Debug)]
struct WindowTransaction {
    rectangle: Rectangle<i32, Logical>,
}

impl WindowTransaction {
    fn new<S>(current_state: &Window<S>) -> Self {
        Self { rectangle: current_state.rectangle }
    }
}

/// Cached window textures.
#[derive(Default, Debug)]
struct TextureCache {
    /// Size of all textures combined.
    size: Size<i32, Logical>,
    textures: Vec<Texture>,
}

impl TextureCache {
    /// Reset the texture cache.
    fn reset(&mut self, size: Size<i32, Logical>) {
        self.textures.clear();
        self.size = size;
    }

    /// Add a new texture.
    fn push(&mut self, texture: Texture) {
        self.textures.push(texture);
    }
}

/// Common surface functionality.
pub trait Surface {
    fn get_surface(&self) -> Option<&WlSurface>;

    /// Check if the window has been closed.
    fn alive(&self) -> bool;

    /// Request application shutdown.
    fn send_close(&self);

    /// Send a configure for the latest window properties.
    fn reconfigure(&self, size: Size<i32, Logical>);

    /// Window's acknowledged size.
    fn acked_size(&self) -> Size<i32, Logical>;

    /// Geometry of the window's visible bounds.
    fn geometry(&self) -> Rectangle<i32, Logical>;
}

impl Surface for ToplevelSurface {
    fn get_surface(&self) -> Option<&WlSurface> {
        self.get_surface()
    }

    fn alive(&self) -> bool {
        self.alive()
    }

    fn send_close(&self) {
        self.send_close()
    }

    fn reconfigure(&self, size: Size<i32, Logical>) {
        let result = self.with_pending_state(|state| {
            state.size = Some(size);

            // Mark window as tiled, using maximized fallback if tiling is unsupported.
            if self.version() >= 2 {
                state.states.set(State::TiledBottom);
                state.states.set(State::TiledRight);
                state.states.set(State::TiledLeft);
                state.states.set(State::TiledTop);
            } else {
                state.states.set(State::Maximized);
            }

            // Always use server-side decoration.
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });

        if result.is_ok() {
            self.send_configure();
        }
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        let surface = match self.get_surface() {
            Some(surface) => surface,
            None => return Size::default(),
        };

        compositor::with_states(surface, |states| {
            let attributes = states
                .data_map
                .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                .and_then(|attributes| attributes.lock().ok());

            attributes.and_then(|attributes| attributes.current.size)
        })
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    fn geometry(&self) -> Rectangle<i32, Logical> {
        self.get_surface()
            .and_then(|surface| {
                compositor::with_states(surface, |states| {
                    states.cached_state.current::<SurfaceCachedState>().geometry
                })
                .ok()
                .flatten()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct CatacombLayerSurface {
    pub exclusive_zone: ExclusiveZone,
    pub anchor: Anchor,
    surface: LayerSurface,
}

impl From<LayerSurface> for CatacombLayerSurface {
    fn from(surface: LayerSurface) -> Self {
        Self { surface, exclusive_zone: Default::default(), anchor: Default::default() }
    }
}

impl Surface for CatacombLayerSurface {
    fn get_surface(&self) -> Option<&WlSurface> {
        self.surface.get_surface()
    }

    fn alive(&self) -> bool {
        self.surface.alive()
    }

    fn send_close(&self) {
        self.surface.send_close()
    }

    fn reconfigure(&self, size: Size<i32, Logical>) {
        let result = self.surface.with_pending_state(|state| {
            state.size = Some(size);
        });

        if result.is_ok() {
            self.surface.send_configure();
        }
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        let surface = match self.surface.get_surface() {
            Some(surface) => surface,
            None => return Size::default(),
        };

        compositor::with_states(surface, |states| {
            let attributes = states
                .data_map
                .get::<Mutex<LayerSurfaceAttributes>>()
                .and_then(|attributes| attributes.lock().ok());

            attributes.and_then(|attributes| attributes.current.size)
        })
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    fn geometry(&self) -> Rectangle<i32, Logical> {
        Rectangle::from_loc_and_size((0, 0), self.acked_size())
    }
}

/// Wayland client window state.
#[derive(Debug)]
pub struct Window<S = ToplevelSurface> {
    /// Initial size configure status.
    pub initial_configure_sent: bool,

    /// Buffers pending to be imported.
    pub buffers_pending: bool,

    /// Last configure size acked by the client.
    pub acked_size: Size<i32, Logical>,

    /// Desired window dimensions.
    rectangle: Rectangle<i32, Logical>,

    /// Attached surface.
    surface: S,

    /// Texture cache, storing last window state.
    texture_cache: TextureCache,

    /// Window is currently visible on the output.
    visible: bool,

    /// Transaction for atomic upgrades.
    transaction: Option<WindowTransaction>,
}

impl<S: Surface> Window<S> {
    /// Create a new Toplevel window.
    pub fn new(surface: S) -> Self {
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

    /// Send a frame request to the window.
    pub fn request_frame(&mut self, runtime: u32) {
        self.with_surfaces(|_, surface_data| {
            let mut attributes = surface_data.cached_state.current::<SurfaceAttributes>();
            for callback in attributes.frame_callbacks.drain(..) {
                callback.done(runtime);
            }
        });
    }

    /// Geometry of the window's visible bounds.
    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        self.surface.geometry()
    }

    /// Check window liveliness.
    pub fn alive(&self) -> bool {
        self.surface.alive()
    }

    /// Render this window's buffers.
    ///
    /// If no location is specified, the textures cached location will be used.
    pub fn draw(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        output: &Output,
        scale: f64,
        bounds: impl Into<Option<Rectangle<i32, Logical>>>,
    ) {
        // Skip updating windows during transactions.
        if self.transaction.is_none() && self.buffers_pending {
            self.import_buffers(renderer);
        }

        let bounds = bounds.into().unwrap_or_else(|| {
            // Center window inside its space.
            let mut bounds = self.rectangle;
            bounds.loc.x += ((bounds.size.w - self.texture_cache.size.w) / 2).max(0);
            bounds.loc.y += ((bounds.size.h - self.texture_cache.size.h) / 2).max(0);
            bounds
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
                    let texture = Texture::from_surface(texture.clone(), location, &data);
                    self.texture_cache.push(texture);
                    return TraversalAction::DoChildren(location);
                }

                // Import and cache the buffer.

                let buffer = match &data.buffer {
                    Some(buffer) => buffer,
                    None => return TraversalAction::SkipChildren,
                };

                let attributes = surface_data.cached_state.current::<SurfaceAttributes>();
                let damage: Vec<_> = attributes
                    .damage
                    .iter()
                    .map(|damage| match damage {
                        Damage::Buffer(rect) => *rect,
                        Damage::Surface(rect) => {
                            rect.to_buffer(data.scale, data.transform, &data.size)
                        },
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
                        let texture = Texture::from_surface(texture, location, &data);
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

    /// Send a configure for the latest window properties.
    fn reconfigure(&mut self) {
        let size = match &self.transaction {
            Some(transaction) => transaction.rectangle.size,
            None => self.rectangle.size,
        };

        self.surface.reconfigure(size);
    }

    /// Change the window dimensions.
    fn set_dimensions(&mut self, transaction: &Transaction, rectangle: Rectangle<i32, Logical>) {
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
        let rectangle = Rectangle::from_loc_and_size((0, 0), output.available().size);
        self.set_dimensions(transaction, rectangle);
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
    pub fn apply_transaction(&mut self) {
        let transaction = match self.transaction.take() {
            Some(transaction) => transaction,
            None => return,
        };

        self.rectangle = transaction.rectangle;
    }

    /// Check if the transaction is ready for application.
    fn transaction_done(&self) -> bool {
        self.transaction.as_ref().map_or(true, |t| t.rectangle.size == self.acked_size)
    }

    /// Handle common surface commit logic for surfaces of any kind.
    fn surface_commit_common(&mut self, surface: &WlSurface, output: &Output) {
        // Cancel transactions on the commit after the configure was acked.
        self.acked_size = self.surface.acked_size();

        // Handle surface buffer changes.
        compositor::with_surface_tree_upward(
            surface,
            (),
            |_, data, _| {
                let mut attributes = data.cached_state.current::<SurfaceAttributes>();
                if let Some(assignment) = attributes.buffer.take() {
                    data.data_map.insert_if_missing(|| RefCell::new(SurfaceBuffer::new()));
                    let buffer = data.data_map.get::<RefCell<SurfaceBuffer>>().unwrap();
                    buffer.borrow_mut().update_buffer(&attributes, assignment);
                    self.buffers_pending = true;
                }
                TraversalAction::DoChildren(())
            },
            |_, _, _| (),
            |_, _, _| true,
        );

        // Send initial configure after the first commit.
        if !self.initial_configure_sent {
            self.initial_configure_sent = true;
            self.reconfigure();
        }

        // Advertise current output to visible surfaces.
        if self.visible {
            output.enter(surface);
        }
    }
}

impl Window {
    /// Handle a surface commit for this window.
    fn surface_commit(&mut self, surface: &WlSurface, output: &Output) {
        self.surface_commit_common(surface, output);
    }
}

impl Window<CatacombLayerSurface> {
    /// Handle a surface commit for this window.
    fn surface_commit(
        &mut self,
        surface: &WlSurface,
        output: &mut Output,
        transaction: &Transaction,
    ) {
        self.update_dimensions(output, transaction);
        self.surface_commit_common(surface, output);
    }

    /// Recompute the window's size and location.
    fn update_dimensions(&mut self, output: &mut Output, transaction: &Transaction) {
        let surface = match self.surface.get_surface() {
            Some(surface) => surface,
            None => return,
        };

        let (mut size, margin, anchor, exclusive) = compositor::with_states(surface, |states| {
            let state = states.cached_state.current::<LayerSurfaceCachedState>();
            (state.size, state.margin, state.anchor, state.exclusive_zone)
        })
        .unwrap_or_default();
        let output_size = output.screen_size();

        // Update exclusive zones.
        let old_anchor = mem::replace(&mut self.surface.anchor, anchor);
        let old_exclusive = mem::replace(&mut self.surface.exclusive_zone, exclusive);
        output.exclusive.reset(old_anchor, old_exclusive);
        output.exclusive.update(anchor, exclusive);

        let exclusive = match exclusive {
            ExclusiveZone::Neutral => output.exclusive,
            _ => ExclusiveSpace::default(),
        };

        // Window size.
        if size.w == 0 && anchor.contains(Anchor::LEFT | Anchor::RIGHT) {
            size.w = output_size.w - exclusive.left - exclusive.right;
            if anchor.contains(Anchor::RIGHT) {
                size.w -= margin.right;
            }
        }
        if size.h == 0 && anchor.contains(Anchor::TOP | Anchor::BOTTOM) {
            size.h = output_size.h - exclusive.top - exclusive.bottom;
            if anchor.contains(Anchor::BOTTOM) {
                size.h -= margin.bottom;
            }
        }

        // Window location.
        let x = if anchor.contains(Anchor::LEFT) {
            margin.left + exclusive.left
        } else if anchor.contains(Anchor::RIGHT) {
            output_size.w - size.w - margin.right - exclusive.right
        } else {
            (output_size.w - size.w) / 2
        };
        let y = if anchor.contains(Anchor::TOP) {
            margin.top + exclusive.top
        } else if anchor.contains(Anchor::BOTTOM) {
            output_size.h - size.h - margin.bottom - exclusive.bottom
        } else {
            (output_size.h - size.h) / 2
        };

        let dimensions = Rectangle::from_loc_and_size((x, y), size);
        self.set_dimensions(transaction, dimensions);
    }
}
