// Opfer — fountain-coded optical file transfer, as a Tauri desktop + mobile
// app. Two tabs: Send (pick any file, stream it as animated QR) and Receive
// (point the camera at another device's screen, reconstruct + save the file).

import { initSend, stopSend, resumeSend } from "./send";
import { initReceive, stopReceive } from "./receive";

type Tab = "send" | "receive";

function activate(tab: Tab): void {
  for (const btn of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    btn.classList.toggle("active", btn.dataset.tab === tab);
  }
  document.getElementById("panel-send")!.classList.toggle("active", tab === "send");
  document.getElementById("panel-receive")!.classList.toggle("active", tab === "receive");

  // Stop whatever the other tab was doing so we don't keep the camera or the
  // QR render loop alive in the background, and resume the tab we're entering.
  if (tab === "send") {
    stopReceive();
    resumeSend(); // restart the animated QR if a file is still loaded
  } else {
    stopSend();
  }
}

window.addEventListener("DOMContentLoaded", () => {
  // On phones the very top of the screen sits under the camera / status bar, so
  // give the title a top buffer there. Desktop keeps the default (no class).
  if (/android|iphone|ipad|ipod/i.test(navigator.userAgent)) {
    document.body.classList.add("mobile");
  }

  initSend();
  initReceive();

  for (const btn of document.querySelectorAll<HTMLButtonElement>(".tab")) {
    btn.addEventListener("click", () => activate(btn.dataset.tab as Tab));
  }
});
