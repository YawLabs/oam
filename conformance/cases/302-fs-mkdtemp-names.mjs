// fs.mkdtemp's names, in every form: the prefix as given -- relative to the
// cwd, never joined to os.tmpdir() -- followed by six characters of
// [A-Za-z0-9], libuv's uv_fs_mkdtemp. And the path a failure names: node's
// binding hands libuv `prefix + "XXXXXX"` unresolved, so the sync error names
// that template; the async one names what libuv's copy holds afterwards (the
// template on Windows, the last name mkdtemp(3) tried elsewhere -- and on
// Windows the empty string when libuv refuses the template outright).
//
// Regression guard: oam appended a 19-digit nanosecond timestamp and created
// every relative prefix under the system temp dir, returning the absolute
// path (`mkdtempSync("sub/x-")` failed ENOENT where node made `sub/x-AbC123`
// beside the cwd), and an empty prefix succeeded where node refuses `XXXXX`
// with EINVAL. Every name is random, so this prints shapes, never names.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const SUFFIX = /^[A-Za-z0-9]{6}$/;
const isName = (prefix, got) =>
  typeof got === "string" && got.length === prefix.length + 6 && got.startsWith(prefix) &&
  SUFFIX.test(got.slice(prefix.length));
const mode = (p) => (process.platform === "win32" ? "-" : (fs.statSync(p).mode & 0o777).toString(8));
function made(label, prefix, got) {
  console.log(
    `${label}: name=${isName(prefix, got)} absolute=${path.isAbsolute(got)}` +
    ` dir=${fs.statSync(got).isDirectory()} mode=${mode(got)}`,
  );
}
// The tail of a failed call's path: the template's X's, or a generated name.
const tail = (p) => (p.endsWith("XXXXXX") ? "template" : SUFFIX.test(p.slice(-6)) ? "name" : "other");
function failed(label, prefix, e) {
  const stem = e.path.slice(0, -6) === prefix;
  console.log(
    `${label}: ${e.code} syscall=${e.syscall} stem=${stem} tail=${tail(e.path)}` +
    ` message-names-path=${e.message.endsWith(`, mkdtemp '${e.path}'`)}`,
  );
}

const home = process.cwd();
const tmpBase = path.join(os.tmpdir(), "oam-conf-302-");
const base = fs.mkdtempSync(tmpBase);
console.log(`absolute prefix: name=${isName(tmpBase, base)}`);
process.chdir(base);
try {
  fs.mkdirSync("sub");
  made("relative", "rel-", fs.mkdtempSync("rel-"));
  console.log(`relative is in cwd: ${fs.readdirSync(".").some((n) => isName("rel-", n))}`);
  made("subdir", "sub/x-", fs.mkdtempSync("sub/x-"));
  made("platform separator", `sub${path.sep}y-`, fs.mkdtempSync(`sub${path.sep}y-`));
  made("dot segments", "./sub/../z-", fs.mkdtempSync("./sub/../z-"));
  made("trailing separator", "sub/", fs.mkdtempSync("sub/"));
  made("unicode", "é中-", fs.mkdtempSync("é中-"));
  made("buffer prefix", "buf-", fs.mkdtempSync(Buffer.from("buf-")));
  made("url prefix", path.join(base, "url-"), fs.mkdtempSync(pathToFileURL(path.join(base, "url-"))));
  made("absolute", path.join(base, "abs-"), fs.mkdtempSync(path.join(base, "abs-")));
  made("promises", "as-", await fsp.mkdtemp("as-"));
  made("fs.promises", "fp-", await fs.promises.mkdtemp("fp-"));
  made("callback", "cb-", await new Promise((res, rej) => fs.mkdtemp("cb-", (e, d) => (e ? rej(e) : res(d)))));

  // Six fresh characters per name, from all of [A-Za-z0-9].
  const names = new Set();
  let shaped = 0;
  for (let i = 0; i < 300; i++) {
    const d = fs.mkdtempSync("many-");
    names.add(d);
    if (isName("many-", d)) shaped++;
  }
  const chars = [...names].map((d) => d.slice(-6)).join("");
  console.log(
    `300 names: distinct=${names.size} shaped=${shaped} lower=${/[a-z]/.test(chars)}` +
    ` upper=${/[A-Z]/.test(chars)} digit=${/[0-9]/.test(chars)}`,
  );

  // Failures.
  try { fs.mkdtempSync("nope/x-"); } catch (e) {
    console.log(`sync missing parent: ${e.code} path=${e.path} message=${e.message}`);
  }
  await fsp.mkdtemp("nope/y-").catch((e) => failed("promises missing parent", "nope/y-", e));
  await new Promise((res) => fs.mkdtemp("nope/z-", (e) => { failed("callback missing parent", "nope/z-", e); res(); }));
  fs.writeFileSync("afile", "");
  try { fs.mkdtempSync("afile/x-"); } catch (e) {
    console.log(`sync parent is a file: ${e.code} path=${e.path}`);
  }
  // node's binding gives an empty prefix five X's, which libuv refuses on
  // Windows and glibc (macOS's mkdtemp(3) is not measured: see
  // docs/node-divergences.md).
  if (process.platform !== "darwin") {
    try { fs.mkdtempSync(""); console.log("empty prefix: made"); } catch (e) {
      console.log(`empty prefix: ${e.code} path=${e.path} message=${e.message}`);
    }
    await fsp.mkdtemp("").then(
      () => console.log("promises empty prefix: made"),
      (e) => console.log(`promises empty prefix: ${e.code} path=${JSON.stringify(e.path)} message=${e.message}`),
    );
  }
} finally {
  process.chdir(home);
  fs.rmSync(base, { recursive: true, force: true });
}
