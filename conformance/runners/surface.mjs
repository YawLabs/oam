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
  "stream/consumers", "string_decoder", "timers", "tls", "trace_events",
  "tty", "url", "util", "v8", "vm", "worker_threads", "zlib",
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

console.log(JSON.stringify({ modules, globals }));
