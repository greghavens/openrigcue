//! The sole C ABI boundary. GTK objects and callbacks stay on their owner
//! thread. Libraries are pinned because GType retains their function pointers.
#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_ulong, c_void, CStr, CString};
use std::mem::ManuallyDrop;
use std::ptr;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    native_window_plan, CloseAction, ConfigReader, DesktopError, Geometry, Monitor, SharedGeometry, WindowPlan,
};

type Object = *mut c_void;
type DestroyNotify = Option<unsafe extern "C" fn(Object, Object)>;

#[cfg(test)]
static INITIALIZATION_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

macro_rules! api {
    ($name:ident, [$($library:literal),+], { $($field:ident: $ty:ty),+ $(,)? }) => {
        struct $name {
            $($field: $ty,)+
            _libraries: ManuallyDrop<Vec<libloading::Library>>,
        }
        impl $name {
            fn load() -> Result<Self, DesktopError> {
                let libraries = vec![$(open_library($library)?),+];
                $(let $field = libraries.iter().find_map(|library| {
                    // SAFETY: the declarations below match the published C ABI.
                    unsafe { library.get::<$ty>(concat!(stringify!($field), "\0").as_bytes()).ok().map(|s| *s) }
                }).ok_or_else(|| DesktopError::Library(stringify!($field).into()))?;)+
                Ok(Self { $($field,)+ _libraries: ManuallyDrop::new(libraries) })
            }
        }
    }
}

fn open_library(name: &str) -> Result<libloading::Library, DesktopError> {
    // SAFETY: only the named system GTK/WebKit libraries are loaded. Their ABI
    // symbols are resolved before any object is created.
    unsafe { libloading::Library::new(name) }.map_err(|e| DesktopError::Library(e.to_string()))
}

#[repr(C)]
#[derive(Default)]
struct Rectangle {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
}

