// Builtin-surface inventory: which Node builtin modules load, and which
// runtime globals exist. Prints one JSON document; works under BOTH oam
// and node (the differential harness uses it for both columns).
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

// Node 22 top-level builtin modules (the canonical list oam targets).
const MODULES = [
  "assert", "async_hooks", "buffer", "child_process", "cluster", "console",
  "crypto", "dgram", "diagnostics_channel", "dns", "domain", "events", "fs",
  "fs/promises", "http", "http2", "https", "inspector", "module", "net",
  "os", "path", "perf_hooks", "process", "punycode", "querystring",
  "readline", "repl", "stream", "stream/promises", "stream/web",
  "stream/consumers", "string_decoder", "timers", "timers/promises", "tls",
  "trace_events", "tty", "url", "util", "v8", "vm", "worker_threads", "zlib",
];

const GLOBALS = [
  "Buffer", "URL", "URLSearchParams", "TextEncoder", "TextDecoder",
  "TextEncoderStream", "TextDecoderStream", "ReadableStream",
  "WritableStream", "TransformStream", "fetch", "crypto", "process",
  "performance", "queueMicrotask", "setImmediate", "setTimeout",
  "setInterval", "atob", "btoa", "structuredClone", "AbortController",
  "AbortSignal", "EventTarget", "Event", "Blob", "FormData", "Headers",
  "Request", "Response", "WebSocket", "BroadcastChannel",
];

const modules = {};
for (const name of MODULES) {
  try {
    require(name);
    modules[name] = true;
  } catch {
    modules[name] = false;
  }
}

const globals = {};
for (const name of GLOBALS) {
  globals[name] = globalThis[name] !== undefined;
}

// --------------------------------------------------------------- exports
// Per-module EXPORT NAMES, not just "does the module load".
//
// Why this exists: a builtin's ESM named exports are its module object's own
// enumerable string keys (oam derives them in cjs.rs::facade_with_prelude;
// node from BuiltinModule's exportKeys). A name the module object lacks is
// not a lazy runtime error -- `import { statfsSync } from "node:fs"` is a
// LINK-time SyntaxError that kills the whole program before a line of it
// runs, even if the importer never calls the function. Bundled CLIs import
// the union of everything their dependency tree touches, so one absent key
// takes down a program that would never have called it.
//
// The MODULES list above is the boolean breadth metric and is deliberately
// left alone; the export diff runs over the FULL public builtin set below so
// the gate sees legacy aliases and prefix-only modules too.
const ALL_MODULES = [
  "_http_agent", "_http_client", "_http_common", "_http_incoming",
  "_http_outgoing", "_http_server", "_stream_duplex", "_stream_passthrough",
  "_stream_readable", "_stream_transform", "_stream_wrap", "_stream_writable",
  "_tls_common", "_tls_wrap", "assert", "assert/strict", "async_hooks",
  "buffer", "child_process", "cluster", "console", "constants", "crypto",
  "dgram", "diagnostics_channel", "dns", "dns/promises", "domain", "events",
  "fs", "fs/promises", "http", "http2", "https", "inspector",
  "inspector/promises", "module", "net", "os", "path", "path/posix",
  "path/win32", "perf_hooks", "process", "punycode", "querystring",
  "readline", "readline/promises", "repl", "stream", "stream/consumers",
  "stream/promises", "stream/web", "string_decoder", "sys", "test", "timers",
  "timers/promises", "tls", "trace_events", "tty", "url", "util",
  "util/types", "v8", "vm", "wasi", "worker_threads", "zlib",
];

// Fixed (sorted) iteration order on both runtimes so a module with
// require-time side effects cannot make the result order-dependent.
const exportNames = {};
for (const name of [...ALL_MODULES].sort()) {
  try {
    // The `node:` prefix is required for prefix-only builtins (node:test)
    // and harmless for the rest.
    const m = require(`node:${name}`);
    exportNames[name] =
      m !== null && (typeof m === "object" || typeof m === "function")
        ? Object.keys(m).sort()
        // A primitive module.exports has no named exports at all; null keeps
        // that distinct from "module absent" (false) and "exports nothing"
        // (an empty array).
        : null;
  } catch {
    exportNames[name] = false;
  }
}

// Global OBJECT SHAPES, carried in the same map under a `global:` prefix.
//
// The `globals` map above is a boolean per name -- exactly the blind spot the
// module map had before export names were added: `Buffer` can be present and
// still be missing half its statics, and nothing notices. Folding these into
// exportNames means the diff, the ratchet and the gate all apply unchanged.
//
// Statics and prototype are probed separately because they fail
// independently: a constructor can carry every static and still lack the
// instance methods callers actually use.
for (const name of GLOBALS) {
  const value = globalThis[name];
  if (value === undefined || value === null) continue;
  if (typeof value !== "function" && typeof value !== "object") continue;
  // Own property names, not keys: class statics and prototype methods are
  // non-enumerable, so Object.keys() would report almost nothing.
  exportNames[`global:${name}`] = Object.getOwnPropertyNames(value).sort();
  const proto = value.prototype;
  if (proto && (typeof proto === "object" || typeof proto === "function")) {
    exportNames[`global:${name}.prototype`] = Object.getOwnPropertyNames(proto).sort();
  }
}

console.log(JSON.stringify({ modules, globals, exportNames }));
