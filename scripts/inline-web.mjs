// Post-build step for the `web/` static build: inline the entry JS and CSS into
// index.html so the page also works when opened directly as a file:// (double-
// clicked), not just when served.
//
// Why this is needed: Chrome refuses to fetch an EXTERNAL `<script type="module"
// src=…>` from a file:// origin (treated as cross-origin `null` → blocked), so a
// double-clicked page loads nothing. An INLINE module script isn't fetched, so it
// runs fine from file://. We inline only the entry JS + CSS; the QR-decode worker,
// fountain worker and zxing WASM stay as external files because they're only used
// by the Receive path, which needs a served (secure) context for the camera anyway
// — from file:// Receive stops gracefully at the camera check before touching them.

import { readFileSync, writeFileSync, unlinkSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const webDir = join(dirname(fileURLToPath(import.meta.url)), "..", "web");
const htmlPath = join(webDir, "index.html");
let html = readFileSync(htmlPath, "utf8");

// `</script>` inside a bundled string literal would prematurely close an inline
// script tag; escaping the slash keeps the JS byte-identical but HTML-safe.
const scriptSafe = (code) => code.replace(/<\/script>/gi, "<\\/script>");

let jsRel = null;
let cssRel = null;

// Inline the entry module: <script type="module" ... src="./assets/xxx.js"></script>
html = html.replace(
  /<script\b[^>]*\bsrc="\.?\/?(assets\/[^"']+\.js)"[^>]*><\/script>/i,
  (_m, rel) => {
    const code = readFileSync(join(webDir, rel), "utf8");
    jsRel = rel;
    return `<script type="module">\n${scriptSafe(code)}\n</script>`;
  },
);

// Inline the stylesheet: <link rel="stylesheet" ... href="./assets/xxx.css">
html = html.replace(
  /<link\b[^>]*\bhref="\.?\/?(assets\/[^"']+\.css)"[^>]*>/i,
  (_m, rel) => {
    const css = readFileSync(join(webDir, rel), "utf8");
    cssRel = rel;
    return `<style>\n${css}\n</style>`;
  },
);

if (!jsRel) throw new Error("inline-web: entry <script> not found in web/index.html");
if (!cssRel) throw new Error("inline-web: stylesheet <link> not found in web/index.html");

writeFileSync(htmlPath, html);

// The entry JS/CSS are now inlined, so their standalone files are dead — remove
// them. The worker + WASM assets stay (the Receive path loads them when served).
for (const rel of [jsRel, cssRel]) {
  try {
    unlinkSync(join(webDir, rel));
  } catch {
    /* already gone — fine */
  }
}
console.log(`inline-web: inlined + removed ${jsRel} and ${cssRel}; workers + WASM kept for served Receive`);
