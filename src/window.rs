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
use smithay::reexports::wayland_protocols::xdg_shell::server::xdg_toplevel::State;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};
use smithay::wayland::compositor::{
    self, SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction,
};
use smithay::wayland::shell::wlr_layer::{
    Anchor, ExclusiveZone, KeyboardInteractivity, Layer, LayerSurface, LayerSurfaceAttributes,
    LayerSurfaceCachedState,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, SurfaceCachedState, ToplevelSurface, XdgPopupSurfaceRoleAttributes,
    XdgToplevelSurfaceRoleAttributes,
};
use xdg_decoration::v1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

use crate::drawing::{Graphics, SurfaceBuffer, Texture, MAX_DAMAGE_AGE};
use crate::input::{Gesture, TouchState, HOLD_DURATION};
use crate::layer::Layers;
use crate::orientation::Orientation;
use crate::output::{ExclusiveSpace, Output};
use crate::overview::{Direction, DragAndDrop, Overview};

/// Horizontal sensitivity of the application overview.
const OVERVIEW_HORIZONTAL_SENSITIVITY: f64 = 250.;

/// Maximum time before a transaction is cancelled.
const MAX_TRANSACTION_DURATION: Duration = Duration::from_millis(200);

/// Container tracking all known clients.
#[derive(Debug)]
pub struct Windows {
    start_time: Instant,

    primary: Weak<RefCell<Window>>,
    secondary: Weak<RefCell<Window>>,
    view: View,

    windows: Vec<Rc<RefCell<Window>>>,
    orphan_popups: Vec<Window<PopupSurface>>,
    layers: Layers,

    transaction: Option<Transaction>,

    last_focus: Option<WlSurface>,
    focus: Option<WlSurface>,

    /// Orientation used for the window's current rendered state.
    ///
    /// This is used to keep rendering at the previous orientation when a device was rotated and
    /// there is an active transaction pending waiting for clients to submit new buffers.
    orientation: Orientation,

    /// Compositor damage beyond window-internal changes.
    fully_damaged: bool,
    damage: Vec<Rectangle<f64, Physical>>,
}

