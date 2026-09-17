"use strict";

/**
 * preload.js — 1:1 port of pi-web-desktop/electron/preload.js.
 *
 * WHAT CHANGED, AND WHY IT MATTERS
 * --------------------------------
 * Electron ran this in an ISOLATED world with `contextIsolation: true`, and
 * `contextBridge` was the only way anything reached the page. Tauri injects
 * initialization scripts into the page's MAIN world — the same world the agent's
 * rendered output runs in. There is no isolation boundary left to hide behind,
 * so:
 *
 *   - everything here is reachable (and forgeable) by page script;
 *   - the four commands it can reach are therefore deliberately parameterless
 *     or trivially-typed, and each one's worst case is a native dialog the user
 *     must confirm, a menu bar toggling, a repainted title bar, or an npm
 *     install of one fixed package from one fixed registry.
 *
 * See PORTING.md ("坑 2" and "坑 8") for the full threat note.
 *
 * Injection timing is preserved: Tauri runs initialization scripts at document
 * start, before any page script, which is what `window.piDesktop` needs —
 * pi-web 0.7.13+ probes for it while the session sidebar mounts.
 */

(function () {
  // --- IPC shim --------------------------------------------------------------
  // Tauri exposes the raw invoke bridge as __TAURI_INTERNALS__.invoke on every
  // page the webview loads, including the remote http://127.0.0.1 origin (which
  // is why the remote capability in capabilities/remote.json is mandatory —
  // without it the calls are rejected).
  function invoke(cmd, payload) {
    var internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== "function") {
      return Promise.reject(new Error("tauri ipc unavailable"));
    }
    return internals.invoke(cmd, payload || {});
  }

  // Only the embedded pi-web page gets the UI grafts; the shell's own status
  // pages must not get the notice-shelf restyling, the brand rename, or — worst
  // of all — a theme report, which would overwrite the user's saved theme with
  // whatever colour the splash happens to be.
  //
  // The Electron original keyed this off `location.protocol !== "http:"`, which
  // no longer discriminates: Tauri serves the bundled pages over http too
  // (http://tauri.localhost on Windows). Guessing the local origin instead was
  // the first attempt and it was wrong in a way that only showed up as a
  // silently reset theme — so the test is now a marker the shell pages carry
  // and pi-web cannot: `<meta name="piwd-shell">`. That marker only exists once
  // <head> is parsed, so every caller goes through `ready()` below rather than
  // asking at document-start.
  function isShellPage() {
    return !!document.querySelector('meta[name="piwd-shell"]');
  }

  /** Run `cb` once the document is parsed — never earlier, see isShellPage(). */
  function ready(cb) {
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", cb, { once: true });
    } else {
      cb();
    }
  }

  /** Run `cb` on the embedded pi-web page only. */
  function onPiWeb(cb) {
    ready(function () {
      if (!isShellPage()) cb();
    });
  }

  // ---------------------------------------------------------------------------
  // Bridges pi-web itself looks for
  // ---------------------------------------------------------------------------
  window.piWebDesktop = { isDesktop: true, platform: "win32", shell: "tauri" };

  // pi-web's OWN desktop bridge (upstream v0.7.13+): native directory picker.
  // Contract (pi-web components/SessionSidebar.tsx):
  //   selectDirectory(): Promise<string | null>
  window.piDesktop = {
    selectDirectory: function () {
      return invoke("select_directory");
    },
  };

  // ---------------------------------------------------------------------------
  // In-page update-result CTA (top-right toast)
  // ---------------------------------------------------------------------------
  var UI =
    "var(--font-ui, 'Segoe UI', Selawik, system-ui, -apple-system, BlinkMacSystemFont, 'PingFang SC', 'Microsoft YaHei', sans-serif)";

  var STATUS = {
    updated: "var(--tile-green, #60A917)",
    latest: "var(--accent, #0050EF)",
    available: "var(--tile-amber, #F0A30A)",
    error: "var(--tile-red, #E51400)",
  };

  var hostEl = null;

  function whenBody(cb) {
    if (document.body) cb();
    else document.addEventListener("DOMContentLoaded", cb, { once: true });
  }

  function ensureHost() {
    if (hostEl && document.body && document.body.contains(hostEl)) return hostEl;
    hostEl = document.createElement("div");
    hostEl.id = "pi-web-desktop-cta-host";
    var s = hostEl.style;
    s.position = "fixed";
    s.top = "64px";
    s.right = "20px";
    s.zIndex = "2147483647";
    s.display = "flex";
    s.flexDirection = "column";
    s.gap = "10px";
    s.pointerEvents = "none";
    hostEl.attachShadow({ mode: "open" });
    document.body.appendChild(hostEl);
    return hostEl;
  }

  function renderNotice(notice) {
    whenBody(function () {
      var accent = STATUS[notice.status] || STATUS.latest;
      var root = ensureHost().shadowRoot;

      var card = document.createElement("div");
      var cs = card.style;
      cs.pointerEvents = "auto";
      cs.boxSizing = "border-box";
      cs.width = "340px";
      cs.padding = "13px 15px";
      cs.borderRadius = "0";
      cs.background = "var(--bg-panel, #ffffff)";
      cs.color = "var(--text, #1a1a1a)";
      cs.border = "1px solid var(--border, rgba(0,0,0,0.06))";
      cs.borderLeft = "3px solid " + accent;
      cs.boxShadow = "0 2px 14px rgba(0,0,0,0.22)";
      cs.fontFamily = UI;
      cs.fontSize = "13px";
      cs.lineHeight = "1.5";
      cs.opacity = "0";
      cs.transform = "translateY(-10px)";
      cs.transition = "opacity .22s ease, transform .26s cubic-bezier(0.1,0.9,0.2,1)";

      var head = document.createElement("div");
      head.style.display = "flex";
      head.style.alignItems = "center";
      head.style.gap = "8px";

      var dot = document.createElement("span");
      var ds = dot.style;
      ds.flex = "0 0 auto";
      ds.width = "8px";
      ds.height = "8px";
      ds.borderRadius = "0";
      ds.background = accent;
      ds.boxShadow = "0 0 0 3px color-mix(in srgb, " + accent + " 15%, transparent)";

      var title = document.createElement("div");
      title.textContent = notice.title || "检查更新";
      title.style.flex = "1 1 auto";
      title.style.fontWeight = "600";
      title.style.fontSize = "13.5px";
      title.style.letterSpacing = "0.2px";

      var close = document.createElement("button");
      close.textContent = "✕";
      close.setAttribute("aria-label", "关闭");
      var xs = close.style;
      xs.flex = "0 0 auto";
      xs.cursor = "pointer";
      xs.border = "none";
      xs.background = "transparent";
      xs.color = "var(--text-dim, #9ca3af)";
      xs.fontFamily = UI;
      xs.fontSize = "12px";
      xs.lineHeight = "1";
      xs.padding = "2px 4px";
      xs.borderRadius = "0";
      close.addEventListener("mouseenter", function () {
        close.style.color = "var(--text, #1a1a1a)";
      });
      close.addEventListener("mouseleave", function () {
        close.style.color = "var(--text-dim, #9ca3af)";
      });

      head.appendChild(dot);
      head.appendChild(title);
      head.appendChild(close);
      card.appendChild(head);

      if (notice.message) {
        var msg = document.createElement("div");
        msg.textContent = notice.message;
        msg.style.marginTop = "9px";
        msg.style.fontSize = "13px";
        msg.style.color = "var(--text, #1a1a1a)";
        card.appendChild(msg);
      }
      if (notice.detail) {
        var det = document.createElement("div");
        det.textContent = notice.detail;
        det.style.marginTop = "3px";
        det.style.fontSize = "12px";
        det.style.color = "var(--text-muted, #6b7280)";
        card.appendChild(det);
      }

      var timer = null;
      var dismissed = false;
      var dismiss = function () {
        if (dismissed) return;
        dismissed = true;
        if (timer) clearTimeout(timer);
        card.style.opacity = "0";
        card.style.transform = "translateY(-8px)";
        setTimeout(function () {
          card.remove();
        }, 220);
      };
      close.addEventListener("click", dismiss);

      if (notice.action && notice.action.id) {
        var act = document.createElement("button");
        act.textContent = notice.action.label || "更新";
        var as = act.style;
        as.marginTop = "12px";
        as.width = "100%";
        as.cursor = "pointer";
        as.padding = "9px 12px";
        as.border = "none";
        as.borderRadius = "0";
        as.background = "var(--accent, #0050EF)";
        as.color = "#ffffff";
        as.fontFamily = UI;
        as.fontSize = "12.5px";
        as.fontWeight = "600";
        as.letterSpacing = "0.3px";
        act.addEventListener("mouseenter", function () {
          act.style.background = "var(--accent-hover, #2F6BFF)";
        });
        act.addEventListener("mouseleave", function () {
          act.style.background = "var(--accent, #0050EF)";
        });
        act.addEventListener("click", function () {
          act.disabled = true;
          act.textContent = "正在更新…";
          act.style.opacity = "0.75";
          act.style.cursor = "default";
          // The only action id the shell ever queues is "apply-update"; it maps
          // to the parameterless apply_update command.
          invoke("apply_update").catch(function () {});
        });
        card.appendChild(act);
      }

      root.appendChild(card);
      requestAnimationFrame(function () {
        card.style.opacity = "1";
        card.style.transform = "translateY(0)";
      });

      var sticky = notice.status === "error" || !!(notice.action && notice.action.id);
      if (!sticky) timer = setTimeout(dismiss, 8000);
    });
  }

  // Delivery channel. Electron used ipcRenderer.on(); here the Rust side calls
  // this function through webview.eval(), which needs no ACL permission at all
  // and therefore keeps the remote capability down to the four invoke commands.
  window.__piwdNotice = function (notice) {
    if (notice) {
      try {
        renderNotice(notice);
      } catch (e) {
        /* a broken toast must never take the app down */
      }
    }
  };

  // ---------------------------------------------------------------------------
  // Readability fix for pi-web's own notice shelf
  // ---------------------------------------------------------------------------
  onPiWeb(function fixNoticeShelf() {

    var FONT = 14;
    var LINE = 1.5;
    var PAD_Y = 12;
    var DOT = 7;
    var DOT_TOP = Math.round(PAD_Y + (FONT * LINE) / 2 - DOT / 2);

    var CSS = [
      ".notice-shelf-item{",
      "height:auto !important;min-height:0 !important;max-height:40vh !important;",
      "overflow-y:auto !important;align-items:flex-start !important;",
      "font-size:" + FONT + "px !important;line-height:" + LINE + " !important;",
      "}",
      ".notice-shelf-item>span:first-child{margin-top:" + DOT_TOP + "px !important;}",
      ".notice-shelf-item>span:first-child+span{",
      "white-space:pre-wrap !important;text-overflow:clip !important;",
      "overflow:visible !important;overflow-wrap:anywhere !important;",
      "padding:" + PAD_Y + "px 0 !important;",
      "}",
    ].join("");

    function inject() {
      if (document.getElementById("pi-web-desktop-notice-shelf-style")) return;
      var parent = document.head || document.documentElement;
      if (!parent) {
        document.addEventListener("DOMContentLoaded", inject, { once: true });
        return;
      }
      var st = document.createElement("style");
      st.id = "pi-web-desktop-notice-shelf-style";
      st.textContent = CSS;
      parent.appendChild(st);
    }
    inject();
  });

  // ---------------------------------------------------------------------------
  // Theme sync — pi-web's light/dark toggle → the native window frame
  // ---------------------------------------------------------------------------
  onPiWeb(function syncNativeTheme() {

    var last = null;

    function report() {
      var theme = document.documentElement.classList.contains("dark") ? "dark" : "light";
      if (theme === last) return;
      last = theme;
      invoke("theme_changed", { theme: theme }).catch(function () {});
    }

    function init() {
      report();
      new MutationObserver(report).observe(document.documentElement, {
        attributes: true,
        attributeFilter: ["class"],
      });
    }

    if (document.documentElement) init();
    else document.addEventListener("DOMContentLoaded", init, { once: true });
  });

  // ---------------------------------------------------------------------------
  // Brand rename — "Pi Web" → "Pi" in the embedded page
  // ---------------------------------------------------------------------------
  onPiWeb(function renameBrand() {

    var FROM = "Pi Web";
    var TO = "Pi";
    var SKIP = { SCRIPT: 1, STYLE: 1, TEXTAREA: 1, NOSCRIPT: 1 };

    function fixText(node) {
      if (!node || node.nodeType !== Node.TEXT_NODE || node.nodeValue !== FROM) return;
      var el = node.parentElement;
      if (!el || SKIP[el.tagName] || el.isContentEditable) return;
      node.nodeValue = TO;
    }

    function scan(root) {
      if (!root) return;
      if (root.nodeType === Node.TEXT_NODE) return fixText(root);
      if (root.nodeType !== Node.ELEMENT_NODE || SKIP[root.tagName]) return;
      var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      var n;
      while ((n = walker.nextNode())) fixText(n);
    }

    function init() {
      scan(document.body);
      new MutationObserver(function (records) {
        for (var i = 0; i < records.length; i++) {
          var r = records[i];
          if (r.type === "characterData") fixText(r.target);
          else Array.prototype.forEach.call(r.addedNodes, scan);
        }
      }).observe(document.body, { childList: true, subtree: true, characterData: true });
    }

    if (document.body) init();
    else document.addEventListener("DOMContentLoaded", init, { once: true });
  });
})();
