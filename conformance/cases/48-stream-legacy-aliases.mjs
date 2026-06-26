// Node keeps the legacy internal stream module names as require()able builtins
// that alias the public stream constructors: require('_stream_readable') ===
// stream.Readable, and likewise for writable/duplex/transform/passthrough.
// oam registers them as builtin aliases (Rust resolver routes _stream_* to the
// node: branch; node_compat.js registry factories map them to the exports).
// ESM cannot import the legacy CJS-only builtins directly, so use createRequire.
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const stream = require("stream");

console.log("readable=" + (require("_stream_readable") === stream.Readable));
console.log("writable=" + (require("_stream_writable") === stream.Writable));
console.log("duplex=" + (require("_stream_duplex") === stream.Duplex));
console.log("transform=" + (require("_stream_transform") === stream.Transform));
console.log("passthrough=" + (require("_stream_passthrough") === stream.PassThrough));
