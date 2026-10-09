//! VectorCraft on Android.
//!
//! Runs the same [`vectorcraft_ui_egui::VectorcraftApp`] as the desktop app inside a
//! `GameActivity` (android-activity's `game-activity` backend, which eframe needs for the soft
//! keyboard and accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then
//! packaged by the Gradle project in `android/`.
//!
//! The services mirror the web shell's (`apps/vectorcraft-web`), with Android behind them:
//! - File › Open asks `MainActivity.pickOpen()` (Storage Access Framework); the bytes come back
//!   on a Java thread through `nativeDeliverFile` into `Services::inbox`;
//! - File › Place… asks `MainActivity.pickPlace()`; the files come back through
//!   `nativeDeliverPlace` into `Services::place_inbox`;
//! - Save and Export "download" to `Downloads/VectorCraft/<name>` through
//!   `MainActivity.saveToDownloads` (MediaStore, no dialog), and saving the same name again
//!   overwrites that file;
//! - File › Print hands the job's PDF to Android's print framework (`MainActivity.printPdf`);
//! - Data Recovery copies, preferences, swatch and graphic style libraries live in the app's
//!   private files directory;
//! - no TCP control server, no system clipboard beyond its text, no drag and drop.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use android_activity::AndroidApp;
use jni::objects::{JByteArray, JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};
use vectorcraft_engine::Session;
use vectorcraft_engine::cmd::fileio;
use vectorcraft_engine::cmd::recovery::FolderStore;
use vectorcraft_ui_egui::graphics::GraphicsLoss;
use vectorcraft_ui_egui::i18n;
use vectorcraft_ui_egui::place::{PlaceArrival, PlaceInbox};
use vectorcraft_ui_egui::print::{PrintJob, PrintService, Printer};
use vectorcraft_ui_egui::{Inbox, Services, UiState, VectorcraftApp};

const LOG_TAG: &str = "vectorcraft";

/// Files the Kotlin side delivers for File › Open (name, bytes); the app opens them on its next frame.
static INBOX: OnceLock<Inbox> = OnceLock::new();
/// Files the Kotlin side delivers for File › Place…; the Place dialog takes them on the next frame.
static PLACE_INBOX: OnceLock<PlaceInbox> = OnceLock::new();
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

fn inbox() -> &'static Inbox {
    INBOX.get_or_init(Inbox::default)
}

fn place_inbox() -> &'static PlaceInbox {
    PLACE_INBOX.get_or_init(PlaceInbox::default)
}

/// The app inside eframe's runner, as the desktop's `App` in `apps/vectorcraft/src/main.rs`.
struct Shell {
    app: VectorcraftApp,
    /// Where the UI preferences are saved on exit.
    prefs: PathBuf,
    /// Reported by wgpu when the graphics device is lost.
    graphics_loss: GraphicsLoss,
    /// The graphics device was lost and the unsaved changes are kept for Data Recovery.
    graphics_lost: bool,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(why) = self.graphics_loss.take() {
            self.graphics_lost = self.app.graphics_lost(&why);
            self.app.status(if self.graphics_lost {
                "The graphics device was lost: unsaved changes are kept for Data Recovery. Save your documents and restart VectorCraft"
            } else {
                "The graphics device was lost: save your documents and restart VectorCraft"
            });
        }
        self.app.logic(ctx);
        if self.app.ui.status == "quit" {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }

