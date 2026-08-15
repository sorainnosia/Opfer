//! Native Android camera capture + QR decode, to bypass the WebView's 30 fps
//! `getUserMedia` cap.
//!
//! Flow: camera2 (NDK) opens the rear camera and drives an `AImageReader` at the
//! requested fps; each YUV_420_888 frame's Y plane (already grayscale) is decoded
//! natively with `rxing`; only the decoded frame BYTES are streamed to the
//! frontend over a Tauri `Channel`. Raw frames never cross the IPC bridge.
//!
//! ⚠️ VERIFICATION NOTE: this file cannot be compiled/tested off-device here.
//! The camera2 section is unsafe NDK FFI whose exact `ndk-sys` symbol names and
//! signatures vary between crate versions — treat it as a working scaffold that
//! needs a round of on-device `cargo apk`/`tauri android` iteration. The decode,
//! threading and Tauri wiring are the stable parts.

// This module is the native capture + decode pipeline. The DECODE half (FrameCtx,
// the `decode` module, the coordinator/worker/sender/watchdog threads, and all the
// image helpers) is platform-neutral and compiles everywhere. Only the CAMERA half
// is platform-specific: `mod native` (camera2, Android) and `mod desktop` (nokhwa,
// desktop) — each feeds frames into `FrameCtx::store_frame` and implements
// `CameraSource`. The name stays `camera_android` for git history; it's really the
// shared camera module now.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tauri::ipc::Channel;

use crate::{DecodedFrame, PreviewData};

const PREVIEW_W: usize = 480; // preview thumbnail width (throttled ~3/s, so affordable)

/// A running camera. `close()` stops capture and releases the device; dropping the
/// box afterwards is a no-op. `Send` so the Session can be torn down off-thread.
trait CameraSource: Send {
    fn close(&self);
}

/// Handle kept alive for the duration of a capture session.
struct Session {
    stop: Arc<AtomicBool>,
    camera: Box<dyn CameraSource>,
    ctx: Arc<FrameCtx>,
    decode_thread: Option<std::thread::JoinHandle<()>>,
    worker_threads: Vec<std::thread::JoinHandle<()>>,
    watchdog_thread: Option<std::thread::JoinHandle<()>>,
    sender_thread: Option<std::thread::JoinHandle<()>>,
}

/// Spawn `ctx.workers` decode workers tagged with generation `gen`. A worker
/// exits as soon as `ctx.worker_gen` moves past its own `gen` — that's how the
/// watchdog retires a stalled generation and replaces it with a fresh one.
fn spawn_workers(ctx: &Arc<FrameCtx>, gen: u32) -> Vec<std::thread::JoinHandle<()>> {
    let mut threads = Vec::new();
    for i in 0..ctx.workers {
        let wctx = ctx.clone();
        if let Ok(t) = std::thread::Builder::new()
            .name(format!("opfer-worker{gen}-{i}"))
            .spawn(move || decode_worker(wctx, gen))
        {
            threads.push(t);
        }
    }
    threads
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// Completion flag for the most recent stop()'s detached teardown. start() waits
/// on it so a new session never spins up while the previous pipeline's threads are
/// still winding down (which made repeated stop/start progressively less stable).
static LAST_TEARDOWN: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Start native capture at `target_fps` and stream decoded frames to `channel`.
/// The camera callback only STORES the latest frame; a dedicated decode thread
/// does the (slow) QR decode — so a slow decode can never stall the camera.
pub fn start(
    channel: Channel<DecodedFrame>,
    log: Channel<String>,
    target_fps: u32,
    width: i32,
    height: i32,
    grid: u32,
    lock: bool,
    workers: u32,
    ev: f32,
    ev_auto: bool,
    sharpen: bool,
) -> Result<(), String> {
    // Clean handoff: wait (bounded) for the previous session's detached teardown
    // to finish so the old decode/worker/sender threads are gone before we open a
    // new camera + spawn a fresh pipeline. We POLL a flag rather than join() — the
    // sender may be mid Channel::send (which can need this thread), so a join could
    // deadlock. Give up after ~1.5s so a wedged teardown can never block Start.
    if let Some(done) = LAST_TEARDOWN.lock().unwrap().take() {
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while !done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let mut guard = SESSION.lock().unwrap();
    if guard.is_some() {
        return Err("camera already running".into());
    }
    let stop = Arc::new(AtomicBool::new(false));

    // Pool size from the UI, clamped to something sane. Queue holds ~one frame per
    // worker so every worker always has something to pull (+small headroom).
    let workers = (workers as usize).clamp(1, 16);
    let work_max = (workers + 2).max(3);

    let ctx = Arc::new(FrameCtx {
        channel,
        log,
        stop: stop.clone(),
        fps_range: [target_fps as i32, target_fps as i32],
        orientation: AtomicI32::new(90), // default rear-camera orientation
        capture_count: AtomicU32::new(0),
        coord_frames: AtomicU32::new(0),
        worker_gen: AtomicU32::new(0),
        attempts: AtomicU32::new(0),
        decoded_count: AtomicU32::new(0),
        forwarded: AtomicU32::new(0),
        latest: Mutex::new(None),
        signal: Condvar::new(),
        work_queue: Mutex::new(VecDeque::new()),
        work_signal: Condvar::new(),
        workers,
        work_max,
        outbox: Mutex::new(VecDeque::new()),
        out_signal: Condvar::new(),
        seen: Mutex::new((0, HashSet::new())),
        grid: (grid.max(1)) as usize,
        sharpen,
    });

    // Open the platform camera. camera2 (Android) supports the AF/AE tuning
    // (lock/ev/ev_auto); nokhwa (desktop) uses device defaults for those.
    #[cfg(target_os = "android")]
    let camera: Box<dyn CameraSource> = Box::new(
        native::Camera::open(target_fps, width, height, lock, ev, ev_auto, ctx.clone())
            .map_err(|e| format!("camera2 open failed: {e}"))?,
    );
    #[cfg(not(target_os = "android"))]
    let camera: Box<dyn CameraSource> = {
        let _ = (lock, ev, ev_auto); // camera2-only controls
        Box::new(
            desktop::Camera::open(target_fps, width, height, ctx.clone())
                .map_err(|e| format!("camera open failed: {e}"))?,
        )
    };

    let dctx = ctx.clone();
    let decode_thread = std::thread::Builder::new()
        .name("opfer-decode".into())
        .spawn(move || decode_loop(dctx))
        .ok();

    // Decode worker POOL (generation 0). If a frame makes rxing hang and consumes
    // the whole pool, the watchdog below bumps the generation and spawns a fresh
    // pool so decoding self-heals (the stuck workers are abandoned).
    let worker_threads = spawn_workers(&ctx, 0);

    // Watchdog: detects a wedged pool (attempts frozen while the camera keeps
    // delivering) and respawns it.
    let wdctx = ctx.clone();
    let watchdog_thread = std::thread::Builder::new()
        .name("opfer-watchdog".into())
        .spawn(move || watchdog_loop(wdctx))
        .ok();

    // Dedicated sender thread: the ONLY place Channel::send is called. If a send
    // blocks (UI thread momentarily busy), only this thread waits — the decode
    // thread keeps decoding into the drop-oldest outbox and the camera keeps
    // running, so the pipeline self-heals instead of wedging forever.
    let sctx = ctx.clone();
    let sender_thread = std::thread::Builder::new()
        .name("opfer-sender".into())
        .spawn(move || sender_loop(sctx))
        .ok();

    *guard = Some(Session {
        stop,
        camera,
        ctx,
        decode_thread,
        worker_threads,
        watchdog_thread,
        sender_thread,
    });
    Ok(())
}

pub fn stop() {
    let taken = SESSION.lock().unwrap().take();
    if let Some(mut session) = taken {
        // Signal stop synchronously so both threads stop touching the Channel
        // immediately (no more sends that could block).
        session.stop.store(true, Ordering::SeqCst);
        session.ctx.signal.notify_all();
        session.ctx.work_signal.notify_all(); // wake the decode workers
        session.ctx.out_signal.notify_all(); // wake the sender thread so it exits
        // Release the camera SYNCHRONOUSLY so the camera2 device is free the moment
        // stop() returns — otherwise switching to the WebView camera hits "camera
        // not found" because camera2 still holds it. This is safe now: the camera
        // callback does no Channel::send, so close() (which waits only for an
        // in-flight callback) cannot deadlock on the main thread.
        session.camera.close();
        // Publish a completion flag the NEXT start() waits on, so the new session
        // can't overlap this one's threads (the source of the repeated-restart
        // instability).
        let done = Arc::new(AtomicBool::new(false));
        *LAST_TEARDOWN.lock().unwrap() = Some(done.clone());
        // Join the worker/sender/decode threads OFF the caller thread: the sender
        // may be mid Channel::send (which needs the main thread), so joining here
        // could deadlock. Detaching is fine — stop is set and the camera (their
        // frame source) is already closed, so they exit promptly on their own.
        std::thread::spawn(move || {
            if let Some(t) = session.decode_thread.take() {
                let _ = t.join();
            }
            if let Some(t) = session.watchdog_thread.take() {
                let _ = t.join();
            }
            // Workers are NOT joined: one stuck in a hung rxing call could never be
            // joined, and would block teardown forever. The stop flag makes healthy
            // workers exit on their own; wedged ones are abandoned (the process
            // reclaims them on exit). Dropping the Vec just detaches them.
            drop(session.worker_threads);
            if let Some(t) = session.sender_thread.take() {
                let _ = t.join();
            }
            // Threads (except any abandoned wedged worker) are gone — let start() proceed.
            done.store(true, Ordering::SeqCst);
        });
    }
}

/// Decode COORDINATOR: pulls the latest camera frame, ships a throttled preview,
/// and hands the frame to the worker pool. It NEVER runs rxing or waits on a
/// decode, so preview + capture-fps always flow — even while every worker grinds.
fn decode_loop(ctx: Arc<FrameCtx>) {
    let mut dframe: u32 = 0;
    let mut last_preview = std::time::Instant::now() - Duration::from_secs(1);
    loop {
        if dframe % 60 == 0 {
            log::info!("coord alive dec={dframe} caps={}", ctx.capture_count.load(Ordering::Relaxed));
        }
        let frame = {
            let mut latest = ctx.latest.lock().unwrap();
            while latest.is_none() {
                if ctx.stop.load(Ordering::SeqCst) {
                    return;
                }
                let (g, _) = ctx
                    .signal
                    .wait_timeout(latest, Duration::from_millis(200))
                    .unwrap();
                latest = g;
            }
            if ctx.stop.load(Ordering::SeqCst) {
                return;
            }
            latest.take()
        };
        let Some((luma, w, h)) = frame else { continue };

        // Throttled preview (from this frame, before the luma is moved to a worker).
        if last_preview.elapsed() >= Duration::from_millis(300) {
            last_preview = std::time::Instant::now();
            if let Ok(pv) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                downscale_preview(&luma, w, h, ctx.orientation.load(Ordering::Relaxed))
            })) {
                ctx.push_out(DecodedFrame {
                    codes: Vec::new(),
                    fps: ctx.fps_range,
                    preview: Some(pv),
                    captures: ctx.capture_count.load(Ordering::Relaxed),
                });
            }
        }

        // Enqueue the frame for the worker pool (drop-oldest, wakes one worker).
        ctx.push_work(luma, w, h);
        ctx.coord_frames.fetch_add(1, Ordering::Relaxed);
        dframe = dframe.wrapping_add(1);
    }
}