api!(Gtk, ["libgtk-3.so.0", "libgdk-3.so.0", "libgobject-2.0.so.0", "libglib-2.0.so.0", "libgdk_pixbuf-2.0.so.0"], {
    gtk_init_check: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) -> c_int,
    g_set_prgname: unsafe extern "C" fn(*const c_char),
    gtk_events_pending: unsafe extern "C" fn() -> c_int,
    gtk_main_iteration_do: unsafe extern "C" fn(c_int) -> c_int,
    gtk_window_new: unsafe extern "C" fn(c_int) -> Object,
    gtk_window_set_title: unsafe extern "C" fn(Object, *const c_char),
    gtk_window_set_default_size: unsafe extern "C" fn(Object, c_int, c_int),
    gtk_window_set_decorated: unsafe extern "C" fn(Object, c_int),
    gtk_window_set_resizable: unsafe extern "C" fn(Object, c_int),
    gtk_window_set_position: unsafe extern "C" fn(Object, c_int),
    gtk_window_move: unsafe extern "C" fn(Object, c_int, c_int),
    gtk_window_get_position: unsafe extern "C" fn(Object, *mut c_int, *mut c_int),
    gtk_window_iconify: unsafe extern "C" fn(Object),
    gtk_window_deiconify: unsafe extern "C" fn(Object),
    gtk_window_present: unsafe extern "C" fn(Object),
    gtk_window_set_icon: unsafe extern "C" fn(Object, Object),
    gtk_widget_set_size_request: unsafe extern "C" fn(Object, c_int, c_int),
    gtk_widget_get_scale_factor: unsafe extern "C" fn(Object) -> c_int,
    gtk_widget_get_style_context: unsafe extern "C" fn(Object) -> Object,
    gtk_css_provider_new: unsafe extern "C" fn() -> Object,
    gtk_css_provider_load_from_data: unsafe extern "C" fn(Object,*const c_char,isize,*mut Object) -> c_int,
    gtk_style_context_add_provider: unsafe extern "C" fn(Object,Object,u32),
    gtk_widget_realize: unsafe extern "C" fn(Object),
    gtk_widget_show_all: unsafe extern "C" fn(Object),
    gtk_widget_destroy: unsafe extern "C" fn(Object),
    gtk_container_add: unsafe extern "C" fn(Object, Object),
    gtk_image_new_from_pixbuf: unsafe extern "C" fn(Object) -> Object,
    gtk_menu_new: unsafe extern "C" fn() -> Object,
    gtk_menu_item_new_with_label: unsafe extern "C" fn(*const c_char) -> Object,
    gtk_menu_shell_append: unsafe extern "C" fn(Object, Object),
    gtk_menu_popup: unsafe extern "C" fn(Object, Object, Object, Option<unsafe extern "C" fn(Object,*mut c_int,*mut c_int,*mut c_int,Object)>, Object, u32, u32),
    gtk_window_set_keep_above: unsafe extern "C" fn(Object, c_int),
    gtk_window_resize: unsafe extern "C" fn(Object, c_int, c_int),
    gtk_container_set_border_width: unsafe extern "C" fn(Object, u32),
    gtk_widget_set_visible: unsafe extern "C" fn(Object, c_int),
    gtk_widget_get_visible: unsafe extern "C" fn(Object) -> c_int,
    gtk_widget_set_no_show_all: unsafe extern "C" fn(Object, c_int),
    gtk_box_new: unsafe extern "C" fn(c_int, c_int) -> Object,
    gtk_box_pack_start: unsafe extern "C" fn(Object, Object, c_int, c_int, u32),
    gtk_box_pack_end: unsafe extern "C" fn(Object, Object, c_int, c_int, u32),
    gtk_label_new: unsafe extern "C" fn(*const c_char) -> Object,
    gtk_label_set_line_wrap: unsafe extern "C" fn(Object, c_int),
    gtk_label_set_max_width_chars: unsafe extern "C" fn(Object, c_int),
    gtk_label_set_xalign: unsafe extern "C" fn(Object, f32),
    gtk_button_new_with_label: unsafe extern "C" fn(*const c_char) -> Object,
    gtk_button_set_label: unsafe extern "C" fn(Object, *const c_char),
    gtk_scrolled_window_new: unsafe extern "C" fn(Object, Object) -> Object,
    gtk_text_view_new: unsafe extern "C" fn() -> Object,
    gtk_text_view_get_buffer: unsafe extern "C" fn(Object) -> Object,
    gtk_text_view_set_editable: unsafe extern "C" fn(Object, c_int),
    gtk_text_view_set_wrap_mode: unsafe extern "C" fn(Object, c_int),
    gtk_text_buffer_set_text: unsafe extern "C" fn(Object, *const c_char, c_int),
    gdk_atom_intern: unsafe extern "C" fn(*const c_char, c_int) -> Object,
    gtk_clipboard_get: unsafe extern "C" fn(Object) -> Object,
    gtk_clipboard_set_text: unsafe extern "C" fn(Object, *const c_char, c_int),
    gtk_clipboard_set_can_store: unsafe extern "C" fn(Object, Object, c_int),
    gtk_clipboard_store: unsafe extern "C" fn(Object),
    gdk_screen_get_default: unsafe extern "C" fn() -> Object,
    gdk_screen_get_width: unsafe extern "C" fn(Object) -> c_int,
    gdk_screen_get_height: unsafe extern "C" fn(Object) -> c_int,
    gdk_screen_get_resolution: unsafe extern "C" fn(Object) -> f64,
    gdk_display_get_default: unsafe extern "C" fn() -> Object,
    gdk_display_get_n_monitors: unsafe extern "C" fn(Object) -> c_int,
    gdk_display_get_monitor: unsafe extern "C" fn(Object, c_int) -> Object,
    gdk_monitor_get_geometry: unsafe extern "C" fn(Object, *mut Rectangle),
    g_type_name_from_instance: unsafe extern "C" fn(Object) -> *const c_char,
    g_signal_connect_data: unsafe extern "C" fn(Object, *const c_char, Option<unsafe extern "C" fn()>, Object, DestroyNotify, c_int) -> c_ulong,
    g_signal_handler_disconnect: unsafe extern "C" fn(Object, c_ulong),
    g_object_unref: unsafe extern "C" fn(Object),
    g_object_ref_sink: unsafe extern "C" fn(Object) -> Object,
    gdk_pixbuf_loader_new: unsafe extern "C" fn() -> Object,
    gdk_pixbuf_loader_write: unsafe extern "C" fn(Object, *const u8, usize, *mut Object) -> c_int,
    gdk_pixbuf_loader_close: unsafe extern "C" fn(Object, *mut Object) -> c_int,
    gdk_pixbuf_loader_get_pixbuf: unsafe extern "C" fn(Object) -> Object
});

api!(WebKit, ["libwebkit2gtk-4.1.so.0"], {
    webkit_web_context_new_ephemeral: unsafe extern "C" fn() -> Object,
    webkit_web_view_new_with_context: unsafe extern "C" fn(Object) -> Object,
    webkit_web_view_load_uri: unsafe extern "C" fn(Object, *const c_char),
    webkit_navigation_action_get_request: unsafe extern "C" fn(Object) -> Object,
    webkit_uri_request_get_uri: unsafe extern "C" fn(Object) -> *const c_char,
    webkit_download_cancel: unsafe extern "C" fn(Object)
});

struct State {
    gtk: Rc<Gtk>,
    webkit: RefCell<Option<Rc<WebKit>>>,
    window: Cell<Object>,
    splash: Cell<Object>,
    menu: Cell<Object>,
    menu_position: Cell<(i32, i32)>,
    menu_action: RefCell<Option<crate::tray::Action>>,
    quit: Cell<bool>,
    shutting_down: Cell<bool>,
    connect_closed: Cell<bool>,
    config: RefCell<Option<ConfigReader>>,
    geometry: SharedGeometry,
    dpi: Cell<f64>,
}

/// Owns the GTK thread, splash and native webview. Rc makes this !Send/!Sync.
pub struct Desktop {
    state: Rc<State>,
    screen_signal: Option<(Object, c_ulong)>,
}

