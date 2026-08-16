// fs.glob / fs.globSync / fs/promises.glob + path.matchesGlob.
//
// Pure-JS implementation: the runtime compiles the glob to a regex and walks
// readdir results. Node v22 validates only cwd / exclude / withFileTypes and
// silently IGNORES everything else callers borrow from the glob npm package
// (nodir, mark, absolute, include, follow, nocase, an aborted signal) -- the
// legs below pin the agreed behavior for each, which for most options means
// "the option does nothing". Case sensitivity is platform-derived (win32 and
// darwin fold, linux does not), never user-supplied; literal path components
// are FS-resolved (pattern-cased in the result) except under a globstar,
// where node compares its trailing literal strictly. The case also covers
// the four core patterns (*, **, character classes, brace expansion) and a
// few error shapes.
//
// The case runs identically under oam and real Node v22 so the
// node-differential harness catches any silent divergence. Some legs have
// platform-dependent output (case folding, whether FOO.JS and foo.js can
// coexist); that is fine -- the differential compares oam against the local
// node on the SAME host.
import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import os from "node:os";

// Fixed pid-keyed scratch dir (matches the pattern in 09-fs-sync.mjs etc.):
// the same scratch is reused, so the differential harness sees identical
// output across oam and Node.
fs.rmSync(path.join(os.tmpdir(), `oam-conf-glob-${process.pid}`), { recursive: true, force: true });
const root = path.join(os.tmpdir(), `oam-conf-glob-${process.pid}`);
fs.mkdirSync(root, { recursive: true });
fs.mkdirSync(path.join(root, "a", "b"), { recursive: true });
fs.mkdirSync(path.join(root, "c"), { recursive: true });
// Junction-style loops are not tested here: the natives don't expose
// reparse-point detection, so follow:true can't terminate on Windows
// junctions. The default follow:false is the supported path.
fs.writeFileSync(path.join(root, "foo.js"), "");
fs.writeFileSync(path.join(root, "FOO.JS"), "");
fs.writeFileSync(path.join(root, "bar.ts"), "");
fs.writeFileSync(path.join(root, "a", "b.js"), "");
fs.writeFileSync(path.join(root, "a", "Baz.js"), "");
fs.writeFileSync(path.join(root, "a", "c.txt"), "");
fs.writeFileSync(path.join(root, "a", "b", "deep.js"), "");
fs.writeFileSync(path.join(root, "c", "leaf.js"), "");

const show = (label, value) => {
  console.log(label, "=>", JSON.stringify(value));
};

// --- basic *
show("*.js", fs.globSync("*.js", { cwd: root }).sort());
// --- ** matches the cwd itself AND every descendant
show("**", fs.globSync("**", { cwd: root }).sort());
// --- **/*.js: files only, not the directories `**` would otherwise surface
show("**/*.js", fs.globSync("**/*.js", { cwd: root }).sort());
// --- a/**/deep.js: `**` matches zero or more intermediate segments
show("a/**/deep.js", fs.globSync("a/**/deep.js", { cwd: root }).sort());
// --- [Bb]*.js: case-sensitive character class (no nocase)
show("[Bb]*.js", fs.globSync("[Bb]*.js", { cwd: root }).sort());
// --- nocase is IGNORED (node validates only cwd/exclude/withFileTypes):
// on linux both FOO.JS and foo.js exist and matching stays case-sensitive,
// so only foo.js comes back; on win32/darwin the two creates collapsed into
// one file and platform folding matches it either way.
show("nocase foo.*", fs.globSync("foo.*", { cwd: root, nocase: true }).sort());
// --- platform case folding for magic patterns: FOO.* matches foo.js on
// win32/darwin (node's internal matcher is nocase there), never on linux.
show("upper magic FOO.*", fs.globSync("FOO.*", { cwd: root }).sort());
// --- literal components are FS-resolved and come back PATTERN-cased on
// case-insensitive platforms (A/b.js -> "A/b.js" on win32/darwin, no match
// on linux).
show("upper literal dir", fs.globSync("A/b.js", { cwd: root }).sort());
// --- ...but a literal after a globstar is compared STRICTLY even on
// case-insensitive platforms: no match anywhere.
show("globstar upper literal", fs.globSync("**/BAZ.js", { cwd: root }).sort());
// --- {a,b} brace expansion: matches .js OR .ts in cwd
show("*.{js,ts}", fs.globSync("*.{js,ts}", { cwd: root }).sort());
// --- absolute: with an absolute pattern, node and oam agree on the path.
// Compare only the basename so the test is independent of the tmpdir prefix.
const absRoot = path.resolve(root);
show("absolute", fs.globSync(path.join(absRoot, "foo.js"), { absolute: true }).map((p) => path.basename(p)).sort());
// --- mark: directories end with `/`
show("mark", fs.globSync("a", { cwd: root, mark: true }).sort());
// --- nodir: drop directory entries
show("nodir", fs.globSync("**", { cwd: root, nodir: true }).sort());
// --- withFileTypes returns Dirent; we sort by the entry's own name
const wft = fs.globSync("**/*.js", { cwd: root, withFileTypes: true });
show("withFileTypes", wft.map((d) => d.name).sort());
// --- include: pre-filter on directory listings; `**` ignores include
show("include b*", fs.globSync("**", { cwd: root, include: "b*" }).sort());
// --- exclude as a function: drop everything inside a/
show("exclude fn", fs.globSync("**/*.js", { cwd: root, exclude: (p) => p.startsWith("a/") }).sort());
// --- exclude as a string array: drop foo.js. (A bare string is not accepted
// by node v22's `exclude` validator; only function or string[] pass.)
show("exclude arr", fs.globSync("**/*.js", { cwd: root, exclude: ["foo.*"] }).sort());
// --- exclude with an unexpected shape throws (matches node)
try {
  fs.globSync("**/*.js", { cwd: root, exclude: { pattern: "foo.*" } });
  show("exclude bad shape", "NO THROW");
} catch (e) {
  show("exclude bad shape", { code: e.code, name: e.name });
}

