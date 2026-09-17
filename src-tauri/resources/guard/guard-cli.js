"use strict";

/**
 * guard-cli.js — the ONLY glue between the Rust shell and the two JS modules it
 * is forbidden to rewrite (`runtime-guard.js`, `updater.js`).
 *
 * Those two files were paid for with two production incidents (an interrupted
 * install truncated the live `@next/swc-*.node`) and they carry a regression
 * suite (`npm run test:guard`). They are copied here BYTE-FOR-BYTE from
 * pi-web-desktop/electron/. Nothing in this file may change their behaviour —
 * it only reproduces the context object `main.js` used to build (`guardCtx()`:
 * a logger plus an `installInto` bound to the bundled npm) and turns each entry
 * point into one line of JSON on stdout.
 *
 * Usage:
 *   node guard-cli.js <command> --node <exe> --npm-cli <path> --node-dir <dir>
 *                     --runtime <dir> --registry <url> [--log <file>] [...]
 *
 * Commands:
 *   recover    --dirs <a>[,<b>...]     reconcile interrupted swaps (boot, first)
 *   verify                             preflight / acceptance test
 *   installed                          installed pi-web + pi-coding-agent versions
 *   latest                             newest published version (network)
 *   provision  [--spec <pkg@ver>]      staging install -> verify -> atomic swap
 *
 * Protocol: exactly ONE line of JSON on stdout, always `{"ok":true,...}` or
 * `{"ok":false,"error":"..."}`, and the process exits 0 either way — a non-zero
 * exit is reserved for "this script itself could not run". Progress (npm's
 * stderr) is streamed as `{"progress":"..."}` lines on STDERR, so the single
 * stdout line stays parseable no matter how chatty npm is.
 *
 * `provision` needs the server stopped before the swap. The Rust side cannot be
 * called back into from here, so it stops the server BEFORE invoking this
 * command (see the `stopServer` note in provision below) — the one behavioural
 * difference from main.js, and a conservative one: the old server is down for
 * the download too, instead of only for the rename.
 */

const path = require("path");
const fs = require("fs");
const os = require("os");

const updater = require("./updater");
const runtimeGuard = require("./runtime-guard");

// ---------------------------------------------------------------------------
// argv
// ---------------------------------------------------------------------------
function parseArgv(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) out[a.slice(2)] = argv[++i];
    else out._.push(a);
  }
  return out;
}

const args = parseArgv(process.argv.slice(2));
const command = args._[0];

// ---------------------------------------------------------------------------
// Logging — same debug log the Rust side appends to, same one-line format, so a
// single file still tells the whole boot story.
// ---------------------------------------------------------------------------
const LOG_FILE = args.log || path.join(os.tmpdir(), "pi-web-desktop-debug.log");
function dbg(msg) {
  try {
    fs.appendFileSync(LOG_FILE, `[${new Date().toISOString()}] [guard] ${msg}\n`);
  } catch {
    /* ignore */
  }
}

/** npm stderr -> one JSON line per chunk on our stderr, for the updating UI. */
function progress(text) {
  try {
    process.stderr.write(JSON.stringify({ progress: String(text) }) + "\n");
  } catch {
    /* ignore */
  }
}

// ---------------------------------------------------------------------------
// ctx — main.js's updaterCtx() + guardCtx(), verbatim in spirit
// ---------------------------------------------------------------------------
function baseCtx() {
  return {
    bundledNode: args.node || process.execPath,
    npmCli: args["npm-cli"],
    nodeDir: args["node-dir"],
    runtimeDir: args.runtime,
    registry: args.registry || "https://registry.npmmirror.com",
  };
}

function guardCtx() {
  const base = baseCtx();
  return {
    ...base,
    dbg,
    installInto: (dir, spec, onProgress) => updater.installInto(base, dir, spec, onProgress),
  };
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------
const COMMANDS = {
  /**
   * Reconcile an interrupted swap in EVERY candidate runtime location before
   * anything else looks at the runtime — main.js's recoverRuntimeCandidates().
   * The candidate list comes from Rust because only Rust knows where userData
   * is; a crash between the swap's two renames can leave the seed dir missing,
   * which is exactly why the caller must not have picked a runtime dir yet.
   */
  async recover() {
    const dirs = String(args.dirs || "").split(",").map((s) => s.trim()).filter(Boolean);
    const actions = [];
    for (const dir of dirs) {
      try {
        const r = await runtimeGuard.recoverInterruptedSwap({ dbg }, dir);
        if (r && r.recovered) {
          dbg(`recoverInterruptedSwap(${dir}) -> ${r.action}`);
          actions.push({ dir, action: r.action });
        }
      } catch (e) {
        // Non-fatal, exactly as in main.js: a failed recovery must not stop boot.
        dbg(`recoverInterruptedSwap(${dir}) failed (non-fatal): ${(e && e.message) || e}`);
        actions.push({ dir, error: String((e && e.message) || e) });
      }
    }
    return { actions };
  },

  /** Boot preflight. Same call the staging acceptance test makes. */
  async verify() {
    const ctx = guardCtx();
    const dir = args.dir || ctx.runtimeDir;
    const check = await runtimeGuard.verifyRuntime(ctx, dir);
    return {
      verified: check.ok,
      healable: check.healable,
      failures: check.failures,
      summary: check.ok ? "" : runtimeGuard.describeFailures(check.failures),
    };
  },

  /** Versions on disk — no network. */
  async installed() {
    const rt = args.runtime;
    return {
      piWeb: updater.getInstalledVersion(rt),
      agent: updater.getInstalledAgentVersion(rt),
    };
  },

  /** Newest published version, and whether it beats what is installed. */
  async latest() {
    const ctx = baseCtx();
    const installedVersion = updater.getInstalledVersion(ctx.runtimeDir);
    const latestVersion = await updater.getLatestVersion(ctx);
    return {
      latest: latestVersion,
      installed: installedVersion,
      agent: updater.getInstalledAgentVersion(ctx.runtimeDir),
      newer: updater.isNewer(latestVersion, installedVersion),
      pkg: updater.PKG,
    };
  },

  /**
   * The single write path: staging install -> verify -> atomic swap.
   *
   * `--spec` absent means "reinstall what the lockfile pins" (the self-heal
   * case, which deliberately does NOT sneak in an upgrade).
   *
   * NOTE on stopServer: runtime-guard calls it only after staging passes
   * verification, so in Electron the old server kept serving through the whole
   * download. Rust has already stopped the server before spawning us, so we
   * pass no stopServer and the guard swaps straight in. Behaviour of the guard
   * itself is unchanged; the difference is documented in PORTING.md.
   */
  async provision() {
    const ctx = guardCtx();
    await runtimeGuard.provisionRuntime(ctx, {
      spec: args.spec || null,
      reason: args.reason || "provision",
      onProgress: progress,
    });
    return {
      piWeb: updater.getInstalledVersion(ctx.runtimeDir),
      agent: updater.getInstalledAgentVersion(ctx.runtimeDir),
    };
  },
};

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------
function emit(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

(async () => {
  const fn = COMMANDS[command];
  if (!fn) {
    emit({ ok: false, error: `unknown command: ${command || "(none)"}` });
    return;
  }
  try {
    emit({ ok: true, ...(await fn()) });
  } catch (e) {
    // npm failures carry their diagnosis in stderr, not in message.
    const detail = String((e && e.stderr) || "").trim();
    emit({
      ok: false,
      error: String((e && e.message) || e),
      detail: detail ? detail.slice(-4000) : undefined,
    });
  }
})().catch((e) => {
  emit({ ok: false, error: String((e && e.message) || e) });
});
