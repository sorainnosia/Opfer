// Receiver: camera → WASM QR decode in workers → fountain decoder → file saved
// natively to the Downloads folder (Tauri fs), under the sender's original name.
//
// Field lessons baked in (from the original PoC):
// - iOS treats `frameRate: {ideal: 60}` as a suggestion and delivers 30.
//   Demand `exact` first (it works at 1280-wide), fall back to `ideal`.
// - requestVideoFrameCallback chains survive a stopped stream and resume on the
//   next one — a generation counter prevents zombie capture loops.
// - Progress must track frames COLLECTED: LT peeling back-loads its solve
//   cascade, so blocks-solved looks stalled and then teleports to done.

import QRCode from "qrcode";
import { invoke, Channel } from "@tauri-apps/api/core";
import { downloadDir, join } from "@tauri-apps/api/path";
import { writeFile } from "@tauri-apps/plugin-fs";
import { save } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";

import { fnv1a } from "./shared/protocol";
import { unwrapFile } from "./container";
import { isTauri, isAndroid, downloadFile } from "./platform";

const OVERHEAD_EST = 1.18; // expected frames ≈ K × this (robust-soliton ε)

let startBtn: HTMLButtonElement;
let stopBtn: HTMLButtonElement;
let video: HTMLVideoElement;
let preview: HTMLElement;
let stats: HTMLElement;
let progressEl: HTMLElement;
let bar: HTMLElement;
let result: HTMLElement;
let settings: HTMLElement;
let metricsEl: HTMLElement;
let nativePreview: HTMLCanvasElement; // low-rate preview canvas for native mode
let nativeLog: HTMLElement; // native camera diagnostics panel
let fpsHint: HTMLElement; // "lower tx fps" nudge shown when capture is slow + nothing decodes
let cameraStartTs = 0; // when the current camera session started (for the hint's grace period)

const metric = (id: string) => document.getElementById(id)!;

let stream: MediaStream | null = null;
let fountain: Worker | null = null; // owns the LTDecoder off the main thread
let startTs = 0;

/** Latest snapshot of the fountain worker's decoder state, for the metrics panel
 * (the decoder itself lives in the worker now — the main thread only mirrors it). */
interface DecoderStats {
  framesNew: number;
  framesDup: number;
  k: number;
  blockLen: number;
  totalLen: number; // transmitted (possibly compressed) size
  origLen: number; // original uncompressed file size (for display)
}
let decoderStats: DecoderStats | null = null;
let captureGen = 0;
let done = false;
let running = false;
let nativeActive = false; // native Android camera path is running (no webview camera)
let statsTimer: number | undefined;
let onPreviewResize: (() => void) | null = null; // window-resize handler for the camera box

/** Size the camera box from the window's width AND height. Camera frames are
 * ~4:3, so the box is fit within whatever space is left below the header,
 * stats and metrics — recomputed live on every resize. */
function sizePreview(): void {
  const top = preview.getBoundingClientRect().top;
  const BOTTOM_RESERVE = 88; // progress bar + result breathing room
  const availH = window.innerHeight - top - BOTTOM_RESERVE;
  const availW = window.innerWidth * 0.985; // near-full width — minimal side padding
  const wByH = availH * (4 / 3); // width implied by the available height at 4:3
  const size = Math.max(220, Math.min(availW, wByH, 1100));
  preview.style.width = `${Math.round(size)}px`;
}

let workers: Worker[] = [];
let busy: boolean[] = [];
const captureTimes: number[] = [];
const decodeTimes: number[] = [];
const dropTimes: number[] = []; // captureFrame calls where every worker was busy
const sendTimes = new Map<number, number>(); // frameId → postMessage time, for decode latency
const decodeLatencies: number[] = []; // recent per-frame decode round-trip times (ms)

const grab = document.createElement("canvas");
let frameId = 0;
let colorDecode = false; // split camera frame into R/G/B planes (must match sender)
let fastDecode = false; // EFFECTIVE fast-decode flag sent to workers (tryHarder off)
let fastMode: "auto" | "off" | "on" = "auto"; // user setting
let autoFast = false; // adaptive controller latched fast decode on (auto mode)
let dropHighCount = 0; // consecutive stats ticks with high frame-drop
// Max QR codes to look for per frame = the sender's grid² (set in start() from the
// grid setting). Matching it lets zxing stop early instead of scanning the whole
// image for up to 9 codes when only, say, 1 (grid 1) exists — a real speedup.
let maxSymbols = 4;

