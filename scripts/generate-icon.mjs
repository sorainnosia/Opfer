// Generates app-icon.svg — a branded source icon for `tauri icon`.
//
// The motif is the app itself: a real QR matrix (encoding "OPFER") rendered
// in the brand amber on a dark rounded card. `tauri icon` rasterises this SVG
// into every platform's icon set (Windows .ico, macOS .icns, Android mipmaps…).
//
//   node scripts/generate-icon.mjs   →   writes ./app-icon.svg

import QRCode from "qrcode";
import { writeFileSync } from "node:fs";

const SIZE = 1024; // canvas is SIZE×SIZE
const SAFE = 600; // QR fits within this centred box (Android adaptive safe zone)
const RADIUS = 200; // rounded-card corner radius
const AMBER = "#ffb257";

// Build the QR matrix. ECC "M" keeps the module count low for a crisp icon.
const qr = QRCode.create("OPFER", { errorCorrectionLevel: "M" });
const S = qr.modules.size;
const data = qr.modules.data;

// Integer module size so every square lands on a whole pixel.
const M = Math.floor(SAFE / S);
const qrPx = M * S;
const offset = Math.round((SIZE - qrPx) / 2);
const r = Math.max(1, Math.round(M * 0.16)); // slight rounding on each module

let rects = "";
for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    if (!data[y * S + x]) continue;
    const px = offset + x * M;
    const py = offset + y * M;
    rects += `<rect x="${px}" y="${py}" width="${M}" height="${M}" rx="${r}" ry="${r}"/>`;
  }
}

const svg = `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${SIZE}" height="${SIZE}" viewBox="0 0 ${SIZE} ${SIZE}">
  <defs>
    <linearGradient id="card" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#241d12"/>
      <stop offset="1" stop-color="#12100a"/>
    </linearGradient>
    <radialGradient id="glow" cx="0.5" cy="0.5" r="0.55">
      <stop offset="0" stop-color="#ffb257" stop-opacity="0.16"/>
      <stop offset="1" stop-color="#ffb257" stop-opacity="0"/>
    </radialGradient>
  </defs>
  <rect x="0" y="0" width="${SIZE}" height="${SIZE}" rx="${RADIUS}" ry="${RADIUS}" fill="url(#card)"/>
  <rect x="24" y="24" width="${SIZE - 48}" height="${SIZE - 48}" rx="${RADIUS - 20}" ry="${RADIUS - 20}"
        fill="none" stroke="#2e2718" stroke-width="8"/>
  <rect x="0" y="0" width="${SIZE}" height="${SIZE}" fill="url(#glow)"/>
  <g fill="${AMBER}">
${rects}
  </g>
</svg>
`;

writeFileSync(new URL("../app-icon.svg", import.meta.url), svg);
console.log(`wrote app-icon.svg — QR matrix ${S}×${S}, module ${M}px, ${qrPx}px total`);
