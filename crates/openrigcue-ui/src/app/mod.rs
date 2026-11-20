//! The UI application, `MyApp`: the service documents and store setters the interface uses, the timers and
//! threads, the page events and the main loop, running on the UI thread of [`crate::host`].

mod mainloop;
pub mod shell;
mod timers;

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use openrigcue_model::native_value::LegacyValue;
use serde_json::Value;

use crate::backend::{at, patch_for, set_at, Backend, BackendExt, Failure};
use crate::host::{AppThread, AttributeAnswer, UiApp};
use crate::remi::gui::{cb, cb_native, emit, HasGui, JS_CALLBACK};
use crate::remi::{Gui, JsFlags, Translate, WidgetId};

pub use mainloop::hardware_status;

/// The pause at the end of every main loop pass.
pub const MAIN_LOOP_PERIOD: Duration = Duration::from_millis(500);

/// The shared state as the service publishes it: the status, the settings, the
/// loaded tune and the run-time switches.
#[derive(Debug, Clone, Default)]
pub struct Docs {
    pub status: Value,
    pub settings: Value,
    pub tune: Value,
    pub controls: Value,
    /// The game catalogue.
    pub games: Value,
}

struct Timer {
    id: u64,
    due: Instant,
    run: Box<dyn FnOnce(&mut MyApp) + Send>,
    wake: Arc<timers::Wake>,
}

/// The translator hooks for the widget tree.
struct LiveTranslator {
    backend: Arc<dyn Backend>,
    excluded: Vec<String>,
}

impl Translate for LiveTranslator {
    /// A text without a translation of its own that starts with spaces (an indented entry of a list, such as the
    /// effect rows below a group) is looked up without them, and the indentation is kept.
    fn translate(&self, text: &str) -> String {
        let translated = self.backend.translate(text);
        let rest = text.trim_start_matches(' ');
        if translated != text || rest.is_empty() || rest.len() == text.len() {
            return translated;
        }
        format!("{}{}", &text[..text.len() - rest.len()], self.backend.translate(rest))
    }

    /// Whether a text is never translated: product and game names, addresses, numbers, versions, speeds,
    /// percentages, distances, ranges and COM ports are shown as they are.
    fn is_excluded_key(&self, key: &str) -> bool {
        if self.excluded.iter().any(|e| e == key) {
            return true;
        }
        excluded_pattern(key)
    }
}

/// Whether `key` has the form of a text that is never translated: an address, a number, a version, a speed, a
/// percentage, a distance, a range or a COM port.
pub fn excluded_pattern(key: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let number = |s: &str| {
        let s = s.strip_prefix('-').unwrap_or(s);
        match s.split_once('.') {
            Some((a, b)) => digits(a) && digits(b),
            None => digits(s),
        }
    };
    let ip = {
        let parts: Vec<&str> = key.split('.').collect();
        parts.len() == 4 && parts.iter().all(|p| digits(p) && p.len() <= 3)
    };
    let version = {
        let parts: Vec<&str> = key.split('.').collect();
        parts.len() >= 2 && parts.iter().all(|p| digits(p))
    };
    let with_suffix = |suffixes: &[&str], inner: &dyn Fn(&str) -> bool| {
        suffixes.iter().any(|suffix| {
            key.strip_suffix(suffix).is_some_and(|rest| {
                let rest = rest.strip_suffix(' ').unwrap_or(rest);
                inner(rest)
            })
        })
    };
    let speed = with_suffix(&["Km/h", "Mph"], &|s| digits(s));
    let percentage = with_suffix(&["%"], &|s| digits(s));
    let distance = with_suffix(&["mm"], &|s| number(s) && !s.starts_with('-'));
    let range = |text: &str, unit: &str| {
        text.split_once('-').is_some_and(|(a, b)| {
            let a = a.strip_suffix(' ').unwrap_or(a);
            let b = b.strip_prefix(' ').unwrap_or(b);
            let strip =
                |s: &str| if unit.is_empty() { Some(s.to_owned()) } else { s.strip_suffix(unit).map(str::to_owned) };
            matches!((strip(a), strip(b)), (Some(a), Some(b)) if digits(&a) && digits(&b))
        })
    };
    let com = key.strip_prefix("COM").is_some_and(digits);
    ip || number(key) || version || speed || percentage || distance || range(key, "") || range(key, "%") || com
}