impl Desktop {
    pub fn new() -> Result<Self, DesktopError> {
        Self::with_geometry(Arc::new(Mutex::new(None)))
    }

    pub fn with_geometry(geometry: SharedGeometry) -> Result<Self, DesktopError> {
        static OWNER: OnceLock<std::thread::ThreadId> = OnceLock::new();
        static INITIALIZED: OnceLock<bool> = OnceLock::new();
        let owner = OWNER.get_or_init(|| std::thread::current().id());
        if *owner != std::thread::current().id() {
            return Err(DesktopError::Thread);
        }
        if INITIALIZED.get() == Some(&false) {
            return Err(DesktopError::Display);
        }
        let gtk = Rc::new(Gtk::load()?);
        // SAFETY: initialize once on the owner thread, with no argument rewriting.
        // A failed init forbids subsequent GTK calls, including later retries.
        if !*INITIALIZED.get_or_init(|| {
            #[cfg(test)]
            INITIALIZATION_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // The program name becomes the window's Wayland app id and X11 WM_CLASS, which the desktop
            // matches to `openrigcue.desktop` for the window and task bar icon.
            unsafe {
                (gtk.g_set_prgname)(c"openrigcue".as_ptr());
                (gtk.gtk_init_check)(ptr::null_mut(), ptr::null_mut()) != 0
            }
        }) {
            return Err(DesktopError::Display);
        }
        let state = Rc::new(State {
            gtk,
            webkit: RefCell::new(None),
            window: Cell::new(ptr::null_mut()),
            splash: Cell::new(ptr::null_mut()),
            menu: Cell::new(ptr::null_mut()),
            menu_position: Cell::new((0, 0)),
            menu_action: RefCell::new(None),
            quit: Cell::new(false),
            shutting_down: Cell::new(false),
            connect_closed: Cell::new(false),
            config: RefCell::new(None),
            geometry,
            dpi: Cell::new(0.0),
        });
        // SAFETY: GTK has initialized; GdkScreen is borrowed for the process lifetime.
        let screen = unsafe { (state.gtk.gdk_screen_get_default)() };
        let screen_signal = if screen.is_null() {
            None
        } else {
            Some((screen, connect(&state, screen, c"monitors-changed", monitors_changed as *const ())))
        };
        Ok(Self { state, screen_signal })
    }

    /// Decode/create before reading ShowSplashScreen.
    pub fn splash(
        &mut self,
        png: &[u8],
        show: impl FnOnce() -> Result<bool, DesktopError>,
    ) -> Result<(), DesktopError> {
        let gtk = &self.state.gtk;
        // SAFETY: all objects below are on the initialized GTK thread. The loader
        // owns the pixbuf until the image/window have taken their own references.
        unsafe {
            let loader = (gtk.gdk_pixbuf_loader_new)();
            if loader.is_null() {
                return Err(operation("creating splash image loader"));
            }
            let loaded = (gtk.gdk_pixbuf_loader_write)(loader, png.as_ptr(), png.len(), ptr::null_mut()) != 0;
            let closed = (gtk.gdk_pixbuf_loader_close)(loader, ptr::null_mut()) != 0;
            let pixbuf = (gtk.gdk_pixbuf_loader_get_pixbuf)(loader);
            if !loaded || !closed || pixbuf.is_null() {
                (gtk.g_object_unref)(loader);
                return Err(operation("decoding splash image"));
            }
            let window = (gtk.gtk_window_new)(0);
            if window.is_null() {
                (gtk.g_object_unref)(loader);
                return Err(operation("creating splash window"));
            }
            self.state.splash.set(window);
            let css = (gtk.gtk_css_provider_new)();
            if !css.is_null() {
                let style = c"* { background-color: #2b2b2b; }";
                (gtk.gtk_css_provider_load_from_data)(css, style.as_ptr(), -1, ptr::null_mut());
                (gtk.gtk_style_context_add_provider)((gtk.gtk_widget_get_style_context)(window), css, 600);
                (gtk.g_object_unref)(css);
            }
            (gtk.gtk_window_set_title)(window, c"OpenRigCue".as_ptr());
            (gtk.gtk_window_set_decorated)(window, 0);
            (gtk.gtk_window_set_resizable)(window, 0);
            (gtk.gtk_window_set_position)(window, 1); // GTK_WIN_POS_CENTER
            let image = (gtk.gtk_image_new_from_pixbuf)(pixbuf);
            (gtk.g_object_unref)(loader);
            if image.is_null() {
                return Err(operation("creating splash image"));
            }
            (gtk.gtk_container_add)(window, image);
            let visible = show()?;
            (gtk.gtk_widget_realize)(window);
            let screen = (gtk.gdk_screen_get_default)();
            let dpi = if screen.is_null() { 96.0 } else { (gtk.gdk_screen_get_resolution)(screen) };
            self.state
                .dpi
                .set(if dpi > 0.0 { dpi } else { 96.0 } * f64::from((gtk.gtk_widget_get_scale_factor)(window)));
            if visible {
                (gtk.gtk_widget_show_all)(window);
            }
        }
        self.dispatch_pending();
        Ok(())
    }

