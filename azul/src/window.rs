//! Window creation module

use std::{
    time::Duration,
    fmt,
    rc::Rc,
    marker::PhantomData,
    io::Error as IoError,
    sync::atomic::{AtomicUsize, Ordering},
};
use webrender::{
    api::{
        LayoutRect, PipelineId, Epoch, DocumentId,
        RenderApi, ExternalScrollId, RenderNotifier, DeviceIntSize,
    },
    Renderer, RendererOptions, RendererKind, ShaderPrecacheFlags, WrShaders,
    // renderer::RendererError; -- not currently public in WebRender
};
use glium::{
    IncompatibleOpenGl, Display, SwapBuffersError,
    debug::DebugCallbackBehavior,
    glutin::{
        self, EventsLoop, AvailableMonitorsIter, ContextTrait, CombinedContext, CreationError,
        MonitorId, ContextError, ContextBuilder, WindowId as GliumWindowId,
        Window as GliumWindow, WindowBuilder as GliumWindowBuilder, Icon, Context,
        dpi::{LogicalSize, PhysicalSize}
    },
    backend::{Context as BackendContext, Facade, glutin::DisplayCreationError},
};
use gleam::gl::{self, Gl};
use azul_css::{Css, ColorF, ColorU};
#[cfg(debug_assertions)]
use azul_css::HotReloadHandler;
use {
    FastHashMap,
    dom::{Texture, Callback, NodeData, NodeType},
    window_state::{WindowState, MouseState, KeyboardState},
    traits::Layout,
    compositor::Compositor,
    app::FrameEventInfo,
    app_resources::AppResources,
    id_tree::NodeId,
    default_callbacks::{
        DefaultCallbackSystem, StackCheckedPointer, DefaultCallback, DefaultCallbackId
    },
    ui_state::UiState,
    display_list::ScrolledNodes,
    focus::FocusTarget,
    id_tree::{Node, NodeHierarchy},
};
pub use webrender::api::HitTestItem;

static LAST_PIPELINE_ID: AtomicUsize = AtomicUsize::new(0);

fn new_pipeline_id() -> PipelineId {
    PipelineId(LAST_PIPELINE_ID.fetch_add(1, Ordering::SeqCst) as u32, 0)
}

/// User-modifiable fake window
#[derive(Clone)]
pub struct FakeWindow<T: Layout> {
    /// The window state for the next frame
    pub state: WindowState,
    /// The user can push default callbacks in this `DefaultCallbackSystem`,
    /// which get called later in the hit-testing logic
    pub(crate) default_callbacks: DefaultCallbackSystem<T>,
    /// An Rc to the original WindowContext - this is only so that
    /// the user can create textures and other OpenGL content in the window
    /// but not change any window properties from underneath - this would
    /// lead to mismatch between the
    pub(crate) read_only_window: Rc<Display>,
}

impl<T: Layout> FakeWindow<T> {

    /// Returns a read-only window which can be used to create / draw
    /// custom OpenGL texture during the `.layout()` phase
    pub fn read_only_window(&self) -> ReadOnlyWindow {
        ReadOnlyWindow {
            inner: self.read_only_window.clone()
        }
    }

    pub fn get_physical_size(&self) -> (u32, u32) {
        let hidpi = self.get_hidpi_factor();
        self.state.size.dimensions.to_physical(hidpi).into()
    }

    /// Returns the current HiDPI factor.
    pub fn get_hidpi_factor(&self) -> f64 {
        self.state.size.hidpi_factor
    }

    pub(crate) fn set_keyboard_state(&mut self, kb: &KeyboardState) {
        self.state.keyboard_state = kb.clone();
    }

    pub(crate) fn set_mouse_state(&mut self, mouse: &MouseState) {
        self.state.mouse_state = *mouse;
    }

    /// Returns the current keyboard keyboard state. We don't want the library
    /// user to be able to modify this state, only to read it.
    pub fn get_keyboard_state<'a>(&'a self) -> &'a KeyboardState {
        self.state.get_keyboard_state()
    }

    /// Returns the current windows mouse state. We don't want the library
    /// user to be able to modify this state, only to read it
    pub fn get_mouse_state<'a>(&'a self) -> &'a MouseState {
        self.state.get_mouse_state()
    }

    /// Adds a default callback to the window. The default callbacks are
    /// cleared after every frame, so two-way data binding widgets have to call this
    /// on every frame they want to insert a default callback.
    ///
    /// Returns an ID by which the callback can be uniquely identified (used for hit-testing)
    #[must_use]
    pub fn add_callback(
        &mut self,
        callback_ptr: StackCheckedPointer<T>,
        callback_fn: DefaultCallback<T>)
    -> DefaultCallbackId
    {
        use default_callbacks::get_new_unique_default_callback_id;

        let default_callback_id = get_new_unique_default_callback_id();
        self.default_callbacks.add_callback(default_callback_id, callback_ptr, callback_fn);
        default_callback_id
    }
}

/// Read-only window which can be used to create / draw
/// custom OpenGL texture during the `.layout()` phase
#[derive(Clone)]
pub struct ReadOnlyWindow {
    pub inner: Rc<Display>,
}

impl Facade for ReadOnlyWindow {
    fn get_context(&self) -> &Rc<BackendContext> {
        self.inner.get_context()
    }
}

impl ReadOnlyWindow {

    // Since webrender is asynchronous, we can't let the user draw
    // directly onto the frame or the texture since that has to be timed
    // with webrender
    pub fn create_texture(&self, width: u32, height: u32) -> Texture {
        use glium::texture::texture2d::Texture2d;
        let tex = Texture2d::empty(&*self.inner, width, height).unwrap();
        Texture::new(tex)
    }

    /// Make the window active (OpenGL) - necessary before
    /// starting to draw on any window-owned texture
    pub fn make_current(&self) {
        use glium::glutin::ContextTrait;
        let gl_window = self.inner.gl_window();
        unsafe { gl_window.make_current().unwrap() };
    }