/// Decode WORKER (one of a pool, tagged with generation `my_gen`): the only place
/// rxing runs. Takes the latest queued frame, decodes it, and pushes any codes to
/// the outbox. Exits once the watchdog retires this generation (`worker_gen` moves
/// past `my_gen`) — but only when it next reaches the loop top, so a worker hung in
/// rxing is simply abandoned while a fresh generation takes over.
fn decode_worker(ctx: Arc<FrameCtx>, my_gen: u32) {
    use base64::Engine;
    let mut decoder = decode::Decoder::new();
    let mut wframe: u32 = 0;
    loop {
        if ctx.worker_gen.load(Ordering::SeqCst) != my_gen {
            return; // retired by the watchdog
        }
        // Pull the next frame off the shared FIFO (each worker pops its own).
        let job = {
            let mut q = ctx.work_queue.lock().unwrap();
            while q.is_empty() {
                if ctx.stop.load(Ordering::SeqCst) || ctx.worker_gen.load(Ordering::SeqCst) != my_gen
                {
                    return;
                }
                let (g, _) = ctx
                    .work_signal
                    .wait_timeout(q, Duration::from_millis(200))
                    .unwrap();
                q = g;
            }
            if ctx.stop.load(Ordering::SeqCst) {
                return;
            }
            q.pop_front()
        };
        let Some((luma, w, h)) = job else { continue };

        ctx.attempts.fetch_add(1, Ordering::Relaxed);
        let grid = ctx.grid;
        let sharpen = ctx.sharpen;
        // Move `luma` into the decode (so the grid-1 fallback can consume it without
        // a copy); borrow `decoder` so it survives for the next loop iteration.
        let dec = &mut decoder;
        let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            dec.decode(luma, w, h, grid, sharpen)
        }));
        let (payloads, detected) = match decoded {
            Ok(x) => x,
            Err(_) => {
                log::warn!("w#{wframe}: DECODE PANIC (recovered)");
                (Vec::new(), 0)
            }
        };
        if wframe < 10 || (detected > 0 && payloads.is_empty()) {
            log::info!("w#{wframe} {w}x{h}: detected={detected} payloads={}", payloads.len());
        }
        if !payloads.is_empty() && !ctx.stop.load(Ordering::SeqCst) {
            ctx.decoded_count
                .fetch_add(payloads.len() as u32, Ordering::Relaxed);
            // Forward only frames we haven't sent before (dedup by session,seq).
            let mut codes: Vec<String> = Vec::new();
            {
                let mut seen = ctx.seen.lock().unwrap();
                for p in &payloads {
                    match frame_id(p) {
                        Some((sid, seq)) => {
                            if seen.0 != sid {
                                seen.0 = sid; // new session → forget old seqs
                                seen.1.clear();
                            }
                            if seen.1.insert(seq) {
                                codes.push(base64::engine::general_purpose::STANDARD.encode(p));
                            }
                        }
                        // Unparseable header: forward and let JS decide.
                        None => codes.push(base64::engine::general_purpose::STANDARD.encode(p)),
                    }
                }
            }
            if !codes.is_empty() {
                ctx.forwarded.fetch_add(codes.len() as u32, Ordering::Relaxed);
                ctx.push_out(DecodedFrame {
                    codes,
                    fps: ctx.fps_range,
                    preview: None,
                    captures: ctx.capture_count.load(Ordering::Relaxed),
                });
            }
        }
        wframe = wframe.wrapping_add(1);
    }
}

/// Watchdog: if the worker pool wedges (decode `attempts` frozen) while the camera
/// keeps delivering frames, retire the stalled generation and spawn a fresh pool so
/// decoding self-heals. The wedged workers are abandoned (a hung rxing call can't be
/// interrupted). Costs nothing when the pool is healthy.
fn watchdog_loop(ctx: Arc<FrameCtx>) {
    let mut last_att = ctx.attempts.load(Ordering::Relaxed);
    let mut last_caps = ctx.capture_count.load(Ordering::Relaxed);
    let mut stalls = 0u32;
    let mut respawns = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(750));
        if ctx.stop.load(Ordering::SeqCst) {
            return;
        }
        let att = ctx.attempts.load(Ordering::Relaxed);
        let caps = ctx.capture_count.load(Ordering::Relaxed);
        let workers_stuck = att == last_att;
        let camera_alive = caps != last_caps;
        last_att = att;
        last_caps = caps;

        if workers_stuck && camera_alive {
            stalls += 1;
            if stalls >= 2 {
                // ~1.5s wedged with fresh frames available → respawn the pool.
                stalls = 0;
                respawns += 1;
                if respawns > 200 {
                    log::error!("decode pool wedged repeatedly — giving up");
                    return;
                }
                let gen = ctx.worker_gen.fetch_add(1, Ordering::SeqCst) + 1;
                log::warn!("decode pool stalled — respawn #{respawns} (gen {gen})");
                let _ = spawn_workers(&ctx, gen); // detached; exits on stop/next gen
                ctx.work_signal.notify_all();
            }
        } else {
            stalls = 0;
        }
    }
}