/// The globals as one read found them.
pub struct ReadDocs {
    status: Option<Value>,
    settings: Option<Value>,
    profile: Option<Value>,
}

/// Reads the status, the settings and the loaded tune from the service.
pub fn read_docs(backend: &dyn Backend) -> ReadDocs {
    ReadDocs {
        status: backend.get("status").ok(),
        settings: backend.get("settings").ok(),
        profile: backend.get("profile").ok(),
    }
}

/// The application instance.
pub struct MyApp {
    update_lock: Arc<crate::host::UpdateLock>,
    pub gui: Gui<MyApp>,
    pub backend: Arc<dyn Backend>,
    pub docs: Docs,
    timers: Vec<Timer>,
    next_timer: u64,
    timer_app: Weak<Mutex<MyApp>>,
    /// Whether the deferred start has run.
    initialization_done: bool,
    /// The root container of the page.
    pub main_container: Option<WidgetId>,
    /// The threads `deferred_main` started (the main loop among them), handed to the host.
    threads: Vec<AppThread<MyApp>>,
    /// The file each download widget serves at `/<id>/download`, by widget identifier: file name and contents.
    pub downloads: std::collections::HashMap<String, (String, Vec<u8>)>,
    /// How many payloads each export-all download widget has been given: every invocation adds one more.
    pub(crate) export_all_downloads: std::collections::HashMap<String, usize>,
    /// The body's `loading` container made for the first page.
    loading: Option<WidgetId>,
    /// The main loop's state between iterations.
    pub ml: mainloop::LoopState,
    /// The error of a synchronous widget callback; a browser callback reports it at its own boundary.
    pub(crate) callback_error: Option<String>,
    /// The path of the file being imported.
    pub ml_import_path: Option<String>,
    /// The interface: its theme, screen, notifications and dialogs.
    pub ui: crate::ui::UiState,
}

/// The body's `onpageshow` / `onresize` script, with its leading line break.
const JS_BODY_SIZE: &str = "\n            var params={};\n            params['width']=window.innerWidth;\n            params['height']=window.innerHeight;\n            remi.sendCallbackParam('%(emitter_identifier)s','%(event_name)s',params);";

impl HasGui for MyApp {
    fn gui(&mut self) -> &mut Gui<MyApp> {
        &mut self.gui
    }

    fn class_event(
        &mut self,
        id: WidgetId,
        signature: &'static crate::remi::classes::Signature,
        bound: &crate::remi::classes::Bound,
    ) -> Option<Result<Vec<LegacyValue>, String>> {
        let _ = id;
        // The interface's actions: name, argument and value, passed on as texts.
        (signature.qualname == "OrcAction.onact")
            .then(|| Ok((0..3).map(|index| LegacyValue::Str(bound.positional[index].clone())).collect()))
    }

    /// The application's own methods at its identifier: `onpixelratio`, which takes the browser's pixel ratio,
    /// and `onerror`, which takes a page error's message, source, line, column and error text and only writes a
    /// debug record.
    fn app_method(&mut self, id: WidgetId, name: &str, parameters: &[(&str, &str)]) -> Option<Result<(), String>> {
        let bind = |signature| crate::remi::classes::bind(signature, false, parameters.iter().copied());
        Some(match name {
            "onpixelratio" => bind(&crate::remi::parameters::PIXEL_RATIO).map(|bound| {
                emit(self, id, name, &[Value::String(bound.positional[0].clone())]);
            }),
            "onerror" => bind(&crate::remi::parameters::ON_ERROR).map(|bound| {
                let [message, source, lineno, colno, error] = &bound.positional[..] else { unreachable!() };
                tracing::debug!(
                    target: "remi.server",
                    "App.onerror event occurred in webpage:\n            \nMESSAGE:{message}\nSOURCE:{source}\nLINENO:{lineno}\nCOLNO:{colno}\\ERROR:{error}\n"
                );
            }),
            _ => return None,
        })
    }
}