    /// Unbind the current framebuffer manually. Is also executed on `Drop`.
    ///
    /// TODO: Is it necessary to expose this or is it enough to just
    /// unbind the framebuffer on drop?
    pub fn unbind_framebuffer(&self) {
        let gl = get_gl_context(&*self.inner).unwrap();

        gl.bind_framebuffer(gl::FRAMEBUFFER, 0);
    }

    pub fn get_gl_context(&self) -> Rc<Gl> {
        // Can only fail when the API was initialized from WebGL,
        // which can't happen, since that would already crash on startup
        get_gl_context(&*self.inner).unwrap()
    }
}

impl Drop for ReadOnlyWindow {
    fn drop(&mut self) {
        self.unbind_framebuffer();
    }
}

pub struct LayoutInfo<'a, 'b, T: 'b + Layout> {
    pub window: &'b mut FakeWindow<T>,
    pub resources: &'a AppResources,
}

impl<T: Layout> fmt::Debug for FakeWindow<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "FakeWindow {{\
                state: {:?}, \
                read_only_window: Rc<Display>, \
            }}", self.state)
    }
}

/// Information about the callback that is passed to the callback whenever a callback is invoked
pub struct CallbackInfo<'a, T: 'a + Layout> {
    /// The callback can change the focus - note that the focus is set before the
    /// next frames' layout() function is invoked, but the current frames callbacks are not affected.
    pub focus: Option<FocusTarget>,
    /// The ID of the window that the event was clicked on (for indexing into
    /// `app_state.windows`). `app_state.windows[event.window]` should never panic.
    pub window_id: &'a GliumWindowId,
    /// The ID of the node that was hit. You can use this to query information about
    /// the node, but please don't hard-code any if / else statements based on the `NodeId`
    pub hit_dom_node: NodeId,
    /// UiState containing the necessary data for testing what
    pub(crate) ui_state: &'a UiState<T>,
    /// What items are currently being hit
    pub(crate) hit_test_items: &'a [HitTestItem],
    /// The (x, y) position of the mouse cursor, **relative to top left of the element that was hit**.
    pub cursor_relative_to_item: Option<(f32, f32)>,
    /// The (x, y) position of the mouse cursor, **relative to top left of the window**.
    pub cursor_in_viewport: Option<(f32, f32)>,
}

impl<'a, T: 'a + Layout> Clone for CallbackInfo<'a, T> {
    fn clone(&self) -> Self {
        Self {
            focus: self.focus.clone(),
            window_id: self.window_id,
            hit_dom_node: self.hit_dom_node,
            ui_state: self.ui_state,
            hit_test_items: self.hit_test_items,
            cursor_relative_to_item: self.cursor_relative_to_item,
            cursor_in_viewport: self.cursor_in_viewport,
        }
    }
}

impl<'a, T: 'a + Layout> fmt::Debug for CallbackInfo<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CallbackInfo {{ \
            focus: {:?}, \
            window_id: {:?}, \
            hit_dom_node: {:?}, \
            ui_state: {:?}, \
            hit_test_items: {:?}, \
            cursor_relative_to_item: {:?}, \
            cursor_in_viewport: {:?}, \
        }}",
            self.focus,
            self.window_id,
            self.hit_dom_node,
            self.ui_state,
            self.hit_test_items,
            self.cursor_relative_to_item,
            self.cursor_in_viewport,
        )
    }
}

/// Iterator that, starting from a certain starting point, returns the
/// parent node until it gets to the root node.
pub struct ParentNodesIterator<'a> {
    current_item: NodeId,
    node_hierarchy: &'a NodeHierarchy,
}

impl<'a> ParentNodesIterator<'a> {

    /// Returns what node ID the iterator is currently processing
    pub fn current_node(&self) -> NodeId {
        self.current_item
    }

    /// Returns the offset into the parent of the current node or None if the item has no parent
    pub fn current_index_in_parent(&self) -> Option<usize> {
        if self.node_hierarchy[self.current_item].has_parent() {
            Some(self.node_hierarchy.get_index_in_parent(self.current_item))
        } else {
            None
        }
    }
}

impl<'a> Iterator for ParentNodesIterator<'a> {
    type Item = NodeId;

    /// For each item in the current item path, returns the index of the item in the parent
    fn next(&mut self) -> Option<NodeId> {
        let new_parent = self.node_hierarchy[self.current_item].parent?;
        self.current_item = new_parent;
        Some(new_parent)
    }
}