/// Sender thread: the ONLY caller of `Channel::send`. Drains the outbox and
/// pushes frames to the frontend; if a send blocks (UI momentarily busy), only
/// this thread waits. Also emits the once/sec in-app liveness beacon, so if the
/// beacon freezes we know the *transport* stalled (decode/camera are unaffected).
fn sender_loop(ctx: Arc<FrameCtx>) {
    let mut last_beacon = std::time::Instant::now();
    loop {
        // Wait for work (or wake every 500ms to emit the beacon / re-check stop).
        let batch: Vec<DecodedFrame> = {
            let mut q = ctx.outbox.lock().unwrap();
            if q.is_empty() {
                if ctx.stop.load(Ordering::SeqCst) {
                    return;
                }
                let (g, _) = ctx
                    .out_signal
                    .wait_timeout(q, Duration::from_millis(500))
                    .unwrap();
                q = g;
            }
            q.drain(..).collect()
        };
        if ctx.stop.load(Ordering::SeqCst) {
            return;
        }
        // COALESCE the whole batch into ONE message: merge all codes, keep the
        // most recent preview + capture count. Rapid-fire per-frame sends were
        // saturating the webview IPC and stalling the UI thread; one larger
        // message carries the same data with a fraction of the IPC calls.
        if !batch.is_empty() {
            let mut codes: Vec<String> = Vec::new();
            let mut preview = None;
            let mut captures = 0u32;
            for f in batch {
                codes.extend(f.codes);
                if f.preview.is_some() {
                    preview = f.preview;
                }
                captures = captures.max(f.captures);
            }
            if !codes.is_empty() || preview.is_some() {
                let _ = ctx.channel.send(DecodedFrame {
                    codes,
                    fps: ctx.fps_range,
                    preview,
                    captures,
                });
            }
            // Rate-limit sends (~10/s). The decode thread keeps filling the
            // drop-oldest outbox meanwhile; the next pass coalesces it.
            std::thread::sleep(Duration::from_millis(100));
        }
        if last_beacon.elapsed() >= Duration::from_secs(1) {
            last_beacon = std::time::Instant::now();
            let caps = ctx.capture_count.load(Ordering::Relaxed);
            let coord = ctx.coord_frames.load(Ordering::Relaxed);
            let att = ctx.attempts.load(Ordering::Relaxed);
            let dec = ctx.decoded_count.load(Ordering::Relaxed);
            let fwd = ctx.forwarded.load(Ordering::Relaxed);
            let gen = ctx.worker_gen.load(Ordering::Relaxed);
            let _ = ctx.log.send(format!(
                "alive caps={caps} coord={coord} att={att} dec={dec} fwd={fwd} gen={gen}"
            ));
        }
    }
}

/// Box-averaging downscale of the luma plane to a grayscale preview thumbnail.
/// Averaging (vs nearest-neighbour subsampling) keeps the higher-res preview
/// smooth instead of grainy/aliased.
fn downscale_preview(luma: &[u8], w: u32, h: u32, orientation: i32) -> PreviewData {
    use base64::Engine;
    let (w, h) = (w as usize, h as usize);
    let pw = PREVIEW_W.min(w).max(1);
    let ph = (pw * h / w).max(1);
    let mut out = vec![0u8; pw * ph];
    for y in 0..ph {
        let sy0 = y * h / ph;
        let sy1 = ((y + 1) * h / ph).max(sy0 + 1).min(h);
        for x in 0..pw {
            let sx0 = x * w / pw;
            let sx1 = ((x + 1) * w / pw).max(sx0 + 1).min(w);
            let mut sum = 0u32;
            let mut cnt = 0u32;
            for yy in sy0..sy1 {
                let row = yy * w;
                for xx in sx0..sx1 {
                    sum += luma[row + xx] as u32;
                    cnt += 1;
                }
            }
            out[y * pw + x] = (sum / cnt.max(1)) as u8;
        }
    }
    PreviewData {
        w: pw as u32,
        h: ph as u32,
        gray: base64::engine::general_purpose::STANDARD.encode(&out),
        orientation,
    }
}

/// Shared context between the camera callback and the decode thread.
struct FrameCtx {
    channel: Channel<DecodedFrame>,
    log: Channel<String>,
    stop: Arc<AtomicBool>,
    fps_range: [i32; 2], // the fps range requested (for the UI)
    orientation: AtomicI32, // sensor orientation, set during open()
    capture_count: AtomicU32, // total camera frames received
    coord_frames: AtomicU32, // coordinator loop iterations (frames handed to workers)
    worker_gen: AtomicU32, // bumped to retire a stalled worker generation (watchdog)
    attempts: AtomicU32, // total decode attempts by the worker pool (success or not)
    decoded_count: AtomicU32, // total successful decodes (pre-dedup)
    forwarded: AtomicU32, // total UNIQUE frames forwarded to JS (post-dedup)
    latest: Mutex<Option<(Vec<u8>, u32, u32)>>, // most recent luma (coordinator drains it)
    signal: Condvar,
    // Coordinator → worker-pool handoff: a bounded FIFO the whole pool pulls from
    // concurrently (each worker pops one frame), so all workers stay fed and decode
    // in parallel. Bounded to `work_max` (≈pool size) with drop-OLDEST, so under
    // load we always keep the freshest frames without ever blocking the coordinator.
    work_queue: Mutex<VecDeque<(Vec<u8>, u32, u32)>>,
    work_signal: Condvar,
    workers: usize,  // decode pool size (from the UI "decode workers" setting)
    work_max: usize, // max frames buffered in work_queue (drop-oldest beyond this)
    outbox: Mutex<VecDeque<DecodedFrame>>, // coordinator/workers → sender (drop-oldest)
    out_signal: Condvar,
    // Native-side dedup: (current sessionId, set of seqs already forwarded). The
    // worker pool re-decodes the same on-screen QR many times; forwarding only the
    // FIRST sighting of each (session,seq) keeps the JS receiver from being flooded
    // with duplicates (which stalled it during the heavy end-game peeling ~90%).
    seen: Mutex<(u16, HashSet<u32>)>,
    grid: usize, // sender's grid (codes per side); workers tile the frame into grid²
    sharpen: bool, // apply a 3×3 sharpen to each decode crop (opt-in test setting)
}

/// Max frames buffered for the sender. Holds one rate-limit window's worth of
/// decodes with headroom; if the UI stalls and sends back up, the decode thread
/// drops the OLDEST queued frame rather than block — losing a few fountain frames
/// is free (they're redundant), a wedged pipeline is not.
const OUTBOX_MAX: usize = 16;

impl FrameCtx {
    /// Queue a decoded frame for the sender thread. Never blocks: if the outbox is
    /// full (sender stuck on a slow send), the oldest frame is dropped.
    fn push_out(&self, f: DecodedFrame) {
        if let Ok(mut q) = self.outbox.lock() {
            while q.len() >= OUTBOX_MAX {
                q.pop_front();
            }
            q.push_back(f);
        }
        self.out_signal.notify_one();
    }

    /// Queue a camera frame for the decode worker pool. Never blocks: keeps the
    /// freshest `work_max` frames (drop-oldest) so a busy pool never stalls the
    /// coordinator, and wakes one free worker to pull it.
    fn push_work(&self, luma: Vec<u8>, w: u32, h: u32) {
        if let Ok(mut q) = self.work_queue.lock() {
            while q.len() >= self.work_max {
                q.pop_front();
            }
            q.push_back((luma, w, h));
        }
        self.work_signal.notify_one();
    }

    /// Called from the camera thread for each frame's Y plane. Just stashes the
    /// latest frame and wakes the decode thread — NO decoding here, so the camera
    /// is never blocked (a slow decode would otherwise stall the buffer queue).
    fn store_frame(&self, luma: Vec<u8>, w: u32, h: u32) {
        if self.stop.load(Ordering::SeqCst) {
            return;
        }
        let n = self.capture_count.fetch_add(1, Ordering::Relaxed) + 1;
        // Heartbeat goes to LOGCAT ONLY (log::info!), never the UI Channel. This
        // callback must do NO Channel::send of ANY kind (data OR log): a blocking
        // send would stall the callback, fill AImageReader's buffer queue, and
        // permanently freeze frame delivery. It only stores the latest frame +
        // wakes the decode thread; every Channel send happens on the decode thread.
        if n <= 5 || n % 30 == 0 {
            log::info!("cam frame #{n} {w}x{h}");
        }
        // MOVE the buffer in (no second copy — the callback already made the only
        // tight copy from the AImage). Drop any un-consumed previous frame.
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some((luma, w, h));
        }
        self.signal.notify_one();
    }
}

/// Extract `(sessionId, seq)` from a decoded frame payload (magic-checked), for
/// native-side dedup. Mirrors the 20-byte header in `protocol.ts`:
/// [0]=0xD1 [1]=0x0C [2..4]=sessionId(u16 LE) [4..8]=seq(u32 LE).
fn frame_id(payload: &[u8]) -> Option<(u16, u32)> {
    if payload.len() < 20 || payload[0] != 0xd1 || payload[1] != 0x0c {
        return None;
    }
    let sid = u16::from_le_bytes([payload[2], payload[3]]);
    let seq = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    Some((sid, seq))
}

