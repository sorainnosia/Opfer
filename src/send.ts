// Sender: turn ANY picked file into an endless fountain-coded QR stream.
//
// Ported from the original web PoC, generalised from "one bundled image" to
// "any file the user picks", using Tauri's native file dialog + filesystem so
// it works identically on desktop and mobile. The file name travels inside the
// payload (see container.ts) so the receiver can save it correctly.
//
// Tuning notes distilled from the parent experiment:
// - Frame payload sets the QR version; denser wins on goodput as long as the
//   receiver can still decode it. 1465 bytes ≈ V27 is a safe middle ground.
// - The mask pattern is pinned (any declared mask is valid to a decoder);
//   this skips the spec's 8-way mask evaluation and speeds generation ~4×.
// - Displays need each frame shown for ≥2 refresh cycles; 24 fps on 60 Hz is
//   comfortable.
// - Error correction stays at L: the fountain layer already handles erasures.

import QRCode from "qrcode";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";
import { basename } from "@tauri-apps/api/path";
import { isTauri, isAndroid, keepAwake } from "./platform";

import { LTEncoder } from "./shared/fountain";
import { HEADER_LEN, fnv1a, packFrame, type FrameHeader } from "./shared/protocol";
import { wrapFile, type CompressMethod } from "./container";

const MARGIN = 4; // quiet-zone modules
const LOOKAHEAD = 3;

let pickBtn: HTMLButtonElement;
let clearBtn: HTMLButtonElement;
let canvas: HTMLCanvasElement;
let stage: HTMLElement;
let status: HTMLElement;
let cfgFps: HTMLSelectElement;
let cfgBytes: HTMLSelectElement;
let cfgEcc: HTMLSelectElement;
let cfgGrid: HTMLSelectElement;
let cfgColor: HTMLSelectElement;
let cfgSize: HTMLInputElement;
let cfgCompress: HTMLSelectElement; // gzip the payload before encoding (opt-in)
let hiddenFileInput: HTMLInputElement; // native WebView picker (Android bridge fix)

let payload: Uint8Array | null = null; // wrapped container (name + file bytes)
let rawContent: Uint8Array | null = null; // original file bytes, kept so we can re-wrap on toggle
let displayName = "";
let generation = 0; // bumped on every restart/stop; stale loops see it and die
let onResize: (() => void) | null = null; // active window-resize handler
let collapsedStageTop = 0; // stage top measured while the settings panel is collapsed

function detachResize(): void {
  if (onResize) {
    window.removeEventListener("resize", onResize);
    onResize = null;
  }
}

export function initSend(): void {
  pickBtn = document.getElementById("pick") as HTMLButtonElement;
  clearBtn = document.getElementById("clear") as HTMLButtonElement;
  canvas = document.getElementById("qr") as HTMLCanvasElement;
  stage = document.getElementById("send-stage")!;
  status = document.getElementById("send-status")!;
  cfgFps = document.getElementById("cfg-fps") as HTMLSelectElement;
  cfgBytes = document.getElementById("cfg-bytes") as HTMLSelectElement;
  cfgEcc = document.getElementById("cfg-ecc") as HTMLSelectElement;
  cfgGrid = document.getElementById("cfg-grid") as HTMLSelectElement;
  cfgColor = document.getElementById("cfg-color") as HTMLSelectElement;
  cfgSize = document.getElementById("cfg-size") as HTMLInputElement;
  cfgCompress = document.getElementById("cfg-compress") as HTMLSelectElement;

  hiddenFileInput = document.getElementById("hidden-file-input") as HTMLInputElement;
  // Android: read the picked file's bytes via the browser File API (the Tauri
  // dialog/fs bridge drops the result on Android, so the QR never appears).
  hiddenFileInput.addEventListener("change", () => {
    const file = hiddenFileInput.files?.[0];
    if (!file) return;
    status.textContent = "reading file…";
    void file
      .arrayBuffer()
      .then((buf) => loadPayload(file.name, new Uint8Array(buf)))
      .catch((err) => {
        status.textContent = `✗ couldn't read file: ${err instanceof Error ? err.message : String(err)}`;
      });
  });

  pickBtn.addEventListener("click", () => void pickFile());
  clearBtn.addEventListener("click", () => clearFile());
  for (const el of [cfgFps, cfgBytes, cfgEcc, cfgGrid, cfgColor, cfgSize]) {
    el.addEventListener("change", () => {
      if (payload) void startStream();
    });
  }
  // Toggling compression changes the CONTAINER itself, so re-wrap from the raw
  // file bytes (not just restart the stream) and re-derive the session/header.
  cfgCompress.addEventListener("change", () => {
    if (rawContent) void loadPayload(displayName, rawContent);
  });
  // Opening/closing the settings panel changes the room above the QR — re-fit.
  document
    .querySelector<HTMLDetailsElement>("#panel-send details.settings")
    ?.addEventListener("toggle", () => onResize?.());
}