impl MyApp {
    pub fn new(backend: Arc<dyn Backend>) -> MyApp {
        let excluded = backend.excluded_strings();
        let translator = Arc::new(LiveTranslator { backend: backend.clone(), excluded });
        MyApp {
            update_lock: Arc::new(crate::host::UpdateLock::default()),
            gui: Gui::new(translator),
            backend,
            docs: Docs::default(),
            timers: Vec::new(),
            next_timer: 0,
            timer_app: Weak::new(),
            initialization_done: false,
            main_container: None,
            threads: Vec::new(),
            downloads: std::collections::HashMap::new(),
            export_all_downloads: std::collections::HashMap::new(),
            loading: None,
            ml: mainloop::LoopState::default(),
            callback_error: None,
            ml_import_path: None,
            ui: crate::ui::UiState::default(),
        }
    }

    // ---- timers ---------------------------------------------------------------------------------------------

    /// Starts a timer that runs `run` once after `delay`; the returned id cancels it.
    pub fn timer(&mut self, delay: Duration, run: impl FnOnce(&mut MyApp) + Send + 'static) -> u64 {
        self.timer_with_update_lock(delay, None, run)
    }

    pub(crate) fn update_locked_timer(
        &mut self,
        delay: Duration,
        run: impl FnOnce(&mut MyApp) + Send + 'static,
    ) -> u64 {
        self.timer_with_update_lock(delay, Some(self.update_lock.clone()), run)
    }

    fn timer_with_update_lock(
        &mut self,
        delay: Duration,
        update_lock: Option<Arc<crate::host::UpdateLock>>,
        run: impl FnOnce(&mut MyApp) + Send + 'static,
    ) -> u64 {
        self.next_timer += 1;
        let id = self.next_timer;
        let due = Instant::now() + delay;
        let wake = Arc::new(timers::Wake::default());
        self.timers.push(Timer { id, due, run: Box::new(run), wake: wake.clone() });
        if self.timer_app.strong_count() != 0 {
            timers::start(self.timer_app.clone(), id, due, wake, update_lock);
        }
        id
    }

    /// Cancels the timer `id` if it has not started running.
    pub fn cancel_timer(&mut self, id: u64) {
        self.timers.retain(|timer| timer.id != id || !timer.wake.cancel());
    }

    fn run_timers(&mut self, now: Instant) {
        // Detached fixtures retain their explicit clock-driven API. The product
        // host starts independent Timer jobs, unrelated to its GUI update loop.
        if self.timer_app.strong_count() != 0 {
            return;
        }
        loop {
            let Some(index) = self.timers.iter().position(|t| t.due <= now) else { return };
            let timer = self.timers.remove(index);
            (timer.run)(self);
        }
    }

    // ---- the service ---------------------------------------------------------------------------------------

    /// Asks the service to speak `text`: the service queues it to be spoken while Voice Notifications is on. A
    /// failed request is logged.
    pub fn speak_out(&mut self, text: &str) {
        if let Err(failure) = self.backend.post("voice", serde_json::json!({ "message": text })) {
            tracing::warn!(%failure, "UI call failed");
        }
    }

    /// Reads the shared state again (status, settings, loaded tune).
    pub fn refresh_docs(&mut self) {
        let docs = read_docs(&*self.backend);
        self.apply_docs(docs);
    }

    /// Takes the globals [`read_docs`] read (a failed read keeps the previous document).
    pub fn apply_docs(&mut self, docs: ReadDocs) {
        if let Some(status) = docs.status {
            self.docs.controls = status.get("controls").cloned().unwrap_or(Value::Null);
            self.docs.status = status;
        }
        if let Some(settings) = docs.settings {
            self.docs.settings = settings;
        }
        if let Some(profile) = docs.profile {
            self.docs.tune = profile.get("tune").cloned().unwrap_or(Value::Null);
        }
    }