impl Windows {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            fully_damaged: true,
            orphan_popups: Default::default(),
            transaction: Default::default(),
            orientation: Default::default(),
            last_focus: Default::default(),
            secondary: Default::default(),
            windows: Default::default(),
            primary: Default::default(),
            damage: Default::default(),
            layers: Default::default(),
            focus: Default::default(),
            view: Default::default(),
        }
    }

    /// Add a new window.
    pub fn add(&mut self, surface: ToplevelSurface, output: &Output) {
        // Focus new windows.
        self.focus = surface.get_surface().cloned();

        self.windows.push(Rc::new(RefCell::new(Window::new(surface))));
        self.set_primary(output, self.windows.len() - 1);
        self.set_secondary(output, None);
    }

    /// Add a new layer shell window.
    pub fn add_layer(&mut self, layer: Layer, surface: impl Into<CatacombLayerSurface>) {
        self.layers.add(layer, surface.into());
    }

    /// Add a new popup window.
    pub fn add_popup(&mut self, popup: PopupSurface) {
        self.orphan_popups.push(Window::new(popup));
    }

    /// Find the XDG shell window responsible for a specific surface.
    pub fn find_xdg(&mut self, wl_surface: &WlSurface) -> Option<RefMut<Window>> {
        // Get root surface.
        let mut wl_surface = Cow::Borrowed(wl_surface);
        while let Some(surface) = compositor::get_parent(&wl_surface) {
            wl_surface = Cow::Owned(surface);
        }

        self.windows
            .iter_mut()
            .map(|window| window.borrow_mut())
            .find(|window| window.surface().map_or(false, |surface| surface.eq(&wl_surface)))
    }

    /// Handle a surface commit for any window.
    pub fn surface_commit(&mut self, surface: &WlSurface, output: &mut Output) {
        // Get the topmost surface for window comparison.
        let mut root_surface = Cow::Borrowed(surface);
        while let Some(parent) = compositor::get_parent(&root_surface) {
            root_surface = Cow::Owned(parent);
        }

        // Find a window matching the root surface.
        macro_rules! find_window {
            ($windows:expr) => {{
                $windows.find(|window| window.surface() == Some(&root_surface))
            }};
        }

        // Handle XDG surface commits.
        let mut windows = self.windows.iter().map(|window| window.borrow_mut());
        if let Some(mut window) = find_window!(windows) {
            window.surface_commit_common(surface, output);
            return;
        }

        // Handle popup orphan adoption.
        self.orphan_surface_commit(&root_surface);

        // Apply popup surface commits.
        for window in &mut self.windows {
            window.borrow_mut().popup_surface_commit(&root_surface, surface, output);
        }

        // Handle layer shell surface commits.
        let old_exclusive = output.exclusive;
        let transaction = self.transaction.get_or_insert(Transaction::new(self));
        if let Some(window) = find_window!(self.layers.iter_mut()) {
            window.surface_commit(surface, output, transaction);
        }

        // Resize windows after exclusive zone change.
        if output.exclusive != old_exclusive {
            self.resize_all(output);
        }
    }

    /// Handle orphan popup surface commits.
    ///
    /// After the first surface commit, every popup should have a parent set. This function puts it
    /// at the correct location in the window tree below its parent.
    ///
    /// Popups will be dismissed if a surface commit is made for them without any parent set. They
    /// will also be dismissed if the parent is not currently visible.
    pub fn orphan_surface_commit(&mut self, root_surface: &WlSurface) -> Option<()> {
        let mut orphans = self.orphan_popups.iter();
        let index = orphans.position(|popup| popup.surface() == Some(root_surface))?;
        let mut popup = self.orphan_popups.swap_remove(index);
        let parent = popup.parent()?;

        // Try and add it to the primary window.
        if let Some(primary) = self.primary.upgrade() {
            popup = primary.borrow_mut().add_popup(popup, &parent)?;
        }

        // Try and add it to the secondary window.
        if let Some(secondary) = self.secondary.upgrade() {
            popup = secondary.borrow_mut().add_popup(popup, &parent)?;
        }

        // Dismiss popup if it wasn't added to either of the visible windows.
        popup.surface.send_popup_done();

        Some(())
    }

    /// Draw the current window state.
    pub fn draw(
        &mut self,
        renderer: &mut Gles2Renderer,
        frame: &mut Gles2Frame,
        graphics: &mut Graphics,
        output: &Output,
        buffer_age: u8,
    ) {
        // Reset damage.
        self.fully_damaged = false;

        self.layers.draw_background(renderer, frame, output, buffer_age);

        match self.view {
            View::Workspace => {
                self.with_visible(|window| {
                    window.draw(renderer, frame, output, 1., None, buffer_age)
                });
            },
            View::DragAndDrop(ref dnd) => {
                self.with_visible(|window| {
                    window.draw(renderer, frame, output, 1., None, buffer_age)
                });
                dnd.draw(renderer, frame, output, &self.windows, graphics);
            },
            View::Overview(ref mut overview) => {
                overview.draw(renderer, frame, output, &self.windows, graphics);
            },
        }

        self.layers.draw_foreground(renderer, frame, output, buffer_age);
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

        // Cleanup old popup windows.
        for window in &mut self.windows {
            window.borrow_mut().refresh_popups();
        }

        // Start D&D on long touch in overview.
        if let View::Overview(overview) = &mut self.view {
            if overview.hold_start.map_or(false, |start| start.elapsed() >= HOLD_DURATION) {
                let index = overview.focused_index(self.windows.len());
                let dnd = DragAndDrop::new(overview.last_drag_point, overview.x_offset, index);
                self.view = View::DragAndDrop(dnd);
                self.fully_damaged = true;
            }
        }
    }

    /// Check for focus change requests.
    ///
    /// If `Some` is returned, the focus should be changed. A value of `Some(None)` indicates that
    /// the focus should be cleared.
    pub fn focus_request(&mut self) -> Option<Option<WlSurface>> {
        let new_focus = match &mut self.view {
            View::Overview(_) | View::DragAndDrop(_) => None,
            View::Workspace => self.focus.clone(),
        };

        (self.last_focus != new_focus).then(|| {
            self.last_focus = new_focus;
            self.last_focus.clone()
        })
    }

    /// Reap dead visible windows.
    ///
    /// This will reorder and resize visible windows when any of them has died.
    fn refresh_visible(&mut self, output: &Output) {
        let mut focus = self.focus.take();

        let transaction = self.start_transaction();
        let primary = transaction.primary.upgrade();
        let secondary = transaction.secondary.upgrade();

        // Remove dead primary/secondary windows.
        if secondary.as_ref().map_or(true, |window| !window.borrow().alive()) {
            focus = primary.as_ref().and_then(|window| window.borrow().surface().cloned());
            transaction.secondary = Weak::new();
        }
        if primary.map_or(true, |window| !window.borrow().alive()) {
            focus = secondary.and_then(|window| window.borrow().surface().cloned());
            transaction.primary = mem::take(&mut transaction.secondary);
        }

        transaction.update_visible_dimensions(output);

        self.focus = focus;
    }

    /// Create a new transaction, or access the active one.
    fn start_transaction(&mut self) -> &mut Transaction {
        self.transaction.get_or_insert(Transaction::new(self))
    }

    /// Attempt to execute pending transactions.
    pub fn update_transaction(&mut self) {
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
        self.orientation = transaction.orientation;
        self.secondary = transaction.secondary;
        self.primary = transaction.primary;
        self.fully_damaged = true;
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
        for window in self.layers.iter_mut() {
            window.update_dimensions(output, transaction);
        }
    }

    /// Update output orientation.
    pub fn update_orientation(&mut self, output: &mut Output) {
        let transaction = self.start_transaction();
        transaction.orientation = output.orientation();
        self.resize_all(output);
    }

    /// Get the current rendering orientation.
    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Check if any window was damaged since the last redraw.
    pub fn damaged(&mut self) -> bool {
        self.fully_damaged
            || self.primary.upgrade().map_or(false, |window| window.borrow().damaged())
            || self.secondary.upgrade().map_or(false, |window| window.borrow().damaged())
            || self.layers.iter().any(|window| window.damaged())
    }

    /// Damage since last redraw.
    pub fn damage(&mut self, output: &Output, buffer_age: &mut u8) -> &[Rectangle<f64, Physical>] {
        self.damage.clear();

        // Skip damage calculation for full redraws.
        if *buffer_age > MAX_DAMAGE_AGE as u8 || self.fully_damaged {
            *buffer_age = 0;
            let size = output.physical_resolution().to_f64();
            self.damage.push(Rectangle::from_loc_and_size((0., 0.), size));
            return self.damage.as_slice();
        }

        let buffer_age = *buffer_age as usize;

        if let Some(window) = self.primary.upgrade() {
            window.borrow().append_damage(&mut self.damage, buffer_age);
        }

        if let Some(window) = self.secondary.upgrade() {
            window.borrow().append_damage(&mut self.damage, buffer_age);
        }

        for window in self.layers.iter() {
            window.append_damage(&mut self.damage, buffer_age);
        }

        self.damage.as_slice()
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
            overview.y_offset = 0.;
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
        if !self.windows.is_empty() && window_bounds.contains(point.to_i32_round()) {
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
    pub fn on_drag(
        &mut self,
        output: &Output,
        touch_state: &mut TouchState,
        mut point: Point<f64, Logical>,
    ) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::DragAndDrop(dnd) => {
                // Cancel velocity and clamp if touch position is outside the screen.
                let output_size = output.size().to_f64();
                if point.x < 0.
                    || point.x > output_size.w
                    || point.y < 0.
                    || point.y > output_size.h
                {
                    point.x = point.x.clamp(0., output_size.w - 1.);
                    point.y = point.y.clamp(0., output_size.h - 1.);
                    touch_state.cancel_velocity();
                }

                let delta = point - mem::replace(&mut dnd.touch_position, point);
                dnd.window_position += delta;

                // Redraw when the D&D window is moved.
                self.fully_damaged = true;

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

        // Cancel velocity once drag actions are completed.
        if overview.should_close(output) || overview.overdrag_limited(self.windows.len()) {
            touch_state.cancel_velocity();
        }

        overview.last_overdrag_step = None;
        overview.hold_start = None;

        // Redraw when cycling through the overview.
        self.fully_damaged = true;
    }

    /// Handle touch drag release.
    pub fn on_drag_release(&mut self, output: &Output) {
        match self.view {
            View::Overview(ref mut overview) => {
                let should_close = overview.should_close(output);

                overview.last_overdrag_step = Some(Instant::now());
                overview.y_offset = 0.;

                // Close window if y offset exceeds the threshold.
                if should_close && !self.windows.is_empty() {
                    let index = overview.focused_index(self.windows.len());
                    self.windows[index].borrow_mut().surface.send_close();
                    self.windows.remove(index);
                    self.refresh_visible(output);

                    // Close overview after all windows were closed.
                    if self.windows.is_empty() {
                        self.set_view(View::Workspace);
                    }
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

    /// Handle touch gestures.
    pub fn on_gesture(&mut self, output: &Output, gesture: Gesture) {
        match (gesture, self.view) {
            (Gesture::Overview, _) if !self.windows.is_empty() => {
                self.set_view(View::Overview(Overview::default()));
            },
            (Gesture::Home, View::Workspace) => {
                self.set_secondary(output, None);
                self.set_primary(output, None);
                self.set_view(View::Workspace);
                self.focus = None;
            },
            (Gesture::Home, View::Overview(_)) => self.set_view(View::Workspace),
            _ => (),
        }
    }

    /// Check which surface is at a specific touch point.
    ///
    /// If the window at the touch location accepts keyboard input, this function will also change
    /// focus to the root window associated with the touch surface.
    pub fn touch_surface_at(&mut self, position: Point<f64, Logical>) -> Option<OffsetSurface> {
        // Prevent window interaction in Overview/DnD.
        match self.view {
            View::Workspace => (),
            _ => return None,
        };

        // If window is present, return its surface at the position.
        macro_rules! return_matching {
            ($window:expr) => {{
                if let Some(window) = $window {
                    // Focus the touched window.
                    if !window.deny_focus {
                        self.focus = window.surface().cloned();
                    }

                    return window.surface_at(position);
                }
            }};
        }

        return_matching!(self.layers.foreground_window_at(position));

        let primary = self.primary.upgrade();
        return_matching!(primary.as_ref().map(|w| w.borrow()).filter(|w| w.contains(position)));

        let secondary = self.secondary.upgrade();
        return_matching!(secondary.as_ref().map(|w| w.borrow()).filter(|w| w.contains(position)));

        return_matching!(self.layers.background_window_at(position));

        // Unfocus when touching outside of window bounds.
        self.focus = None;

        None
    }

    /// Application runtime.
    pub fn runtime(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// Check if the overview is currently visible.
    pub fn overview_active(&self) -> bool {
        matches!(self.view, View::Overview(_))
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

        // Update window focus.
        self.focus = window.and_then(|window| window.borrow().surface().cloned());

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
        if transaction.secondary.strong_count() == 0 && transaction.primary.strong_count() != 0 {
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

        // Update window focus.
        self.focus = window.and_then(|window| window.borrow().surface().cloned());

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

        transaction.secondary = weak_window;
        transaction.update_visible_dimensions(output);
    }
}

/// Atomic changes to [`Windows`].
#[derive(Clone, Debug)]
pub struct Transaction {
    primary: Weak<RefCell<Window>>,
    secondary: Weak<RefCell<Window>>,
    orientation: Orientation,
    view: Option<View>,
    start: Instant,
}

impl Transaction {
    fn new(current_state: &Windows) -> Self {
        Self {
            primary: current_state.primary.clone(),
            secondary: current_state.secondary.clone(),
            orientation: current_state.orientation,
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
    fn geometry(&self) -> Rectangle<i32, Logical> {
        self.get_surface()
            .and_then(|surface| {
                compositor::with_states(surface, |states| {
                    states.cached_state.current::<SurfaceCachedState>().geometry
                })
                .ok()
                .flatten()
            })
            .unwrap_or_else(|| Rectangle::from_loc_and_size((0, 0), self.acked_size()))
    }
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
}

impl Surface for PopupSurface {
    fn get_surface(&self) -> Option<&WlSurface> {
        self.get_surface()
    }

    fn alive(&self) -> bool {
        self.alive()
    }

    fn send_close(&self) {}

    fn reconfigure(&self, _size: Size<i32, Logical>) {
        let _ = self.send_configure();
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        self.with_pending_state(|state| state.positioner.rect_size).unwrap_or_default()
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

    /// Whether the window should be excluded for keyboard focus.
    pub deny_focus: bool,

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

    /// Popup windows.
    popups: Vec<Window<PopupSurface>>,
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
            deny_focus: Default::default(),
            acked_size: Default::default(),
            rectangle: Default::default(),
            visible: Default::default(),
            popups: Default::default(),
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

        // Request new frames from all popups.
        for window in &mut self.popups {
            window.request_frame(runtime);
        }
    }

    /// Geometry of the window's visible bounds.
    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        self.surface.geometry()
    }

    /// Check window liveliness.
    pub fn alive(&self) -> bool {
        self.surface.alive()
    }

    /// Check if this window contains a specific point.
    pub fn contains(&self, point: Point<f64, Logical>) -> bool {
        self.bounds().to_f64().contains(point)
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
        buffer_age: u8,
    ) {
        // Update surface buffers unless transaction is active.
        if self.transaction.is_none() && self.buffers_pending {
            self.import_buffers(renderer);
        }

        let bounds = bounds.into().unwrap_or_else(|| self.bounds());

        for texture in &mut self.texture_cache.textures {
            texture.draw_at(frame, output, bounds, scale, buffer_age);
        }

        // Draw popup tree.
        for popup in &mut self.popups {
            let loc = popup.rectangle.loc + bounds.loc;
            let popup_bounds = Rectangle::from_loc_and_size(loc, (i32::MAX, i32::MAX));
            popup.draw(renderer, frame, output, scale, popup_bounds, buffer_age);
        }
    }

    /// Default window location and size.
    fn bounds(&self) -> Rectangle<i32, Logical> {
        // Center window inside its space.
        let mut bounds = self.rectangle;
        bounds.loc.x += ((bounds.size.w - self.texture_cache.size.w) / 2).max(0);
        bounds.loc.y += ((bounds.size.h - self.texture_cache.size.h) / 2).max(0);
        bounds
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
                    self.texture_cache.push(texture.clone());
                    return TraversalAction::DoChildren(location);
                }

                let buffer = match &data.buffer {
                    Some(buffer) => buffer,
                    None => return TraversalAction::SkipChildren,
                };

                // Import and cache the buffer.
                match renderer.import_buffer(buffer, Some(surface_data), data.damage.buffer()) {
                    Some(Ok(texture)) => {
                        // Release SHM buffers after import.
                        if let Some(BufferType::Shm) = renderer::buffer_type(buffer) {
                            data.buffer = None;
                        }

                        // Update and cache the texture.
                        let texture = Texture::from_surface(Rc::new(texture), location, &data);
                        self.texture_cache.push(texture.clone());
                        data.texture = Some(texture);
                        data.damage.clear();

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
    fn with_surfaces<F: FnMut(&WlSurface, &SurfaceData)>(&self, mut fun: F) {
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
                    let mut buffer =
                        data.data_map.get::<RefCell<SurfaceBuffer>>().unwrap().borrow_mut();
                    buffer.update_buffer(&attributes, assignment);
                    self.buffers_pending = true;

                    // Update surface damage.
                    buffer.add_damage(output.scale(), attributes.damage.drain(..));
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

    /// Find subsurface at the specified location.
    fn surface_at(&self, position: Point<f64, Logical>) -> Option<OffsetSurface> {
        // Check popups top to bottom first.
        let relative = position - self.rectangle.loc.to_f64();
        let popup = self.popups.iter().find_map(|popup| popup.surface_at(relative));
        if let Some(popup_surface) = popup {
            return Some(popup_surface);
        }

        let result = RefCell::new(None);
        compositor::with_surface_tree_upward(
            self.surface.get_surface()?,
            self.bounds().loc,
            |wl_surface, surface_data, location| {
                let mut location = *location;
                if surface_data.role == Some("subsurface") {
                    let current = surface_data.cached_state.current::<SubsurfaceCachedState>();
                    location += current.location;
                }

                // Calculate surface's bounding box.
                let size = surface_data
                    .data_map
                    .get::<RefCell<SurfaceBuffer>>()
                    .map_or_else(Size::default, |data| data.borrow().size);
                let surface_rect = Rectangle::from_loc_and_size(location.to_f64(), size.to_f64());

                // Check if the position is within the surface bounds.
                if surface_rect.contains(position) {
                    let surface = OffsetSurface::new(wl_surface.clone(), location);
                    *result.borrow_mut() = Some(surface);
                    TraversalAction::SkipChildren
                } else {
                    TraversalAction::DoChildren(location)
                }
            },
            |_, _, _| {},
            |_, _, _| result.borrow().is_none(),
        );
        result.into_inner()
    }

    /// Add popup at the correct position in the popup tree.
    ///
    /// This function will return the popup when no matching parent surface could be found in the
    /// popup tree.
    fn add_popup(
        &mut self,
        mut popup: Window<PopupSurface>,
        parent: &WlSurface,
    ) -> Option<Window<PopupSurface>> {
        if self.surface.get_surface() == Some(parent) {
            self.popups.push(popup);
            return None;
        }

        for window in &mut self.popups {
            popup = match window.add_popup(popup, parent) {
                Some(popup) => popup,
                None => return None,
            };
        }

        Some(popup)
    }

    /// Apply surface commits for popups.
    fn popup_surface_commit(
        &mut self,
        root_surface: &WlSurface,
        surface: &WlSurface,
        output: &Output,
    ) {
        for window in &mut self.popups {
            if window.surface.get_surface() == Some(root_surface) {
                window.surface_commit_common(surface, output);
                window.rectangle.loc = window.position();
                return;
            }

            window.popup_surface_commit(root_surface, surface, output);
        }
    }

    /// Refresh popup windows.
    fn refresh_popups(&mut self) {
        for i in (0..self.popups.len()).rev() {
            self.popups[i].refresh_popups();

            if !self.popups[i].alive() {
                self.popups.swap_remove(i);
            }
        }
    }

    /// Get primary window surface.
    fn surface(&self) -> Option<&WlSurface> {
        self.surface.get_surface()
    }

    /// Check if this window requires a redraw.
    fn damaged(&self) -> bool {
        self.transaction.is_none() && self.buffers_pending
    }

    /// Append damage from all surfaces to a vector.
    ///
    /// Import the window's pending buffers.
    ///
    /// The passed `damage` vector will be updated with all surfaces damage history within
    /// `buffer_age` range.
    fn append_damage(&self, damage: &mut Vec<Rectangle<f64, Physical>>, buffer_age: usize) {
        // Ignore damage while transactions are still active.
        if self.transaction.is_some() {
            return;
        }

        self.with_surfaces(|_, data| {
            let buffer = match data.data_map.get::<RefCell<SurfaceBuffer>>().map(RefCell::borrow) {
                Some(buffer) if buffer.damage.damaged() => buffer,
                _ => return,
            };

            for buffer_damage in &buffer.damage.physical()[..buffer_age] {
                damage.push(*buffer_damage);
            }
        });
    }
}

impl Window<PopupSurface> {
    /// Get the parent of this popup.
    fn parent(&self) -> Option<WlSurface> {
        let surface = match self.surface.get_surface() {
            Some(surface) => surface,
            None => return None,
        };

        compositor::with_states(surface, |states| {
            let attributes = states.data_map.get::<Mutex<XdgPopupSurfaceRoleAttributes>>()?;
            attributes.lock().ok()?.parent.clone()
        })
        .ok()
        .flatten()
    }

    /// Get popup window offset from parent.
    fn position(&self) -> Point<i32, Logical> {
        self.surface
            .with_pending_state(|state| state.positioner.get_geometry().loc)
            .unwrap_or_default()
    }
}

impl Window<CatacombLayerSurface> {
    /// Handle a surface commit for layer shell windows.
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

        let state = compositor::with_states(surface, |states| {
            *states.cached_state.current::<LayerSurfaceCachedState>()
        })
        .unwrap_or_default();
        let output_size = output.size();

        // Update keyboard interactivity.
        self.deny_focus = state.keyboard_interactivity == KeyboardInteractivity::None;

        // Update exclusive zones.
        let mut size = state.size;
        let old_exclusive = mem::replace(&mut self.surface.exclusive_zone, state.exclusive_zone);
        let old_anchor = mem::replace(&mut self.surface.anchor, state.anchor);
        output.exclusive.reset(old_anchor, old_exclusive);
        output.exclusive.update(state.anchor, state.exclusive_zone);

        let exclusive = match state.exclusive_zone {
            ExclusiveZone::Neutral => output.exclusive,
            _ => ExclusiveSpace::default(),
        };

        // Window size.
        if size.w == 0 && state.anchor.contains(Anchor::LEFT | Anchor::RIGHT) {
            size.w = output_size.w - exclusive.left - exclusive.right;
            if state.anchor.contains(Anchor::RIGHT) {
                size.w -= state.margin.right;
            }
        }
        if size.h == 0 && state.anchor.contains(Anchor::TOP | Anchor::BOTTOM) {
            size.h = output_size.h - exclusive.top - exclusive.bottom;
            if state.anchor.contains(Anchor::BOTTOM) {
                size.h -= state.margin.bottom;
            }
        }

        // Window location.
        let x = if state.anchor.contains(Anchor::LEFT) {
            state.margin.left + exclusive.left
        } else if state.anchor.contains(Anchor::RIGHT) {
            output_size.w - size.w - state.margin.right - exclusive.right
        } else {
            (output_size.w - size.w) / 2
        };
        let y = if state.anchor.contains(Anchor::TOP) {
            state.margin.top + exclusive.top
        } else if state.anchor.contains(Anchor::BOTTOM) {
            output_size.h - size.h - state.margin.bottom - exclusive.bottom
        } else {
            (output_size.h - size.h) / 2
        };

        let dimensions = Rectangle::from_loc_and_size((x, y), size);
        self.set_dimensions(transaction, dimensions);
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

/// Surface with offset from its window origin.
pub struct OffsetSurface {
    pub offset: Point<i32, Logical>,
    pub surface: WlSurface,
}

impl OffsetSurface {
    fn new(surface: WlSurface, offset: Point<i32, Logical>) -> Self {
        Self { surface, offset }
    }
}