export function initReceive(): void {
  startBtn = document.getElementById("start") as HTMLButtonElement;
  stopBtn = document.getElementById("stop") as HTMLButtonElement;
  video = document.getElementById("video") as HTMLVideoElement;
  preview = document.getElementById("preview")!;
  stats = document.getElementById("recv-status")!;
  progressEl = document.getElementById("progress")!;
  bar = document.getElementById("bar")!;
  result = document.getElementById("result")!;
  settings = document.getElementById("recv-settings")!;
  metricsEl = document.getElementById("metrics")!;
  nativePreview = document.getElementById("native-preview") as HTMLCanvasElement;
  nativeLog = document.getElementById("native-log")!;
  fpsHint = document.getElementById("fps-hint")!;

  startBtn.onclick = () => void start();
  stopBtn.onclick = () => stopReceive();

  // Optimize workers for this device: decoding is CPU-bound, so the most useful
  // parallelism is roughly the core count. Pick the largest offered option ≤ cores.
  const cfgWorkers = document.getElementById("cfg-workers") as HTMLSelectElement;
  const cores = navigator.hardwareConcurrency || 4;
  const opts = [...cfgWorkers.options].map((o) => Number(o.value)).sort((a, b) => a - b);
  const best = opts.filter((n) => n <= cores).pop() ?? opts[0]!;
  cfgWorkers.value = String(best);

  // On the Tauri Android app, default the native camera2 path ON — it bypasses
  // the WebView's 30fps cap. In a plain browser there is no native path, so leave
  // it off and use getUserMedia (the web build's only camera route).
  if (isTauri && isAndroid) {
    (document.getElementById("cfg-native") as HTMLSelectElement).value = "on";
  }
}

/** Called when the Receive tab is left — stop the camera and workers. */
export function stopReceive(): void {
  if (!running) return;
  teardown();
  // reset UI back to the pre-start state so it can be started again
  settings.style.display = "";
  startBtn.style.display = "";
  stopBtn.style.display = "none";
  preview.style.display = "none";
  metricsEl.style.display = "none";
  progressEl.style.display = "none";
  fpsHint.style.display = "none";
  // Deliberately keep the native diagnostics log visible after Stop so its last
  // state can still be read (it's cleared again on the next Start).
  stats.textContent = "Point the camera at the sender's flickering code.";
}

function teardown(): void {
  running = false;
  captureGen++; // kill any capture loop
  if (nativeActive) {
    nativeActive = false;
    void invoke("stop_native_camera").catch(() => undefined);
  }
  stream?.getTracks().forEach((t) => t.stop());
  stream = null;
  for (const w of workers) w.terminate();
  workers = [];
  busy = [];
  if (fountain) {
    fountain.terminate();
    fountain = null;
  }
  decoderStats = null;
  if (statsTimer !== undefined) {
    clearInterval(statsTimer);
    statsTimer = undefined;
  }
  if (onPreviewResize) {
    window.removeEventListener("resize", onPreviewResize);
    onPreviewResize = null;
  }
}

/** True when a getUserMedia rejection is about permission (not missing hardware
 * or unsupported constraints). getUserMedia rejects with a DOMException, which
 * is NOT an Error subclass in every engine — so read name/message defensively. */
function isPermissionError(err: unknown): boolean {
  const e = err as { name?: string; message?: string } | null;
  if (!e) return false;
  const name = e.name ?? "";
  const msg = e.message ?? "";
  return (
    name === "NotAllowedError" ||
    name === "SecurityError" ||
    name === "PermissionDeniedError" /* legacy */ ||
    /permission|denied|not allowed/i.test(msg)
  );
}

/** Open the camera, prioritising the requested frame rate. Phones often expose
 * 60 fps only at a specific/lower resolution, and pinning a width can push the
 * browser onto a 30 fps sensor mode — so if the preferred (resolution + exact
 * fps) fails, we retry demanding the exact fps with NO resolution constraint
 * before finally settling for a best-effort (ideal) frame rate. */
async function openCamera(base: MediaTrackConstraints, fps: number): Promise<MediaStream> {
  const env: MediaTrackConstraints = { facingMode: "environment" };
  const attempts: MediaTrackConstraints[] = [
    { ...base, frameRate: { exact: fps } }, // preferred resolution @ exact fps
    { ...env, frameRate: { exact: fps } }, // ANY resolution @ exact fps (fps wins)
    { ...base, frameRate: { ideal: fps } }, // preferred resolution, best-effort fps
    { ...env, frameRate: { ideal: fps } }, // last resort
  ];
  let lastErr: unknown;
  for (const video of attempts) {
    try {
      return await navigator.mediaDevices.getUserMedia({ audio: false, video });
    } catch (e) {
      if (isPermissionError(e)) throw e; // don't mask a denial as a constraints failure
      lastErr = e;
    }
  }
  throw lastErr ?? new Error("camera unavailable");
}

/** Native Android capture+decode: camera2 streams decoded QR frame bytes over a
 * Tauri Channel (base64), which we feed straight into the fountain decoder. */
type PreviewPv = { w: number; h: number; gray: string; orientation: number };
type NativeFrame = {
  codes: string[];
  fps: [number, number];
  preview?: PreviewPv;
  captures: number;
};

const previewOff = document.createElement("canvas"); // scratch canvas, reused