    /// Assigns `value` at `path` in the loaded tune.
    pub fn set_tune(&mut self, path: &[&str], value: Value) {
        set_at(&mut self.docs.tune, path, value.clone());
        if let Ok(tune) = self.backend.patch("profile", patch_for(path, value)) {
            if let Some(tune) = tune.get("tune").cloned().or(Some(tune)).filter(Value::is_object) {
                self.docs.tune = tune;
            }
        }
    }

    /// Assignment of a stored value through existing parents; the JSON document is a cache.
    pub(crate) fn set_tune_native_checked(
        &mut self,
        path: &[&str],
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), String> {
        self.backend.assign_tune_value(path, value).map_err(|error| error.detail)?;
        self.refresh_tune_cache();
        Ok(())
    }

    fn refresh_tune_cache(&mut self) {
        if let Ok(profile) = self.backend.get("profile") {
            self.docs.tune = profile.get("tune").cloned().unwrap_or(profile);
        }
    }

    /// A shaker tune assignment and its synchronous channel-group refresh.
    pub(crate) fn set_shaker_value_checked(
        &mut self,
        table: &str,
        key: &str,
        value: &openrigcue_model::native_value::LegacyValue,
    ) -> Result<(), String> {
        self.backend.assign_shaker_value(table, key, value).map_err(|error| error.detail)?;
        self.refresh_tune_cache();
        Ok(())
    }

    /// Assigns `value` to `key` of `section` in the settings.
    pub(crate) fn set_motion_setting_native_checked(&mut self, key: &str, value: &LegacyValue) -> Result<(), String> {
        self.backend.assign_motion_setting(key, value).map_err(|error| error.detail)?;
        if let Ok(settings) = self.backend.get("settings") {
            self.docs.settings = settings;
        }
        Ok(())
    }

    pub(crate) fn set_setting_native_checked(
        &mut self,
        section: &str,
        key: &str,
        value: &LegacyValue,
    ) -> Result<(), String> {
        self.backend.assign_setting_value(section, key, value).map_err(|error| error.detail)?;
        if let Ok(settings) = self.backend.get("settings") {
            self.docs.settings = settings;
        }
        Ok(())
    }

    /// Assigns `value` to `key` of `section` in the settings, through the typed settings API.
    pub fn set_setting(&mut self, path: &[&str], value: Value) {
        set_at(&mut self.docs.settings, path, value.clone());
        if let Ok(settings) = self.backend.patch("settings", patch_for(path, value)) {
            if settings.is_object() {
                self.docs.settings = settings;
            }
        }
    }

    /// Assigns one of the run-time switches the service keeps.
    pub fn set_control(&mut self, path: &[&str], value: Value) {
        set_at(&mut self.docs.controls, path, value.clone());
        if let Ok(controls) = self.backend.patch("controls", patch_for(path, value)) {
            if controls.is_object() {
                self.docs.controls = controls;
            }
        }
    }

    /// A runtime write whose failure stops the caller before its next UI operation.
    pub(crate) fn set_control_checked(&mut self, path: &[&str], value: Value) -> Result<(), String> {
        let controls = self.backend.patch("controls", patch_for(path, value.clone())).map_err(|error| error.detail)?;
        if controls.is_object() {
            self.docs.controls = controls;
        } else {
            set_at(&mut self.docs.controls, path, value);
        }
        Ok(())
    }

    pub(crate) fn assign_belt_switch(&mut self, enabled: bool) -> Result<(), String> {
        self.backend.assign_belt_switch(enabled).map_err(|error| error.detail)?;
        set_at(&mut self.docs.controls, &["switches", "belt"], Value::Bool(enabled));
        Ok(())
    }

    pub fn tune(&self, path: &[&str]) -> &Value {
        at(&self.docs.tune, path)
    }

    pub fn setting(&self, path: &[&str]) -> &Value {
        at(&self.docs.settings, path)
    }

