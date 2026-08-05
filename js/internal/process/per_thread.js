// node's lib/internal/process/per_thread.js is where process.execve lives, and
// a stack frame naming this module is part of its observable behaviour -- node
// core code (and node's own test suite) reads the frame to tell an execve
// failure from anything else on the way out.
//
// This file exists so that frame can be TRUE rather than fabricated: it is
// compiled under the origin `node:internal/process/per_thread` (see
// origin_name in crates/oam_engine/build.rs), so a trace pointing here points
// at a module that really is in oam's builtin registry, holding really is the
// implementation being named.
//
// A factory rather than a plain script because the implementation needs
// node_compat's closure (natives, the error codes table, the error shaper),
// which does not exist at global scope.
globalThis.__oamPerThread = function (natives, codes, applyNodeErrorShape) {
  // Named `execve`, and NOT defined as a method, so the frame reads
  // `execve (node:internal/process/per_thread:...)` the way node's does.
  function execve(execPath, args, env) {
    // execve(2) REPLACES the current process image: on success nothing after
    // this call ever runs -- no 'exit' handlers, no unwind, no flush (the op
    // flushes first for that reason). Node has it on POSIX only.
    if (natives.platform === "win32") {
      throw applyNodeErrorShape(
        new TypeError("process.execve is unavailable on the current platform"),
        "ERR_FEATURE_UNAVAILABLE_ON_PLATFORM",
      );
    }
    // A worker replacing the image would take its siblings with it.
    if (
      typeof natives.workerIsMainThread === "function" &&
      !natives.workerIsMainThread()
    ) {
      throw applyNodeErrorShape(
        new TypeError("process.execve() is not available in workers"),
        "ERR_WORKER_UNSUPPORTED_OPERATION",
      );
    }
    if (typeof execPath !== "string") {
      throw new codes.ERR_INVALID_ARG_TYPE("execPath", "string", execPath);
    }
    if (!Array.isArray(args)) {
      throw new codes.ERR_INVALID_ARG_TYPE("args", "Array", args);
    }
    // Everything crosses into C as a NUL-terminated string, so an embedded NUL
    // would silently TRUNCATE the value. Node refuses rather than pass a
    // shortened argv, and names the bad index.
    const NUL = String.fromCharCode(0);
    const clean = (v) => typeof v === "string" && !v.includes(NUL);
    for (let i = 0; i < args.length; i++) {
      if (!clean(args[i])) {
        throw new codes.ERR_INVALID_ARG_VALUE(
          `args[${i}]`,
          args[i],
          "must be a string without null bytes",
        );
      }
    }
    if (env === null || typeof env !== "object") {
      throw new codes.ERR_INVALID_ARG_TYPE("env", "object", env);
    }
    for (const key of Object.keys(env)) {
      if (!clean(key) || !clean(env[key])) {
        throw new codes.ERR_INVALID_ARG_VALUE(
          "env",
          env,
          "must be an object with string keys and values without null bytes",
        );
      }
    }
    const envp = Object.keys(env).map((k) => `${k}=${env[k]}`);
    const errnoName = natives.processExecve(execPath, args, envp);
    // Only reachable when execve FAILED -- success never returns.
    throw applyNodeErrorShape(
      new Error(`process.execve failed with error code ${errnoName}`),
      "ERR_OPERATION_FAILED",
    );
  }

  return { execve };
};
