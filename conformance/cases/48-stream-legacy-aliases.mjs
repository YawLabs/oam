// Node's legacy internal stream module names (_stream_readable, _writable,
// _duplex, _transform, _passthrough, _wrap) used to be require()able aliases
// of the public stream constructors. Node v26 removed all six. This case
// first asserted the aliases (require('_stream_readable') === stream.Readable);
// node 26 cannot run that as written -- its first require throws
// MODULE_NOT_FOUND -- so it now probes the same names for their v26 shape:
// gone from every door (require, builtinModules, isBuiltin,
// process.getBuiltinModule), while the public classes stay where they were.
// ESM cannot import the legacy CJS-only names directly, so use createRequire.
import { createRequire, builtinModules, isBuiltin } from "node:module";
const require = createRequire(import.meta.url);
const stream = require("stream");

for (const name of [
  "_stream_readable",
  "_stream_writable",
  "_stream_duplex",
  "_stream_transform",
  "_stream_passthrough",
  "_stream_wrap",
]) {
  let bare;
  try {
    require(name);
    bare = "loaded";
  } catch (e) {
    bare = e.code;
  }
  let prefixed;
  try {
    require("node:" + name);
    prefixed = "loaded";
  } catch {
    prefixed = "throws";
  }
  console.log(
    name,
    "require=" + bare,
    "node:=" + prefixed,
    "listed=" + builtinModules.includes(name),
    "isBuiltin=" + isBuiltin(name) + "/" + isBuiltin("node:" + name),
    "getBuiltinModule=" + typeof process.getBuiltinModule(name),
  );
}
console.log(
  "public",
  ["Readable", "Writable", "Duplex", "Transform", "PassThrough"]
    .map((k) => typeof stream[k])
    .join(","),
);