    pub fn setting_checked(&self, path: &[&str]) -> Result<&Value, String> {
        crate::backend::field(&self.docs.settings, path)
    }

    pub fn status(&self, path: &[&str]) -> &Value {
        at(&self.docs.status, path)
    }

    pub fn control(&self, path: &[&str]) -> &Value {
        at(&self.docs.controls, path)
    }

    /// One column of the running game's feature row; `None` when there is no row for the
    /// game, stand-by included, or no such column.
    pub fn game_feature(&self, column: &str) -> Option<bool> {
        let features = crate::catalog::features();
        let openrigcue_model::native_value::LegacyValue::Str(game) = self.backend.game_value().ok()? else {
            return None;
        };
        let row = features.games.get(&game)?;
        if !features.columns.iter().any(|c| c == column) {
            return None;
        }
        Some(row.iter().any(|c| c == column))
    }

    /// Whether the Windows rules apply (always).
    pub fn win32(&self) -> bool {
        true
    }

    /// A file the service makes ([`Backend::fetch`]); a failure is logged and the handler stops.
    pub fn call_fetch(&self, path: &str) -> Option<(Vec<u8>, String)> {
        match self.backend.fetch(path) {
            Ok(file) => Some(file),
            Err(failure) => {
                tracing::warn!(%failure, "UI call failed");
                None
            }
        }
    }

    /// A service call whose failure ends the handler: logged, the handler stops.
    pub fn call(&self, result: Result<Value, Failure>) -> Option<Value> {
        match result {
            Ok(value) => Some(value),
            Err(failure) => {
                tracing::warn!(%failure, "UI call failed");
                None
            }
        }
    }

    // ---- application events ----------------------------------------------------------------------------------

    /// Builds the first page: the body's `loading` container replaced by one with the `loading-message` label, and the
    /// loading label in `main_container`.
    fn build_main(&mut self, body: WidgetId) -> WidgetId {
        let new_loading = self.gui.create("div", "orc-loading");
        self.gui.set_style(new_loading, "display", "none");
        self.gui.set_identifier(new_loading, "loading");
        let loading_message = self.gui.create("p", "orc-loading-message");
        self.gui.set_text(
            loading_message,
            if self.win32() {
                "The mobile companion can only be reached while OpenRigCue is running on your PC."
            } else {
                "Standing by until the Magicbox is on the network"
            },
        );
        self.gui.set_identifier(loading_message, "loading-message");
        self.gui.append(new_loading, loading_message, "");
        self.loading = Some(new_loading);
        let container = self.gui.create("div", "orc");
        let loading = self.gui.create("p", "orc-loading-message");
        self.gui.set_text(loading, "loading...");
        self.gui.append(container, loading, "loading_label");
        self.main_container = Some(container);
        // The body's `onload`, `ononline`, `onpagehide`, `onpageshow` and
        // `onresize` connected to the application's methods, in this order (their JavaScript attributes follow
        // it).
        self.gui.on(body, "onload", Some(JS_CALLBACK), JsFlags::NONE, cb(|app: &mut MyApp, _, _| app.onload()));
        self.gui.on(
            body,
            "ononline",
            Some(JS_CALLBACK),
            JsFlags::NONE,
            // The application's `ononline` handler.
            cb(|_: &mut MyApp, _, _| tracing::debug!(target: "remi.server", "App.ononline event occurred")),
        );
        self.gui.on(
            body,
            "onpagehide",
            Some(JS_CALLBACK),
            JsFlags::NONE,
            cb(|app: &mut MyApp, _, _| {
                if let Err(error) = app.onpagehide() {
                    app.callback_error = Some(error);
                }
            }),
        );
        self.gui.on(
            body,
            "onpageshow",
            Some(JS_BODY_SIZE),
            JsFlags::NONE,
            cb(|app: &mut MyApp, body, _| app.onpageshow(body)),
        );
        self.gui.on(
            body,
            "onresize",
            Some(JS_BODY_SIZE),
            JsFlags::NONE,
            cb(|app: &mut MyApp, body, args| {
                if let Err(error) = app.onresize(body, args) {
                    app.callback_error = Some(error);
                }
            }),
        );
        self.gui.on(
            body,
            "onpixelratio",
            None,
            JsFlags::NONE,
            cb(|app: &mut MyApp, _, args| {
                if let Err(error) = app.onpixelratio(args) {
                    app.callback_error = Some(error);
                }
            }),
        );
        container
    }

