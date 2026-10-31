"use strict";

/**
 * Provision or update `runtime-seed` to the latest published `@agegr/pi-web`
 * (and its bundled `@earendil-works/pi-coding-agent`).
 *
 * Works both on a fresh clone where `runtime-seed/` does not exist yet and on
 * an existing workspace when tracking upstream releases. Prefers the bundled
 * `vendor/node` runtime when present so host Node version differences cannot
 * affect native prebuild selection.
 */

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
const SEED_DIR = path.join(ROOT, "runtime-seed");
const REGISTRY = process.env.PI_WEB_REGISTRY || "https://registry.npmmirror.com";

fs.mkdirSync(SEED_DIR, { recursive: true });
const manifest = path.join(SEED_DIR, "package.json");
if (!fs.existsSync(manifest)) {
  fs.writeFileSync(
    manifest,
    JSON.stringify({ name: "runtime-seed", version: "1.0.0", private: true }, null, 2) + "\n",
    "utf8"
  );
}

const bundledNode = path.join(ROOT, "vendor", "node", process.platform === "win32" ? "node.exe" : "bin/node");
const bundledNpmCli = path.join(ROOT, "vendor", "node", "node_modules", "npm", "bin", "npm-cli.js");

const args = [
  "install",
  "@agegr/pi-web@latest",
  "--omit=dev",
  "--no-audit",
  "--no-fund",
  `--registry=${REGISTRY}`,
];

console.log(`[seed] installing @agegr/pi-web@latest into ${SEED_DIR} (${REGISTRY})`);
if (fs.existsSync(bundledNode) && fs.existsSync(bundledNpmCli)) {
  execFileSync(bundledNode, [bundledNpmCli, ...args], {
    cwd: SEED_DIR,
    stdio: "inherit",
    env: {
      ...process.env,
      PATH: path.dirname(bundledNode) + path.delimiter + (process.env.PATH || ""),
    },
  });
} else {
  execFileSync(process.platform === "win32" ? "npm.cmd" : "npm", args, {
    cwd: SEED_DIR,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
}

const piWebVer = JSON.parse(
  fs.readFileSync(path.join(SEED_DIR, "node_modules", "@agegr", "pi-web", "package.json"), "utf8")
).version;
const agentVer = JSON.parse(
  fs.readFileSync(
    path.join(SEED_DIR, "node_modules", "@earendil-works", "pi-coding-agent", "package.json"),
    "utf8"
  )
).version;
console.log(`[seed] ready: @agegr/pi-web ${piWebVer} · @earendil-works/pi-coding-agent ${agentVer}`);