function drawNativePreview(pv: PreviewPv): void {
  const { w, h, gray, orientation } = pv;
  const bin = atob(gray);
  const rgba = new Uint8ClampedArray(w * h * 4);
  for (let i = 0; i < w * h; i++) {
    const v = bin.charCodeAt(i);
    rgba[i * 4] = v;
    rgba[i * 4 + 1] = v;
    rgba[i * 4 + 2] = v;
    rgba[i * 4 + 3] = 255;
  }
  // Draw the sensor image to a scratch canvas, then rotate it upright onto the
  // visible one by the sensor orientation (rear cameras are usually mounted 90°).
  previewOff.width = w;
  previewOff.height = h;
  previewOff.getContext("2d")!.putImageData(new ImageData(rgba, w, h), 0, 0);
  const rot = (((orientation % 360) + 360) % 360) as number;
  const swap = rot === 90 || rot === 270;
  nativePreview.width = swap ? h : w;
  nativePreview.height = swap ? w : h;
  const ctx = nativePreview.getContext("2d")!;
  ctx.save();
  ctx.translate(nativePreview.width / 2, nativePreview.height / 2);
  ctx.rotate((rot * Math.PI) / 180);
  ctx.drawImage(previewOff, -w / 2, -h / 2);
  ctx.restore();
}

let cameraPermissionGranted = false;

/** The native camera2 (NDK) path does NOT trigger Android's runtime CAMERA
 * permission prompt — only the WebView's getUserMedia does. So before opening
 * camera2, briefly open getUserMedia (which the WebView routes through Android's
 * permission flow, granting android.permission.CAMERA), then release it. Returns
 * false if the user denies. Cached per session so it only prompts once. */
async function ensureCameraPermission(): Promise<boolean> {
  if (cameraPermissionGranted) return true;
  const md = navigator.mediaDevices;
  if (!md?.getUserMedia) return true; // can't pre-request; let native try anyway
  try {
    const stream = await md.getUserMedia({ video: true });
    stream.getTracks().forEach((t) => t.stop());
    // Give the OS a moment to fully release the camera before camera2 grabs it.
    await new Promise((r) => setTimeout(r, 300));
    cameraPermissionGranted = true;
    return true;
  } catch {
    return false;
  }
}

async function startNativeCamera(width: number, fps: number): Promise<void> {
  // Android: camera2 won't raise the runtime permission prompt on its own, so
  // pre-request via getUserMedia. Desktop (nokhwa) accesses the camera directly and
  // grabbing it via getUserMedia first would just contend with nokhwa — skip it.
  if (isAndroid && !(await ensureCameraPermission())) {
    running = false;
    settings.style.display = "";
    startBtn.style.display = "";
    stopBtn.style.display = "none";
    metricsEl.style.display = "none";
    stats.textContent = "✗ camera permission denied — enable Camera for this app in Settings";
    return;
  }

  nativeActive = true;
  metric("m-path").textContent = "native camera";
  stats.textContent = `native camera @${fps}fps — starting…`;

  // Show the low-rate native preview canvas (instead of the <video>) + log panel.
  video.style.display = "none";
  nativePreview.style.display = "block";
  preview.style.display = "block";
  nativeLog.textContent = "";
  nativeLog.style.display = "block";
  sizePreview();
  onPreviewResize = () => {
    if (running) sizePreview();
  };
  window.addEventListener("resize", onPreviewResize);

  // Diagnostics from the native camera path → on-screen log panel.
  const logChannel = new Channel<string>();
  logChannel.onmessage = (line) => {
    nativeLog.textContent += line + "\n";
    nativeLog.scrollTop = nativeLog.scrollHeight;
  };

  let lastCaptures = 0;
  const channel = new Channel<NativeFrame>();
  channel.onmessage = ({ codes, fps: granted, preview: pv, captures }) => {
    if (done) return;
    const now = performance.now();
    // Messages arrive at the DECODE rate; the `captures` counter is the camera
    // frame count, so push its delta to captureTimes → capture fps = camera fps.
    const delta = Math.max(0, Math.min(300, captures - lastCaptures));
    lastCaptures = captures;
    for (let i = 0; i < delta; i++) captureTimes.push(now);
    metric("m-path").textContent = `native ${granted[1]}fps`;
    if (pv) drawNativePreview(pv);
    // Hand the base64 payloads straight to the fountain worker — parseFrame, the
    // base64 decode and the peeling cascade all run off this (Channel-draining) thread.
    routeCodesB64(codes);
  };

  const grid = Number(
    (document.getElementById("cfg-native-grid") as HTMLSelectElement).value,
  );
  const lockFocus =
    (document.getElementById("cfg-native-lock") as HTMLSelectElement).value === "on";
  // The "decode workers" setting drives the native decode pool size too.
  const workers = Number(
    (document.getElementById("cfg-workers") as HTMLSelectElement).value,
  );
  // Exposure bias (EV): more negative = shorter exposure = less motion blur (crisper
  // modules → higher decode success), at the cost of brightness. "auto" lets the
  // camera hill-climb EV by decode success, starting from −1.5.
  const evRaw = (document.getElementById("cfg-native-ev") as HTMLSelectElement).value;
  const evAuto = evRaw === "auto";
  const ev = evAuto ? -1.5 : Number(evRaw);
  // Optional 3×3 sharpen on each decode crop — may crisp soft modules, may amplify noise.
  const sharpen =
    (document.getElementById("cfg-native-sharpen") as HTMLSelectElement).value === "on";
  try {
    await invoke("start_native_camera", {
      channel,
      log: logChannel,
      fps,
      width,
      // 16:9 is far more likely than 4:3 to offer a 60 fps sensor mode.
      height: Math.round((width * 9) / 16),
      grid,
      lock: lockFocus,
      workers,
      ev,
      evAuto,
      sharpen,
    });
  } catch (err) {
    nativeActive = false;
    running = false;
    settings.style.display = "";
    startBtn.style.display = "";
    stopBtn.style.display = "none";
    metricsEl.style.display = "none";
    stats.textContent = `✗ native camera: ${err instanceof Error ? err.message : String(err)}`;
    return;
  }
  statsTimer = window.setInterval(updateStats, 500);
  requestWakeLock();
}