    pub fn shared_geometry(&self) -> SharedGeometry {
        self.state.geometry.clone()
    }

    /// Queried on the GTK thread at Save, not just the last configure event.
    pub fn capture_geometry(&self) -> Geometry {
        let geometry = geometry(&self.state);
        *self.state.geometry.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(geometry.clone());
        geometry
    }

    pub fn set_config_reader(&mut self, read: ConfigReader) {
        *self.state.config.borrow_mut() = Some(read);
    }

    /// Whether the main window currently exists (it does not before it is first opened or after it was closed).
    pub fn has_window(&self) -> bool {
        !self.state.window.get().is_null()
    }

    /// Second-instance restore/activate of an existing window; unlike tray Open it also requests activation.
    /// It creates no window: the launcher opens a closed one through [`Desktop::open`].
    pub fn activate_existing(&self) {
        let window = self.state.window.get();
        if !window.is_null() {
            unsafe {
                (self.state.gtk.gtk_window_deiconify)(window);
                (self.state.gtk.gtk_window_present)(window);
            }
        }
    }

    /// A replacement launcher closes only an existing main window. Normal
    /// destroyed/closed policy decides whether the running service also exits.
    pub fn close_existing(&self) {
        let window = self.state.window.get();
        if !window.is_null() {
            unsafe { (self.state.gtk.gtk_widget_destroy)(window) };
        }
    }

    pub fn show_menu(&mut self, x: i32, y: i32, action: crate::tray::Action) -> Result<(), DesktopError> {
        self.state.menu_position.set((x, y));
        *self.state.menu_action.borrow_mut() = Some(action);
        let gtk = &self.state.gtk;
        let mut menu = self.state.menu.get();
        unsafe {
            if menu.is_null() {
                menu = (gtk.gtk_menu_new)();
                if menu.is_null() {
                    return Err(operation("creating tray menu"));
                }
                (gtk.g_object_ref_sink)(menu);
                self.state.menu.set(menu);
                for (label, callback) in [(c"Open", menu_open as *const ()), (c"Quit", menu_quit as *const ())] {
                    let item = (gtk.gtk_menu_item_new_with_label)(label.as_ptr());
                    if item.is_null() {
                        return Err(operation("creating tray menu item"));
                    }
                    (gtk.gtk_menu_shell_append)(menu, item);
                    connect(&self.state, item, c"activate", callback);
                }
                (gtk.gtk_widget_show_all)(menu);
            }
            // Position is a hint on Wayland; the compositor owns placement.
            (gtk.gtk_menu_popup)(
                menu,
                ptr::null_mut(),
                ptr::null_mut(),
                Some(menu_position),
                Rc::as_ptr(&self.state) as Object,
                0,
                0,
            );
        }
        Ok(())
    }

    /// Tray Open restores an existing window; after it was closed, it creates one.
    pub fn open(&mut self, url: &str) -> Result<(), DesktopError> {
        let window = self.state.window.get();
        if !window.is_null() {
            // Tray restore does not read the geometry/config again.
            unsafe {
                (self.state.gtk.gtk_window_deiconify)(window);
            }
            return Ok(());
        }
        let config = self.state.config.borrow().as_ref().ok_or_else(|| operation("missing desktop configuration"))?();
        let gtk = &self.state.gtk;
        let monitors = monitors(&self.state);
        // SAFETY: borrowed screen, initialized GTK thread.
        let (width, height) = unsafe {
            let screen = (gtk.gdk_screen_get_default)();
            if screen.is_null() {
                return Err(operation("missing desktop screen"));
            }
            ((gtk.gdk_screen_get_width)(screen), (gtk.gdk_screen_get_height)(screen))
        };
        let plan = native_window_plan(&config, &monitors, self.state.dpi.get(), width, height)?;
        let dimensions = [plan.width, plan.height, plan.x, plan.y].map(|n| {
            n.integer()
                .and_then(|integer| c_int::try_from(integer).ok())
                .ok_or_else(|| operation("window integer exceeds the native desktop range"))
        });
        let [width, height, x, y] = dimensions;
        let (width, height, x, y) = (width?, height?, x?, y?);
        let uri = CString::new(url).map_err(|_| operation("window URI contains NUL"))?;
        let webkit = Rc::new(WebKit::load()?);
        *self.state.webkit.borrow_mut() = Some(webkit.clone());
        // SAFETY: GObjects are used on their GTK owner thread; context retains no
        // persistent client storage (an ephemeral web context).
        unsafe {
            let context = (webkit.webkit_web_context_new_ephemeral)();
            if context.is_null() {
                return Err(operation("creating private web context"));
            }
            let view = (webkit.webkit_web_view_new_with_context)(context);
            if view.is_null() {
                (gtk.g_object_unref)(context);
                return Err(operation("creating webview"));
            }
            connect(&self.state, context, c"download-started", download_started as *const ());
            (gtk.g_object_unref)(context);
            let window = (gtk.gtk_window_new)(0);
            if window.is_null() {
                // Sink the unparented floating widget before releasing it.
                (gtk.g_object_ref_sink)(view);
                (gtk.g_object_unref)(view);
                return Err(operation("creating application window"));
            }
            self.state.window.set(window);
            self.state.connect_closed.set(plan.connect_closed);
            (gtk.gtk_window_set_title)(window, c"OpenRigCue".as_ptr());
            (gtk.gtk_window_set_default_size)(window, width, height);
            (gtk.gtk_widget_set_size_request)(window, plan.min_width, plan.min_height);
            (gtk.gtk_window_move)(window, x, y);
            (gtk.gtk_container_add)(window, view);
            connect(&self.state, window, c"destroy", destroyed as *const ());
            connect(&self.state, window, c"configure-event", configured as *const ());
            connect(&self.state, view, c"load-changed", load_changed as *const ());
            connect(&self.state, view, c"create", external_window as *const ());
            set_icon(gtk, window);
            (webkit.webkit_web_view_load_uri)(view, uri.as_ptr());
            (gtk.gtk_widget_show_all)(window);
        }
        self.dismiss_splash();
        cache_geometry(&self.state);
        Ok(())
    }