    /// The page loaded: `deferred_main` (once).
    fn onload(&mut self) {
        tracing::debug!(target: "openrigcue_ui::onload", "inside onload");
        self.deferred_main();
        tracing::debug!(target: "openrigcue_ui::onload", "deferred main completed");
        tracing::debug!(target: "remi.server", "App.onload event occurred");
    }

    /// The page was shown: asks the browser for its pixel ratio, then tells the service that the application
    /// has loaded.
    fn onpageshow(&mut self, body: WidgetId) {
        tracing::debug!(target: "remi.server", "App.onpageshow event occurred");
        self.request_pixel_ratio(body);
        self.sub_app_loaded();
    }

    /// Has the browser report its pixel ratio (after the page is shown or resized).
    fn request_pixel_ratio(&mut self, body: WidgetId) {
        let identifier = self.gui.identifier(body).to_owned();
        self.gui.execute_javascript(&format!(
            "var params={{}}; params['pixel_ratio']=window.devicePixelRatio; remi.sendCallbackParam('{identifier}','onpixelratio',params);"
        ));
    }

    /// Tells the service that the application has loaded; the service then applies the configured log level.
    fn sub_app_loaded(&mut self) {
        let _ = self.call(self.backend.post("app-loaded", Value::Null));
    }

    /// The page was hidden: when the setting that makes the window's close button exit the program is on, the
    /// open files are closed and the program quits. That setting must be readable as a boolean.
    fn onpagehide(&mut self) -> Result<(), String> {
        self.close_if_requested()?;
        tracing::debug!(target: "remi.server", "App.onpagehide event occurred");
        Ok(())
    }

    fn close_if_requested(&mut self) -> Result<(), String> {
        use openrigcue_cueing::{str2bool, PyValue};
        let value = self.backend.native_setting("APP", "x_button_exit").map_err(|error| error.detail)?;
        let quit = str2bool(&PyValue::from(&value))
            .map_err(|error| format!("{}: {}", error.exception().name(), error.message()))?;
        if quit && self.win32() {
            self.backend.close_opened_files();
            self.backend.quit_after_pagehide().map_err(|error| error.detail)?;
        }
        Ok(())
    }

    /// The window was resized: its width and height are stored in the settings, then the pixel ratio is asked
    /// for.
    fn onresize(&mut self, body: WidgetId, args: &[Value]) -> Result<(), String> {
        match args.len() {
            0 => {
                return Err(
                    "TypeError: MyApp.onresize() missing 2 required positional arguments: 'width' and 'height'".into()
                )
            }
            1 => return Err("TypeError: MyApp.onresize() missing 1 required positional argument: 'height'".into()),
            2 => {}
            count => {
                return Err(format!(
                    "TypeError: MyApp.onresize() takes 4 positional arguments but {} were given",
                    count + 2
                ))
            }
        }
        self.assign_window_setting("window_width", args[0].clone())?;
        self.assign_window_setting("window_height", args[1].clone())?;
        tracing::debug!(
            target: "remi.server",
            "App.onresize event occurred. Width:{} Height:{}",
            crate::backend::text(&args[0]),
            crate::backend::text(&args[1])
        );
        self.request_pixel_ratio(body);
        Ok(())
    }

    /// The browser reported its pixel ratio: it is stored in the settings.
    fn onpixelratio(&mut self, args: &[Value]) -> Result<(), String> {
        match args.len() {
            0 => {
                return Err(
                    "TypeError: MyApp.onpixelratio() missing 1 required positional argument: 'pixel_ratio'".into()
                )
            }
            1 => {}
            count => {
                return Err(format!(
                    "TypeError: MyApp.onpixelratio() takes 2 positional arguments but {} were given",
                    count + 1
                ))
            }
        }
        self.assign_window_setting("pixel_ratio", args[0].clone())
    }

