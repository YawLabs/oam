// os.constants.errno is the POSITIVE POSIX errno table for the HOST platform --
// not libuv's negative numbers (that is util.getSystemErrorMap, case 84) and not
// one table shared across platforms. Linux, macOS and Windows disagree on the
// numbers (EAGAIN is 11 on Linux, 35 on macOS) AND on the key set (Windows adds
// the whole WSA* block and drops EDQUOT/EMULTIHOP/ESTALE). A runtime that ships
// one hardcoded table decodes a raw errno to the wrong name on two platforms out
// of three -- and any library that maps errno -> code from this table (node-gyp
// addons, graceful-fs style retry logic) then misroutes.
//
// The differential runs node on the SAME host, so this pins the complete
// per-platform table without the case having to know which platform it is on.
import os from "node:os";

const errno = os.constants.errno;
const keys = Object.keys(errno).sort();
console.log("count", keys.length);
// RAW Object.keys order -- node emits the WSA* block ascending by value, not
// alphabetically, and a sorted-only list can never catch an ordering drift.
console.log("key-order", JSON.stringify(Object.keys(errno)));
console.log("keys", JSON.stringify(keys));
for (const key of keys) {
  console.log(`${key}=${errno[key]}`);
}

// Shape: plain data properties, all numbers, no duplicate-free assumption (a
// platform legitimately aliases EWOULDBLOCK to EAGAIN and EOPNOTSUPP to ENOTSUP).
const types = [...new Set(keys.map((k) => typeof errno[k]))].sort();
console.log("value-types", JSON.stringify(types));
const nonInteger = keys.filter((k) => !Number.isInteger(errno[k]));
console.log("non-integer", JSON.stringify(nonInteger));
const descriptors = [
  ...new Set(
    keys.map((k) => {
      const d = Object.getOwnPropertyDescriptor(errno, k);
      return d.get || d.set ? "accessor" : `data w=${d.writable} e=${d.enumerable} c=${d.configurable}`;
    }),
  ),
].sort();
console.log("descriptors", JSON.stringify(descriptors));

// os.constants.errno must be the same table node:constants re-exports.
const legacy = await import("node:constants");
const shared = keys.filter((k) => k in legacy.default);
console.log("legacy-shared", shared.length);
console.log("legacy-agree", shared.every((k) => legacy.default[k] === errno[k]));