impl<'a, T: 'a + Layout> CallbackInfo<'a, T> {

    /// Creates an iterator that starts at the current DOM node and continouusly
    /// returns the parent NodeId, until it gets to the root component.
    pub fn parent_nodes<'b>(&'b self) -> ParentNodesIterator<'b> {
        ParentNodesIterator {
            current_item: self.hit_dom_node,
            node_hierarchy: &self.ui_state.dom.arena.node_layout,
        }
    }

    /// For any node ID, returns what the position in its parent it is, plus the parent itself.
    /// Returns `None` on the root ID (because the root has no parent, therefore it's the 1st item)
    ///
    /// Note: Index is 0-based (first item has the index of 0)
    pub fn get_index_in_parent(&self, node_id: NodeId) -> Option<(usize, NodeId)> {
        let node_layout = &self.ui_state.dom.arena.node_layout;

        if node_id.index() > node_layout.len() {
            return None; // node_id out of range
        }

        let parent = node_layout[node_id].parent?;
        Some((node_layout.get_index_in_parent(node_id), parent))
    }

    // Functions that are may be called from the user callback
    // - the `CallbackInfo` contains a `&mut UiState`, which can be
    // used to query DOM information when the callbacks are run

    /// Returns the hierarchy of the given node ID
    pub fn get_node<'b>(&'b self, node_id: NodeId) -> Option<&'b Node> {
        self.ui_state.dom.arena.node_layout.internal.get(node_id.index())
    }

    /// Returns the node hierarchy (DOM tree order)
    pub fn get_node_hierarchy<'b>(&'b self) -> &'b NodeHierarchy {
        &self.ui_state.dom.arena.node_layout
    }

    /// Returns the node content of a specific node
    pub fn get_node_content<'b>(&'b self, node_id: NodeId) -> Option<&'b NodeData<T>> {
        self.ui_state.dom.arena.node_data.internal.get(node_id.index())
    }

    /// Returns the index of the target NodeId (the target that received the event)
    /// in the targets parent or None if the target is the root node
    pub fn target_index_in_parent(&self) -> Option<usize> {
        if self.get_node(self.hit_dom_node)?.parent.is_some() {
            Some(self.ui_state.dom.arena.node_layout.get_index_in_parent(self.hit_dom_node))
        } else {
            None
        }
    }

    /// Returns the parent of the given `NodeId` or None if the target is the root node.
    pub fn parent(&self, node_id: NodeId) -> Option<NodeId> {
        self.get_node(node_id)?.parent
    }

    /// Returns the parent of the current target or None if the target is the root node.
    pub fn target_parent(&self) -> Option<NodeId> {
        self.parent(self.hit_dom_node)
    }

    /// Checks whether the target of the CallbackInfo has a certain node type
    pub fn target_is_node_type(&self, node_type: NodeType<T>) -> bool {
        if let Some(self_node) = self.get_node_content(self.hit_dom_node) {
            self_node.is_node_type(node_type)
        } else {
            false
        }
    }

    /// Checks whether the target of the CallbackInfo has a certain ID
    pub fn target_has_id(&self, id: &str) -> bool {
        if let Some(self_node) = self.get_node_content(self.hit_dom_node) {
            self_node.has_id(id)
        } else {
            false
        }
    }

    /// Checks whether the target of the CallbackInfo has a certain class
    pub fn target_has_class(&self, class: &str) -> bool {
        if let Some(self_node) = self.get_node_content(self.hit_dom_node) {
            self_node.has_class(class)
        } else {
            false
        }
    }

    /// Traverses up the hierarchy, checks whether any parent has a certain ID,
    /// the returns that parent
    pub fn any_parent_has_id(&self, id: &str) -> Option<NodeId> {
        self.parent_nodes().find(|parent_id| {
            if let Some(self_node) = self.get_node_content(*parent_id) {
                self_node.has_id(id)
            } else {
                false
            }
        })
    }

    /// Traverses up the hierarchy, checks whether any parent has a certain class
    pub fn any_parent_has_class(&self, class: &str) -> Option<NodeId> {
        self.parent_nodes().find(|parent_id| {
            if let Some(self_node) = self.get_node_content(*parent_id) {
                self_node.has_class(class)
            } else {
                false
            }
        })
    }
}

/// Options on how to initially create the window
#[derive(Debug, Clone)]
pub struct WindowCreateOptions<T: Layout> {
    /// State of the window, set the initial title / width / height here.
    pub state: WindowState,
    /// OpenGL clear color
    pub background: ColorF,
    /// Clear the stencil buffer with the given value. If not set, stencil buffer is not cleared
    pub clear_stencil: Option<i32>,
    /// Clear the depth buffer with the given value. If not set, depth buffer is not cleared
    pub clear_depth: Option<f32>,
    /// How should the screen be updated - as fast as possible
    /// or retained & energy saving?
    pub update_mode: UpdateMode,
    /// Which monitor should the window be created on?
    pub monitor: WindowMonitorTarget,
    /// How precise should the mouse updates be?
    pub mouse_mode: MouseMode,
    /// Should the window update regardless if the mouse is hovering
    /// over the window? (useful for games vs. applications)
    pub update_behaviour: UpdateBehaviour,
    /// Renderer type: Hardware-with-software-fallback, pure software or pure hardware renderer?
    pub renderer_type: RendererType,
    /// Win32 menu callbacks
    pub menu_callbacks: FastHashMap<u16, Callback<T>>,
    /// Sets the window icon (Windows and Linux only). Usually 16x16 px or 32x32px
    pub window_icon: Option<Icon>,
    /// Windows only: Sets the 256x256 taskbar icon during startup
    pub taskbar_icon: Option<Icon>,
    /// Windows only: Sets `WS_EX_NOREDIRECTIONBITMAP` on the window
    pub no_redirection_bitmap: bool,
}

impl<T: Layout> Default for WindowCreateOptions<T> {
    fn default() -> Self {
        Self {
            state: WindowState::default(),
            background: ColorF { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            clear_stencil: None,
            clear_depth: None,
            update_mode: UpdateMode::default(),
            monitor: WindowMonitorTarget::default(),
            mouse_mode: MouseMode::default(),
            update_behaviour: UpdateBehaviour::default(),
            renderer_type: RendererType::default(),
            menu_callbacks: FastHashMap::default(),
            window_icon: None,
            taskbar_icon: None,
            no_redirection_bitmap: false,
        }
    }
}

/// Force a specific renderer.
/// By default, Azul will try to use the hardware renderer and fall
/// back to the software renderer if it can't create an OpenGL 3.2 context.
/// However, in some cases a hardware renderer might create problems
/// or you want to force either a software or hardware renderer.
///
/// If the field `renderer_type` on the `WindowCreateOptions` is not
/// `RendererType::Default`, the `create_window` method will try to create
/// a window with the specific renderer type and **crash** if the renderer is
/// not available for whatever reason.
///
/// If you don't know what any of this means, leave it at `Default`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RendererType {
    Default,
    Hardware,
    Software,
}

impl Default for RendererType {
    fn default() -> Self {
        RendererType::Default
    }
}

/// Should the window be updated only if the mouse cursor is hovering over it?
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum UpdateBehaviour {
    /// Redraw the window only if the mouse cursor is
    /// on top of the window
    UpdateOnHover,
    /// Always update the screen, regardless of the
    /// position of the mouse cursor
    UpdateAlways,
}

impl Default for UpdateBehaviour {
    fn default() -> Self {
        UpdateBehaviour::UpdateOnHover
    }
}

/// In which intervals should the screen be updated
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UpdateMode {
    /// Retained = the screen is only updated when necessary.
    /// Underlying GlImages will be ignored and only updated when the UI changes
    Retained,
    /// Fixed update every X duration.
    FixedUpdate(Duration),
    /// Draw the screen as fast as possible.
    AsFastAsPossible,
}

