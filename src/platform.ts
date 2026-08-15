// Platform shim: the SAME bundle runs both as the Tauri desktop/mobile app and
// as a plain served web page (the static `web/` build). Tauri-only capabilities
// (OS keep-awake, native file save, the camera2 path) fall back to browser APIs
// when Tauri isn't present, so the web build "just works" with no code fork.
//
// Detection is by the globals the Tauri runtime injects; when they're absent we
// are in an ordinary browser and take the web path.

import { invoke } from "@tauri-apps/api/core";

const w = typeof window !== "undefined" ? (window as unknown as Record<string, unknown>) : {};

/** True when running inside the Tauri runtime (desktop or mobile app). */
export const isTauri: boolean = "__TAURI_INTERNALS__" in w || "__TAURI__" in w;

/** True on Android (either the Tauri Android app or a mobile browser). */
export const isAndroid: boolean =
  typeof navigator !== "undefined" && /android/i.test(navigator.userAgent);

// ---- keep-awake ----------------------------------------------------------
// Tauri: an OS-level display hold via the `keep_awake` command (reliable even
// when the WebView ignores wakeLock). Browser: the Screen Wake Lock API, which
// the OS auto-releases when the tab is hidden — so we re-acquire on visibility.

let wakeSentinel: { release?: () => Promise<void> } | null = null;
let wantWake = false;

type WakeLockNav = Navigator & {
  wakeLock?: { request(kind: "screen"): Promise<{ release?: () => Promise<void> }> };
};

async function browserWake(on: boolean): Promise<void> {
  wantWake = on;
  try {
    const wl = (navigator as WakeLockNav).wakeLock;
    if (on) {
      if (!wakeSentinel && wl?.request) wakeSentinel = await wl.request("screen");
    } else if (wakeSentinel) {
      await wakeSentinel.release?.();
      wakeSentinel = null;
    }
  } catch {
    /* wake lock unsupported/denied — the transfer still runs, screen may dim */
  }
}

if (typeof document !== "undefined") {
  // A hidden→visible tab drops its wake lock; re-take it if we still want it.
  document.addEventListener("visibilitychange", () => {
    if (wantWake && document.visibilityState === "visible") void browserWake(true);
  });
}

/** Hold the display awake (on=true) or release it (on=false). Fire-and-forget. */
export function keepAwake(on: boolean): void {
  if (isTauri) {
    void invoke("keep_awake", { on }).catch(() => {
      /* command absent — nothing else to do */
    });
    return;
  }
  void browserWake(on);
}

// ---- file save -----------------------------------------------------------

/** Browser file save: hand the bytes to the user as a download. Returns the
 * suggested filename (the browser chooses the actual location). */
export function downloadFile(name: string, content: Uint8Array): string {
  // Copy into a fresh ArrayBuffer so Blob gets a plain BlobPart (content may be
  // a subarray view over a larger buffer).
  const buf = content.slice().buffer;
  const url = URL.createObjectURL(new Blob([buf], { type: "application/octet-stream" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name.replace(/[/\\]/g, "_") || "received.bin";
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Revoke a moment later so the download has started.
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
  return a.download;
}