async function start(): Promise<void> {
  // Idempotent: never let a prior run's worker/timer/stream survive into this
  // one. A leaked fountain worker keeps a 100ms stats interval AND a full
  // LTDecoder alive, stealing CPU from the main thread + the new worker — which
  // re-creates the exact main-thread saturation this refactor removed, making
  // every run after the first slow ("reverts to before"). Cheap insurance.
  if (running || fountain || workers.length) teardown();

  // fresh run
  done = false;
  running = true;
  frameId = 0;
  cameraStartTs = performance.now();
  fpsHint.style.display = "none";
  dropTimes.length = 0;
  decodeLatencies.length = 0;
  sendTimes.clear();
  result.replaceChildren();
  video.style.display = ""; // reset to webview defaults; native mode overrides below
  nativePreview.style.display = "none";
  nativeLog.style.display = "none";

  const captureWidth = Number((document.getElementById("cfg-width") as HTMLSelectElement).value);
  const captureFps = Number((document.getElementById("cfg-capfps") as HTMLSelectElement).value);
  const workerCount = Number((document.getElementById("cfg-workers") as HTMLSelectElement).value);
  colorDecode = (document.getElementById("cfg-color-decode") as HTMLSelectElement).value === "on";
  fastMode = (document.getElementById("cfg-fast") as HTMLSelectElement).value as typeof fastMode;
  autoFast = false;
  dropHighCount = 0;
  fastDecode = fastMode === "on"; // "auto" starts thorough, escalates only if frames drop
  // Codes-per-frame = the sender's grid² (reuse the "grid (match sender)" setting).
  // Sizing zxing's symbol cap to this lets it stop scanning early, and drives the
  // worker's grid-aware downscale.
  const symGrid = Math.max(1, Number((document.getElementById("cfg-native-grid") as HTMLSelectElement).value));
  maxSymbols = symGrid * symGrid;
  // Native camera2 only exists under Tauri; a plain browser always uses getUserMedia.
  const nativeMode =
    isTauri && (document.getElementById("cfg-native") as HTMLSelectElement).value === "on";
  settings.style.display = "none";
  startBtn.style.display = "none";
  stopBtn.style.display = "";
  metricsEl.style.display = "grid";

  // Both capture paths feed the same off-thread fountain decoder.
  startFountain();

  // Native Android path: camera2 captures + decodes off-webview (bypasses the
  // 30 fps getUserMedia cap). No live preview — frames never enter the webview.
  if (nativeMode) {
    await startNativeCamera(captureWidth, captureFps);
    return;
  }

  if (!navigator.mediaDevices?.getUserMedia) {
    teardown();
    settings.style.display = "";
    startBtn.style.display = "";
    stopBtn.style.display = "none";
    metricsEl.style.display = "none";
    stats.textContent =
      "✗ camera unavailable — this platform's webview did not grant a secure " +
      "getUserMedia context. Try 'native camera' on Android, or see the README.";
    return;
  }
  preview.style.display = "block";
  sizePreview();
  onPreviewResize = () => {
    if (running) sizePreview();
  };
  window.addEventListener("resize", onPreviewResize);

  const base: MediaTrackConstraints = {
    facingMode: "environment",
    width: { ideal: captureWidth },
    height: { ideal: Math.round((captureWidth * 3) / 4) },
  };
  // Requesting the stream is what raises the OS/webview permission prompt.
  // Because this runs on every Start press, pressing Start again after a denial
  // re-requests access (and re-prompts wherever the platform allows it).
  try {
    stream = await openCamera(base, captureFps);
  } catch (err) {
    teardown(); // also removes the resize listener added above
    settings.style.display = "";
    startBtn.style.display = "";
    stopBtn.style.display = "none";
    preview.style.display = "none";
    metricsEl.style.display = "none";
    if (isPermissionError(err)) {
      stats.textContent =
        '✗ camera permission not granted — press "Start camera" to request it ' +
        "again. If no prompt appears, allow camera access for this app in your " +
        "system / site settings, then retry.";
    } else {
      stats.textContent = `✗ camera: ${err instanceof Error ? err.message : String(err)}`;
    }
    return;
  }
  video.srcObject = stream;
  await video.play().catch(() => undefined);
  const track = stream.getVideoTracks()[0];
  const s = track?.getSettings();
  const caps = track?.getCapabilities?.();
  const maxFps = caps?.frameRate?.max;
  const gotFps = Math.round(Number(s?.frameRate) || 0);
  // Show what the camera ACTUALLY granted plus its max supported fps: if granted
  // and max are both 30, this webview/camera only offers 30 fps (a platform
  // limit, not a bug); if max is 60 but granted 30, it's a constraints problem.
  stats.textContent =
    `camera ${s?.width}×${s?.height}@${gotFps}fps` +
    (maxFps ? ` (cam max ${Math.round(maxFps)}fps)` : "") +
    " — searching for a stream…";

  for (let i = 0; i < workerCount; i++) {
    const w = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
    const slot = i;
    w.onmessage = (e: MessageEvent) => {
      const { id, results } = e.data as { id: number; results: Uint8Array[] };
      if (id === -1) return; // warm-up
      busy[slot] = false;
      const t0 = sendTimes.get(id);
      if (t0 !== undefined) {
        sendTimes.delete(id);
        decodeLatencies.push(performance.now() - t0);
        if (decodeLatencies.length > 60) decodeLatencies.shift();
      }
      // A frame can hold many codes — forward them all to the fountain worker.
      if (results.length) routeBytes(results.map((b) => b.buffer as ArrayBuffer));
    };
    workers.push(w);
    busy.push(false);
  }

  // Confirm the OffscreenCanvas path actually decodes on this webview before
  // relying on it; if it can't round-trip a known QR, fall back to the
  // main-thread capture path so decoding can't silently break.
  if (useBitmap && workers[0]) {
    const ok = await verifyBitmapPath(workers[0]);
    if (!ok) {
      useBitmap = false;
      stats.textContent =
        `camera ${s?.width}×${s?.height}@${s?.frameRate} — searching (fast capture off)…`;
    }
  }

  metric("m-path").textContent = useBitmap ? "gpu bitmap" : "cpu main";
  captureGen++;
  scheduleFrame(captureGen);
  statsTimer = window.setInterval(updateStats, 500);
  requestWakeLock();
}