    fn on_exit(&mut self) {
        save_prefs(&self.app, &self.prefs);
    }
}

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    log::info!("VectorCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    // The UI language follows the phone's languages (the desktop reads the locale environment).
    match locale_tags() {
        Ok(tags) => i18n::set_system_locales(&tags),
        Err(e) => log::warn!("couldn't read the system languages: {e}"),
    }
    let prefs = data_dir.join("ui.json");
    let saved = read_prefs(&prefs);
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "VectorCraft",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            let mut app = VectorcraftApp::new(Session::new(), services(&data_dir));
            if let Some(ui) = saved {
                app.ui = ui.sanitized();
            }
            vectorcraft_ui_egui::prefs_dialog::restore(&mut app);
            // User Defined swatch and graphic style libraries live next to the preferences.
            app.session.swatch_libraries.set_user_dir(Some(data_dir.join("Swatches").to_string_lossy().to_string()));
            app.session.style_libraries.set_user_dir(Some(data_dir.join("Graphic Styles").to_string_lossy().to_string()));
            let graphics_loss = GraphicsLoss::default();
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                let summary = format!("{} ({:?}, {:?})", info.name.trim(), info.backend, info.device_type);
                log::info!("rendering with {summary}");
                app.graphics_adapter = Some(summary);
                let (loss, ctx) = (graphics_loss.clone(), cc.egui_ctx.clone());
                rs.device.set_device_lost_callback(move |reason, msg| loss.report(&ctx, format!("{reason:?}: {msg}")));
            }
            // No OS window to decorate: the app bar is plain.
            app.integrated_titlebar = false;
            app.custom_titlebar = false;
            Ok(Box::new(Shell { app, prefs, graphics_loss, graphics_lost: false }))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("VectorCraft stopped: {e}");
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// The platform services, the web shell's set (`apps/vectorcraft-web/src/web.rs`) on Android:
/// `download` makes Save and Export name a file instead of picking a path, and the pickers are
/// asynchronous (their files arrive through the inboxes).
fn services(data_dir: &Path) -> Services {
    Services {
        // File → Place…: the picked files go to the Place dialog.
        place_async: Some(Box::new(|| {
            if let Err(e) = pick_place() {
                log::error!("couldn't open the file picker: {e}");
            }
        })),
        place_inbox: Some(place_inbox().clone()),
        // File → Open: the picker is asynchronous; the file arrives later through the inbox.
        open_async: Some(Box::new(|| {
            if let Err(e) = pick_open() {
                log::error!("couldn't open the file picker: {e}");
            }
        })),
        // Save and Export: the file's name in Downloads/VectorCraft (the activity reports the result).
        download: Some(Box::new(|name: &str, bytes: &[u8]| {
            if let Err(e) = save_to_downloads(&file_name(name), bytes) {
                log::error!("saving {name} failed: {e}");
            }
        })),
        inbox: Some(inbox().clone()),
        // Data Recovery copies: a folder in the app's private files, locked like the desktop's.
        recovery_store: Some(Arc::new(FolderStore::new(data_dir.join("Data Recovery")))),
        // File → Print: Android's print dialog.
        print: Some(Box::new(AndroidPrint)),
        // Help → Discord / website / GitHub, the Discord button, About and Home links.
        open_url: Some(Box::new(|url: &str| {
            if let Err(e) = open_url(url) {
                log::error!("couldn't open {url}: {e}");
            }
        })),
        // Edit → Paste from the menu bar (egui's Paste event only comes with the keyboard).
        clipboard_read: Some(Box::new(|| match read_clipboard() {
            Ok(text) => text,
            Err(e) => {
                log::error!("couldn't read the clipboard: {e}");
                None
            }
        })),
        ..Default::default()
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// The saved UI preferences (`ui.json` in the app's files), as the desktop reads them.
fn read_prefs(path: &Path) -> Option<UiState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Save the UI preferences, as the desktop does on exit. Best effort: a failed write keeps the
/// previous file.
fn save_prefs(app: &VectorcraftApp, path: &Path) {
    let mut ui = app.ui.clone();
    ui.engine_prefs = app.session.prefs.to_json();
    if let Ok(bytes) = serde_json::to_vec_pretty(&ui) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = fileio::write_atomic(path, &bytes) {
            log::warn!("couldn't save the preferences: {e}");
        }
    }
}

/// File → Print on Android: the job's PDF goes to the system's print dialog (which picks the
/// printer, or Save as PDF).
struct AndroidPrint;

impl PrintService for AndroidPrint {
    fn printers(&mut self) -> Vec<Printer> {
        // Android doesn't tell apps about printers: its print dialog lists them.
        vec![]
    }

    fn print(&mut self, job: &PrintJob) -> Result<String, String> {
        print_pdf(job.title, job.pdf)?;
        Ok(format!("Printing “{}”: pick the printer in Android's print dialog", job.title))
    }
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// What a method returning a `String` or `null` answered → the string.
fn java_string(env: &mut Env<'_>, ret: JObject<'_>) -> jni::errors::Result<Option<String>> {
    if ret.is_null() {
        return Ok(None);
    }
    let text = env.cast_local::<JString>(ret)?;
    Ok(Some(text.to_string()))
}

/// `MainActivity.pickOpen()`: show the system file picker; the result comes through the inbox.
fn pick_open() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `MainActivity.pickPlace()`: show the system file picker for several files; they come through
/// the place inbox.
fn pick_place() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickPlace"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `MainActivity.saveToDownloads(name, bytes)`: `null` on success, else the error message.
fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jname = JString::from_str(env, name)?;
        let jbytes = env.byte_array_from_slice(bytes)?;
        let ret = env
            .call_method(activity, jni_str!("saveToDownloads"), jni_sig!("(Ljava/lang/String;[B)Ljava/lang/String;"), &[(&jname).into(), (&jbytes).into()])?
            .l()?;
        match java_string(env, ret)? {
            None => Ok(Ok(())),
            Some(message) => Ok(Err(message)),
        }
    })?
}

