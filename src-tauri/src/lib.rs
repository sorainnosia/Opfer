// Native camera capture + QR decode. camera2 (Android) or nokhwa (desktop) feed
// the same decode pipeline; only decoded bytes cross to the frontend.
mod camera_android;

/// One CAMERA FRAME's worth of results from the native Android path, sent for
/// every frame (even with no codes) so the frontend can measure the true camera
/// fps. `codes` holds base64 of each QR's raw byte-mode payload (what the web
/// decoder's `.bytes` returns), so JS can `parseFrame` each unchanged.
#[derive(Clone, serde::Serialize)]
pub struct DecodedFrame {
    pub codes: Vec<String>,
    /// Camera-reported frame rate range actually granted, e.g. [30,30] or [60,60].
    pub fps: [i32; 2],
    /// Occasional low-res grayscale thumbnail for aiming (None on most frames).
    pub preview: Option<PreviewData>,
    /// Total camera frames captured so far — the frontend derives capture fps
    /// from its delta (decode messages are emitted at the slower decode rate).
    pub captures: u32,
}

/// A small grayscale preview thumbnail. `gray` is base64 of `w*h` luma bytes.
#[derive(Clone, serde::Serialize)]
pub struct PreviewData {
    pub w: u32,
    pub h: u32,
    pub gray: String,
    /// Sensor orientation in degrees (0/90/180/270) — rotate the preview by this.
    pub orientation: i32,
}

/// Start native camera capture + decode (Android only), streaming decoded frame
/// bytes over `channel`. On other platforms this is a no-op error.
#[tauri::command]
fn start_native_camera(
    channel: tauri::ipc::Channel<DecodedFrame>,
    log: tauri::ipc::Channel<String>,
    fps: u32,
    width: i32,
    height: i32,
    grid: u32,
    lock: bool,
    workers: u32,
    ev: f32,
    ev_auto: bool,
    sharpen: bool,
) -> Result<(), String> {
    camera_android::start(
        channel, log, fps, width, height, grid, lock, workers, ev, ev_auto, sharpen,
    )
}

#[tauri::command]
fn stop_native_camera() {
    camera_android::stop();
}

/// Keep the display (and system) awake while the sender is streaming QR frames.
/// Screen dim/sleep pauses the sender's `requestAnimationFrame`, which freezes the
/// animated code — the receiver then scans a static frame and stalls near the end
/// of a transfer. `navigator.wakeLock` is unreliable in a desktop WebView, so we
/// hold an OS-level display request for as long as sending is active.
#[tauri::command]
fn keep_awake(on: bool) {
    #[cfg(windows)]
    keep_awake_win::set(on);
    #[cfg(not(windows))]
    let _ = on;
}

#[cfg(windows)]
mod keep_awake_win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    extern "system" {
        fn SetThreadExecutionState(es_flags: u32) -> u32;
    }

    struct KeepAwake {
        active: AtomicBool,
        stop: Mutex<bool>,
        cvar: Condvar,
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    fn state() -> &'static KeepAwake {
        static S: OnceLock<KeepAwake> = OnceLock::new();
        S.get_or_init(|| KeepAwake {
            active: AtomicBool::new(false),
            stop: Mutex::new(false),
            cvar: Condvar::new(),
            thread: Mutex::new(None),
        })
    }

    pub fn set(on: bool) {
        let s = state();
        if on {
            if s.active.swap(true, Ordering::SeqCst) {
                return; // already awake
            }
            *s.stop.lock().unwrap() = false;
            // The execution-state request is bound to the calling THREAD's lifetime,
            // so hold it on a dedicated thread that stays alive until we stop.
            let t = std::thread::spawn(move || {
                unsafe {
                    SetThreadExecutionState(
                        ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
                    );
                }
                let mut stop = s.stop.lock().unwrap();
                while !*stop {
                    stop = s.cvar.wait(stop).unwrap();
                }
                unsafe {
                    SetThreadExecutionState(ES_CONTINUOUS); // clear on the way out
                }
            });
            *s.thread.lock().unwrap() = Some(t);
        } else {
            if !s.active.swap(false, Ordering::SeqCst) {
                return; // already off
            }
            *s.stop.lock().unwrap() = true;
            s.cvar.notify_all();
            if let Some(t) = s.thread.lock().unwrap().take() {
                let _ = t.join();
            }
        }
    }
}

