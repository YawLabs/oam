// The LIVE export names of oam's own modules -- the `oam:` family and the
// `oam` global -- as one JSON document.
//
// Runs under oam only (node has no `oam:` specifiers and no `oam` global),
// so unlike surface.mjs there is no second column: the oracle for these
// modules is the JS in js/, and the thing being gated is
// crates/oam_ts/types/oam.d.ts, which oam_ts injects into every check. A
// name here with no declaration there is a program that RUNS and fails to
// type-check on its own runtime's module.
//
// Names are the module object's own enumerable keys, `default` included:
// the loader publishes the module object as the default export too, so
// `import mcp from "oam:mcp"` is a real import that needs a real
// declaration.

const MODULES = ["oam:mcp", "oam:test", "oam:ai", "oam:permissions"];

const exportNames = {};
for (const specifier of MODULES) {
  const namespace = await import(specifier);
  exportNames[specifier] = Object.keys(namespace).sort();
}
// The global is not a module, but it is the same contract: a member the
// declarations miss is a TS2339 on working code.
exportNames["oam"] = Object.keys(globalThis.oam).sort();

console.log(JSON.stringify({ exportNames }));
