// The SHAPE of process's identity surface: whether each property is a data
// property or an accessor, and its writable/enumerable/configurable bits.
// Node freezes some (process.argv0 is non-writable non-configurable,
// process.features is non-configurable) and leaves others writable
// (process.argv, process.execPath). Code that saves/restores process.argv,
// or defineProperty-shims process.platform in a test harness, depends on those
// bits -- and an accessor where Node has a data property breaks
// Object.assign / spread / structured snapshots of process.
//
// Deliberately prints NO VALUES for anything whose value legitimately differs
// between runtimes (version strings, paths, pid). Shapes only.
//
// process.versions member NAMES are also excluded on purpose: oam publishes its
// REAL dependency tree there rather than fabricating Node's, so the key SET is a
// permanent, intended divergence. The descriptor SHAPE of those members is not
// -- so this pins the distinct descriptor tuples across all members instead.

const tuple = (obj, key) => {
  const d = Object.getOwnPropertyDescriptor(obj, key);
  if (d === undefined) return "MISSING";
  if (d.get || d.set) {
    return `accessor get=${!!d.get} set=${!!d.set} e=${d.enumerable} c=${d.configurable}`;
  }
  return `data w=${d.writable} e=${d.enumerable} c=${d.configurable}`;
};

// 1) The identity properties on process itself.
for (const key of [
  "version",
  "versions",
  "arch",
  "platform",
  "release",
  "config",
  "pid",
  "features",
      ]) {
  console.log(`process.${key} ${tuple(process, key)} typeof=${typeof process[key]}`);
}

// 2) process.versions members -- descriptor shape only (see header note).
const versionTuples = [
  ...new Set(Object.keys(process.versions).map((k) => tuple(process.versions, k))),
].sort();
const versionTypes = [
  ...new Set(Object.keys(process.versions).map((k) => typeof process.versions[k])),
].sort();
console.log("versions member-descriptors", JSON.stringify(versionTuples));
console.log("versions member-types", JSON.stringify(versionTypes));
// Both runtimes must carry these two regardless of the rest of the tree.
for (const key of ["node", "v8"]) {
  console.log(`versions.${key} ${tuple(process.versions, key)}`);
}
console.log("versions own-symbols", Object.getOwnPropertySymbols(process.versions).length);

// 3) process.release -- only the members both runtimes carry. oam omits
// sourceUrl / headersUrl / libUrl on purpose: node's point at nodejs.org, and
// oam publishing those would be a claim about a build it did not produce
// (docs/node-divergences.md). So assert the shared members' descriptors, not
// the key list.
for (const key of ["name", "lts"]) {
  console.log(`  release.${key} ${tuple(process.release, key)} typeof=${typeof process.release[key]}`);
}

// 4) argv / argv0 / execPath: BEHAVIOUR, not descriptor shape. oam must read
// all three lazily (the embedder declares argv after the process module
// instantiates), so they are getter-backed there where node has plain data
// properties. What has to agree is what callers actually do: argv and
// execPath are assignable and read back, argv0 is inert.
{
  const savedArgv = process.argv.slice();
  const savedExec = process.execPath;
  process.argv = ["a", "b", "--flag"];
  console.log("argv assign ->", JSON.stringify(process.argv));
  process.argv = savedArgv;
  process.execPath = "/fake/exec/path";
  console.log("execPath assign ->", process.execPath);
  process.execPath = savedExec;
  const beforeArgv0 = process.argv0;
  try {
    process.argv0 = "clobbered";
  } catch {
    // strict-mode TypeError on the non-writable/getter-only property
  }
  console.log("argv0 is inert ->", process.argv0 === beforeArgv0);
  console.log("argv/execPath/argv0 typeof ->",
    typeof process.argv, typeof process.execPath, typeof process.argv0);
}

// 4) process.features -- key ORDER is part of the contract (it is what
//    Object.keys / JSON.stringify surface), plus each member's descriptor.
console.log("features keys", JSON.stringify(Object.keys(process.features)));
for (const key of Object.keys(process.features)) {
  console.log(`  features.${key} ${tuple(process.features, key)} typeof=${typeof process.features[key]}`);
}
console.log("features extensible", Object.isExtensible(process.features));
