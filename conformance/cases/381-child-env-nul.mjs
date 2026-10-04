// A NUL in a child's env, through each child_process entry point. node's
// normalizeSpawnArguments runs validateArgumentNullCheck on every name, and on
// every value that is a string, before anything starts: either one holding a
// NUL throws ERR_INVALID_ARG_VALUE synchronously ("The property
// 'options.env['FOO']' must be a string without null bytes"). A pair whose
// value is undefined is skipped first, so its name is never checked. A value
// that is not a string is stringified without the check, and the pair reaches
// the child as a C string, so it ends at the first NUL.
//
// oam stringified every pair without the check: the NUL reached the native
// spawn, which refused it with EINVAL (an `error` on the spawnSync result, a
// thrown EINVAL from spawn) instead of node's TypeError.
import {
  exec,
  execFile,
  execFileSync,
  execSync,
  fork,
  spawn,
  spawnSync,
} from "node:child_process";
import { fileURLToPath } from "node:url";

const self = fileURLToPath(import.meta.url);
const node = process.execPath;

function attempt(label, fn) {
  try {
    const out = fn();
    console.log(label, "no throw", out === undefined ? "" : out);
  } catch (e) {
    console.log(label, e.name, e.code, JSON.stringify(e.message));
  }
}

const routes = {
  spawnSync: (env) => {
    const r = spawnSync(node, ["-e", "0"], { env });
    return r.error ? r.error.code : r.status;
  },
  spawn: (env) => {
    spawn(node, ["-e", "0"], { env }).on("error", () => {});
  },
  execFileSync: (env) => {
    execFileSync(node, ["-e", "0"], { env });
  },
  execSync: (env) => {
    execSync(`"${node}" -e 0`, { env });
  },
  execFile: (env) => {
    execFile(node, ["-e", "0"], { env }, () => {});
  },
  exec: (env) => {
    exec(`"${node}" -e 0`, { env }, () => {});
  },
  fork: (env) => {
    fork(self, ["child"], { env }).on("error", () => {});
  },
};

if (process.argv[2] === "child") {
  process.exit(0);
}

const base = { SYSTEMROOT: process.env.SYSTEMROOT, PATH: process.env.PATH };
for (const [name, run] of Object.entries(routes)) {
  attempt(`${name} value`, () => run({ ...base, FOO: "a\u0000b" }));
  attempt(`${name} name`, () => run({ ...base, ["F\u0000O"]: "x" }));
}

// An undefined value is left out before its name is looked at.
attempt("undefined value, NUL name", () => {
  const r = spawnSync(node, ["-e", "0"], { env: { ...base, ["K\u0000"]: undefined } });
  return r.status;
});

// A non-string value: stringified, then cut at its first NUL.
const r = spawnSync(
  node,
  ["-e", "console.log(JSON.stringify([process.env.FOO, process.env.N]))"],
  {
    env: { ...base, FOO: { toString: () => "a\u0000b" }, N: 5 },
    encoding: "utf8",
  },
);
console.log("non-string value", r.status, r.stdout.trim());