/** Called when the Send tab is left — kill the animation loop. The picked file
 * is kept so returning to the tab can resume it (see resumeSend). */
export function stopSend(): void {
  generation++; // any running pump/tick sees a stale gen and returns
  detachResize();
  keepAwake(false); // release the display hold when not streaming
}

/** Called when the Send tab is (re-)entered — restart the loop if a file is
 * still loaded. Without this, leaving and returning leaves a frozen QR. */
export function resumeSend(): void {
  if (payload) void startStream();
}

/** Clear the current file so the "Choose file to send…" button reappears. */
function clearFile(): void {
  stopSend(); // stop the render loop (also detaches the resize handler)
  payload = null;
  rawContent = null;
  displayName = "";
  stage.style.display = "none";
  clearBtn.style.display = "none";
  pickBtn.style.display = "";
  const ctx = canvas.getContext("2d");
  if (ctx) ctx.clearRect(0, 0, canvas.width, canvas.height);
  status.textContent = "Pick any file — it streams as animated QR codes.";
}

async function pickFile(): Promise<void> {
  // Use the WebView/browser's own <input type="file"> when NOT on Tauri desktop:
  //  - plain browser (the web build): there is no Tauri dialog/fs bridge at all.
  //  - Tauri Android: that bridge is unreliable (the picker returns but the result
  //    never reaches JS, so the QR never shows).
  // Only Tauri desktop uses the native dialog (it reads arbitrary paths cleanly).
  if (!isTauri || isAndroid) {
    hiddenFileInput.value = ""; // reset so re-picking the same file still fires change
    hiddenFileInput.click();
    return; // the change handler takes over
  }

  const path = await open({ multiple: false, directory: false, title: "Choose a file to send" });
  if (!path) return;
  status.textContent = "reading file…";
  try {
    const content = await readFile(path);
    const name = await basename(path);
    await loadPayload(name, content);
  } catch (err) {
    status.textContent = `✗ couldn't read file: ${err instanceof Error ? err.message : String(err)}`;
  }
}

/// Take the picked file's name + bytes, wrap them, and start streaming the QR.
async function loadPayload(name: string, content: Uint8Array): Promise<void> {
  displayName = name;
  rawContent = content; // keep the originals so a compress-toggle can re-wrap
  payload = await wrapFile(name, content, cfgCompress.value as CompressMethod);
  // Streaming state: hide "Choose file", reveal "Clear".
  pickBtn.style.display = "none";
  clearBtn.style.display = "";
  await startStream();
  requestWakeLock();
}