impl Default for UpdateMode {
    fn default() -> Self {
        UpdateMode::Retained
    }
}

/// Mouse configuration
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MouseMode {
    /// A mouse event is only fired if the cursor has moved at least 1px.
    /// More energy saving, but less precision.
    Normal,
    /// High-precision mouse input (useful for games)
    ///
    /// This disables acceleration and uses the raw values
    /// provided by the mouse.
    DirectInput,
}

impl Default for MouseMode {
    fn default() -> Self {
        MouseMode::Normal
    }
}

/// Error that could happen during window creation
#[derive(Debug)]
pub enum WindowCreateError {
    /// WebGl is not supported by WebRender
    WebGlNotSupported,
    /// Couldn't create the display from the window and the EventsLoop
    DisplayCreateError(DisplayCreationError),
    /// OpenGL version is either too old or invalid
    Gl(IncompatibleOpenGl),
    /// Could not create an OpenGL context
    Context(ContextError),
    /// Could not create a window
    CreateError(CreationError),
    /// Could not swap the front & back buffers
    SwapBuffers(::glium::SwapBuffersError),
    /// IO error
    Io(::std::io::Error),
    /// WebRender creation error (probably OpenGL missing?)
    Renderer/*(RendererError)*/,
}

impl_display! {
    WindowCreateError,
    {
        DisplayCreateError(e) => format!("Could not create the display from the window and the EventsLoop: {}", e),
        Gl(e) => format!("{}", e),
        Context(e) => format!("{}", e),
        CreateError(e) => format!("{}", e),
        SwapBuffers(e) => format!("{}", e),
        Io(e) => format!("{}", e),
        WebGlNotSupported => "WebGl is not supported by WebRender",
        Renderer => "Webrender creation error (probably OpenGL missing?)",
    }
}

impl_from!(SwapBuffersError, WindowCreateError::SwapBuffers);
impl_from!(CreationError, WindowCreateError::CreateError);
impl_from!(IoError, WindowCreateError::Io);
impl_from!(IncompatibleOpenGl, WindowCreateError::Gl);
impl_from!(DisplayCreationError, WindowCreateError::DisplayCreateError);
impl_from!(ContextError, WindowCreateError::Context);

struct Notifier { }

impl RenderNotifier for Notifier {
    fn clone(&self) -> Box<RenderNotifier> {
        Box::new(Notifier { })
    }

    // NOTE: Rendering is single threaded (because that's the nature of OpenGL),
    // so when the Renderer::render() function is finished, then the rendering
    // is finished and done, the rendering is currently blocking (but only takes about 0.. There is no point in implementing RenderNotifier, it only leads to
    // synchronization problems (when handling Event::Awakened).

    fn wake_up(&self) { }
    fn new_frame_ready(&self, _id: DocumentId, _scrolled: bool, _composite_needed: bool, _render_time: Option<u64>) { }
}

/// Iterator over connected monitors (for positioning, etc.)
pub struct MonitorIter {
    inner: AvailableMonitorsIter,
}

impl Iterator for MonitorIter {
    type Item = MonitorId;
    fn next(&mut self) -> Option<MonitorId> {
        self.inner.next()
    }
}

/// Select on which monitor the window should pop up.
#[derive(Clone)]
pub enum WindowMonitorTarget {
    /// Window should appear on the primary monitor
    Primary,
    /// Use `Window::get_available_monitors()` to select the correct monitor
    Custom(MonitorId)
}

impl fmt::Debug for WindowMonitorTarget {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::WindowMonitorTarget::*;
        match *self {
            Primary =>  write!(f, "WindowMonitorTarget::Primary"),
            Custom(_) =>  write!(f, "WindowMonitorTarget::Custom(_)"),
        }
    }
}

impl Default for WindowMonitorTarget {
    fn default() -> Self {
        WindowMonitorTarget::Primary
    }
}

/// Represents one graphical window to be rendered
pub struct Window<T: Layout> {
    /// System that can identify this window
    pub(crate) id: GliumWindowId,
    /// Stores the create_options: necessary because by default, the window is hidden
    /// and only gets shown after the first redraw.
    pub(crate) create_options: WindowCreateOptions<T>,
    /// Current state of the window, stores the keyboard / mouse state,
    /// visibility of the window, etc. of the LAST frame. The user never sets this
    /// field directly, but rather sets the WindowState he wants to have for the NEXT frame,
    /// then azul compares the changes (i.e. if we are currently in fullscreen mode and
    /// the user wants the next screen to be in fullscreen mode, too, simply do nothing), then it
    /// updates this field to reflect the changes.
    ///
    /// This field is initialized from the `WindowCreateOptions`.
    pub(crate) state: WindowState,
    /// The display, i.e. the window
    pub(crate) display: Rc<Display>,
    /// The `WindowInternal` allows us to solve some borrowing issues
    pub(crate) internal: WindowInternal,
    /// States of scrolling animations, updated every frame
    pub(crate) scroll_states: ScrollStates,
    // The background thread that is running for this window.
    // pub(crate) background_thread: Option<JoinHandle<()>>,
    /// The style applied to the current window
    pub(crate) css: Css,
    /// An optional style hot-reloader for the current window, only available with debug_assertions
    /// enabled
    #[cfg(debug_assertions)]
    pub(crate) css_loader: Option<Box<dyn HotReloadHandler>>,
    /// Purely a marker, so that `app.run()` can infer the type of `T: Layout`
    /// of the `WindowCreateOptions`, so that we can write:
    ///
    /// ```rust,ignore
    /// app.run(Window::new(WindowCreateOptions::new(), Css::default()).unwrap());
    /// ```
    ///
    /// instead of having to annotate the type:
    ///
    /// ```rust,ignore
    /// app.run(Window::new(WindowCreateOptions::<MyAppData>::new(), Css::default()).unwrap());
    /// ```
    marker: PhantomData<T>,
}

pub(crate) struct ScrollStates(pub(crate) FastHashMap<ExternalScrollId, ScrollState>);

impl ScrollStates {
    pub fn new() -> ScrollStates {
        ScrollStates(FastHashMap::default())
    }

