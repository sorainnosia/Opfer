// Fountain-decode worker: owns the LTDecoder so the peeling cascade, index
// sampling and XOR reductions run OFF the main thread. At large K (e.g. a 5 MB
// file → ~3500 blocks) that per-frame math — plus the base64 decode and the
// per-frame progress-bar reflow it used to trigger — saturated the main thread,
// which also has to drain the Tauri Channel and paint the preview. Moving it
// here keeps the main thread a thin router.
//
// Protocol
//   main → worker:
//     { type: "reset" }                       start a fresh transfer
//     { type: "b64", codes: string[] }        native path: raw base64 QR payloads
//     { type: "bytes", bufs: ArrayBuffer[] }  webview path: decoded bytes (transferred)
//   worker → main:
//     { type: "session", k, blockLen, totalLen }        a new session began
//     { type: "stats", framesNew, framesDup, k, blockLen, totalLen }   throttled (100ms)
//     { type: "done", payload, hashOk, totalLen, seconds }   payload transferred

import { LTDecoder } from "./shared/fountain";
import { fnv1a, parseFrame } from "./shared/protocol";

const ctx = self as unknown as {
  onmessage: ((e: MessageEvent) => void) | null;
  postMessage(msg: unknown, transfer?: Transferable[]): void;
};

let decoder: LTDecoder | null = null;
let sessionId = 0;
let payloadFnv = 0; // expected FNV-1a of the completed payload (from the header)
let origLen = 0; // original uncompressed file size (for the UI), from the header
let startTs = 0;
let done = false;
let statsDirty = false; // a frame changed counts since the last stats post

/** Reset to the pre-transfer state (new Start pressed on the receiver). */
function reset(): void {
  decoder = null;
  sessionId = 0;
  done = false;
  statsDirty = false;
}

/** Feed one decoded QR payload into the fountain. Creates/resets the decoder on
 * a new session id (same trigger the main thread used to own). */
function feed(bytes: Uint8Array): void {
  if (done) return;
  const parsed = parseFrame(bytes);
  if (!parsed) return;
  const { header, block } = parsed;
  if (!decoder || sessionId !== header.sessionId) {
    decoder = new LTDecoder(header.k, header.blockLen, header.sessionId, header.totalLen);
    sessionId = header.sessionId;
    payloadFnv = header.payloadFnv;
    origLen = header.origLen;
    startTs = now();
    ctx.postMessage({
      type: "session",
      k: decoder.k,
      blockLen: decoder.blockLen,
      totalLen: decoder.totalLen,
      origLen,
    });
  }
  decoder.addFrame(header.seq, block);
  statsDirty = true;

  if (decoder.isComplete) {
    const payload = decoder.assemble()!;
    const seconds = (now() - startTs) / 1000;
    const hashOk = fnv1a(payload) === payloadFnv;
    done = true;
    // Transfer the payload buffer (zero-copy) — one copy per transfer, not per frame.
    ctx.postMessage(
      { type: "done", payload: payload.buffer, hashOk, totalLen: decoder.totalLen, seconds },
      [payload.buffer],
    );
  }
}

/** performance.now() is available in workers; fall back to a monotonic-ish 0 if
 * a host somehow lacks it (never breaks determinism — only stats timing). */
function now(): number {
  return typeof performance !== "undefined" ? performance.now() : 0;
}

/** Post a stats snapshot at most ~10×/s (only when something changed) so the
 * main thread's metrics stay live without a message per decoded frame. */
setInterval(() => {
  if (!statsDirty || !decoder) return;
  statsDirty = false;
  ctx.postMessage({
    type: "stats",
    framesNew: decoder.framesNew,
    framesDup: decoder.framesDup,
    k: decoder.k,
    blockLen: decoder.blockLen,
    totalLen: decoder.totalLen,
    origLen,
  });
}, 100);

ctx.onmessage = (e: MessageEvent) => {
  const d = e.data as
    | { type: "reset" }
    | { type: "b64"; codes: string[] }
    | { type: "bytes"; bufs: ArrayBuffer[] };
  if (d.type === "reset") {
    reset();
    return;
  }
  if (d.type === "b64") {
    for (const b64 of d.codes) {
      const bin = atob(b64);
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      feed(bytes);
    }
    return;
  }
  if (d.type === "bytes") {
    for (const buf of d.bufs) feed(new Uint8Array(buf));
  }
};