/// `MainActivity.printPdf(title, pdf)`: `null` on success, else the error message.
fn print_pdf(title: &str, pdf: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jtitle = JString::from_str(env, title)?;
        let jbytes = env.byte_array_from_slice(pdf)?;
        let ret = env
            .call_method(activity, jni_str!("printPdf"), jni_sig!("(Ljava/lang/String;[B)Ljava/lang/String;"), &[(&jtitle).into(), (&jbytes).into()])?
            .l()?;
        match java_string(env, ret)? {
            None => Ok(Ok(())),
            Some(message) => Ok(Err(message)),
        }
    })?
}

/// `MainActivity.openUrl(url)`: Help menu links in the browser.
fn open_url(url: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jurl = JString::from_str(env, url)?;
        env.call_method(activity, jni_str!("openUrl"), jni_sig!("(Ljava/lang/String;)V"), &[(&jurl).into()])?;
        Ok(())
    })
}

/// `MainActivity.readClipboard()`: the clipboard's text, if it holds any.
fn read_clipboard() -> Result<Option<String>, String> {
    with_activity(|env, activity| {
        let ret = env.call_method(activity, jni_str!("readClipboard"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        java_string(env, ret)
    })
}

/// `MainActivity.localeTags()`: the phone's languages as BCP 47 tags, most preferred first.
fn locale_tags() -> Result<Vec<String>, String> {
    with_activity(|env, activity| {
        let ret = env.call_method(activity, jni_str!("localeTags"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
        let tags = java_string(env, ret)?.unwrap_or_default();
        Ok(tags.split(',').filter(|t| !t.is_empty()).map(str::to_string).collect())
    })
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// A file's contents from a Java thread, into `inbox`, and wake the app.
fn deliver(name: String, bytes: Vec<u8>, to_place: bool) {
    log::info!("received {name} ({} bytes){}", bytes.len(), if to_place { " to place" } else { "" });
    if to_place {
        place_inbox().lock().unwrap_or_else(PoisonError::into_inner).push(PlaceArrival { name, bytes, drop: None });
    } else {
        inbox().lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// `MainActivity.nativeDeliverFile(name, bytes)`: a file picked for File › Open.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_vectorcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        deliver(name, bytes, false);
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}

/// `MainActivity.nativeDeliverPlace(name, bytes)`: a file picked for File › Place….
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_vectorcraft_MainActivity_nativeDeliverPlace<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        deliver(name, bytes, true);
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