    /// A message window that stays above other windows and blocks until it is closed: `message`, an OK button
    /// and a "More Details" button that reveals `hint`, the read-only `details` text and "Copy Details".
    /// The splash, if any, is removed first.
    pub fn details_dialog(&self, title: &str, message: &str, hint: &str, details: &str) -> Result<(), DesktopError> {
        self.dismiss_splash();
        let notice = Notice::build(&self.state.gtk, title, message, hint, details)?;
        while !notice.done.get() {
            // SAFETY: the initialized GTK thread; the handlers reached from here only use `notice`.
            unsafe {
                (self.state.gtk.gtk_main_iteration_do)(1);
            }
        }
        Ok(())
    }

    /// Nonblocking GLib dispatch lets the launcher poll its existing Stop/Restart
    /// channel without a second GTK thread or a second service loop.
    pub fn dispatch_pending(&self) -> bool {
        unsafe {
            for _ in 0..256 {
                if (self.state.gtk.gtk_events_pending)() == 0 {
                    break;
                }
                (self.state.gtk.gtk_main_iteration_do)(0);
            }
        }
        self.state.quit.get()
    }

    fn dismiss_splash(&self) {
        let splash = self.state.splash.replace(ptr::null_mut());
        if !splash.is_null() {
            unsafe {
                (self.state.gtk.gtk_widget_destroy)(splash);
            }
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        self.state.shutting_down.set(true);
        if let Some((screen, id)) = self.screen_signal.take() {
            unsafe {
                (self.state.gtk.g_signal_handler_disconnect)(screen, id);
            }
        }
        self.dismiss_splash();
        let menu = self.state.menu.replace(ptr::null_mut());
        if !menu.is_null() {
            unsafe {
                (self.state.gtk.gtk_widget_destroy)(menu);
                (self.state.gtk.g_object_unref)(menu);
            }
        }
        let window = self.state.window.replace(ptr::null_mut());
        if !window.is_null() {
            unsafe {
                (self.state.gtk.gtk_widget_destroy)(window);
            }
        }
    }
}

fn operation(message: &str) -> DesktopError {
    DesktopError::Operation(message.into())
}

/// The widgets of a details dialog. Its signal handlers receive a pointer to this value, which outlives the
/// window: dropping it destroys the window, and that disconnects the handlers.
struct Notice {
    gtk: Rc<Gtk>,
    window: Cell<Object>,
    /// The hint and the details text, hidden until "More Details".
    details: Object,
    copy: Object,
    toggle: Object,
    text: CString,
    done: Cell<bool>,
}

impl Notice {
    fn build(gtk: &Rc<Gtk>, title: &str, message: &str, hint: &str, details: &str) -> Result<Box<Self>, DesktopError> {
        let c_text = |text: &str| CString::new(text).map_err(|_| operation("message text contains NUL"));
        let (title, message, hint, text) = (c_text(title)?, c_text(message)?, c_text(hint)?, c_text(details)?);
        // SAFETY: all objects are created and used on the initialized GTK thread. The window owns its children;
        // the handlers' data pointer is the boxed value returned below, whose drop destroys the window.
        unsafe {
            let window = (gtk.gtk_window_new)(0);
            if window.is_null() {
                return Err(operation("creating message window"));
            }
            let column = (gtk.gtk_box_new)(1, 0);
            let summary = (gtk.gtk_label_new)(message.as_ptr());
            let pane = (gtk.gtk_box_new)(1, 10);
            let advice = (gtk.gtk_label_new)(hint.as_ptr());
            let scroll = (gtk.gtk_scrolled_window_new)(ptr::null_mut(), ptr::null_mut());
            let view = (gtk.gtk_text_view_new)();
            let bar = (gtk.gtk_box_new)(0, 8);
            let ok = (gtk.gtk_button_new_with_label)(c"OK".as_ptr());
            let copy = (gtk.gtk_button_new_with_label)(c"Copy Details".as_ptr());
            let toggle = (gtk.gtk_button_new_with_label)(c"More Details".as_ptr());
            let widgets = [column, summary, pane, advice, scroll, view, bar, ok, copy, toggle];
            if widgets.iter().any(|widget| widget.is_null()) {
                for widget in widgets.into_iter().filter(|widget| !widget.is_null()) {
                    // Sink the unparented floating widget before releasing it.
                    (gtk.g_object_ref_sink)(widget);
                    (gtk.g_object_unref)(widget);
                }
                (gtk.gtk_widget_destroy)(window);
                return Err(operation("creating message window contents"));
            }
            (gtk.gtk_window_set_title)(window, title.as_ptr());
            (gtk.gtk_window_set_keep_above)(window, 1);
            (gtk.gtk_window_set_position)(window, 1); // GTK_WIN_POS_CENTER
            (gtk.gtk_window_set_resizable)(window, 0);
            (gtk.gtk_container_set_border_width)(window, 18);
            (gtk.gtk_container_add)(window, column);
            for label in [summary, advice] {
                (gtk.gtk_label_set_line_wrap)(label, 1);
                (gtk.gtk_label_set_max_width_chars)(label, 80);
                (gtk.gtk_label_set_xalign)(label, 0.0);
            }
            (gtk.gtk_box_pack_start)(column, summary, 0, 1, 0);
            (gtk.gtk_text_view_set_editable)(view, 0);
            (gtk.gtk_text_view_set_wrap_mode)(view, 2); // GTK_WRAP_WORD
            (gtk.gtk_text_buffer_set_text)((gtk.gtk_text_view_get_buffer)(view), text.as_ptr(), -1);
            (gtk.gtk_container_add)(scroll, view);
            (gtk.gtk_box_pack_start)(pane, advice, 0, 1, 0);
            (gtk.gtk_box_pack_start)(pane, scroll, 1, 1, 0);
            (gtk.gtk_box_pack_start)(column, pane, 1, 1, 12);
            (gtk.gtk_box_pack_start)(column, bar, 0, 1, 12);
            // From the right: OK, Copy Details, More Details.
            for button in [ok, copy, toggle] {
                (gtk.gtk_box_pack_end)(bar, button, 0, 0, 0);
            }
            // The details pane and its copy button stay hidden when the window is shown; the pane's children
            // are made visible now so that showing the pane later shows them too.
            (gtk.gtk_widget_show_all)(pane);
            for hidden in [pane, copy] {
                (gtk.gtk_widget_set_visible)(hidden, 0);
                (gtk.gtk_widget_set_no_show_all)(hidden, 1);
            }
            let notice = Box::new(Self {
                gtk: gtk.clone(),
                window: Cell::new(window),
                details: pane,
                copy,
                toggle,
                text,
                done: Cell::new(false),
            });
            let data = ptr::from_ref::<Self>(&notice) as Object;
            let handlers: [(Object, &CStr, unsafe extern "C" fn(Object, Object)); 4] = [
                (window, c"destroy", notice_closed),
                (ok, c"clicked", notice_ok),
                (copy, c"clicked", notice_copy),
                (toggle, c"clicked", notice_toggle),
            ];
            for (object, signal, handler) in handlers {
                (gtk.g_signal_connect_data)(
                    object,
                    signal.as_ptr(),
                    Some(std::mem::transmute::<unsafe extern "C" fn(Object, Object), unsafe extern "C" fn()>(handler)),
                    data,
                    None,
                    0,
                );
            }
            (gtk.gtk_widget_show_all)(window);
            Ok(notice)
        }
    }
}

impl Drop for Notice {
    fn drop(&mut self) {
        let window = self.window.replace(ptr::null_mut());
        if !window.is_null() {
            // SAFETY: the window is alive (its destroy handler clears the cell) and this is the GTK thread.
            unsafe {
                (self.gtk.gtk_widget_destroy)(window);
            }
        }
    }
}

/// Runs a details dialog's handler; no Rust panic may unwind across the C signal invocation.
fn notice(data: Object, run: impl FnOnce(&Notice)) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(unsafe { &*(data as *const Notice) })));
    if result.is_err() {
        tracing::error!("desktop callback panicked");
    }
}
unsafe extern "C" fn notice_closed(_window: Object, data: Object) {
    notice(data, |notice| {
        notice.window.set(ptr::null_mut());
        notice.done.set(true);
    });
}
unsafe extern "C" fn notice_ok(_button: Object, data: Object) {
    notice(data, |notice| notice.done.set(true));
}
unsafe extern "C" fn notice_copy(_button: Object, data: Object) {
    notice(data, |notice| unsafe {
        let gtk = &notice.gtk;
        let clipboard = (gtk.gtk_clipboard_get)((gtk.gdk_atom_intern)(c"CLIPBOARD".as_ptr(), 0));
        if !clipboard.is_null() {
            (gtk.gtk_clipboard_set_text)(clipboard, notice.text.as_ptr(), -1);
            // Hand the text to the session's clipboard manager so it outlives this process.
            (gtk.gtk_clipboard_set_can_store)(clipboard, ptr::null_mut(), 0);
            (gtk.gtk_clipboard_store)(clipboard);
        }
    });
}
unsafe extern "C" fn notice_toggle(_button: Object, data: Object) {
    notice(data, |notice| unsafe {
        let gtk = &notice.gtk;
        let window = notice.window.get();
        if window.is_null() {
            return;
        }
        let show = (gtk.gtk_widget_get_visible)(notice.details) == 0;
        (gtk.gtk_widget_set_visible)(notice.details, c_int::from(show));
        (gtk.gtk_widget_set_visible)(notice.copy, c_int::from(show));
        (gtk.gtk_button_set_label)(notice.toggle, if show { c"Hide Details" } else { c"More Details" }.as_ptr());
        (gtk.gtk_window_set_resizable)(window, c_int::from(show));
        if show {
            (gtk.gtk_window_resize)(window, 760, 560);
        }
    });
}

