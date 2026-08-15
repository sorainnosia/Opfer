// The original PoC only ever sent a bundled PNG, so the wire protocol carries
// no filename. To transfer *any* file and save it under its real name, we wrap
// the file in a tiny self-describing container and hand THAT to the fountain
// encoder. The frame protocol (protocol.ts) and LT codec (fountain.ts) stay
// byte-for-byte unchanged — they just see an opaque payload.
//
// Container layout (little-endian):
//   0  u8   codec       compression codec of `content` (see CODEC_* below)
//   1  u16  nameLen     length of the UTF-8 file name in bytes
//   3  ...  name        UTF-8 file name (no path)
//   +   ...  content     the file bytes, encoded per `codec`
//
// The `codec` byte makes compression self-describing: the receiver reads it and
// runs the matching decoder — no receiver-side setting needed. It's an extension
// point: today 0=raw, 1=gzip; a WASM codec (brotli/zstd) would just claim id 2+
// and register a decoder in `DECODERS`. nameLen is capped at 1024.

const NAME_MAX = 1024;

// Compression codecs. Add a new id here (+ its encoder/decoder) to introduce a
// codec; the receiver dispatches on the byte, so old and new senders coexist.
const CODEC_RAW = 0;
const CODEC_GZIP = 1; // native Compression Streams (zero dependency)
const CODEC_BROTLI = 2; // brotli-wasm — better ratio than gzip on text, lazy-loaded

/** How aggressively brotli compresses (0–11). 9 is a strong ratio at a fraction
 * of q11's time — compression happens once at wrap, but we don't want the QR to
 * take seconds to appear on a large file. */
const BROTLI_QUALITY = 9;

/** The compression choice offered in the UI. */
export type CompressMethod = "off" | "gzip" | "brotli";

/** True if the browser/webview exposes the Compression Streams API (Chromium and
 * modern Safari do; used opportunistically so an old engine just sends raw). */
function canGzip(): boolean {
  return typeof CompressionStream !== "undefined" && typeof DecompressionStream !== "undefined";
}

async function pipe(data: Uint8Array, transform: GenericTransformStream): Promise<Uint8Array> {
  const stream = new Blob([data as BlobPart])
    .stream()
    .pipeThrough(transform as unknown as ReadableWritablePair<Uint8Array, Uint8Array>);
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

// brotli-wasm is loaded lazily (a separate chunk + its .wasm) the first time it's
// actually needed, so it never weighs on startup or the gzip/raw paths.
interface Brotli {
  compress(buf: Uint8Array, options?: { quality?: number }): Uint8Array;
  decompress(buf: Uint8Array): Uint8Array;
}
let brotliMod: Promise<Brotli> | null = null;
function loadBrotli(): Promise<Brotli> {
  if (!brotliMod) brotliMod = import("brotli-wasm").then((m) => m.default) as Promise<Brotli>;
  return brotliMod;
}

/** Decoder table, keyed by codec id — this is what "the receiver knows which to
 * use" means: it looks the content's codec byte up here. Unknown id → null. */
const DECODERS: Record<number, (d: Uint8Array) => Promise<Uint8Array>> = {
  [CODEC_RAW]: async (d) => d,
  [CODEC_GZIP]: (d) => pipe(d, new DecompressionStream("gzip")),
  [CODEC_BROTLI]: async (d) => (await loadBrotli()).decompress(d),
};

/** Wrap a file for transfer, optionally compressing the content with `method`.
 * Compression is only KEPT if it actually shrinks the content (already-compressed
 * files like zip/jpg/mp4 won't, so they're sent raw), and any codec failure falls
 * back to raw — so it never corrupts and never bloats. The codec byte records what
 * was used, so the receiver decompresses automatically. */
export async function wrapFile(
  name: string,
  content: Uint8Array,
  method: CompressMethod = "off",
): Promise<Uint8Array> {
  let nameBytes = new TextEncoder().encode(name);
  if (nameBytes.length > NAME_MAX) {
    // Truncate on a UTF-8 boundary by decoding back with fatal=false.
    nameBytes = nameBytes.subarray(0, NAME_MAX);
    const safe = new TextDecoder("utf-8").decode(nameBytes).replace(/�+$/, "");
    nameBytes = new TextEncoder().encode(safe);
  }

  let codec = CODEC_RAW;
  let stored = content;
  try {
    if (method === "gzip" && canGzip()) {
      const gz = await pipe(content, new CompressionStream("gzip"));
      if (gz.length < content.length) {
        stored = gz;
        codec = CODEC_GZIP;
      }
    } else if (method === "brotli") {
      const br = (await loadBrotli()).compress(content, { quality: BROTLI_QUALITY });
      if (br.length < content.length) {
        stored = br;
        codec = CODEC_BROTLI;
      }
    }
  } catch {
    /* codec unavailable/failed on this engine — send raw */
  }

  const out = new Uint8Array(3 + nameBytes.length + stored.length);
  const dv = new DataView(out.buffer);
  dv.setUint8(0, codec);
  dv.setUint16(1, nameBytes.length, true);
  out.set(nameBytes, 3);
  out.set(stored, 3 + nameBytes.length);
  return out;
}

export interface UnwrappedFile {
  name: string;
  content: Uint8Array;
}

/** Reverse `wrapFile`: read the name and content, running the decoder named by
 * the codec byte. Returns null on a malformed/undecodable payload. */
export async function unwrapFile(payload: Uint8Array): Promise<UnwrappedFile | null> {
  if (payload.length < 3) return null;
  const dv = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const codec = dv.getUint8(0);
  const nameLen = dv.getUint16(1, true);
  if (3 + nameLen > payload.length) return null;
  const name = new TextDecoder("utf-8").decode(payload.subarray(3, 3 + nameLen));
  const decode = DECODERS[codec];
  if (!decode) return null; // unknown codec (e.g. a newer sender) — can't inflate
  try {
    const content = await decode(payload.subarray(3 + nameLen));
    return { name, content };
  } catch {
    return null; // flagged compressed but couldn't inflate → corrupt
  }
}
