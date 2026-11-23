// new vm.SourceTextModule(sourceText, options) checks its arguments as
// node's does, pinned against node on the same host: sourceText a string,
// options an object when given, and options.context, when set, an object
// (null, an array and a function are not) that is a contextified one --
// two errors, with node's two wordings. oam named the first argument
// "code", read a non-object options as none, and left the context to the
// native compile, which threw an error of its own.
//
// vm.SourceTextModule needs --experimental-vm-modules, so the checks run in
// a child started with it.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

if (process.argv[2] === "child") {
  const report = (label, fn) => {
    try {
      const mod = fn();
      console.log(label, "constructed", mod.context === undefined ? "no context" : typeof mod.context);
    } catch (err) {
      console.log(label, err.name, err.code, err.message);
    }
  };
  const make = (source, options) => new vm.SourceTextModule(source, options);
  report("context number", () => make("1", { context: 1 }));
  report("context string", () => make("1", { context: "x" }));
  report("context null", () => make("1", { context: null }));
  report("context undefined", () => make("1", { context: undefined }));
  report("context plain object", () => make("1", { context: {} }));
  report("context function", () => make("1", { context: function context() {} }));
  report("context array", () => make("1", { context: [] }));
  report("context contextified", () => make("1", { context: vm.createContext() }));
  report("options number", () => make("1", 5));
  report("options null", () => make("1", null));
  report("options undefined", () => make("1"));
  report("sourceText number", () => make(1, { context: 1 }));
} else {
  const child = spawnSync(
    process.execPath,
    ["--experimental-vm-modules", "--no-warnings", fileURLToPath(import.meta.url), "child"],
    { encoding: "utf8" },
  );
  process.stdout.write(child.stdout);
  console.log("child exit", child.status);
}