    /// NOTE: This has to be a getter, because we need to update
    #[must_use]
    pub(crate) fn get_scroll_amount(&mut self, scroll_id: &ExternalScrollId) -> Option<(f32, f32)> {
        let entry = self.0.get_mut(&scroll_id)?;
        Some(entry.get())
    }

    /// Updating the scroll amount does not update the `entry.used_this_frame`,
    /// since that is only relevant when we are actually querying the renderer.
    pub(crate) fn scroll_node(&mut self, scroll_id: &ExternalScrollId, scroll_by_x: f32, scroll_by_y: f32) {
        if let Some(entry) = self.0.get_mut(scroll_id) {
            entry.add(scroll_by_x, scroll_by_y);
        }
    }

    pub(crate) fn ensure_initialized_scroll_state(&mut self, scroll_id: ExternalScrollId, overflow_x: f32, overflow_y: f32) {
        self.0.entry(scroll_id).or_insert_with(|| ScrollState::new(overflow_x, overflow_y));
    }

    /// Removes all scroll states that weren't used in the last frame
    pub(crate) fn remove_unused_scroll_states(&mut self) {
        self.0.retain(|_, state| state.used_this_frame);
    }
}

#[derive(Debug, Copy, Clone)]
pub struct ScrollState {
    /// Amount in pixel that the current node is scrolled
    scroll_amount_x: f32,
    scroll_amount_y: f32,
    overflow_x: f32,
    overflow_y: f32,
    /// Was the scroll amount used in this frame?
    used_this_frame: bool,
}

impl ScrollState {
    fn new(overflow_x: f32, overflow_y: f32) -> Self {
        ScrollState {
            scroll_amount_x: 0.0,
            scroll_amount_y: 0.0,
            overflow_x: overflow_x,
            overflow_y: overflow_y,
            used_this_frame: true,
        }
    }

    pub fn get(&mut self) -> (f32, f32) {
        self.used_this_frame = true;
        (self.scroll_amount_x, self.scroll_amount_y)
    }

    pub fn add(&mut self, x: f32, y: f32) {
        self.scroll_amount_x = self.overflow_x.min(self.scroll_amount_x + x).max(0.0);
        self.scroll_amount_y = self.overflow_y.min(self.scroll_amount_y + y).max(0.0);
    }
}

impl Default for ScrollState {
    fn default() -> Self {
        ScrollState {
            scroll_amount_x: 0.0,
            scroll_amount_y: 0.0,
            overflow_x: 0.0,
            overflow_y: 0.0,
            used_this_frame: true,
        }
    }
}

pub(crate) struct WindowInternal {
    pub(crate) last_scrolled_nodes: ScrolledNodes,
    pub(crate) epoch: Epoch,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) document_id: DocumentId,
}

// TODO: Right now it's not very ergonomic to cache shaders between
// renderers - notify webrender about this.
const WR_SHADER_CACHE: Option<&mut WrShaders> = None;