// ---------------------------------------------------------------------------
// Native QR decode (rxing) — binary-safe, multiple codes per frame.
// ---------------------------------------------------------------------------
mod decode {
    use rxing::{
        BarcodeFormat, DecodeHints, RXingResult, RXingResultMetadataType, RXingResultMetadataValue,
    };

    /// Wraps rxing so the decode thread just hands us a luma buffer.
    pub struct Decoder;

    impl Decoder {
        pub fn new() -> Self {
            Decoder
        }

        /// Decode QR codes from the grayscale frame. Returns (payloads, attempts).
        /// For `grid` ≥ 2 it tiles the centered square into grid×grid cells and
        /// decodes each, so an N-grid frame yields up to N² codes/frame (vs 1) — the
        /// big throughput lever, since the sender packs N² independent codes per
        /// display frame. Every decode is the bounded reader (TryHarder OFF) on a
        /// small crop, so no path can grind/hang the worker pool.
        pub fn decode(
            &mut self,
            luma: Vec<u8>,
            width: u32,
            height: u32,
            grid: usize,
            sharpen: bool,
        ) -> (Vec<Vec<u8>>, usize) {
            let mut hints = DecodeHints::default();
            hints.TryHarder = Some(false); // bounded — never grinds/hangs
            let (w, h) = (width as usize, height as usize);

            if grid <= 1 {
                // Single code: center crop + pad (its quiet zone may be clipped when
                // framed to fill the view), then a full-frame fallback for a large
                // code that overflows the crop.
                let (crop, side) = crop_center_square(&luma, w, h);
                // Downscale 2× when heavily oversampled. QR decode cost scales with
                // pixel count, and a dense (high-byte) code framed at 2560 has ~12
                // px/module — 6 after 2×, still comfortably decodable. This is the
                // key grid-1 speedup: dense codes at 60fps are otherwise the slowest
                // decode. It never touches grid-2 (its cells are far below the
                // threshold), so grid-2's pixels-per-module is unaffected.
                let (crop, side) = if side >= DOWNSCALE_MIN {
                    let (c, ns, _) = downscale2(&crop, side, side);
                    (c, ns)
                } else {
                    (crop, side)
                };
                let crop = if sharpen { sharpen3x3(&crop, side, side) } else { crop };
                let pad = (side / 12).max(24);
                let (buf, bw, bh) = pad_white(&crop, side, side, pad);
                if let Ok(r) = detect_qr_global(buf, bw as u32, bh as u32, &mut hints) {
                    let p = extract_payload(&r);
                    if !p.is_empty() {
                        return (p, 1);
                    }
                }
                // Fallback: whole frame, downscaled 2× (same as the primary) when
                // oversampled. A FULL-RES fallback was tried and LOST: the primary's
                // misses are blur/focus-limited, not resolution-limited, so full res
                // recovers nothing yet costs 4× the pixels on ~30% of frames — which ate
                // the worker headroom (att/coord fell 100%→94%). Downscaled keeps the
                // pool pinned at the camera rate. Either branch hands rxing an owned
                // buffer with no copy.
                let (fluma, fw, fh) = if w >= DOWNSCALE_MIN {
                    downscale2(&luma, w, h)
                } else {
                    (luma, w, h)
                };
                let fluma = if sharpen { sharpen3x3(&fluma, fw, fh) } else { fluma };
                return match detect_qr_global(fluma, fw as u32, fh as u32, &mut hints) {
                    Ok(r) => (extract_payload(&r), 1),
                    Err(_) => (Vec::new(), 0),
                };
            }

            // GRID (≥2): tile the centered square (where the framed grid sits) into
            // grid×grid cells and decode each. Each cell crop is expanded by a small
            // overlap so a code straddling a boundary from imperfect framing is still
            // captured whole. NO white padding here: the sender bakes a 4-module
            // quiet zone into every cell (MARGIN=4), which the crop already contains —
            // so we hand rxing the raw crop directly. That skips an allocation + copy
            // per cell AND shrinks the image the binarizer scans, which is the hot
            // path (grid² decodes per frame) — a real speedup with no loss of margin.
            let side = w.min(h);
            let x0 = (w - side) / 2;
            let y0 = (h - side) / 2;
            let cell = side / grid;
            if cell == 0 {
                return (Vec::new(), 0);
            }
            let overlap = cell / 8;
            let mut out = Vec::new();
            for gy in 0..grid {
                for gx in 0..grid {
                    let cx = x0 + gx * cell;
                    let cy = y0 + gy * cell;
                    let rx0 = cx.saturating_sub(overlap);
                    let ry0 = cy.saturating_sub(overlap);
                    let rx1 = (cx + cell + overlap).min(w);
                    let ry1 = (cy + cell + overlap).min(h);
                    let (cw, ch) = (rx1 - rx0, ry1 - ry0);
                    let crop = crop_region(&luma, w, rx0, ry0, cw, ch);
                    let crop = if sharpen { sharpen3x3(&crop, cw, ch) } else { crop };
                    if let Ok(r) = rxing::helpers::detect_in_luma_with_hints(
                        crop,
                        cw as u32,
                        ch as u32,
                        Some(BarcodeFormat::QR_CODE),
                        &mut hints,
                    ) {
                        if let Some(bytes) = extract_payload(&r).pop() {
                            if !bytes.is_empty() {
                                out.push(bytes);
                            }
                        }
                    }
                }
            }
            let n = out.len();
            (out, n)
        }
    }

    /// Decode a single QR using the GlobalHistogramBinarizer instead of rxing's
    /// default Hybrid one. For a uniformly self-lit QR screen, a single global
    /// black-point is ~2–3× faster than Hybrid's per-block local thresholding, with
    /// little accuracy loss — a real att/s win. Used ONLY on the grid-1 path; the
    /// grid≥2 path keeps the Hybrid helper (its tiny cells benefit from local
    /// thresholding, and grid-2 must stay unaffected).
    fn detect_qr_global(
        luma: Vec<u8>,
        width: u32,
        height: u32,
        hints: &mut DecodeHints,
    ) -> rxing::common::Result<RXingResult> {
        use rxing::common::GlobalHistogramBinarizer;
        use rxing::{BinaryBitmap, Luma8LuminanceSource, MultiFormatReader, Reader};
        hints.PossibleFormats = Some(std::collections::HashSet::from([BarcodeFormat::QR_CODE]));
        let mut reader = MultiFormatReader::default();
        reader.decode_with_hints(
            &mut BinaryBitmap::new(GlobalHistogramBinarizer::new(Luma8LuminanceSource::new(
                luma, width, height,
            ))),
            hints,
        )
    }

    /// Pull the raw byte payload out of a decoded QR. Prefers the pre-charset
    /// BYTE_SEGMENTS (exact bytes); falls back to the text as ISO-8859-1 (byte
    /// mode's default charset, where each char maps to exactly one source byte).
    fn extract_payload(r: &RXingResult) -> Vec<Vec<u8>> {
        if let Some(RXingResultMetadataValue::ByteSegments(segs)) = r
            .getRXingResultMetadata()
            .get(&RXingResultMetadataType::BYTE_SEGMENTS)
        {
            if !segs.is_empty() {
                return vec![segs.concat()];
            }
        }
        let t = r.getText();
        if !t.is_empty() {
            return vec![t.chars().map(|c| c as u32 as u8).collect()];
        }
        Vec::new()
    }

    /// Surround a luma buffer with a white (255) border of `pad` px — synthesizes
    /// the quiet zone QR detection needs when the code is framed edge-to-edge.
    fn pad_white(buf: &[u8], w: usize, h: usize, pad: usize) -> (Vec<u8>, usize, usize) {
        let nw = w + 2 * pad;
        let nh = h + 2 * pad;
        let mut out = vec![255u8; nw * nh];
        for y in 0..h {
            let dst = (y + pad) * nw + pad;
            out[dst..dst + w].copy_from_slice(&buf[y * w..y * w + w]);
        }
        (out, nw, nh)
    }

