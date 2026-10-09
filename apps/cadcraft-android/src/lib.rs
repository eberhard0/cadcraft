//! CADCraft on Android.
//!
//! Runs the same [`cadcraft_ui_egui::CadApp`] as the desktop app inside a `GameActivity`
//! (android-activity's `game-activity` backend, which eframe needs for the soft keyboard and
//! accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then packaged by the
//! Gradle project in `android/`.
//!
//! Differences from the desktop app:
//! - no TCP control server, no native menu;
//! - File › Open asks `MainActivity.pickOpen()` (Storage Access Framework); the bytes come back
//!   on a Java thread through `nativeDeliverFile`, are written to the app's private `files/`
//!   directory and opened from there on the next frame (the engine opens and saves paths);
//! - Save / Save As write to that private file (the engine's path), and the shell mirrors every
//!   rewrite to `Downloads/CADCraft/<name>` through `MainActivity.saveToDownloads` (MediaStore,
//!   no dialog); saving the same name again overwrites that file;
//! - the egui panel layout lives in the app's private files directory.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::SystemTime;

use android_activity::AndroidApp;
use cadcraft_engine::Session;
use cadcraft_ui_egui::{CadApp, Services};
use jni::objects::{JByteArray, JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};

const LOG_TAG: &str = "cadcraft";

/// Files the Kotlin side delivers (name, bytes); the app opens them on its next frame.
static INBOX: Mutex<Vec<(String, Vec<u8>)>> = Mutex::new(Vec::new());
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);
/// Private paths the engine may write (picked for Save As, or opened from the picker) with the
/// modification time last mirrored to Downloads; a newer file is copied there on the next frame.
static WATCHED: Mutex<Vec<(PathBuf, Option<SystemTime>)>> = Mutex::new(Vec::new());

/// The eframe app: `CadApp` plus the per-frame inbox drain and the Downloads mirror.
struct Shell {
    app: CadApp,
    files_dir: PathBuf,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_inbox();
        self.app.logic(ctx);
        self.mirror_saves();
        if self.app.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }
}

impl Shell {
    /// Picked files: write each to the private files directory and open it from there.
    fn drain_inbox(&mut self) {
        let files: Vec<(String, Vec<u8>)> = std::mem::take(&mut *INBOX.lock().unwrap_or_else(PoisonError::into_inner));
        for (name, bytes) in files {
            let path = self.files_dir.join(file_name(&name));
            match write_atomic(&path, &bytes) {
                Ok(()) => {
                    watch(&path);
                    self.app.open_path(&path.to_string_lossy());
                }
                Err(e) => self.app.set_status(format!("couldn't store {name}: {e}")),
            }
        }
    }

    /// Copy every watched file the engine rewrote since the last frame to Downloads/CADCraft.
    fn mirror_saves(&mut self) {
        let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
        for (path, seen) in watched.iter_mut() {
            let Some(modified) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()) else { continue };
            if *seen == Some(modified) {
                continue;
            }
            *seen = Some(modified);
            let name = file_name(&path.to_string_lossy());
            match std::fs::read(path).map_err(|e| e.to_string()).and_then(|bytes| save_to_downloads(&name, &bytes)) {
                Ok(()) => self.app.set_status(format!("Saved to Downloads/CADCraft/{name}")),
                Err(e) => self.app.set_status(format!("couldn't save {name} to Downloads: {e}")),
            }
        }
    }
}

/// Remember `path` (with its current modification time, so an existing file is not re-copied).
fn watch(path: &Path) {
    let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
    if watched.iter().any(|(p, _)| p == path) {
        return;
    }
    let seen = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    watched.push((path.to_path_buf(), seen));
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
    let files_dir = data_dir.join("drawings");
    log::info!("CADCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    install_io();
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "CADCraft",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            let mut app = CadApp::new(Session::new(), services(&files_dir));
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                log::info!("wgpu backend {:?}, adapter {}", info.backend, info.name);
                app.set_wgpu(rs);
            }
            Ok(Box::new(Shell { app, files_dir }))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("CADCraft stopped: {e}");
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// File format hooks for the engine (it stays I/O-agnostic), as the desktop and web shells do.
fn install_io() {
    cadcraft_engine::cmd::file::set_io(cadcraft_engine::cmd::file::IoHooks {
        read: |b, name| cadcraft_io::read(b, name).map_err(|e| e.to_string()),
        write: |d, name| cadcraft_io::write(d, name).map_err(|e| e.to_string()),
        plot: Some(|d, space, opts| cadcraft_io::plot(d, space, opts).map_err(|e| e.to_string())),
    });
}

/// The pickers: Open shows the system picker (the file arrives later through the inbox); Save As
/// takes the suggested name as a private path that `Shell::mirror_saves` copies to Downloads.
fn services(files_dir: &Path) -> Services {
    let save_dir = files_dir.to_path_buf();
    Services {
        pick_open: Some(Box::new(|| {
            if let Err(e) = pick_open() {
                log::error!("couldn't open the file picker: {e}");
            }
            None
        })),
        pick_save: Some(Box::new(move |suggested: &str| {
            let path = save_dir.join(file_name(suggested));
            if let Err(e) = std::fs::create_dir_all(&save_dir) {
                log::error!("couldn't create {}: {e}", save_dir.display());
                return None;
            }
            watch(&path);
            Some(path.to_string_lossy().to_string())
        })),
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// Write `bytes` to `path` through a temporary file, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
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

/// `MainActivity.pickOpen()`: show the system file picker; the result comes through the inbox.
fn pick_open() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("()V"), &[])?;
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
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(name, bytes)`: a picked file's contents, from a Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_cadcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        log::info!("received {name} ({} bytes)", bytes.len());
        INBOX.lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