impl<'a, T: Layout> Window<T> {

    /// Creates a new window
    pub(crate) fn new(
        render_api: &mut RenderApi,
        shared_context: &Context,
        events_loop: &EventsLoop,
        options: WindowCreateOptions<T>,
        mut css: Css,
    ) -> Result<Self, WindowCreateError> {

        // NOTE: It would be OK to use &RenderApi here, but it's better
        // to make sure that the RenderApi is currently not in use by anything else.

        // NOTE: Creating a new EventsLoop for the new window causes a segfault.
        // Report this to the winit developers.
        // let events_loop = EventsLoop::new();

        let mut window = GliumWindowBuilder::new()
            .with_title(options.state.title.clone())
            .with_maximized(options.state.is_maximized)
            .with_decorations(options.state.has_decorations)
            .with_visibility(false)
            .with_transparency(options.state.is_transparent)
            .with_multitouch();

        // TODO: Update winit to have:
        //      .with_always_on_top(options.state.is_always_on_top)
        //
        // winit 0.13 -> winit 0.15

        // TODO: Add all the extensions for X11 / Mac / Windows,
        // like setting the taskbar icon, setting the titlebar icon, etc.

        if let Some(icon) = options.window_icon.clone() {
            window = window.with_window_icon(Some(icon));
        }

        #[cfg(target_os = "windows")] {
            if let Some(icon) = options.taskbar_icon.clone() {
                use glium::glutin::os::windows::WindowBuilderExt;
                window = window.with_taskbar_icon(Some(icon));
            }

            if options.no_redirection_bitmap {
                use glium::glutin::os::windows::WindowBuilderExt;
                window = window.with_no_redirection_bitmap(true);
            }
        }

        if let Some(min_dim) = options.state.size.min_dimensions {
            window = window.with_min_dimensions(min_dim);
        }

        if let Some(max_dim) = options.state.size.max_dimensions {
            window = window.with_max_dimensions(max_dim);
        }

        // Only create a context with VSync and SRGB if the context creation works
        let gl_window = create_gl_window(window, &events_loop, Some(shared_context))?;

        // Hide the window until the first draw (prevents flash on startup)
        gl_window.hide();

        let (hidpi_factor, winit_hidpi_factor) = get_hidpi_factor(&gl_window.window(), &events_loop);
        let mut state = options.state.clone();
        state.size.hidpi_factor = hidpi_factor as f64;
        state.size.winit_hidpi_factor = winit_hidpi_factor as f64;

        if options.state.is_fullscreen {
            gl_window.window().set_fullscreen(Some(gl_window.window().get_current_monitor()));
        }

        if let Some(pos) = options.state.position {
            gl_window.window().set_position(pos);
        }

        if options.state.is_maximized && !options.state.is_fullscreen {
            gl_window.window().set_maximized(true);
        } else if !options.state.is_fullscreen {
            gl_window.window().set_inner_size(options.state.size.get_inner_logical_size());
        }

        // #[cfg(debug_assertions)]
        // let display = Display::with_debug(gl_window, DebugCallbackBehavior::DebugMessageOnError)?;
        // #[cfg(not(debug_assertions))]
        let display = Display::with_debug(gl_window, DebugCallbackBehavior::Ignore)?;

        let framebuffer_size = {
            let inner_logical_size = display.gl_window().get_inner_size().unwrap();
            let (width, height): (u32, u32) = inner_logical_size.to_physical(hidpi_factor as f64).into();
            DeviceIntSize::new(width as i32, height as i32)
        };

        let document_id = render_api.add_document(framebuffer_size, 0);
        let epoch = Epoch(0);

        // TODO: The PipelineId is what gets passed to the OutputImageHandler
        // (the code that coordinates displaying the rendered texture).
        //
        // Each window is a "pipeline", i.e a new web page in webrender terms,
        // however, there is only one global renderer, in order to save on memory,
        // The pipeline ID is important, in order to coordinate the rendered textures
        // back to their windows and window positions.
        let pipeline_id = new_pipeline_id();

        let window_id = display.gl_window().id();

        // let (sender, receiver) = channel();
        // let thread = Builder::new().name(options.title.clone()).spawn(move || Self::handle_event(receiver))?;

        css.sort_by_specificity();

        let window = Window {
            id: window_id,
            create_options: options,
            state: state,
            display: Rc::new(display),
            css,
            #[cfg(debug_assertions)]
            css_loader: None,
            scroll_states: ScrollStates::new(),
            internal: WindowInternal {
                epoch: epoch,
                pipeline_id: pipeline_id,
                document_id: document_id,
                last_scrolled_nodes: ScrolledNodes::default(),
            },
            marker: PhantomData,
        };

        Ok(window)
    }

    /// Creates a new window that will automatically load a new style from a given HotReloadHandler.
    /// Only available with debug_assertions enabled.
    #[cfg(debug_assertions)]
    pub(crate) fn new_hot_reload(
        render_api: &mut RenderApi,
        shared_context: &Context,
        events_loop: &EventsLoop,
        options: WindowCreateOptions<T>,
        css_loader: Box<dyn HotReloadHandler>,
    ) -> Result<Self, WindowCreateError>  {
        let mut window = Window::new(render_api, shared_context, events_loop, options, Css::default())?;
        window.css_loader = Some(css_loader);
        Ok(window)
    }

    /// Returns an iterator over all given monitors
    pub fn get_available_monitors() -> MonitorIter {
        MonitorIter {
            inner: EventsLoop::new().get_available_monitors(),
        }
    }

    /// Returns what monitor the window is currently residing on (to query monitor size, etc.).
    pub fn get_current_monitor(&self) -> MonitorId {
        self.display.gl_window().window().get_current_monitor()
    }

    /// Updates the window state, diff the `self.state` with the `new_state`
    /// and updating the platform window to reflect the changes
    ///
    /// Note: Currently, setting `mouse_state.position`, `window.size` or
    /// `window.position` has no effect on the platform window, since they are very
    /// frequently modified by the user (other properties are always set by the
    /// application developer)
    pub(crate) fn update_from_user_window_state(&mut self, new_state: WindowState) {

        let gl_window = self.display.gl_window();
        let window = gl_window.window();
        let old_state = &mut self.state;

        // Compare the old and new state, field by field

        if old_state.title != new_state.title {
            window.set_title(&new_state.title);
            old_state.title = new_state.title;
        }

        if old_state.mouse_state.mouse_cursor_type != new_state.mouse_state.mouse_cursor_type {
            window.set_cursor(new_state.mouse_state.mouse_cursor_type);
            old_state.mouse_state.mouse_cursor_type = new_state.mouse_state.mouse_cursor_type;
        }

        if old_state.is_maximized != new_state.is_maximized {
            window.set_maximized(new_state.is_maximized);
            old_state.is_maximized = new_state.is_maximized;
        }

        if old_state.is_fullscreen != new_state.is_fullscreen {
            if new_state.is_fullscreen {
                window.set_fullscreen(Some(window.get_current_monitor()));
            } else {
                window.set_fullscreen(None);
            }
            old_state.is_fullscreen = new_state.is_fullscreen;
        }

        if old_state.has_decorations != new_state.has_decorations {
            window.set_decorations(new_state.has_decorations);
            old_state.has_decorations = new_state.has_decorations;
        }

        if old_state.is_visible != new_state.is_visible {
            if new_state.is_visible {
                window.show();
            } else {
                window.hide();
            }
            old_state.is_visible = new_state.is_visible;
        }

        if old_state.size.min_dimensions != new_state.size.min_dimensions {
            window.set_min_dimensions(new_state.size.min_dimensions.and_then(|dim| Some(dim.into())));
            old_state.size.min_dimensions = new_state.size.min_dimensions;
        }

        if old_state.size.max_dimensions != new_state.size.max_dimensions {
            window.set_max_dimensions(new_state.size.max_dimensions.and_then(|dim| Some(dim.into())));
            old_state.size.max_dimensions = new_state.size.max_dimensions;
        }
    }

    #[allow(unused_variables)]
    pub(crate) fn update_from_external_window_state(
        &mut self,
        frame_event_info: &mut FrameEventInfo,
        events_loop: &EventsLoop,
    ) {

        if frame_event_info.new_window_size.is_some() || frame_event_info.new_dpi_factor.is_some() {
            #[cfg(target_os = "linux")] {
                self.state.size.hidpi_factor = linux_get_hidpi_factor(
                    &self.display.gl_window().window().get_current_monitor(),
                    events_loop
                );
            }
        }

        if let Some(new_size) = frame_event_info.new_window_size {
            self.state.size.dimensions = new_size;
        }

        if let Some(dpi) = frame_event_info.new_dpi_factor {
            self.state.size.winit_hidpi_factor = dpi;
            frame_event_info.should_redraw_window = true;
        }
    }

    /// Resets the mouse states `scroll_x` and `scroll_y` to 0
    pub(crate) fn clear_scroll_state(&mut self) {
        self.state.mouse_state.scroll_x = 0.0;
        self.state.mouse_state.scroll_y = 0.0;
    }
}