async function startStream(): Promise<void> {
  if (!payload) return;
  keepAwake(true); // hold the display on so screen-sleep can't freeze the QR
  const gen = ++generation;
  const bytes = payload;

  const txFps = Number(cfgFps.value);
  const frameBytes = Number(cfgBytes.value);
  const ecc = cfgEcc.value as "L" | "M" | "Q" | "H";
  const displayPx = Number(cfgSize.value);
  const grid = Math.max(1, Number(cfgGrid.value)); // codes per side (1 = single QR)
  const color = cfgColor.value === "on"; // layer 3 codes into R/G/B channels

  const sessionId = (Math.floor(Math.random() * 0xffff) + 1) & 0xffff;
  const blockLen = frameBytes - HEADER_LEN;
  const encoder = new LTEncoder(bytes, blockLen, sessionId);
  const header: FrameHeader = {
    sessionId,
    seq: 0,
    k: encoder.k,
    blockLen,
    totalLen: bytes.length,
    payloadFnv: fnv1a(bytes),
    // Original (pre-compression) file size, so the receiver can show the real
    // file size even when `bytes` is a compressed container.
    origLen: rawContent ? rawContent.length : bytes.length,
  };

  let version: number | undefined; // locked after the first frame
  let modules = 0; // per-QR module count (size)
  let gridSide = 0; // full tiled image side in modules = grid * (size + 2*MARGIN)
  let scale = 1;
  const staging = document.createElement("canvas");
  const queue: ImageData[] = [];
  let nextSeq = 0;

  detachResize(); // drop any handler from a previous stream
  stage.style.display = "block";

  const STAGE_PAD = 12; // .stage padding (6px each side) around the canvas
  const BOTTOM_RESERVE = 72; // room for the hint line below the QR

  const sizeCanvas = () => {
    const dpr = window.devicePixelRatio || 1;
    const total = gridSide;
    // Fit the white QR box within whatever space is left after the header,
    // buttons and settings above it — recomputed live on every resize so the
    // code always fills the (maximized / resized) window without scrolling.
    const BODY_PAD = 16; // body left/right padding (8px each side)
    const availW = window.innerWidth - BODY_PAD - STAGE_PAD;
    // The stage top moves down when the (collapsible) settings panel expands, so
    // measuring it directly would shrink the QR whenever settings are open. Size
    // the height budget from the stage top AS IF settings were collapsed: cache
    // that value while the panel is closed and reuse it while it's open, so the
    // panel state never changes the code size (the page just scrolls instead).
    const settingsEl = document.querySelector<HTMLDetailsElement>("#panel-send details.settings");
    const rawTop = stage.getBoundingClientRect().top;
    if (!settingsEl?.open) collapsedStageTop = rawTop;
    const stageTop = settingsEl?.open && collapsedStageTop ? collapsedStageTop : rawTop;
    const availH = window.innerHeight - stageTop - BOTTOM_RESERVE - STAGE_PAD;
    // Portrait (phones) → fill the WIDTH and let the code run past the fold; the
    // user scrolls. Landscape (desktop) → also bound by the height left under the
    // title/settings so it never exceeds the window.
    const portrait = window.innerWidth < window.innerHeight;
    const limit = portrait ? availW : Math.min(availW, availH);
    const cssBudget = Math.max(120, Math.min(limit, displayPx));
    scale = Math.max(1, Math.floor((cssBudget * dpr) / total));
    staging.width = total;
    staging.height = total;
    // Internal bitmap stays an INTEGER scale of the module grid (crisp), but the
    // DISPLAYED size fills the whole budget — otherwise the integer floor leaves
    // empty margin and the code visibly shrinks when the module count changes
    // (e.g. denser bytes/frame). `image-rendering: pixelated` keeps edges sharp.
    canvas.width = total * scale;
    canvas.height = total * scale;
    // cssBudget is ALREADY in CSS px (from window.inner*), so display at exactly
    // that — do NOT divide by dpr again. The internal bitmap is total*scale
    // device px; the browser upscales it to fill via image-rendering: pixelated.
    canvas.style.width = `${cssBudget}px`;
    canvas.style.height = `${cssBudget}px`;
  };

  onResize = () => {
    if (gen !== generation || !modules) return;
    sizeCanvas();
  };
  window.addEventListener("resize", onResize);

  // Encode the next fountain frame as a QR matrix. Data length is constant, so
  // every QR locks to the same version — grid cells and colour layers align.
  const nextQR = () => {
    const frame = packFrame({ ...header, seq: nextSeq }, encoder.encode(nextSeq));
    nextSeq++;
    return QRCode.create([{ data: frame, mode: "byte" } as unknown as QRCode.QRCodeSegment], {
      errorCorrectionLevel: ecc,
      version,
      maskPattern: 4,
    });
  };

  // One display frame = a grid×grid tiling of QR codes. In colour mode each
  // cell packs three independent codes into the R/G/B channels (3× the data
  // per cell), so a v27 module is dark-in-red / light-in-green / etc. All codes
  // share the same finder patterns (identical for a fixed version), which stay
  // black in every channel and keep each layer independently decodable.
  const makeFrame = (): ImageData => {
    const first = nextQR();
    if (version === undefined) {
      version = first.version;
      modules = first.modules.size;
      gridSide = grid * (modules + 2 * MARGIN);
      sizeCanvas();
      // Show only the file's base name (e.g. "opfer.exe" → "opfer"); no specs.
      status.textContent = displayName.replace(/\.[^./\\]+$/, "");
    }
    const size = modules;
    const cell = size + 2 * MARGIN; // per-QR side incl. quiet zone
    const img = new ImageData(gridSide, gridSide);
    const px = new Uint32Array(img.data.buffer);
    px.fill(0xffffffff);

    const blitMono = (data: readonly number[] | Uint8Array, ox: number, oy: number) => {
      for (let y = 0; y < size; y++) {
        const row = (oy + MARGIN + y) * gridSide + ox + MARGIN;
        const src = y * size;
        for (let x = 0; x < size; x++) if (data[src + x]) px[row + x] = 0xff000000;
      }
    };
    const blitColor = (
      dR: readonly number[] | Uint8Array,
      dG: readonly number[] | Uint8Array,
      dB: readonly number[] | Uint8Array,
      ox: number,
      oy: number,
    ) => {
      for (let y = 0; y < size; y++) {
        const row = (oy + MARGIN + y) * gridSide + ox + MARGIN;
        const src = y * size;
        for (let x = 0; x < size; x++) {
          const r = dR[src + x] ? 0 : 255;
          const g = dG[src + x] ? 0 : 255;
          const b = dB[src + x] ? 0 : 255;
          // ImageData is little-endian RGBA in a Uint32 → 0xAABBGGRR.
          px[row + x] = (0xff << 24) | (b << 16) | (g << 8) | r;
        }
      }
    };

    let firstUsed = false;
    for (let cy = 0; cy < grid; cy++) {
      for (let cx = 0; cx < grid; cx++) {
        const ox = cx * cell;
        const oy = cy * cell;
        if (!color) {
          const qr = firstUsed ? nextQR() : first;
          firstUsed = true;
          blitMono(qr.modules.data, ox, oy);
        } else {
          const qrR = firstUsed ? nextQR() : first;
          firstUsed = true;
          const qrG = nextQR();
          const qrB = nextQR();
          blitColor(qrR.modules.data, qrG.modules.data, qrB.modules.data, ox, oy);
        }
      }
    }
    return img;
  };

  const pump = () => {
    if (gen !== generation) return; // superseded by a settings change / tab leave
    try {
      while (queue.length < LOOKAHEAD) queue.push(makeFrame());
    } catch (err) {
      // e.g. frame bytes over capacity for the chosen ECC level
      status.textContent = `✗ ${err instanceof Error ? err.message : String(err)}`;
      return;
    }
    setTimeout(pump, 0);
  };
  pump();

  const interval = 1000 / txFps;
  let nextAt = performance.now();
  const tick = (now: number) => {
    if (gen !== generation) return;
    requestAnimationFrame(tick);
    if (now < nextAt) return;
    const img = queue.shift();
    if (!img) {
      nextAt = now + interval;
      return;
    }
    staging.getContext("2d")!.putImageData(img, 0, 0);
    const ctx = canvas.getContext("2d")!;
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(staging, 0, 0, canvas.width, canvas.height);
    nextAt += interval;
    if (now - nextAt > 3 * interval) nextAt = now + interval; // fell behind — don't burst
  };
  requestAnimationFrame(tick);
}

function requestWakeLock(): void {
  try {
    void (
      navigator as Navigator & { wakeLock?: { request(t: "screen"): Promise<unknown> } }
    ).wakeLock?.request("screen");
  } catch {
    /* fine without it */
  }
}

