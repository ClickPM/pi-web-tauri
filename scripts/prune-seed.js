"use strict";

/**
 * Drop source maps from the runtime seed before it is packaged.
 *
 * electron-builder did this with a filter in electron-builder.yml:
 *
 *     filter:
 *       - "**\/*"
 *       - "!**\/*.map"
 *       - "!**\/.cache/**"
 *
 * Tauri's `bundle.resources` map has no exclusion syntax, so the same effect
 * has to be a step instead of a declaration. It runs from `beforeBundleCommand`
 * in tauri.conf.json, i.e. only on `tauri build` — `tauri dev` leaves the seed
 * alone.
 *
 * Safe to delete: `.map` files are debugging aids for the prebuilt `.next`
 * bundle; nothing loads them at runtime, and the Electron build shipped without
 * them in production for months. `npm run seed` brings them back, and so does
 * the in-app update (which installs a fresh npm tree) — this only trims what
 * goes INTO the installer.
 *
 * Measured on pi-web 0.9.1: 8,977 files, ~157MB of a ~900MB seed.
 */

const fs = require("fs");
const path = require("path");

const SEED = path.join(__dirname, "..", "runtime-seed");

let files = 0;
let bytes = 0;

/** Directories that are pure build scratch and never needed at runtime. */
const DROP_DIRS = new Set([".cache"]);

function walk(dir) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (DROP_DIRS.has(entry.name)) {
        try {
          const before = dirSize(full);
          fs.rmSync(full, { recursive: true, force: true });
          bytes += before;
        } catch {
          /* best effort */
        }
        continue;
      }
      walk(full);
    } else if (entry.isFile() && entry.name.endsWith(".map")) {
      try {
        bytes += fs.statSync(full).size;
        fs.unlinkSync(full);
        files++;
      } catch {
        /* a map we cannot delete is not worth failing the build over */
      }
    }
  }
}

function dirSize(dir) {
  let total = 0;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) total += dirSize(full);
    else {
      try {
        total += fs.statSync(full).size;
      } catch {
        /* ignore */
      }
    }
  }
  return total;
}

if (!fs.existsSync(SEED)) {
  console.log("[prune:seed] runtime-seed not present — nothing to prune");
  process.exit(0);
}

walk(SEED);
console.log(
  `[prune:seed] removed ${files.toLocaleString()} source map(s), ${(bytes / 1024 / 1024).toFixed(1)}MB`
);