type VideoRVFC = HTMLVideoElement & { requestVideoFrameCallback?: (cb: () => void) => number };

function scheduleFrame(gen: number): void {
  if (done || gen !== captureGen) return;
  const v = video as VideoRVFC;
  const next = () => {
    if (done || gen !== captureGen) return;
    void captureFrame();
    scheduleFrame(gen);
  };
  if (v.requestVideoFrameCallback) v.requestVideoFrameCallback(next);
  else requestAnimationFrame(next);
}

// Prefer capturing each frame as an ImageBitmap and doing the pixel read
// (getImageData) inside the worker via OffscreenCanvas — that keeps the per-frame
// read off the main thread, which otherwise throttles capture. Falls back to a
// main-thread read where those APIs are missing.
// Whether to use the ImageBitmap/OffscreenCanvas capture path. Starts on if the
// APIs exist, but is confirmed by an actual decode round-trip at camera start
// (verifyBitmapPath) and flipped off if this webview's OffscreenCanvas hands back
// blank frames — so it can never silently zero the decode.
let useBitmap =
  typeof createImageBitmap === "function" && typeof OffscreenCanvas !== "undefined";

/** A small, known-decodable QR rendered to ImageData — the probe for the
 * OffscreenCanvas self-test. */
function makeTestQRImageData(): ImageData {
  const MARGIN = 4;
  const SCALE = 5; // modules upscaled so the probe decodes trivially
  const qr = QRCode.create("OPFER-SELFTEST", { errorCorrectionLevel: "L" });
  const size = qr.modules.size;
  const data = qr.modules.data;
  const total = size + 2 * MARGIN;
  const px = total * SCALE;
  const img = new ImageData(px, px);
  const buf = new Uint32Array(img.data.buffer);
  buf.fill(0xffffffff);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      if (!data[y * size + x]) continue;
      const bx = (x + MARGIN) * SCALE;
      const by = (y + MARGIN) * SCALE;
      for (let dy = 0; dy < SCALE; dy++) {
        const row = (by + dy) * px + bx;
        for (let dx = 0; dx < SCALE; dx++) buf[row + dx] = 0xff000000;
      }
    }
  }
  return img;
}

