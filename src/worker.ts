// QR decode worker: zxing-cpp compiled to WASM. WKWebView (iOS) and Safari
// have never shipped BarcodeDetector, and WebView2 support is uneven, so WASM
// is the only portable way. One frame in flight per worker; the main thread
// drops frames when all workers are busy. Frames are disposable — the fountain
// doesn't care.
//
// Two throughput modes are supported per frame:
//  - grid: one camera image can hold many QR codes at once, so we ask zxing for
//    up to `maxSymbols` and return every valid payload.
//  - colour: the sender packs three codes into the R/G/B channels, so we split
//    the image into three grayscale planes and decode each independently.

import wasmUrl from "zxing-wasm/reader/zxing_reader.wasm?url";
import { prepareZXingModule, readBarcodes } from "zxing-wasm/reader";

prepareZXingModule({
  overrides: {
    locateFile: (path: string, prefix: string) =>
      path.endsWith(".wasm") ? wasmUrl : prefix + path,
  },
});

const ctx = self as unknown as {
  onmessage: ((e: MessageEvent) => void) | null;
  postMessage(msg: unknown, transfer?: Transferable[]): void;
};

async function decodeImage(
  img: ImageData,
  maxSymbols: number,
  fast: boolean,
): Promise<Uint8Array[]> {
  const found = await readBarcodes(img, {
    formats: ["QRCode"],
    maxNumberOfSymbols: maxSymbols,
    // Always skip these — pure wasted work for this channel:
    //  - tryRotate: the sender's codes are always upright.
    //  - tryInvert: always black-on-white, never inverted.
    //  - tryDownscale: after the centered-square crop the grid codes decode at
    //    full resolution, so the extra reduced-res scan pass just burns CPU.
    tryRotate: false,
    tryInvert: false,
    tryDownscale: false,
    // "fast decode" additionally drops the thorough search (tryHarder) — ~2×
    // faster per decode, at the cost of some marginal/blurry codes.
    tryHarder: !fast,
  });
  const out: Uint8Array[] = [];
  for (const r of found) if (r.isValid && r.bytes.length > 0) out.push(r.bytes);
  return out;
}

/** Build a grayscale ImageData from a single RGBA channel (0=R, 1=G, 2=B). */
function channelPlane(rgba: Uint8ClampedArray, w: number, h: number, ch: number): ImageData {
  const gray = new Uint8ClampedArray(w * h * 4);
  for (let i = 0; i < w * h; i++) {
    const v = rgba[i * 4 + ch]!;
    gray[i * 4] = v;
    gray[i * 4 + 1] = v;
    gray[i * 4 + 2] = v;
    gray[i * 4 + 3] = 255;
  }
  return new ImageData(gray, w, h);
}

// Reused OffscreenCanvas for the ImageBitmap path — reading the frame's pixels
// here keeps getImageData off the main thread.
let off: OffscreenCanvas | null = null;
let offCtx: OffscreenCanvasRenderingContext2D | null = null;

// Keep ~this many pixels per code (per grid cell) after downscaling — enough for
// a dense QR (~4-6 px/module) while cutting the pixel count zxing has to scan.
const PX_PER_CODE = 640;

function imageFromBitmap(bitmap: ImageBitmap, grid: number): ImageData {
  // Crop to the centered square: the QR grid is square and aimed at the center,
  // so the off-square bands are background — scanning them just wastes CPU.
  const side = Math.min(bitmap.width, bitmap.height);
  const ox = (bitmap.width - side) >> 1;
  const oy = (bitmap.height - side) >> 1;
  // Downscale during the (GPU) crop draw when oversampled: zxing decode cost is
  // ∝ pixels, and the camera usually delivers far more resolution than a QR needs.
  // Target ~PX_PER_CODE per grid cell so a grid-N frame keeps N×640 across — i.e.
  // grid 1 shrinks aggressively, grid 2/3 keep enough per-code resolution.
  const dst = Math.min(side, PX_PER_CODE * Math.max(1, grid));
  if (!off || off.width !== dst || off.height !== dst) {
    off = new OffscreenCanvas(dst, dst);
    offCtx = off.getContext("2d", { willReadFrequently: true });
  }
  offCtx!.drawImage(bitmap, ox, oy, side, side, 0, 0, dst, dst);
  bitmap.close();
  return offCtx!.getImageData(0, 0, dst, dst);
}

ctx.onmessage = async (e: MessageEvent) => {
  const d = e.data as {
    id: number;
    color: boolean;
    fast: boolean;
    maxSymbols: number;
    bitmap?: ImageBitmap;
    buf?: ArrayBuffer;
    w?: number;
    h?: number;
  };
  const { id, color, fast, maxSymbols } = d;
  // maxSymbols is grid² (max codes in frame) — recover the grid to size the downscale.
  const grid = Math.max(1, Math.round(Math.sqrt(maxSymbols)));
  try {
    const img = d.bitmap
      ? imageFromBitmap(d.bitmap, grid)
      : new ImageData(new Uint8ClampedArray(d.buf!), d.w!, d.h!);
    let results: Uint8Array[];
    if (color) {
      const rgba = img.data;
      const planes = await Promise.all([
        decodeImage(channelPlane(rgba, img.width, img.height, 0), maxSymbols, fast),
        decodeImage(channelPlane(rgba, img.width, img.height, 1), maxSymbols, fast),
        decodeImage(channelPlane(rgba, img.width, img.height, 2), maxSymbols, fast),
      ]);
      results = planes.flat();
    } else {
      results = await decodeImage(img, maxSymbols, fast);
    }
    ctx.postMessage({ id, results });
  } catch {
    ctx.postMessage({ id, results: [] });
  }
};

// warm the WASM so the first real frame doesn't pay instantiation
void readBarcodes(new ImageData(8, 8), { formats: ["QRCode"] })
  .catch(() => undefined)
  .then(() => ctx.postMessage({ id: -1, results: [] }));
