# Opfer — Fountain-coded Optical File Transfer (Tauri, desktop + mobile)

Send a file between two devices using nothing but a **screen and a camera**.
One device shows the file as an endless stream of animated QR codes; the other
points its camera at the screen and reconstructs the file. **No network path
between the devices, no pairing, no server** — the payload travels as light.

This is a [Tauri v2](https://tauri.app) native app on **Windows, macOS,
Linux, Android, and iOS**, with two tabs:

- **Send** — pick *any* file with the native file picker; it immediately starts
  streaming as animated QR codes in a loop.
- **Receive** — start the camera, point it at the sender's screen, and the file
  is decoded and saved to your **Downloads** folder (with a *Save As…* fallback).

## Online Testing
[Online Test](https://opfer.netlify.app/web/index.html)

Online Send/Receive of Opfer support upto 30 fps only, use mobile version for 
better speed.

## How it works

The transfer uses **LT (Luby transform) fountain codes** so the one-way optical
channel needs no back-channel and tolerates dropped frames:

- The sender never transmits the file's blocks directly. Each QR frame is the
  XOR of a pseudorandom subset of blocks, chosen deterministically from the
  frame's sequence number (robust-soliton degree distribution).
- The receiver collects **any** ~`K × 1.15` distinct frames, in any order, and
  peels the file out of them. A missed frame (blur, refresh straddling,
  autofocus hunt) costs a little time, never correctness. Sender and receiver
  frame rates don't need to match.
- Every 20-byte frame header is self-describing (session id, seq, block count,
  block size, total length, FNV-1a hash). There is no handshake: the receiver
  locks onto a stream mid-flight, and restarting the sender (new session id)
  automatically resets the receiver.

Decoding uses [zxing-cpp](https://github.com/zxing-cpp/zxing-cpp) compiled to
WASM, run in Web Workers fed by `requestVideoFrameCallback`. Busy workers drop
frames — the fountain absorbs them.

- **Any file, not a bundled image.** The file name travels inside the payload
  (see [`src/container.ts`](src/container.ts): `u16 nameLen | name | bytes`), so
  the receiver saves it under its real name and extension. The wire protocol and
  LT codec ([`src/shared/`](src/shared/)) are unchanged.
- **Native file pick + native save** via `@tauri-apps/plugin-dialog` and
  `@tauri-apps/plugin-fs`, so it works the same on desktop and mobile — no
  browser download plumbing, no self-signed dev cert.
- **Single app, two tabs** instead of two web pages.

## Architecture

```
index.html            two-tab shell (Send / Receive)
src/main.ts           tab switching + lifecycle (stops camera/QR loop on tab change)
src/send.ts           file pick → LT encode → animated QR canvas loop
src/receive.ts        camera → workers → LT decode → save to Downloads
src/worker.ts         zxing-wasm QR decode worker
src/container.ts      filename wrapper (wrap/unwrap)
src/shared/protocol.ts  frame header + FNV-1a + splitmix32  (unchanged from PoC)
src/shared/fountain.ts  LT encoder/decoder + robust-soliton (unchanged from PoC)
src-tauri/            Rust host: dialog + fs + opener plugins
```

## Prerequisites

- [Node.js](https://nodejs.org) 18+ and npm
- [Rust](https://rustup.rs) (stable) + the
  [Tauri system dependencies](https://tauri.app/start/prerequisites/) for your OS
  (WebView2 on Windows — preinstalled on Win 11; `webkit2gtk` on Linux; Xcode on
  macOS/iOS; Android Studio + NDK for Android).

## Run (desktop)

```bash
npm install
npm run tauri dev      # dev build with hot reload
npm run tauri build    # production bundle / installer
```

Two devices are needed to actually transfer: open the app on both, pick **Send**
on one (choose a file, crank screen brightness), and **Receive** on the other,
then point its camera at the sender's screen.

> You can also run **both tabs on one machine** to smoke-test the UI — Send in
> one window, Receive in another, camera pointed at the first screen.

## Run (mobile)

Tauri builds the same codebase to native mobile targets:

```bash
# Android (needs Android Studio, SDK, NDK, and JAVA_HOME set)
npm run tauri android init
npm run tauri android dev      # on a connected device / emulator

# iOS (macOS + Xcode only)
npm run tauri ios init
npm run tauri ios dev
```

### Camera permissions per platform

The receiver uses the webview's `getUserMedia`. Each platform gates the camera
differently:

| Platform | What's needed | Status in this repo |
|---|---|---|
| **Windows (WebView2)** | Host must grant the WebView2 permission | Handled in [`src-tauri/src/lib.rs`](src-tauri/src/lib.rs): the app auto-allows camera/mic permission requests, so a previously-remembered *Block* can't leave the camera stuck — pressing **Start camera** always works. |
| **macOS (WKWebView)** | `NSCameraUsageDescription` + camera entitlement | Provided in [`src-tauri/Info.plist`](src-tauri/Info.plist) and [`src-tauri/Entitlements.plist`](src-tauri/Entitlements.plist). |
| **iOS** | `NSCameraUsageDescription` | Add the key to `src-tauri/gen/apple/*/Info.plist` after `ios init` (copy from [`src-tauri/Info.plist`](src-tauri/Info.plist)). |
| **Android** | Manifest `CAMERA` permission + feature | After `android init`, add to `src-tauri/gen/android/app/src/main/AndroidManifest.xml`: `<uses-permission android:name="android.permission.CAMERA"/>` and `<uses-feature android:name="android.hardware.camera" android:required="false"/>`. Android also gates the webview separately, so the in-app request triggers the OS prompt. |

If the camera can't start, the Receive tab shows an explanatory message instead
of failing silently.

### Where received files are saved

Writes go through a native Rust command, [`write_file_raw`](src-tauri/src/lib.rs),
that receives the bytes as the **raw IPC request body** (`tauri::ipc::Request`)
and writes them with `std::fs` — avoiding the JSON-array marshaling that makes
the JS↔native bridge crawl on large files. Body layout is `[u32 pathLen][path]
[data]`; Rust creates parent dirs and de-dups the name (`(1)`, `(2)`…). The UI
shows an immediate `Saving …` status and **never drops the file** — every path
has a fallback.

- **Windows / macOS / Linux** — written straight into the user-profile
  **Downloads** folder (fast native write); a **Show in folder** button is shown.
- **Android** — tries a fast native write to the **public Download folder**
  (`/storage/emulated/0/Download`). That works on Android ≤10 (legacy storage);
  on **11+** scoped storage blocks it, so it falls back to the system **Save**
  (SAF) sheet into the public Downloads, and if even that is cancelled it keeps
  the file in app storage rather than losing it.

Received images are also previewed inline.

## Tuning

Both tabs have a collapsed **Settings** panel.

**Send:**

| setting | default | notes |
|---|---|---|
| tx fps | 24 | each frame must own ≥2 refresh cycles of the display |
| bytes / frame | 1465 (≈ QR v27) | denser is faster only if the receiver still decodes it; 2953 (v40) works phone-to-phone at close range but tanks throughput if the camera can't resolve it |
| **codes (grid)** | **1** | show a `grid × grid` tiling of QR codes at once. 2 → 4 codes/frame (~4× data), 3 → 9 codes (~9×) — **if** the receiver still decodes them. Each code is `1/grid` the size, so needs a bigger display / closer, steadier camera |
| **color layers ×3** | **off** | pack three codes into the R/G/B channels of each cell (~3× on top of the grid). *Experimental* — camera white-balance and chroma subsampling bleed channels, so it's fragile and can decode worse than mono. The **receiver's "color decode" must match** |
| error correction | L | the fountain layer already handles erasures; a frame decodes whole or is dropped |
| display size | 900 px | larger QR = easier decode, more screen used |

**Receive:** capture width (1280 is the widest mode iOS runs at true 60 fps),
capture fps (demanded with `exact` first — `ideal: 60` silently delivers 30),
decode workers (auto-set to the device's core count — QR decoding is CPU-bound,
so parallelism up to the cores is the lever; past that it stops helping),
**color decode** (split each frame into R/G/B planes — match the sender's *color
layers*), and **fast decode** (`auto`/`off`/`on`). Fast decode turns zxing's
`tryHarder` off — ≈2× faster per decode at the cost of some marginal codes. In
**auto** (default) the app watches the frame-drop metric and only latches it on
if decode falls behind capture, so it self-tunes toward keeping up without giving
up thoroughness when it isn't needed.

The decoder always skips zxing's rotation and inversion passes (the codes are
always upright and black-on-white) — a safe decoded-fps win. The heavier
`tryHarder`/`tryDownscale` passes stay on unless you enable **fast decode**.

Hold the receiving device steady or prop it — autofocus hunting from hand tremor
is the #1 throughput killer.

### Pushing past the single-QR ceiling

The parent experiment reached ~128 KB/s with **stacked multi-code grids** and an
**error-corrected color channel** on top of the base trick. Both are now built in
as opt-in settings (defaults keep the reliable single mono QR):

- **Grid (multi-code).** The sender tiles `grid²` independent fountain frames per
  image; the receiver asks zxing for up to 9 codes per frame and feeds each to the
  decoder. Throughput scales ~`grid²` **as long as every code still decodes** —
  push `grid` up while watching the receiver's **decode fps**, and back off when it
  drops. Bigger/brighter sender display and a closer, propped camera buy you more
  grid.
- **Color (RGB ×3).** Each cell carries three codes, one per channel. All codes of
  a fixed QR version share identical finder patterns (black in every channel), so
  each layer stays independently decodable in theory. In practice it's fragile —
  treat it as experimental and verify decode fps holds before trusting it.

Realistic guidance: **grid 2–3 is the dependable lever**; color is a gamble that
depends heavily on the specific camera. Tune both against the live decode-fps
metric, not the paper multiplier.

### Native Android camera (bypasses the 30 fps WebView cap)

The Android **WebView caps `getUserMedia` at 30 fps** on many devices, which
limits how many frames the receiver can decode. The Receive tab has a **native
camera (Android)** toggle that routes around it: a Rust module
([`src-tauri/src/camera_android.rs`](src-tauri/src/camera_android.rs)) opens the
camera directly through the **NDK Camera2** API, requests the target fps, decodes
every frame's Y (luma) plane **natively** with `rxing` (binary-safe, multi-code),
and streams only the decoded frame bytes to the frontend over a Tauri `Channel`.
Raw frames never cross the IPC bridge (60 fps of YUV would be ~110 MB/s), so the
bridge stays trivial and the WASM/OffscreenCanvas decode path is bypassed.

- Enable it in Receive settings → **native camera → on**. There's no live preview
  in this mode (frames don't enter the webview).
- Cross-compiles for all four Android ABIs; build with
  [`build_android_all.bat`](build_android_all.bat) (set your SDK/NDK paths) or
  `npm run tauri android build`.
- Uses the `CAMERA` permission (already in the manifest) and picks the first
  camera id (rear on virtually all devices). If a device only reaches 60 fps via a
  *constrained high-speed* session, see the note in `camera_android.rs`.

> Status: the native module **cross-compiles cleanly** (verified against NDK r27d,
> `aarch64-linux-android`), but the camera2 *runtime* path can only be validated
> on a physical device — treat it as ready-to-test, not battle-tested.

## Icons

The app icon is a QR matrix (the app's own motif) in the brand amber on a dark
rounded card. It's generated from a single source and fanned out to every
platform's sizes:

```bash
node scripts/generate-icon.mjs      # writes app-icon.svg (edit the script to restyle)
npx tauri icon app-icon.svg         # regenerates all platform icons
```

`tauri icon` populates:

- **Windows** — [`src-tauri/icons/icon.ico`](src-tauri/icons/) (multi-size ICO,
  referenced by `bundle.icon` in [`tauri.conf.json`](src-tauri/tauri.conf.json)).
- **Android** — every mipmap density (`mdpi`→`xxxhdpi`) under
  `src-tauri/gen/android/app/src/main/res/mipmap-*/` (`ic_launcher`,
  `ic_launcher_round`, and the adaptive-icon `ic_launcher_foreground`). The
  adaptive background color is set to the brand dark in
  `res/values/ic_launcher_background.xml`.
- **macOS / iOS / Linux** — `.icns`, the iOS `AppIcon-*` set, and desktop PNGs.

## Credits

- Concept, protocol, and fountain implementation:
  [decimen-optical-transfer](https://github.com/bashalarmistalt/decimen-optical-transfer)
  by bashalarmistalt (MIT).
- [node-qrcode](https://github.com/soldair/node-qrcode) (generation) and
  [zxing-wasm](https://github.com/Sec-ant/zxing-wasm) (decoding).

## License

MIT