/** Round-trip the probe QR through the real ImageBitmap → worker → OffscreenCanvas
 * → zxing path. Returns false if createImageBitmap throws or the worker decodes
 * nothing (i.e. OffscreenCanvas handed it a blank frame on this webview). */
async function verifyBitmapPath(worker: Worker): Promise<boolean> {
  let bitmap: ImageBitmap;
  try {
    bitmap = await createImageBitmap(makeTestQRImageData());
  } catch {
    return false;
  }
  const saved = worker.onmessage;
  const ok = await new Promise<boolean>((resolve) => {
    const timer = setTimeout(() => resolve(false), 3000); // WASM slow to warm → assume broken
    worker.onmessage = (e: MessageEvent) => {
      const { id, results } = e.data as { id: number; results?: Uint8Array[] };
      if (id === -1) return; // ignore the worker's warm-up ping
      clearTimeout(timer);
      resolve(!!results && results.length > 0);
    };
    worker.postMessage({ id: -2, bitmap, color: false, fast: false, maxSymbols: 1 }, [bitmap]);
  });
  worker.onmessage = saved;
  return ok;
}

async function captureFrame(): Promise<void> {
  const vw = video.videoWidth;
  const vh = video.videoHeight;
  if (!vw || !vh) return;
  const now = performance.now();
  captureTimes.push(now);
  const slot = busy.indexOf(false);
  if (slot === -1) {
    dropTimes.push(now); // all workers busy — dropped (a sign decode can't keep up)
    return;
  }
  busy[slot] = true;

  if (useBitmap) {
    try {
      const bitmap = await createImageBitmap(video);
      if (done || !running) {
        bitmap.close();
        busy[slot] = false;
        return;
      }
      const id = frameId++;
      sendTimes.set(id, performance.now());
      workers[slot]!.postMessage(
        { id, bitmap, color: colorDecode, fast: fastDecode, maxSymbols },
        [bitmap],
      );
    } catch {
      busy[slot] = false; // frame grab failed — free the slot, try next frame
    }
    return;
  }

  // Fallback: read pixels on the main thread — cropped to the centered square
  // (same reasoning as the worker path: the QR grid is square and centered).
  if (grab.width !== vw || grab.height !== vh) {
    grab.width = vw;
    grab.height = vh;
  }
  const ctx = grab.getContext("2d", { willReadFrequently: true })!;
  ctx.drawImage(video, 0, 0);
  const side = Math.min(vw, vh);
  const img = ctx.getImageData((vw - side) >> 1, (vh - side) >> 1, side, side);
  const id = frameId++;
  sendTimes.set(id, performance.now());
  workers[slot]!.postMessage(
    { id, buf: img.data.buffer, w: side, h: side, color: colorDecode, fast: fastDecode, maxSymbols },
    [img.data.buffer],
  );
}

/** Spin up the fountain-decode worker and wire its messages. The worker owns the
 * LTDecoder; the main thread just forwards decoded QR payloads to it and mirrors
 * the progress/stats/completion it posts back. */
function startFountain(): void {
  decoderStats = null;
  fountain = new Worker(new URL("./fountain-worker.ts", import.meta.url), { type: "module" });
  fountain.onmessage = (e: MessageEvent) => {
    const m = e.data as
      | { type: "session"; k: number; blockLen: number; totalLen: number; origLen: number }
      | { type: "stats"; framesNew: number; framesDup: number; k: number; blockLen: number; totalLen: number; origLen: number }
      | { type: "done"; payload: ArrayBuffer; hashOk: boolean; totalLen: number; seconds: number };
    if (m.type === "session") {
      startTs = performance.now();
      decoderStats = { framesNew: 0, framesDup: 0, k: m.k, blockLen: m.blockLen, totalLen: m.totalLen, origLen: m.origLen };
      progressEl.style.display = "block";
    } else if (m.type === "stats") {
      decoderStats = {
        framesNew: m.framesNew,
        framesDup: m.framesDup,
        k: m.k,
        blockLen: m.blockLen,
        totalLen: m.totalLen,
        origLen: m.origLen,
      };
      // The worker throttles these to ≤10/s, so updating the bar here is cheap
      // (no per-frame reflow) yet still smooth.
      const progress = Math.min(0.99, m.framesNew / (m.k * OVERHEAD_EST));
      bar.style.width = `${(progress * 100).toFixed(1)}%`;
    } else if (m.type === "done") {
      void finish(new Uint8Array(m.payload), m.hashOk, m.seconds);
    }
  };
  fountain.postMessage({ type: "reset" });
}

/** Native path: forward raw base64 QR payloads to the worker (the base64 decode
 * happens there too, off the main thread). Counts each code as a decode. */
function routeCodesB64(codes: string[]): void {
  if (done || !fountain || codes.length === 0) return;
  const t = performance.now();
  for (let i = 0; i < codes.length; i++) decodeTimes.push(t);
  fountain.postMessage({ type: "b64", codes });
}

/** Webview path: forward decoded frame bytes to the worker, transferring the
 * backing buffers so there's no copy. Counts each buffer as a decode. */