/// Since the rendering is single-threaded anyways, the renderer is shared across windows.
/// Second, in order to use the font-related functions on the `RenderApi`, we need to
/// store the RenderApi somewhere in the AppResources. However, the `RenderApi` is bound
/// to a window (because OpenGLs function pointer is bound to a window).
///
/// This means that on startup (when calling App::new()), azul creates a fake, hidden display
/// that handles all the rendering, outputs the rendered frames onto a texture, so that the
/// other windows can use said texture. This is also important for animations and multi-window
/// apps later on, but for now the only reason is so that `AppResources::add_font()` has
/// the proper access to the `RenderApi`
pub(crate) struct FakeDisplay {
    /// Main render API that can be used to register and un-register fonts and images
    pub(crate) render_api: RenderApi,
    /// Main renderer, responsible for rendering all windows
    pub(crate) renderer: Option<Renderer>,
    /// Fake / invisible display, only used because OpenGL is tied to a display context
    /// (offscreen rendering is not supported out-of-the-box on many platforms)
    pub(crate) hidden_display: Display,
    /// TODO: Not sure if we even need this, the events loop isn't important
    /// for a window that is never shown
    pub(crate) hidden_events_loop: EventsLoop,
}

impl FakeDisplay {

    /// Creates a new render + a new display
    pub(crate) fn new(renderer_type: RendererType, background: Option<ColorU>) -> Result<Self, WindowCreateError> {

        let events_loop = EventsLoop::new();
        let window = GliumWindowBuilder::new().with_dimensions(LogicalSize::new(10.0, 10.0)).with_visibility(false);
        let gl_window = create_gl_window(window, &events_loop, None)?;
        let (dpi_factor, _) = get_hidpi_factor(&gl_window.window(), &events_loop);
        gl_window.hide();

        let display = Display::with_debug(gl_window, DebugCallbackBehavior::Ignore)?;
        let gl = get_gl_context(&display)?;

        // Note: Notifier is fairly useless, since rendering is completely single-threaded, see comments on RenderNotifier impl
        let notifier = Box::new(Notifier { });
        let (mut renderer, render_api) = create_renderer(gl.clone(), notifier, renderer_type, dpi_factor, background)?;

        renderer.set_external_image_handler(Box::new(Compositor::default()));

        Ok(Self {
            render_api,
            renderer: Some(renderer),
            hidden_display: display,
            hidden_events_loop: events_loop,
        })
    }
}

impl Drop for FakeDisplay {
    fn drop(&mut self) {
        let renderer = self.renderer.take().unwrap();
        renderer.deinit();
    }
}

/// Returns the actual hidpi factor and the winit DPI factor for the current window
fn get_hidpi_factor(window: &GliumWindow, events_loop: &EventsLoop) -> (f64, f64) {
    let monitor = window.get_current_monitor();
    let winit_hidpi_factor = monitor.get_hidpi_factor();

    #[cfg(target_os = "linux")] {
        (linux_get_hidpi_factor(&monitor, &events_loop), winit_hidpi_factor)
    }
    #[cfg(not(target_os = "linux"))] {
        (winit_hidpi_factor, winit_hidpi_factor)
    }
}


fn create_gl_window(window: GliumWindowBuilder, events_loop: &EventsLoop, shared_context: Option<&Context>)
-> Result<CombinedContext, WindowCreateError>
{
    // The shared_context is reversed: If the shared_context is None, then this window is the root window,
    // so the window should be created with new_shared (so the context can be shared to all other windows).
    //
    // If the shared_context is Some() then the window is not a root window, so it should share the existing
    // context, but not re-share it (so, create it normally via ::new() instead of ::new_shared()).

    CombinedContext::new(window.clone(), create_context_builder(true, true, shared_context),  &events_loop).or_else(|_|
    CombinedContext::new(window.clone(), create_context_builder(true, false, shared_context), &events_loop)).or_else(|_|
    CombinedContext::new(window.clone(), create_context_builder(false, true, shared_context), &events_loop)).or_else(|_|
    CombinedContext::new(window.clone(), create_context_builder(false, false,shared_context), &events_loop))
    .map_err(|e| WindowCreateError::CreateError(e))
}

/// ContextBuilder is sadly not clone-able, which is why it has to be re-created
/// every time you want to create a new context. The goals is to not crash on
/// platforms that don't have VSync or SRGB (which are OpenGL extensions) installed.
///
/// Secondly, in order to support multi-window apps, all windows need to share
/// the same OpenGL context - i.e. `builder.with_shared_lists(some_gl_window.context());`
///
/// `allow_sharing_context` should only be true for the root window - so that
/// we can be sure the shared context can't be re-shared by the created window. Only
/// the root window (via `FakeDisplay`) is allowed to manage the OpenGL context.
fn create_context_builder<'a>(
    vsync: bool,
    srgb: bool,
    shared_context: Option<&'a Context>,
) -> ContextBuilder<'a> {

    // See #33 - specifying a specific OpenGL version
    // makes winit crash on older Intel drivers, which is why we
    // don't specify a specific OpenGL version here
    let mut builder = ContextBuilder::new();

    if let Some(shared_context) = shared_context {
        builder = builder.with_shared_lists(shared_context);
    }

    // #[cfg(debug_assertions)] {
    //     builder = builder.with_gl_debug_flag(true);
    // }

    // #[cfg(not(debug_assertions))] {
        builder = builder.with_gl_debug_flag(false);
    // }

    if vsync {
        builder = builder.with_vsync(true);
    }

    if srgb {
        builder = builder.with_srgb(true);
    }

    builder
}

// This exists because RendererOptions isn't Clone-able
fn get_renderer_opts(native: bool, device_pixel_ratio: f32, clear_color: Option<ColorF>) -> RendererOptions {
    use webrender::ProgramCache;
    use css::webrender_translate::wr_translate_color_f;

    // pre-caching shaders means to compile all shaders on startup
    // this can take significant time and should be only used for testing the shaders
    const PRECACHE_SHADER_FLAGS: ShaderPrecacheFlags = ShaderPrecacheFlags::EMPTY;

    RendererOptions {
        resource_override_path: None,
        precache_flags: PRECACHE_SHADER_FLAGS,
        device_pixel_ratio: device_pixel_ratio,
        enable_subpixel_aa: true,
        enable_aa: true,
        clear_color: clear_color.map(wr_translate_color_f),
        cached_programs: Some(ProgramCache::new(None)),
        renderer_kind: if native {
            RendererKind::Native
        } else {
            RendererKind::OSMesa
        },
        .. RendererOptions::default()
    }
}