    /// 3×3 sharpen (unsharp) convolution: center ×5, orthogonal neighbours ×−1
    /// (kernel sums to 1, so overall brightness is preserved). Crisps soft/blurry
    /// modules — but also amplifies sensor noise, so it's opt-in (the "sharpen"
    /// setting). Borders are copied through unchanged. Allocates one output buffer.
    fn sharpen3x3(src: &[u8], w: usize, h: usize) -> Vec<u8> {
        if w < 3 || h < 3 {
            return src.to_vec();
        }
        let mut out = vec![0u8; w * h];
        out[..w].copy_from_slice(&src[..w]); // top row unchanged
        out[(h - 1) * w..].copy_from_slice(&src[(h - 1) * w..]); // bottom row unchanged
        for y in 1..h - 1 {
            let row = y * w;
            out[row] = src[row]; // left column unchanged
            out[row + w - 1] = src[row + w - 1]; // right column unchanged
            for x in 1..w - 1 {
                let c = row + x;
                let v = 5i32 * src[c] as i32
                    - src[c - 1] as i32
                    - src[c + 1] as i32
                    - src[c - w] as i32
                    - src[c + w] as i32;
                out[c] = v.clamp(0, 255) as u8;
            }
        }
        out
    }

    /// Copy an arbitrary `cw×ch` rectangle at `(x0,y0)` out of the luma plane.
    fn crop_region(luma: &[u8], w: usize, x0: usize, y0: usize, cw: usize, ch: usize) -> Vec<u8> {
        let mut out = vec![0u8; cw * ch];
        for y in 0..ch {
            let src = (y0 + y) * w + x0;
            if src + cw <= luma.len() {
                out[y * cw..(y + 1) * cw].copy_from_slice(&luma[src..src + cw]);
            }
        }
        out
    }

    /// Downscale the grid-1 decode input by 2× only above this side length. At the
    /// 2560 capture (1440 center square, ~12 px/module for a dense code) this halves
    /// to ~6 px/module — still robustly decodable — for a 4× cheaper decode. Lower
    /// capture widths stay full-res (they aren't oversampled enough to spare it).
    const DOWNSCALE_MIN: usize = 1400;

    /// Box-average 2× downscale of a luma plane (halves each dimension). O(n) and
    /// far cheaper than the rxing binarize+detect it saves on an oversampled frame.
    fn downscale2(luma: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
        let nw = w / 2;
        let nh = h / 2;
        let mut out = vec![0u8; nw * nh];
        for y in 0..nh {
            let r0 = (2 * y) * w;
            let r1 = (2 * y + 1) * w;
            let dst = y * nw;
            for x in 0..nw {
                let c = 2 * x;
                let s = luma[r0 + c] as u16
                    + luma[r0 + c + 1] as u16
                    + luma[r1 + c] as u16
                    + luma[r1 + c + 1] as u16;
                out[dst + x] = (s / 4) as u8;
            }
        }
        (out, nw, nh)
    }

    /// Copy the centered `min(w,h)`-side square out of the luma plane (full res).
    fn crop_center_square(luma: &[u8], w: usize, h: usize) -> (Vec<u8>, usize) {
        let s = w.min(h);
        let x0 = (w - s) / 2;
        let y0 = (h - s) / 2;
        let mut out = vec![0u8; s * s];
        for y in 0..s {
            let src = (y0 + y) * w + x0;
            if src + s <= luma.len() {
                out[y * s..(y + 1) * s].copy_from_slice(&luma[src..src + s]);
            }
        }
        (out, s)
    }
}

// ---------------------------------------------------------------------------
// camera2 NDK FFI (Android) — the part to verify/iterate on-device.
// ---------------------------------------------------------------------------
#[cfg(target_os = "android")]
impl CameraSource for native::Camera {
    fn close(&self) {
        native::Camera::close(self); // UFCS → the inherent close (frees handles)
    }
}

#[cfg(target_os = "android")]
mod native {
    use super::FrameCtx;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use std::os::raw::{c_int, c_void};
    use std::ptr::null_mut;
    use std::sync::atomic::Ordering;

    /// Owns the camera2 handles for one session; close() releases them in order.
    pub struct Camera {
        manager: *mut ndk_sys::ACameraManager,
        device: *mut ndk_sys::ACameraDevice,
        session: *mut ndk_sys::ACameraCaptureSession,
        reader: *mut ndk_sys::AImageReader,
        request: *mut ndk_sys::ACaptureRequest,
        target: *mut ndk_sys::ACameraOutputTarget,
        session_output: *mut ndk_sys::ACaptureSessionOutput,
        container: *mut ndk_sys::ACaptureSessionOutputContainer,
        ctx_ptr: *const FrameCtx, // Arc<FrameCtx> leaked into the listener; reclaimed in close()
        // `true` until close() runs. The delayed focus/exposure-lock thread holds
        // this mutex while it touches the session/request, and close() holds it
        // while freeing them + sets it false — so the lock thread can never use a
        // freed handle (it checks the flag under the same lock).
        alive: Arc<Mutex<bool>>,
    }

    // SAFETY: the handles are only touched from the owning thread + close(), and
    // the camera framework serialises its own callbacks.
    unsafe impl Send for Camera {}

    const OK_C: ndk_sys::camera_status_t = ndk_sys::camera_status_t(0);
    const OK_M: ndk_sys::media_status_t = ndk_sys::media_status_t(0);
    static CB_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    unsafe extern "C" fn on_disconnected(_c: *mut c_void, _d: *mut ndk_sys::ACameraDevice) {}
    unsafe extern "C" fn on_error(_c: *mut c_void, _d: *mut ndk_sys::ACameraDevice, _e: c_int) {}
    unsafe extern "C" fn on_session(_c: *mut c_void, _s: *mut ndk_sys::ACameraCaptureSession) {}

    /// Read a metadata entry as an owned i32 slice (empty if the tag is absent).
    unsafe fn meta_i32(chars: *mut ndk_sys::ACameraMetadata, tag: u32) -> Vec<i32> {
        let mut entry: ndk_sys::ACameraMetadata_const_entry = std::mem::zeroed();
        if ndk_sys::ACameraMetadata_getConstEntry(chars, tag, &mut entry) == OK_C
            && entry.count > 0
            && !entry.data.i32_.is_null()
        {
            std::slice::from_raw_parts(entry.data.i32_, entry.count as usize).to_vec()
        } else {
            Vec::new()
        }
    }

    /// Read a single-byte metadata entry (None if the tag is absent).
    unsafe fn meta_u8(chars: *mut ndk_sys::ACameraMetadata, tag: u32) -> Option<u8> {
        let mut entry: ndk_sys::ACameraMetadata_const_entry = std::mem::zeroed();
        if ndk_sys::ACameraMetadata_getConstEntry(chars, tag, &mut entry) == OK_C
            && entry.count > 0
            && !entry.data.u8_.is_null()
        {
            Some(*entry.data.u8_)
        } else {
            None
        }
    }

    /// Read a single RATIONAL metadata entry as f64 (None if absent / zero denom).
    unsafe fn meta_rational(chars: *mut ndk_sys::ACameraMetadata, tag: u32) -> Option<f64> {
        let mut entry: ndk_sys::ACameraMetadata_const_entry = std::mem::zeroed();
        if ndk_sys::ACameraMetadata_getConstEntry(chars, tag, &mut entry) == OK_C
            && entry.count > 0
            && !entry.data.r.is_null()
        {
            let r = *entry.data.r;
            if r.denominator != 0 {
                return Some(r.numerator as f64 / r.denominator as f64);
            }
        }
        None
    }