// --- async + callback return the same data. fs.promises.glob returns an
// AsyncIterable in node v22 (not a Promise<array>); Array.fromAsync collects
// it into the same string[] shape fs.globSync would produce.
const asyncResults = await Array.fromAsync(fsp.glob("**/*.js", { cwd: root }));
show("async", asyncResults.sort());

const cbResults = await new Promise((resolve, reject) =>
  fs.glob("**/*.js", { cwd: root }, (err, matches) => {
    if (err) reject(err);
    else resolve(matches);
  }),
);
show("callback", cbResults.sort());

// --- an already-aborted signal is IGNORED by node v22's fs.promises.glob
// (validated empirically: the iteration completes, no ABORT_ERR); pin the
// no-throw so a well-meaning "honor the signal" change surfaces here.
try {
  const ac = new AbortController();
  ac.abort();
  await fsp.glob("**/*.js", { cwd: root, signal: ac.signal });
  show("aborted", "NO THROW");
} catch (e) {
  show("aborted", { code: e.code, name: e.name });
}

// --- a cwd pointing at a FILE yields zero matches, not a throw (node v22
// swallows the readdir ENOTDIR); pinned as NO THROW
try {
  fs.globSync("*.js", { cwd: path.join(root, "foo.js") });
  show("enotdir", "NO THROW");
} catch (e) {
  show("enotdir", e.code);
}

// --- TypeError on non-string pattern
try {
  fs.globSync(42);
  show("bad pattern", "NO THROW");
} catch (e) {
  show("bad pattern", { code: e.code, name: e.name });
}

// --- path.matchesGlob
show("matchesGlob foo.js *.js", path.matchesGlob("foo.js", "*.js"));
show("matchesGlob exact", path.matchesGlob("foo", "foo"));
show("matchesGlob FOO.js", path.matchesGlob("FOO.js", "*.js"));
// --- matchesGlob folds case for MAGIC patterns on win32/darwin only, and
// never for fully-literal patterns (minimatch nocase + nocaseMagicOnly).
show("matchesGlob upper magic", path.matchesGlob("foo.js", "FOO.*"));
show("matchesGlob upper literal", path.matchesGlob("foo.js", "FOO.js"));
show("matchesGlob a/b.js a/*.js", path.matchesGlob("a/b.js", "a/*.js"));
show("matchesGlob posix a/b.js", path.posix.matchesGlob("a/b.js", "a/*.js"));
show("matchesGlob win32 a/b.js", path.win32.matchesGlob("a/b.js", "a/*.js"));
try {
  path.matchesGlob(42, "*");
  show("matchesGlob bad arg", "NO THROW");
} catch (e) {
  show("matchesGlob bad arg", { code: e.code, name: e.name });
}

// --- cleanup
fs.rmSync(root, { recursive: true, force: true });