function routeBytes(bufs: ArrayBuffer[]): void {
  if (done || !fountain || bufs.length === 0) return;
  const t = performance.now();
  for (let i = 0; i < bufs.length; i++) decodeTimes.push(t);
  fountain.postMessage({ type: "bytes", bufs }, bufs);
}

async function finish(payload: Uint8Array, hashOk: boolean, seconds: number): Promise<void> {
  done = true;
  teardown();
  preview.style.display = "none";
  fpsHint.style.display = "none";
  // transfer done — offer a fresh scan
  stopBtn.style.display = "none";
  startBtn.style.display = "";
  settings.style.display = "";
  bar.style.width = "100%";

  const unwrapped = await unwrapFile(payload);
  const name = unwrapped?.name || "received.bin";
  const content = unwrapped?.content ?? payload;
  // Report the ORIGINAL (decompressed) file size, and an effective rate over it —
  // so a compressed transfer shows the real file size and its true delivery speed.
  const kb = Math.round(content.length / 1024);
  const rate = (content.length / 1024 / seconds).toFixed(1);

  const heading = document.createElement("div");
  heading.className = "done";
  heading.textContent = hashOk ? "Transfer Complete!" : "Transfer finished — hash MISMATCH ✗";
  result.append(heading);

  stats.textContent = `${name} · ${kb} KB in ${seconds.toFixed(1)} s · ${rate} KB/s · hash ${
    hashOk ? "verified ✓" : "MISMATCH ✗"
  }`;

  // Save the received file. Large files take a moment to write across the
  // JS↔native bridge, so show immediate "Saving…" feedback, and NEVER lose the
  // file — every path falls back to app storage rather than dropping it.
  //  - Desktop → user-profile Downloads (fast, silent), Save dialog as fallback.
  //  - Android → system Save flow (SAF) into the public Download folder; if that
  //    is cancelled/fails, fall back to app storage so the file is still kept.
  const saveMsg = document.createElement("div");
  saveMsg.className = "saved-path";
  saveMsg.textContent = `Saving ${name} (${kb} KB)…`;
  result.append(saveMsg);

  try {
    if (!isTauri) {
      // Web build: no native fs — hand the file to the browser as a download.
      const saved = downloadFile(name, content);
      saveMsg.textContent = `Downloaded ${saved} — check your browser's downloads`;
    } else if (isAndroid) {
      const safeName = name.replace(/[/\\]/g, "_");
      try {
        // Fast native write straight into the public Download folder. Works on
        // Android ≤10 (legacy storage); throws on 11+ scoped storage.
        const saved = await writeFileRaw(`/storage/emulated/0/Download/${safeName}`, content);
        saveMsg.textContent = `Saved to Downloads: ${saved}`;
      } catch {
        try {
          // Scoped storage blocked the direct write — go through the SAF dialog.
          await saveViaDialog(name, content);
          saveMsg.textContent = `Saved to Downloads: ${name}`;
        } catch {
          // Never lose the file: keep it in app storage via the fast native write.
          const fallback = await writeFileRaw(await join(await downloadDir(), safeName), content);
          saveMsg.textContent = `Saved to app storage: ${fallback}`;
        }
      }
    } else {
      let savedPath = "";
      try {
        savedPath = await saveToDownloads(name, content);
      } catch {
        savedPath = await saveViaDialog(name, content);
      }
      saveMsg.textContent = `Saved to: ${savedPath}`;
      const reveal = document.createElement("button");
      reveal.className = "primary";
      reveal.textContent = "Show in folder";
      reveal.onclick = () => void revealItemInDir(savedPath).catch(() => undefined);
      result.append(reveal);
    }
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    saveMsg.textContent = `✗ could not save file: ${msg}`;
  }

  // Inline preview for images.
  if (/\.(png|jpe?g|gif|webp|bmp|svg)$/i.test(name)) {
    const type = name.toLowerCase().endsWith(".svg") ? "image/svg+xml" : "image/*";
    const img = document.createElement("img");
    img.className = "received";
    img.src = URL.createObjectURL(new Blob([content as BlobPart], { type }));
    result.append(img);
  }
}

/** Write bytes via the native `write_file_raw` command, passing the payload as
 * the RAW IPC body ([u32 pathLen][path][data]) instead of a JSON array — the
 * fast path that doesn't choke the JS↔native bridge on large files. Rust creates
 * parent dirs and de-dups the filename, and returns the actual path written. */
interface WriteResult {
  path: string;
  fnv: number;
}