    impl Camera {
        /// Open the (first, usually rear) camera, wire an AImageReader at (w,h),
        /// demand `fps`, and start a repeating request feeding the reader.
        pub fn open(
            fps: u32,
            w: i32,
            h: i32,
            lock: bool,
            ev: f32,
            ev_auto: bool,
            ctx: Arc<FrameCtx>,
        ) -> Result<Self, String> {
            let logch = ctx.log.clone();
            let logln = |s: String| {
                log::info!("{s}");
                let _ = logch.send(s);
            };
            logln(format!("open() fps={fps} {w}x{h}"));
            unsafe {
                let manager = ndk_sys::ACameraManager_create();
                if manager.is_null() {
                    return Err("ACameraManager_create failed".into());
                }
                let fail = |m: *mut ndk_sys::ACameraManager, msg: &str| -> Result<Camera, String> {
                    ndk_sys::ACameraManager_delete(m);
                    Err(msg.into())
                };

                // Camera id list — pick the first id (id "0" is the rear camera on
                // virtually all devices; a LENS_FACING query could refine this).
                let mut id_list: *mut ndk_sys::ACameraIdList = null_mut();
                if ndk_sys::ACameraManager_getCameraIdList(manager, &mut id_list) != OK_C
                    || id_list.is_null()
                    || (*id_list).numCameras < 1
                {
                    if !id_list.is_null() {
                        ndk_sys::ACameraManager_deleteCameraIdList(id_list);
                    }
                    return fail(manager, "no camera available");
                }
                // ---- Pick the best camera (not just id[0]). Prefer a back-facing
                // sensor, then one that supports the requested resolution, then the
                // requested fps, then the highest-resolution sensor. If the exact
                // requested size isn't offered, snap to the closest supported YUV
                // output size (an unsupported size can silently yield a wrong one). ----
                let fmt = ndk_sys::AIMAGE_FORMATS::AIMAGE_FORMAT_YUV_420_888.0 as i32;
                let tag_facing = ndk_sys::acamera_metadata_tag::ACAMERA_LENS_FACING.0;
                let tag_orient = ndk_sys::acamera_metadata_tag::ACAMERA_SENSOR_ORIENTATION.0;
                let tag_cfgs =
                    ndk_sys::acamera_metadata_tag::ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS.0;
                let tag_fps =
                    ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES.0;
                let ncam = (*id_list).numCameras as usize;
                let req_area = w as i64 * h as i64;
                let mut best_idx = 0usize;
                let mut best_score = i64::MIN;
                let mut best_orient = 0i32;
                let mut rw = w;
                let mut rh = h;
                for i in 0..ncam {
                    let id = *(*id_list).cameraIds.offset(i as isize);
                    let mut chars: *mut ndk_sys::ACameraMetadata = null_mut();
                    if ndk_sys::ACameraManager_getCameraCharacteristics(manager, id, &mut chars)
                        != OK_C
                        || chars.is_null()
                    {
                        continue;
                    }
                    let facing = meta_u8(chars, tag_facing).unwrap_or(1) as i32; // 1 = LENS_FACING_BACK
                    let orient = meta_i32(chars, tag_orient).first().copied().unwrap_or(0);
                    let cfgs = meta_i32(chars, tag_cfgs);
                    let fps_ranges = meta_i32(chars, tag_fps);
                    ndk_sys::ACameraMetadata_free(chars);

                    // Scan YUV output stream configs: [format, w, h, isInput] × N.
                    let mut supports_req = false;
                    let mut max_area = 0i64;
                    let mut snap_w = w;
                    let mut snap_h = h;
                    let mut snap_delta = i64::MAX;
                    let mut k = 0;
                    while k + 3 < cfgs.len() {
                        let (f, cw, ch, is_in) = (cfgs[k], cfgs[k + 1], cfgs[k + 2], cfgs[k + 3]);
                        if f == fmt && is_in == 0 {
                            if cw == w && ch == h {
                                supports_req = true;
                            }
                            let area = cw as i64 * ch as i64;
                            if area > max_area {
                                max_area = area;
                            }
                            let d = (area - req_area).abs();
                            if d < snap_delta {
                                snap_delta = d;
                                snap_w = cw;
                                snap_h = ch;
                            }
                        }
                        k += 4;
                    }
                    // Does any AE target-fps range reach the requested fps?
                    let mut supports_fps = false;
                    let mut j = 0;
                    while j + 1 < fps_ranges.len() {
                        if fps_ranges[j + 1] >= fps as i32 {
                            supports_fps = true;
                        }
                        j += 2;
                    }

                    let mut score = 0i64;
                    if facing == 1 {
                        score += 1_000_000; // back-facing strongly preferred for scanning
                    }
                    if supports_req {
                        score += 400_000;
                    }
                    if supports_fps {
                        score += 200_000;
                    }
                    score += max_area / 10_000; // highest-resolution sensor tiebreak
                    logln(format!(
                        "cam[{i}] facing={facing} reqRes={supports_req} fps{fps}={supports_fps} max={}MP score={score}",
                        max_area / 1_000_000
                    ));
                    if score > best_score {
                        best_score = score;
                        best_idx = i;
                        best_orient = orient;
                        rw = if supports_req { w } else { snap_w };
                        rh = if supports_req { h } else { snap_h };
                    }
                }
                let cam_id = *(*id_list).cameraIds.offset(best_idx as isize);
                ctx.orientation.store(best_orient, Ordering::Relaxed);
                logln(format!(
                    "selected cam[{best_idx}] of {ncam}: {rw}x{rh} orientation={best_orient}°"
                ));

                // YUV_420_888 image reader + its native window (at the chosen size).
                let mut reader: *mut ndk_sys::AImageReader = null_mut();
                let rst = ndk_sys::AImageReader_new(rw, rh, fmt, 4, &mut reader);
                logln(format!("AImageReader_new({rw}x{rh}) status={}", rst.0));
                if rst != OK_M || reader.is_null() {
                    ndk_sys::ACameraManager_deleteCameraIdList(id_list);
                    return fail(manager, "AImageReader_new failed");
                }
                let mut window: *mut ndk_sys::ANativeWindow = null_mut();
                ndk_sys::AImageReader_getWindow(reader, &mut window);

                // Frame listener. Leak the Arc into the C context; reclaimed in close().
                // Keep a clone first so the focus-lock thread can read the live
                // decode counters (attempts/forwarded) to judge whether the lock
                // caught a good focus.
                let lock_ctx = ctx.clone();
                let ctx_ptr = Arc::into_raw(ctx);
                let mut listener = ndk_sys::AImageReader_ImageListener {
                    context: ctx_ptr as *mut c_void,
                    onImageAvailable: Some(on_image_available),
                };
                ndk_sys::AImageReader_setImageListener(reader, &mut listener);

                // Open the device.
                let mut dev_cbs = ndk_sys::ACameraDevice_StateCallbacks {
                    context: null_mut(),
                    onDisconnected: Some(on_disconnected),
                    onError: Some(on_error),
                };
                let mut device: *mut ndk_sys::ACameraDevice = null_mut();
                let st = ndk_sys::ACameraManager_openCamera(manager, cam_id, &mut dev_cbs, &mut device);
                logln(format!("openCamera status={} device_null={}", st.0, device.is_null()));
                ndk_sys::ACameraManager_deleteCameraIdList(id_list); // openCamera copied the id
                if st != OK_C || device.is_null() {
                    ndk_sys::AImageReader_delete(reader);
                    drop(Arc::from_raw(ctx_ptr));
                    return fail(manager, "ACameraManager_openCamera failed");
                }

                // Capture request → reader window, at the requested fps.
                let mut request: *mut ndk_sys::ACaptureRequest = null_mut();
                let crq = ndk_sys::ACameraDevice_createCaptureRequest(
                    device,
                    ndk_sys::ACameraDevice_request_template::TEMPLATE_PREVIEW,
                    &mut request,
                );
                let mut target: *mut ndk_sys::ACameraOutputTarget = null_mut();
                let ot = ndk_sys::ACameraOutputTarget_create(window, &mut target);
                let at = ndk_sys::ACaptureRequest_addTarget(request, target);
                let range = [fps as i32, fps as i32];
                let se = ndk_sys::ACaptureRequest_setEntry_i32(
                    request,
                    ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_TARGET_FPS_RANGE.0,
                    2,
                    range.as_ptr(),
                );
                logln(format!(
                    "request={} target={} addTarget={} fpsSet={}",
                    crq.0, ot.0, at.0, se.0
                ));

                // ---- Screen-scanning camera tuning ----
                // Continuous autofocus keeps the code sharp as the distance shifts.
                let af_mode: u8 = 4; // ACAMERA_CONTROL_AF_MODE_CONTINUOUS_PICTURE
                ndk_sys::ACaptureRequest_setEntry_u8(
                    request,
                    ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AF_MODE.0,
                    1,
                    &af_mode,
                );
                // Negative exposure bias — THE fix for lighting inconsistency. The QR
                // screen emits its own light, so in a dim room the meter reads "dark",
                // lengthens exposure, and the bright flickering code blows out + smears
                // (decode fps collapses). Biasing exposure down freezes the flicker and
                // keeps modules crisp. `ev` is the requested bias (default −1.5); a more
                // negative value shortens exposure further (less motion blur → higher
                // decode success) at the cost of brightness. Applied via the device's
                // comp step, clamped to its supported range.
                // AE-compensation step + range, hoisted so the tune thread can convert
                // EV↔units when `ev_auto` sweeps exposure. `ev_auto` starts at −1.5 and
                // the thread hill-climbs from there by decode success.
                let mut ae_step = 1.0f64 / 3.0;
                let mut ae_range = [-24i32, 24];
                let ev0 = if ev_auto { -1.5 } else { ev };
                let mut chars2: *mut ndk_sys::ACameraMetadata = null_mut();
                if ndk_sys::ACameraManager_getCameraCharacteristics(manager, cam_id, &mut chars2)
                    == OK_C
                    && !chars2.is_null()
                {
                    ae_step = meta_rational(
                        chars2,
                        ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_COMPENSATION_STEP.0,
                    )
                    .unwrap_or(1.0 / 3.0);
                    let cr = meta_i32(
                        chars2,
                        ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_COMPENSATION_RANGE.0,
                    );
                    if cr.len() >= 2 {
                        ae_range = [cr[0], cr[1]];
                    }
                    let units = ((ev0 as f64 / ae_step).round() as i32)
                        .max(ae_range[0])
                        .min(ae_range[1]);
                    let ec = [units];
                    let ecr = ndk_sys::ACaptureRequest_setEntry_i32(
                        request,
                        ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_EXPOSURE_COMPENSATION.0,
                        1,
                        ec.as_ptr(),
                    );
                    logln(format!("ae exposure comp units={units} (ev {ev0}) status={}", ecr.0));
                    ndk_sys::ACameraMetadata_free(chars2);
                }

                // Session output container → capture session.
                let mut session_output: *mut ndk_sys::ACaptureSessionOutput = null_mut();
                ndk_sys::ACaptureSessionOutput_create(window, &mut session_output);
                let mut container: *mut ndk_sys::ACaptureSessionOutputContainer = null_mut();
                ndk_sys::ACaptureSessionOutputContainer_create(&mut container);
                ndk_sys::ACaptureSessionOutputContainer_add(container, session_output);

                let mut sess_cbs = ndk_sys::ACameraCaptureSession_stateCallbacks {
                    context: null_mut(),
                    onClosed: Some(on_session),
                    onReady: Some(on_session),
                    onActive: Some(on_session),
                };
                let mut session: *mut ndk_sys::ACameraCaptureSession = null_mut();
                let cs = ndk_sys::ACameraDevice_createCaptureSession(
                    device,
                    container,
                    &mut sess_cbs,
                    &mut session,
                );
                logln(format!("createCaptureSession status={} session_null={}", cs.0, session.is_null()));
                if cs != OK_C || session.is_null() {
                    ndk_sys::ACameraDevice_close(device);
                    ndk_sys::AImageReader_delete(reader);
                    drop(Arc::from_raw(ctx_ptr));
                    return fail(manager, "createCaptureSession failed");
                }

                // Start the stream.
                let mut seq_id: c_int = 0;
                let rr = ndk_sys::ACameraCaptureSession_setRepeatingRequest(
                    session,
                    null_mut(),
                    1,
                    &mut request,
                    &mut seq_id,
                );
                logln(format!("setRepeatingRequest status={} — streaming, awaiting frames", rr.0));

                let alive = Arc::new(Mutex::new(true));

                // Camera-tuning thread. Runs when focus-lock is on OR exposure is on
                // "auto". Two phases, both scored by the decode-success ratio we already
                // track (decoded/attempt — dedup-independent):
                //   1) EV auto-tune (ev_auto): sweep a few exposure biases, keep the one
                //      that decodes best (lighting-adaptive; starts at −1.5).
                //   2) Focus/exposure LOCK (lock): lock AF+AE so they stop hunting; if the
                //      lock caught a soft focus (low ratio) release and re-hunt, then keep
                //      the best. Locking AE freezes the tuned EV from phase 1.
                // The `alive` mutex makes every request edit race-free vs close().
                if lock || ev_auto {
                    let alive2 = alive.clone();
                    let logch2 = logch.clone();
                    let lctx = lock_ctx; // clone taken before ctx was leaked into the listener
                    let sess = session as usize;
                    let req = request as usize;
                    std::thread::spawn(move || {
                        let session = sess as *mut ndk_sys::ACameraCaptureSession;
                        let mut req_ptr = req as *mut ndk_sys::ACaptureRequest;

                        // Apply any of: EV compensation units, AE lock, one-shot AF trigger
                        // — then re-arm the repeating stream. Returns false if the camera
                        // was closed (bail without touching freed handles).
                        let mut apply =
                            |ev_units: Option<i32>, ae_lock: Option<u8>, af_trigger: Option<u8>| -> bool {
                                let guard = match alive2.lock() {
                                    Ok(g) => g,
                                    Err(_) => return false,
                                };
                                if !*guard {
                                    return false;
                                }
                                unsafe {
                                    if let Some(u) = ev_units {
                                        let ec = [u];
                                        ndk_sys::ACaptureRequest_setEntry_i32(
                                            req_ptr,
                                            ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_EXPOSURE_COMPENSATION.0,
                                            1,
                                            ec.as_ptr(),
                                        );
                                    }
                                    if let Some(l) = ae_lock {
                                        ndk_sys::ACaptureRequest_setEntry_u8(
                                            req_ptr,
                                            ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AE_LOCK.0,
                                            1,
                                            &l,
                                        );
                                    }
                                    if let Some(t) = af_trigger {
                                        ndk_sys::ACaptureRequest_setEntry_u8(
                                            req_ptr,
                                            ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AF_TRIGGER.0,
                                            1,
                                            &t,
                                        );
                                        let mut s1: c_int = 0;
                                        ndk_sys::ACameraCaptureSession_capture(
                                            session, null_mut(), 1, &mut req_ptr, &mut s1,
                                        );
                                        // Return trigger to IDLE so the repeating request
                                        // doesn't keep re-triggering; the lock/cancel took.
                                        let idle: u8 = 0;
                                        ndk_sys::ACaptureRequest_setEntry_u8(
                                            req_ptr,
                                            ndk_sys::acamera_metadata_tag::ACAMERA_CONTROL_AF_TRIGGER.0,
                                            1,
                                            &idle,
                                        );
                                    }
                                    let mut s2: c_int = 0;
                                    ndk_sys::ACameraCaptureSession_setRepeatingRequest(
                                        session, null_mut(), 1, &mut req_ptr, &mut s2,
                                    );
                                }
                                true
                            };

                        // Settle, then measure the decode-success ratio over a window.
                        // Returns (attempts, ratio), or None if the camera was closed.
                        let measure = |settle_ms: u64, window_ms: u64| -> Option<(u32, f32)> {
                            std::thread::sleep(Duration::from_millis(settle_ms));
                            if lctx.stop.load(Ordering::SeqCst) {
                                return None;
                            }
                            let a0 = lctx.attempts.load(Ordering::Relaxed);
                            let d0 = lctx.decoded_count.load(Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(window_ms));
                            if lctx.stop.load(Ordering::SeqCst) {
                                return None;
                            }
                            let da = lctx.attempts.load(Ordering::Relaxed).wrapping_sub(a0);
                            let dd = lctx.decoded_count.load(Ordering::Relaxed).wrapping_sub(d0);
                            Some((da, if da > 0 { dd as f32 / da as f32 } else { 0.0 }))
                        };

                        // ---- Phase 1: EV auto-tune ----
                        if ev_auto {
                            let ev_to_units = |e: f32| {
                                ((e as f64 / ae_step).round() as i32)
                                    .max(ae_range[0])
                                    .min(ae_range[1])
                            };
                            std::thread::sleep(Duration::from_millis(700)); // AE settle + aim
                            if lctx.stop.load(Ordering::SeqCst) {
                                return;
                            }
                            let candidates = [-1.0f32, -1.5, -2.0, -2.5];
                            let mut best_ev = -1.5f32;
                            let mut best_ratio = -1.0f32;
                            for &cand in candidates.iter() {
                                if !apply(Some(ev_to_units(cand)), None, None) {
                                    return;
                                }
                                match measure(450, 550) {
                                    Some((_, r)) => {
                                        if r > best_ratio {
                                            best_ratio = r;
                                            best_ev = cand;
                                        }
                                    }
                                    None => return,
                                }
                            }
                            if !apply(Some(ev_to_units(best_ev)), None, None) {
                                return;
                            }
                            let _ = logch2.send(format!(
                                "ev auto-tuned: best EV={best_ev} (decode ratio={best_ratio:.2})"
                            ));
                        }

                        // ---- Phase 2: focus/exposure lock ----
                        if lock {
                            const EVAL_MS: u64 = 1500;
                            const MIN_ATTEMPTS: u32 = 20;
                            const MIN_RATIO: f32 = 0.55;
                            const MAX_TRIES: u32 = 2;
                            // If EV auto-tune already ran, AE is settled — shorten the converge.
                            let converge = if ev_auto { 400 } else { 1000 };
                            let mut tries = 0u32;
                            loop {
                                std::thread::sleep(Duration::from_millis(converge));
                                if lctx.stop.load(Ordering::SeqCst) {
                                    return;
                                }
                                if !apply(None, Some(1), Some(1)) {
                                    return; // AE_LOCK on + one-shot AF trigger (lock focus)
                                }
                                let (da, ratio) = match measure(0, EVAL_MS) {
                                    Some(x) => x,
                                    None => return,
                                };
                                if da < MIN_ATTEMPTS || ratio >= MIN_RATIO || tries >= MAX_TRIES {
                                    let _ = logch2.send(format!(
                                        "focus/exposure locked (decode ratio={ratio:.2}, refocus tries={tries})"
                                    ));
                                    return;
                                }
                                tries += 1;
                                let _ = logch2.send(format!(
                                    "poor decode after lock (ratio={ratio:.2}) — refocusing #{tries}"
                                ));
                                if !apply(None, Some(0), Some(2)) {
                                    return; // AE_LOCK off + AF cancel (resume continuous AF)
                                }
                            }
                        }
                    });
                }

                Ok(Camera {
                    manager,
                    device,
                    session,
                    reader,
                    request,
                    target,
                    session_output,
                    container,
                    ctx_ptr,
                    alive,
                })
            }
        }