fn connect(state: &Rc<State>, object: Object, signal: &CStr, callback: *const ()) -> c_ulong {
    // SAFETY: each call supplies the callback ABI required by the named signal.
    // Every successful handler owns one Rc until GTK calls release_signal.
    unsafe {
        (state.gtk.g_signal_connect_data)(
            object,
            signal.as_ptr(),
            Some(std::mem::transmute::<*const (), unsafe extern "C" fn()>(callback)),
            Rc::into_raw(state.clone()) as Object,
            Some(release_signal),
            0,
        )
    }
}
unsafe extern "C" fn release_signal(data: Object, _closure: Object) {
    unsafe {
        drop(Rc::from_raw(data as *const State));
    }
}

/// No Rust panic may unwind across a C signal invocation. State is kept alive by
/// the handler's Rc; no RefCell borrow is held while entering GTK recursively.
fn callback(data: Object, run: impl FnOnce(&State)) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let state = unsafe { &*(data as *const State) };
        if !state.shutting_down.get() {
            run(state);
        }
    }));
    if result.is_err() {
        tracing::error!("desktop callback panicked");
    }
}

fn monitors(state: &State) -> Vec<Option<Monitor>> {
    unsafe {
        let display = (state.gtk.gdk_display_get_default)();
        if display.is_null() {
            return Vec::new();
        }
        (0..(state.gtk.gdk_display_get_n_monitors)(display))
            .map(|index| {
                let monitor = (state.gtk.gdk_display_get_monitor)(display, index);
                if monitor.is_null() {
                    return None;
                }
                let mut rect = Rectangle::default();
                (state.gtk.gdk_monitor_get_geometry)(monitor, &mut rect);
                Some(Monitor { x: rect.x, y: rect.y, width: rect.width, height: rect.height })
            })
            .collect()
    }
}

