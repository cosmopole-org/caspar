// The javascript runtime's guest prelude.
//
// Evaluated before a program's own module, in the same context. It provides
// the small surface a creature is entitled to assume exists, built on the two
// natives the Rust side installs (`__caspar_hostCall`, `__caspar_log`) and the
// node-stamped identity string (`__caspar_identity`).
//
// Everything here is deliberately tiny. A creature entity is one bundled file
// with no loader and no filesystem, so this is the whole standard library the
// platform adds on top of QuickJS's own ES2023 globals.
(function (g) {
  "use strict";

  // ── console → the node's per-VM log ────────────────────────────────────
  //
  // QuickJS's own console.log writes to the process stdout, which on a node
  // running dozens of creatures is nobody's log. Every level is routed to the
  // VM's log stream instead, so `casparctl` and `programs/readVmLogs` show it.
  function render(value, seen) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return String(value.stack || value);
    try {
      return JSON.stringify(value, function (k, v) {
        if (typeof v === "object" && v !== null) {
          if (seen.indexOf(v) !== -1) return "[circular]";
          seen.push(v);
        }
        if (typeof v === "bigint") return String(v) + "n";
        if (typeof v === "function") return "[function " + (v.name || "anonymous") + "]";
        if (typeof v === "undefined") return "[undefined]";
        return v;
      });
    } catch (e) {
      return String(value);
    }
  }

  function line(args) {
    var parts = [];
    for (var i = 0; i < args.length; i++) parts.push(render(args[i], []));
    return parts.join(" ");
  }

  function logAt(level) {
    return function () {
      __caspar_log(line(arguments), level);
    };
  }

  g.console = {
    log: logAt("runtime"),
    info: logAt("runtime"),
    debug: logAt("runtime"),
    warn: logAt("warn"),
    error: logAt("error"),
    trace: logAt("runtime"),
  };

  // ── hostCall ───────────────────────────────────────────────────────────
  //
  // The single ABI. Accepts either the raw `{op, input}` object or the JSON
  // string a wasm creature would have written into guest memory, and always
  // answers with the parsed response — because every caller parsed it anyway,
  // and a runtime that hands back a string only invites twenty copies of the
  // same try/catch.
  g.hostCall = function (op, input) {
    var request;
    if (typeof op === "string" && input === undefined && op.charAt(0) === "{") {
      request = op; // already an encoded {op, input} envelope
    } else if (typeof op === "string") {
      request = JSON.stringify({ op: op, input: input === undefined ? {} : input });
    } else {
      request = JSON.stringify(op);
    }
    var raw = __caspar_hostCall(request);
    if (raw === "" || raw === undefined || raw === null) return {};
    try {
      return JSON.parse(raw);
    } catch (e) {
      // A host op that answers with something other than JSON is a host bug,
      // not a guest one — surface the bytes rather than an opaque parse error.
      return { ok: false, error: "host returned non-JSON: " + String(raw).slice(0, 512) };
    }
  };

  // The raw form, for a caller that wants the bytes exactly as the host wrote
  // them (a proxy that forwards a response verbatim, a test).
  g.hostCallRaw = function (request) {
    return __caspar_hostCall(typeof request === "string" ? request : JSON.stringify(request));
  };

  // ── identity ───────────────────────────────────────────────────────────
  //
  // What the NODE says this VM is. Frozen, because a guest that could edit it
  // would still not change what the host stamps on its calls — the only thing
  // an editable copy could do is mislead the code reading it.
  var identity = {};
  try {
    identity = JSON.parse(__caspar_identity) || {};
  } catch (e) {
    identity = {};
  }
  g.caspar = Object.freeze({
    machineId: identity.machineId || "",
    programId: identity.programId || "",
    vmId: identity.vmId || "main",
    storeId: identity.storeId || "",
    runtime: "javascript",
  });

  // ── base64 ─────────────────────────────────────────────────────────────
  var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  g.btoa = function (input) {
    var str = String(input);
    var out = "";
    for (var i = 0; i < str.length; ) {
      var c1 = str.charCodeAt(i++);
      var c2 = str.charCodeAt(i++);
      var c3 = str.charCodeAt(i++);
      if (c1 > 255 || c2 > 255 || c3 > 255) {
        throw new Error("btoa: input contains characters outside the Latin1 range");
      }
      var e1 = c1 >> 2;
      var e2 = ((c1 & 3) << 4) | (c2 >> 4);
      var e3 = ((c2 & 15) << 2) | (c3 >> 6);
      var e4 = c3 & 63;
      if (isNaN(c2)) { e3 = 64; e4 = 64; } else if (isNaN(c3)) { e4 = 64; }
      out += B64.charAt(e1) + B64.charAt(e2) +
        (e3 === 64 ? "=" : B64.charAt(e3)) + (e4 === 64 ? "=" : B64.charAt(e4));
    }
    return out;
  };

  g.atob = function (input) {
    var str = String(input).replace(/[\t\n\f\r ]/g, "");
    if (str.length % 4 === 1) throw new Error("atob: invalid base64 length");
    str = str.replace(/=+$/, "");
    var out = "";
    var bits = 0;
    var acc = 0;
    for (var i = 0; i < str.length; i++) {
      var v = B64.indexOf(str.charAt(i));
      if (v === -1) throw new Error("atob: invalid base64 character");
      acc = (acc << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += String.fromCharCode((acc >> bits) & 0xff);
      }
    }
    return out;
  };

  // ── TextEncoder / TextDecoder (UTF-8 only) ─────────────────────────────
  function TextEncoder() {}
  TextEncoder.prototype.encoding = "utf-8";
  TextEncoder.prototype.encode = function (input) {
    var str = String(input === undefined ? "" : input);
    var bytes = [];
    for (var i = 0; i < str.length; i++) {
      var cp = str.codePointAt(i);
      if (cp > 0xffff) i++;
      if (cp < 0x80) {
        bytes.push(cp);
      } else if (cp < 0x800) {
        bytes.push(0xc0 | (cp >> 6), 0x80 | (cp & 63));
      } else if (cp < 0x10000) {
        bytes.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 63), 0x80 | (cp & 63));
      } else {
        bytes.push(
          0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 63),
          0x80 | ((cp >> 6) & 63), 0x80 | (cp & 63)
        );
      }
    }
    return new Uint8Array(bytes);
  };

  function TextDecoder() {}
  TextDecoder.prototype.encoding = "utf-8";
  TextDecoder.prototype.decode = function (input) {
    if (input === undefined || input === null) return "";
    var bytes = input instanceof Uint8Array ? input : new Uint8Array(input.buffer || input);
    var out = "";
    for (var i = 0; i < bytes.length; ) {
      var b = bytes[i++];
      var cp;
      if (b < 0x80) cp = b;
      else if (b < 0xe0) cp = ((b & 31) << 6) | (bytes[i++] & 63);
      else if (b < 0xf0) cp = ((b & 15) << 12) | ((bytes[i++] & 63) << 6) | (bytes[i++] & 63);
      else cp = ((b & 7) << 18) | ((bytes[i++] & 63) << 12) | ((bytes[i++] & 63) << 6) | (bytes[i++] & 63);
      out += String.fromCodePoint(cp);
    }
    return out;
  };

  g.TextEncoder = TextEncoder;
  g.TextDecoder = TextDecoder;

  if (typeof g.structuredClone !== "function") {
    g.structuredClone = function (value) {
      return value === undefined ? undefined : JSON.parse(JSON.stringify(value));
    };
  }

  // ── the invocation shim ────────────────────────────────────────────────
  //
  // The host calls this, never `update` directly, so an async creature and a
  // synchronous one finish through exactly one path: the result (or the
  // failure) is parked on `__caspar_done`, the host drains the promise job
  // queue, and then reads it. Doing it in JS avoids threading QuickJS's
  // promise state through the Rust side, where a pending job and a rejected
  // promise look alike.
  g.__caspar_done = null;

  // How a thrown value is reported.
  //
  // QuickJS's `error.stack` carries ONLY the frames — unlike V8, where it
  // begins with "Error: message". Reporting `String(e.stack || e)` therefore
  // threw away the one line that says what went wrong and handed the platform
  // a bare "    at <anonymous> (…)", which is a stack trace of nothing.
  function describe(e) {
    if (e === undefined) return "undefined was thrown";
    if (e === null) return "null was thrown";
    if (e instanceof Error || (e && typeof e.message === "string" && typeof e.name === "string")) {
      var head = (e.name || "Error") + ": " + (e.message || "");
      var stack = typeof e.stack === "string" ? e.stack : "";
      return stack ? head + "\n" + stack : head;
    }
    if (typeof e === "object") {
      try { return JSON.stringify(e); } catch (_) { return String(e); }
    }
    return String(e);
  }

  function settle(ok, value, error) {
    g.__caspar_done = { ok: ok, value: value, error: error };
  }

  g.__caspar_invoke = function (input) {
    var fn = g.update;
    if (typeof fn !== "function") {
      settle(false, undefined,
        "this javascript entity exports no `update` function; a creature module must " +
        "assign globalThis.update = function (inputJson) { … }");
      return;
    }
    var result;
    try {
      result = fn(input);
    } catch (e) {
      settle(false, undefined, describe(e));
      return;
    }
    if (result && typeof result.then === "function") {
      result.then(
        function (v) { settle(true, v, undefined); },
        function (e) { settle(false, undefined, describe(e)); }
      );
      return;
    }
    settle(true, result, undefined);
  };

  // What the host reads once the job queue is quiet. Encoded here so a value
  // of any shape crosses the boundary as one string.
  g.__caspar_result = function () {
    var d = g.__caspar_done;
    if (!d) {
      return JSON.stringify({
        state: "pending",
        error: "the entity's update() never settled: a promise it returned is still " +
          "waiting on something this runtime cannot resolve (there is no host async)",
      });
    }
    if (!d.ok) return JSON.stringify({ state: "failed", error: d.error || "unknown error" });
    var v = d.value;
    if (v === undefined || v === null) return JSON.stringify({ state: "ok", text: "" });
    if (typeof v === "string") return JSON.stringify({ state: "ok", text: v });
    try {
      return JSON.stringify({ state: "ok", text: JSON.stringify(v) });
    } catch (e) {
      return JSON.stringify({ state: "ok", text: String(v) });
    }
  };
})(globalThis);