        /// Stop the stream and release every handle. Callbacks are guaranteed to
        /// have stopped (session close + reader delete) before we reclaim the ctx.
        pub fn close(&self) {
            // Hold `alive` across the whole teardown and mark it dead first, so a
            // pending lock thread either finished already or will see false and skip.
            let mut alive = self.alive.lock().unwrap_or_else(|e| e.into_inner());
            *alive = false;
            unsafe {
                log::info!("close(): releasing camera");
                if !self.session.is_null() {
                    ndk_sys::ACameraCaptureSession_stopRepeating(self.session);
                    ndk_sys::ACameraCaptureSession_close(self.session);
                }
                if !self.device.is_null() {
                    ndk_sys::ACameraDevice_close(self.device);
                }
                if !self.reader.is_null() {
                    ndk_sys::AImageReader_delete(self.reader); // stops image callbacks
                }
                if !self.request.is_null() {
                    ndk_sys::ACaptureRequest_free(self.request);
                }
                if !self.target.is_null() {
                    ndk_sys::ACameraOutputTarget_free(self.target);
                }
                if !self.container.is_null() {
                    ndk_sys::ACaptureSessionOutputContainer_free(self.container);
                }
                if !self.session_output.is_null() {
                    ndk_sys::ACaptureSessionOutput_free(self.session_output);
                }
                if !self.manager.is_null() {
                    ndk_sys::ACameraManager_delete(self.manager);
                }
                if !self.ctx_ptr.is_null() {
                    drop(Arc::from_raw(self.ctx_ptr)); // no more callbacks can run now
                }
            }
        }
    }