    fn assign_window_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        self.docs.settings = self.backend.assign_window_setting(key, value).map_err(|error| error.detail)?;
        Ok(())
    }

    /// The deferred start, run only from the page's `onload`: guarded by
    /// `initialization_done`, it builds the frame, runs every page constructor with the threads those start,
    /// and starts the main loop last.
    ///
    /// Nothing of this runs when the service starts: `main` returns only the `loading...` label, and the
    /// first browser's `onload` arrives over its websocket, so that socket is registered before any
    /// constructor thread or `main_loop` changes a widget. Run earlier, those changes could fall between a
    /// connecting browser's handshake render and its first message, where updates go only to the
    /// sockets already registered, and that browser kept a stale page. A reconnect, a reload or another
    /// browser finds `initialization_done` set and does nothing, also after a constructor failed.
    fn deferred_main(&mut self) {
        if self.initialization_done {
            return;
        }
        self.initialization_done = true;
        self.refresh_docs();
        if let Ok(games) = self.backend.get("catalog/games") {
            self.docs.games = games;
        }
        shell::deferred_main(self);
        if let Some(error) = &self.callback_error {
            tracing::error!(%error, "UI initialization failed");
            return;
        }
        // Start the main loop on its own thread.
        self.start_thread("main_loop_ui", Box::new(mainloop::main_loop));
    }

    /// The attached host's state, if this is a running application rather than a detached fixture.
    pub(crate) fn shared_handle(&self) -> Option<Arc<Mutex<Self>>> {
        self.timer_app.upgrade()
    }

    /// Starts `run` on a new named thread: launched at this point,
    /// including calls from an application worker while the GUI host is busy.
    /// Caller-owned fixtures without an attached host retain jobs for inspection.
    pub fn start_thread(
        &mut self,
        name: &'static str,
        run: Box<dyn FnOnce(std::sync::Weak<std::sync::Mutex<MyApp>>) + Send>,
    ) {
        let thread = AppThread { name, run };
        if self.timer_app.strong_count() != 0 {
            thread.start(self.timer_app.clone());
        } else {
            self.threads.push(thread);
        }
    }

    /// One `main_loop` pass on the current thread (the globals read, then the pass).
    pub fn main_loop_pass(&mut self) {
        self.refresh_docs();
        mainloop::iteration(self);
    }

    /// Opens the interface's screen for a section name (`Tuning`, `Games`, `Setup`) and a page name. An unknown
    /// page is kept as the callback error.
    pub fn goto_page(&mut self, global: &str, page: &str) {
        if let Err(error) = self.goto_page_checked(global, page) {
            self.callback_error = Some(error);
        }
    }

    pub fn goto_page_checked(&mut self, global: &str, page: &str) -> Result<(), String> {
        let screen =
            crate::ui::screen_of_page(global, page).ok_or_else(|| format!("unknown page {global} → {page}"))?;
        crate::ui::navigate(self, screen, 0);
        Ok(())
    }

    /// An alert: a warning notice that stays until it is dismissed.
    ///
    /// An internal diagnostic (`SomethingError: detail`) has no translation of its own: it is shown inside a
    /// translated sentence, with the diagnostic as the system gave it.
    pub fn alert(&mut self, text: &str) {
        let shown = if is_internal_diagnostic(text) {
            self.backend.translate(INTERNAL_ERROR).replace("^detail^", text)
        } else {
            self.backend.translate(text)
        };
        // The interface shows it as a warning that stays until dismissed.
        crate::ui::notices::warning(self, "Alert", &shown);
    }
}

/// The sentence an internal diagnostic is shown in.
const INTERNAL_ERROR: &str = "An internal error occurred: ^detail^";

