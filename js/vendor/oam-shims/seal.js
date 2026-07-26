// Last entry of the vendor block in build.rs's js_files: every define() has
// run, primordials are installed -- close the registry. define() now throws
// and the vendor object is frozen, so user code cannot shadow a public
// fallback id (e.g. define('events')) or replace _primordials/require out
// from under the port. require() itself keeps working (the module/factory
// Maps live behind the closure, untouched by the freeze).
"use strict";
globalThis.__oamVendor._seal();