fn cache_geometry(state: &State) {
    *state.geometry.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(geometry(state));
}

fn geometry(state: &State) -> Geometry {
    let window = state.window.get();
    unsafe {
        let display = (state.gtk.gdk_display_get_default)();
        let position = if display.is_null() || window.is_null() {
            None
        } else {
            let name = (state.gtk.g_type_name_from_instance)(display);
            if !name.is_null() && CStr::from_ptr(name).to_bytes().starts_with(b"GdkX11") {
                let (mut x, mut y) = (0, 0);
                (state.gtk.gtk_window_get_position)(window, &mut x, &mut y);
                Some((x, y))
            } else {
                None
            }
        };
        Geometry { position, monitors: monitors(state) }
    }
}

unsafe extern "C" fn configured(_window: Object, _event: Object, data: Object) -> c_int {
    callback(data, cache_geometry);
    0
}
unsafe extern "C" fn monitors_changed(_screen: Object, data: Object) {
    callback(data, cache_geometry);
}
unsafe extern "C" fn menu_open(_item: Object, data: Object) {
    callback(data, |state| {
        let action = state.menu_action.borrow().clone();
        if let Some(action) = action {
            action(crate::TrayAction::Open);
        }
    });
}
unsafe extern "C" fn menu_quit(_item: Object, data: Object) {
    callback(data, |state| {
        let action = state.menu_action.borrow().clone();
        if let Some(action) = action {
            action(crate::TrayAction::Quit);
        }
    });
}
unsafe extern "C" fn menu_position(_menu: Object, x: *mut c_int, y: *mut c_int, push_in: *mut c_int, data: Object) {
    callback(data, |state| {
        let position = state.menu_position.get();
        unsafe {
            *x = position.0;
            *y = position.1;
            *push_in = 1;
        }
    });
}
unsafe extern "C" fn destroyed(_window: Object, data: Object) {
    callback(data, |state| {
        state.window.set(ptr::null_mut());
        if state.connect_closed.get() {
            let read = state.config.borrow().clone();
            if let Some(read) = read {
                match CloseAction::from_config(&read()) {
                    Ok(CloseAction::Quit) => state.quit.set(true),
                    Ok(CloseAction::Minimize) => {} // closed event: no surviving main window to minimize
                    Err(error) => eprintln!("{error}"),
                }
            }
        }
    });
}
unsafe extern "C" fn load_changed(_view: Object, event: c_int, data: Object) {
    if event != 3 {
        return;
    } // WEBKIT_LOAD_FINISHED, including WebKit's own failure page
    callback(data, |state| {
        let window = state.window.get();
        if window.is_null() {
            return;
        }
        let read = state.config.borrow().clone();
        let minimized = read.is_some_and(|read| WindowPlan::starts_minimized(&read()));
        unsafe {
            if minimized {
                (state.gtk.gtk_window_iconify)(window);
            } else {
                (state.gtk.gtk_window_deiconify)(window);
                (state.gtk.gtk_window_present)(window);
            }
        }
    });
}
unsafe extern "C" fn download_started(_context: Object, download: Object, data: Object) {
    callback(data, |state| {
        let webkit = state.webkit.borrow().clone();
        if let Some(webkit) = webkit {
            unsafe {
                (webkit.webkit_download_cancel)(download);
            }
        }
    });
}
unsafe extern "C" fn external_window(_view: Object, action: Object, data: Object) -> Object {
    callback(data, |state| {
        let webkit = state.webkit.borrow().clone();
        if let Some(webkit) = webkit {
            unsafe {
                let request = (webkit.webkit_navigation_action_get_request)(action);
                if request.is_null() {
                    return;
                }
                let uri = (webkit.webkit_uri_request_get_uri)(request);
                if !uri.is_null() {
                    // A separate argv element, never shell text. External
                    // links open in the user's browser.
                    if let Ok(uri) = CStr::from_ptr(uri).to_str() {
                        match std::process::Command::new("xdg-open").arg(uri).spawn() {
                            Ok(mut child) => {
                                std::thread::spawn(move || {
                                    let _ = child.wait();
                                });
                            }
                            Err(error) => tracing::error!(%error, "opening external link"),
                        }
                    }
                }
            }
        }
    });
    ptr::null_mut()
}