fn create_renderer(
    gl: Rc<Gl>,
    notifier: Box<Notifier>,
    renderer_type: RendererType,
    device_pixel_ratio: f64,
    background_color: Option<ColorU>,
) -> Result<(Renderer, RenderApi), WindowCreateError> {

    use self::RendererType::*;

    let opts_native = get_renderer_opts(true, device_pixel_ratio as f32, background_color.map(|color_u| color_u.into()));
    let opts_osmesa = get_renderer_opts(false, device_pixel_ratio as f32, background_color.map(|color_u| color_u.into()));

    let (renderer, sender) = match renderer_type {
        Hardware => {
            // force hardware renderer
            Renderer::new(gl, notifier, opts_native, WR_SHADER_CACHE).unwrap()
        },
        Software => {
            // force software renderer
            Renderer::new(gl, notifier, opts_osmesa, WR_SHADER_CACHE).unwrap()
        },
        Default => {
            // try hardware first, fall back to software
            match Renderer::new(gl.clone(), notifier.clone(), opts_native, WR_SHADER_CACHE) {
                Ok(r) => r,
                Err(_) => Renderer::new(gl, notifier, opts_osmesa, WR_SHADER_CACHE).unwrap()
            }
        }
    };

    let api = sender.create_api();

    Ok((renderer, api))
}

pub(crate) fn get_gl_context(display: &Display) -> Result<Rc<Gl>, WindowCreateError> {
    match display.gl_window().get_api() {
        glutin::Api::OpenGl => Ok(unsafe {
            gl::GlFns::load_with(|symbol| display.gl_window().get_proc_address(symbol) as *const _)
        }),
        glutin::Api::OpenGlEs => Ok(unsafe {
            gl::GlesFns::load_with(|symbol| display.gl_window().get_proc_address(symbol) as *const _)
        }),
        glutin::Api::WebGl => Err(WindowCreateError::WebGlNotSupported),
    }
}

// Only necessary for GlTextures and IFrames that need the
// width and height of their container to calculate their content
#[derive(Debug, Copy, Clone)]
pub struct HidpiAdjustedBounds {
    logical_size: LogicalSize,
    hidpi_factor: f64,
    winit_hidpi_factor: f64,
}

impl HidpiAdjustedBounds {
    pub fn from_bounds(bounds: LayoutRect, hidpi_factor: f64, winit_hidpi_factor: f64) -> Self {
        let logical_size = LogicalSize::new(bounds.size.width as f64, bounds.size.height as f64);
        Self {
            logical_size,
            hidpi_factor,
            winit_hidpi_factor,
        }
    }

    pub fn get_physical_size(&self) -> PhysicalSize {
        self.get_logical_size().to_physical(self.winit_hidpi_factor)
    }

    pub fn get_logical_size(&self) -> LogicalSize {
        // NOTE: hidpi factor, not winit_hidpi_factor!
        LogicalSize::new(self.logical_size.width * self.hidpi_factor, self.logical_size.height * self.hidpi_factor)
    }

    pub fn get_hidpi_factor(&self) -> f64 {
        self.hidpi_factor
    }
}

#[cfg(target_os = "linux")]
fn get_xft_dpi() -> Option<f64>{
    // TODO!
    /*
    #include <X11/Xlib.h>
    #include <X11/Xatom.h>
    #include <X11/Xresource.h>

    double _glfwPlatformGetMonitorDPI(_GLFWmonitor* monitor)
    {
        char *resourceString = XResourceManagerString(_glfw.x11.display);
        XrmDatabase db;
        XrmValue value;
        char *type = NULL;
        double dpi = 0.0;

        XrmInitialize(); /* Need to initialize the DB before calling Xrm* functions */

        db = XrmGetStringDatabase(resourceString);

        if (resourceString) {
            printf("Entire DB:\n%s\n", resourceString);
            if (XrmGetResource(db, "Xft.dpi", "String", &type, &value) == True) {
                if (value.addr) {
                    dpi = atof(value.addr);
                }
            }
        }

        printf("DPI: %f\n", dpi);
        return dpi;
    }
    */
    None
}

/// Return the DPI on X11 systems
#[cfg(target_os = "linux")]
fn linux_get_hidpi_factor(monitor: &MonitorId, events_loop: &EventsLoop) -> f64 {

    use std::env;
    use std::process::Command;
    use glium::glutin::os::unix::EventsLoopExt;

    let winit_dpi = monitor.get_hidpi_factor();
    let winit_hidpi_factor = env::var("WINIT_HIDPI_FACTOR").ok().and_then(|hidpi_factor| hidpi_factor.parse::<f64>().ok());
    let qt_font_dpi = env::var("QT_FONT_DPI").ok().and_then(|font_dpi| font_dpi.parse::<f64>().ok());

    // Execute "gsettings get org.gnome.desktop.interface text-scaling-factor" and parse the output
    let gsettings_dpi_factor =
        Command::new("gsettings")
            .arg("get")
            .arg("org.gnome.desktop.interface")
            .arg("text-scaling-factor")
            .output().ok()
            .map(|output| output.stdout)
            .and_then(|stdout_bytes| String::from_utf8(stdout_bytes).ok())
            .map(|stdout_string| stdout_string.lines().collect::<String>())
            .and_then(|gsettings_output| gsettings_output.parse::<f64>().ok());

    // Wayland: Ignore Xft.dpi
    let xft_dpi = if events_loop.is_x11() { get_xft_dpi() } else { None };

    let options = [winit_hidpi_factor, qt_font_dpi, gsettings_dpi_factor, xft_dpi];
    options.into_iter().filter_map(|x| *x).next().unwrap_or(winit_dpi)
}