/// Whether `text` is an internal diagnostic: a name ending in `Error`, a colon, then the detail.
fn is_internal_diagnostic(text: &str) -> bool {
    text.split_once(": ").is_some_and(|(name, _)| {
        name.len() > "Error".len() && name.ends_with("Error") && name.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

impl UiApp for MyApp {
    fn attach_shared(&mut self, shared: Weak<Mutex<Self>>) {
        self.timer_app = shared;
    }
    fn on_close(&mut self) -> Result<(), String> {
        self.close_if_requested()
    }

    fn close_class_name(&self) -> &'static str {
        "MyApp"
    }

    fn update_lock(&self) -> Option<Arc<crate::host::UpdateLock>> {
        Some(self.update_lock.clone())
    }
    fn callback(
        &mut self,
        id: WidgetId,
        event: &str,
        parameters: &crate::remi::parameters::Parameters,
    ) -> Result<(), String> {
        self.callback_error = None;
        let items = parameters.iter().map(|(key, value)| (key.as_str(), value.as_str()));
        crate::remi::gui::dispatch_wire(self, id, event, items)?;
        self.callback_error.take().map_or(Ok(()), Err)
    }
    fn main(&mut self, body: WidgetId) -> WidgetId {
        self.build_main(body)
    }

    fn title(&self) -> String {
        "OpenRigCue".to_owned()
    }

    fn head(&self) -> String {
        crate::assets::head()
    }

    fn loading_html(&mut self) -> String {
        match self.loading {
            Some(loading) => self.gui.repr(loading, &mut Vec::new()),
            None => String::new(),
        }
    }

    fn poll(&mut self, now: Instant) {
        self.run_timers(now);
    }

    /// The attribute calls: the import taking a file path (`/import/api_set_file_path`, how a second start hands
    /// its profile over) and the downloads serving their file (`/<id>/download`).
    fn attribute_call(&mut self, widget: &str, method: &str, params: &[(String, String)]) -> Option<AttributeAnswer> {
        self.attribute_call_checked(widget, method, params).ok().flatten()
    }

    fn attribute_call_checked(
        &mut self,
        widget: &str,
        method: &str,
        params: &[(String, String)],
    ) -> Result<Option<AttributeAnswer>, String> {
        if widget == "import" && method == "api_set_file_path" {
            // The import is offered once the interface is built (by the first `onload`); before that the call
            // is not answered.
            if !self.ui.mounted {
                return Ok(None);
            }
            let value1 = crate::ui::profiles::import_argument(params)?;
            crate::ui::profiles::offer_launcher_import(self, value1)?;
            return Ok(Some(AttributeAnswer {
                content: b"OK".to_vec(),
                headers: vec![("Content-type".to_owned(), "text/plain".to_owned())],
            }));
        }
        if method == "download" {
            let Some((file_name, data)) = self.downloads.get(widget).cloned() else { return Ok(None) };
            let class = if self.export_all_downloads.contains_key(widget) { "ExportAll" } else { "MyFileDownloader" };
            crate::ui::profiles::download_arguments(class, params)?;
            if let Some(count) = self.export_all_downloads.get_mut(widget) {
                *count = count.saturating_add(1);
                if *count > 1 {
                    return Err("ValueError: too many values to unpack (expected 2)".into());
                }
            }
            return Ok(Some(AttributeAnswer {
                content: data,
                headers: vec![
                    ("Content-type".to_owned(), "application/octet-stream".to_owned()),
                    ("Content-Disposition".to_owned(), format!("attachment; filename=\"{file_name}\"")),
                ],
            }));
        }
        Ok(None)
    }

    fn take_threads(&mut self) -> Vec<AppThread<MyApp>> {
        std::mem::take(&mut self.threads)
    }
}

pub(crate) fn native_callback(
    callback: impl Fn(&mut MyApp, &[LegacyValue]) -> Result<(), String> + Send + Sync + 'static,
) -> Option<crate::remi::Listener<MyApp>> {
    cb_native(move |app: &mut MyApp, _, args| {
        if let Err(error) = callback(app, args) {
            app.callback_error = Some(error);
        }
    })
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) mod shaker_test_backend;