#[derive(serde::Serialize)]
struct WriteResult {
    path: String,
    fnv: u32, // FNV-1a of the bytes actually written — lets the caller verify integrity
}

/// FNV-1a (matches the JS `fnv1a` in protocol.ts) so the frontend can confirm
/// the bytes arrived intact across the bridge.
fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

fn write_unique(path: &str, data: &[u8]) -> Result<WriteResult, String> {
    let target = unique_path(std::path::Path::new(path));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&target, data).map_err(|e| e.to_string())?;
    Ok(WriteResult {
        path: target.to_string_lossy().into_owned(),
        fnv: fnv1a(data),
    })
}

// Fast path: bytes arrive as the RAW IPC request body (no JSON array marshaling),
// which is what keeps large files from choking the bridge. Body layout:
// [u32 LE pathLen][path utf-8][file bytes]. Returns the path + an FNV-1a of the
// written bytes so the caller can detect any transport corruption/truncation.
#[tauri::command]
fn write_file_raw(request: tauri::ipc::Request<'_>) -> Result<WriteResult, String> {
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => b,
        tauri::ipc::InvokeBody::Json(_) => return Err("expected a raw request body".into()),
    };
    if bytes.len() < 4 {
        return Err("payload too short".into());
    }
    let path_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let header_end = 4 + path_len;
    if header_end > bytes.len() {
        return Err("invalid header length".into());
    }
    let path = std::str::from_utf8(&bytes[4..header_end]).map_err(|e| e.to_string())?;
    write_unique(path, &bytes[header_end..])
}

// Safe fallback: bytes arrive as a JSON array of numbers (each 0..=255). Slower,
// but immune to any binary-transport corruption — used only when the raw path's
// integrity check fails (seen on some Android IPC bridges with large payloads).
#[tauri::command]
fn write_file_json(path: String, data: Vec<u8>) -> Result<WriteResult, String> {
    write_unique(&path, &data)
}

/// If `path` exists, return `<stem> (n).<ext>` with the first free n.
fn unique_path(path: &std::path::Path) -> std::path::PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file").to_string();
    let ext = path.extension().and_then(|s| s.to_str()).map(str::to_string);
    let mut n = 1u32;
    loop {
        let fname = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let cand = parent.join(fname);
        if !cand.exists() {
            return cand;
        }
        n += 1;
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            write_file_raw,
            write_file_json,
            start_native_camera,
            stop_native_camera,
            keep_awake
        ])
        .setup(|_app| {
            // Route `log` to logcat so the native camera path can be debugged
            // with `adb logcat -s opfer`.
            #[cfg(target_os = "android")]
            android_logger::init_once(
                android_logger::Config::default()
                    .with_max_level(log::LevelFilter::Info)
                    .with_tag("opfer"),
            );

            // On Windows, WebView2 remembers a camera "block" and then never
            // re-prompts, leaving the Receive tab permanently unable to start
            // the camera. Auto-allow camera/microphone permission requests so
            // pressing "Start camera" always works.
            #[cfg(windows)]
            {
                use tauri::Manager;
                if let Some(window) = _app.get_webview_window("main") {
                    grant_media_permissions(&window);
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(windows)]
fn grant_media_permissions(window: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PERMISSION_KIND_CAMERA, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE,
        COREWEBVIEW2_PERMISSION_STATE_ALLOW,
    };
    use webview2_com::PermissionRequestedEventHandler;

    let _ = window.with_webview(|webview| unsafe {
        let core = match webview.controller().CoreWebView2() {
            Ok(core) => core,
            Err(_) => return,
        };
        let handler = PermissionRequestedEventHandler::create(Box::new(|_sender, args| {
            if let Some(args) = args.as_ref() {
                let mut kind = Default::default();
                if args.PermissionKind(&mut kind).is_ok()
                    && (kind == COREWEBVIEW2_PERMISSION_KIND_CAMERA
                        || kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE)
                {
                    let _ = args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW);
                }
            }
            Ok(())
        }));
        let mut token = Default::default();
        let _ = core.add_PermissionRequested(&handler, &mut token);
    });
}