async function writeFileRaw(path: string, data: Uint8Array): Promise<string> {
  const want = fnv1a(data);
  const pathBytes = new TextEncoder().encode(path);
  const body = new Uint8Array(4 + pathBytes.length + data.length);
  new DataView(body.buffer).setUint32(0, pathBytes.length, true);
  body.set(pathBytes, 4);
  body.set(data, 4 + pathBytes.length);
  // Fast path: pass the ArrayBuffer (not the Uint8Array view) so Tauri sends it
  // as the raw request body rather than JSON. Then verify the bytes survived the
  // trip — some IPC bridges (notably Android with large payloads) mangle/truncate
  // binary bodies, which would silently corrupt the saved file.
  try {
    const res = await invoke<WriteResult>("write_file_raw", body.buffer);
    if ((res.fnv >>> 0) === want) return res.path;
  } catch {
    /* fall through to the byte-safe path */
  }
  // Byte-safe fallback: JSON array of numbers — slower but immune to binary
  // transport corruption.
  const res = await invoke<WriteResult>("write_file_json", { path, data: Array.from(data) });
  if ((res.fnv >>> 0) !== want) throw new Error("file corrupted on write (both paths)");
  return res.path;
}

/** Desktop save: write straight into the user-profile Downloads folder
 * (e.g. C:\Users\<you>\Downloads) via the fast native command. */
async function saveToDownloads(name: string, content: Uint8Array): Promise<string> {
  const target = await join(await downloadDir(), name);
  return writeFileRaw(target, content);
}

/** Android (and desktop fallback) save: the system Save flow. On Android this
 * uses the Storage Access Framework, which returns a writable handle into the
 * REAL public Download folder — the only route Android's scoped storage allows
 * without special "all files" access. Returns the chosen location. */
async function saveViaDialog(name: string, content: Uint8Array): Promise<string> {
  const chosen = await save({ defaultPath: name, title: "Save to Downloads" });
  if (!chosen) throw new Error("save cancelled");
  await writeFile(chosen, content);
  return chosen;
}

function updateStats(): void {
  if (done || !running) return;
  const now = performance.now();
  const prune = (a: number[]) => {
    while (a.length > 0 && a[0]! < now - 2000) a.shift();
  };
  prune(captureTimes);
  prune(decodeTimes);
  prune(dropTimes);
  const capFps = captureTimes.length / 2;
  metric("m-cap").textContent = capFps.toFixed(0);
  metric("m-dec").textContent = (decodeTimes.length / 2).toFixed(1);

  // "Lower the sender's tx fps" nudge: when the camera can't sustain 30 fps AND
  // nothing has decoded yet, the sender is almost certainly changing faster than
  // the camera can catch each frame. Give it a few seconds' grace after start so
  // it doesn't flash during camera warm-up.
  const camElapsed = (now - cameraStartTs) / 1000;
  const nothingDecoded = !decoderStats || decoderStats.framesNew === 0;
  fpsHint.style.display =
    camElapsed > 4 && capFps > 0 && capFps < 30 && nothingDecoded ? "" : "none";
  const cap = captureTimes.length;
  const dropPct = cap ? Math.round((dropTimes.length / cap) * 100) : 0;
  metric("m-drop").textContent = `${(dropTimes.length / 2).toFixed(0)}/s · ${dropPct}%`;
  const lat = decodeLatencies.length
    ? decodeLatencies.reduce((a, b) => a + b, 0) / decodeLatencies.length
    : 0;
  metric("m-declat").textContent = lat ? `${lat.toFixed(0)} ms` : "—";

  // Adaptive optimizer: if capture frames are being dropped, decode isn't keeping
  // up. In "auto" mode, once drops persist, latch fast decode on (tryHarder off,
  // ~2× faster per decode) so decode catches up to capture. One-way per session
  // to avoid oscillation.
  if (fastMode === "auto" && !autoFast) {
    dropHighCount = cap >= 20 && dropPct >= 25 ? dropHighCount + 1 : 0;
    if (dropHighCount >= 3) {
      autoFast = true;
      fastDecode = true;
    }
  }
  metric("m-path").textContent = `${useBitmap ? "gpu" : "cpu"}${fastDecode ? " · fast" : ""}`;
  const ds = decoderStats;
  if (!ds) return;
  const elapsed = (now - startTs) / 1000;
  const kbs = (ds.framesNew * ds.blockLen) / OVERHEAD_EST / 1024 / Math.max(0.1, elapsed);
  metric("m-rate").textContent = `${kbs.toFixed(1)} KB/s`;
  metric("m-time").textContent = `${elapsed.toFixed(0)} s`;
  metric("m-frames").textContent = `${ds.framesNew}/${ds.framesDup}`;
  metric("m-k").textContent = String(ds.k);
  metric("m-block").textContent = `${ds.blockLen} B`;
  // Show the ORIGINAL file size (not the transmitted/compressed size). Fall back
  // to the transmitted size for senders that don't report it (origLen 0).
  const fileLen = ds.origLen || ds.totalLen;
  metric("m-payload").textContent = `${Math.round(fileLen / 1024)} KB`;
}

function requestWakeLock(): void {
  try {
    void (
      navigator as Navigator & { wakeLock?: { request(t: "screen"): Promise<unknown> } }
    ).wakeLock?.request("screen");
  } catch {
    /* fine */
  }
}