    /// C callback invoked by AImageReader when a frame is ready. Runs on the
    /// camera thread. Acquire the latest image, extract the Y plane (respecting
    /// row stride), and forward to the decoder.
    #[no_mangle]
    unsafe extern "C" fn on_image_available(
        context: *mut std::os::raw::c_void,
        reader: *mut ndk_sys::AImageReader,
    ) {
        if context.is_null() || reader.is_null() {
            return;
        }
        // Borrow the FrameCtx without taking ownership.
        let ctx = &*(context as *const FrameCtx);
        let n = CB_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut image: *mut ndk_sys::AImage = std::ptr::null_mut();
        let acq = ndk_sys::AImageReader_acquireLatestImage(reader, &mut image);
        if acq != OK_M || image.is_null() {
            if n < 5 {
                // logcat only — the camera callback must never touch a Channel.
                log::warn!("frame cb#{n}: acquireLatestImage status={}", acq.0);
            }
            return;
        }

        let mut width = 0i32;
        let mut height = 0i32;
        ndk_sys::AImage_getWidth(image, &mut width);
        ndk_sys::AImage_getHeight(image, &mut height);
        if n == 0 {
            log::info!("✓ first frame {width}x{height}");
        }

        // Plane 0 is the Y (luma) plane of YUV_420_888.
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut len: i32 = 0;
        let mut row_stride: i32 = 0;
        ndk_sys::AImage_getPlaneRowStride(image, 0, &mut row_stride);
        if ndk_sys::AImage_getPlaneData(image, 0, &mut data, &mut len) == ndk_sys::media_status_t(0)
            && !data.is_null()
            && width > 0
            && height > 0
        {
            let w = width as usize;
            let h = height as usize;
            let stride = row_stride.max(width) as usize;
            let src = std::slice::from_raw_parts(data, len as usize);
            if ctx.grid >= 2 {
                // Grids ONLY ever decode the centered square (the tiled cells tile
                // that square). So copy JUST that square out of the strided source
                // — skip the left/right margins entirely. At 2560x1440 this is a
                // 1440x1440 (2.0MB) buffer instead of 3.7MB, and there's no second
                // copy downstream: ~46% less copy bandwidth + queue memory per frame.
                let side = w.min(h);
                let x0 = (w - side) / 2;
                let y0 = (h - side) / 2;
                let mut luma = vec![0u8; side * side];
                for y in 0..side {
                    let s = (y0 + y) * stride + x0;
                    if s + side <= src.len() {
                        luma[y * side..(y + 1) * side].copy_from_slice(&src[s..s + side]);
                    }
                }
                ctx.store_frame(luma, side as u32, side as u32);
            } else {
                // Grid 1: the decoder center-crops + full-frame fallback, so keep
                // the whole tight w*h frame. Drop the stride padding while copying.
                let mut luma = vec![0u8; w * h];
                for y in 0..h {
                    let s = y * stride;
                    if s + w <= src.len() {
                        luma[y * w..(y + 1) * w].copy_from_slice(&src[s..s + w]);
                    }
                }
                ctx.store_frame(luma, width as u32, height as u32);
            }
        }

        ndk_sys::AImage_delete(image);
    }
}

// ---------------------------------------------------------------------------
// nokhwa capture (desktop: Windows / Linux / macOS). Feeds the SAME decode
// pipeline as camera2 — only the frame source differs. Camera controls (focus/
// exposure lock, EV) are camera2-only for now; desktop uses device defaults.
// ---------------------------------------------------------------------------
#[cfg(not(target_os = "android"))]
mod desktop {
    use super::FrameCtx;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use nokhwa::pixel_format::LumaFormat;
    use nokhwa::utils::{
        CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    };

    pub struct Camera {
        stop: Arc<AtomicBool>,
    }

    impl Camera {
        /// Open the default camera near `w`×`h` @ `fps` and run a capture thread that
        /// feeds grayscale frames into `ctx.store_frame`. The nokhwa `Camera` is
        /// created INSIDE the thread (it isn't `Send` on every backend); open success
        /// or failure is reported back over a one-shot channel so we can surface it.
        pub fn open(fps: u32, w: i32, h: i32, ctx: Arc<FrameCtx>) -> Result<Camera, String> {
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
            std::thread::Builder::new()
                .name("opfer-desktop-cam".into())
                .spawn(move || {
                    let req = RequestedFormat::new::<LumaFormat>(RequestedFormatType::Closest(
                        CameraFormat::new(
                            Resolution::new(w.max(1) as u32, h.max(1) as u32),
                            FrameFormat::MJPEG,
                            fps,
                        ),
                    ));
                    let mut cam = match nokhwa::Camera::new(CameraIndex::Index(0), req) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(Err(format!("open: {e}")));
                            return;
                        }
                    };
                    if let Err(e) = cam.open_stream() {
                        let _ = tx.send(Err(format!("stream: {e}")));
                        return;
                    }
                    ctx.orientation.store(0, Ordering::Relaxed); // desktop cams are upright
                    let res = cam.resolution();
                    let _ = ctx
                        .log
                        .send(format!("desktop camera opened {}x{}", res.width_x, res.height_y));
                    let _ = tx.send(Ok(()));

                    loop {
                        if stop2.load(Ordering::SeqCst) || ctx.stop.load(Ordering::SeqCst) {
                            break;
                        }
                        match cam.frame() {
                            Ok(buf) => {
                                if let Ok(img) = buf.decode_image::<LumaFormat>() {
                                    let iw = img.width();
                                    let ih = img.height();
                                    ctx.store_frame(img.into_raw(), iw, ih);
                                }
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(3)),
                        }
                    }
                    let _ = cam.stop_stream();
                })
                .map_err(|e| format!("spawn: {e}"))?;

            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => Ok(Camera { stop }),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("camera open timed out".into()),
            }
        }
    }

    impl super::CameraSource for Camera {
        fn close(&self) {
            // Signal the capture thread; it releases the device on its next loop check.
            self.stop.store(true, Ordering::SeqCst);
        }
    }
}