unsafe fn set_icon(gtk: &Gtk, window: Object) {
    let svg = include_bytes!("../../../packaging/appimage/openrigcue.svg");
    unsafe {
        let loader = (gtk.gdk_pixbuf_loader_new)();
        if loader.is_null() {
            return;
        }
        let written = (gtk.gdk_pixbuf_loader_write)(loader, svg.as_ptr(), svg.len(), ptr::null_mut()) != 0;
        let closed = (gtk.gdk_pixbuf_loader_close)(loader, ptr::null_mut()) != 0;
        let pixbuf = (gtk.gdk_pixbuf_loader_get_pixbuf)(loader);
        if written && closed && !pixbuf.is_null() {
            (gtk.gtk_window_set_icon)(window, pixbuf);
        }
        (gtk.g_object_unref)(loader);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires isolated environment with GDK backend disabled"]
    fn failed_display_initialization_cannot_reenter_gtk() {
        assert_eq!(std::env::var("GDK_BACKEND").as_deref(), Ok("invalid-openrigcue-test"));
        let before = INITIALIZATION_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        for _ in 0..2 {
            assert!(matches!(Desktop::new(), Err(DesktopError::Display)));
        }
        assert_eq!(INITIALIZATION_CALLS.load(std::sync::atomic::Ordering::Relaxed) - before, 1);
    }

    #[test]
    #[ignore = "requires a display; opens and closes a message window"]
    fn details_dialog_reveals_details_and_ends_when_closed() {
        let desktop = Desktop::new().unwrap();
        let gtk = desktop.state.gtk.clone();
        let visible = |widget: Object| unsafe { (gtk.gtk_widget_get_visible)(widget) != 0 };
        let notice = Notice::build(&gtk, "Permission error...", "summary", "hint", "operation: write to").unwrap();
        let data = ptr::from_ref::<Notice>(&notice) as Object;
        desktop.dispatch_pending();
        assert!(!visible(notice.details) && !visible(notice.copy));
        unsafe {
            notice_toggle(notice.toggle, data);
            assert!(visible(notice.details) && visible(notice.copy));
            notice_copy(notice.copy, data);
            notice_toggle(notice.toggle, data);
            assert!(!visible(notice.details) && !visible(notice.copy));
            assert!(!notice.done.get());
            notice_ok(ptr::null_mut(), data);
        }
        assert!(notice.done.get());
        drop(notice);
        // Closing the window itself also ends the dialog, and the value no longer destroys it.
        let notice = Notice::build(&gtk, "t", "m", "h", "d").unwrap();
        unsafe { (gtk.gtk_widget_destroy)(notice.window.get()) };
        assert!(notice.done.get() && notice.window.get().is_null());
        assert!(Notice::build(&gtk, "t\0", "m", "h", "d").is_err());
    }

    #[test]
    #[ignore = "resolves host GTK/WebKit libraries, without display initialization"]
    fn runtime_symbols_resolve_without_initializing_the_desktop() {
        Gtk::load().unwrap();
        WebKit::load().unwrap();
    }
}
