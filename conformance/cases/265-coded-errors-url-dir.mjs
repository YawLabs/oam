// The coded errors of url.fileURLToPath, of fs given a URL that is not a
// file: URL, and of an fs.Dir used after close(): the error objects
// themselves, not just code and message.
//
// Regression guard: oam built these by hand -- a plain Error (or TypeError)
// with a `code` -- so they sat on Error.prototype and rendered "Error: msg"
// where node's render "TypeError [ERR_INVALID_URL_SCHEME]: msg", and the URL
// path / host refusals were Errors where node's are TypeErrors (an
// `instanceof TypeError` check, or assert.throws({ name: "TypeError" }),
// failed). Node v22.22.2 builds each on the per-code prototype every coded
// error shares (case 248), and oam now does too.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import url from "node:url";

const desc = (o, k) => {
  const d = Object.getOwnPropertyDescriptor(o, k);
  return d ? `${d.writable ? "w" : ""}${d.enumerable ? "e" : ""}${d.configurable ? "c" : ""}` : "-";
};
const protos = new Map();
function report(label, e) {
  const proto = Object.getPrototypeOf(e);
  const first = protos.get(e.code);
  if (first === undefined) protos.set(e.code, proto);
  console.log(
    `${label}: ${e.code} ctor=${e.constructor.name} name=${e.name} instanceof TypeError=${e instanceof TypeError}` +
    ` proto-is-base=${proto === e.constructor.prototype} base=${Object.getPrototypeOf(proto) === e.constructor.prototype}` +
    ` shared=${first === undefined || first === proto} proto.toString=${desc(proto, "toString")}` +
    ` own=${Object.getOwnPropertyNames(e).sort().join(",")} keys=${Object.keys(e).join(",")}` +
    ` string=${String(e)} stack=${e.stack.split("\n")[0]}` +
    (e.input !== undefined ? ` input=${String(e.input)}` : ""),
  );
}
function shape(label, fn) {
  try {
    fn();
  } catch (e) {
    report(label, e);
    return;
  }
  console.log(`${label}: no throw`);
}

shape("fileURLToPath(http:)", () => url.fileURLToPath("http://example.invalid/x"));
shape("fileURLToPath(new URL(http:))", () => url.fileURLToPath(new URL("http://example.invalid/x")));
shape("fileURLToPath encoded / (windows)", () => url.fileURLToPath("file:///C:/a%2fb", { windows: true }));
shape("fileURLToPath encoded \\ (windows)", () => url.fileURLToPath("file:///C:/a%5cb", { windows: true }));
shape("fileURLToPath no drive (windows)", () => url.fileURLToPath("file:///a/b", { windows: true }));
shape("fileURLToPath encoded / (posix)", () => url.fileURLToPath("file:///a%2fb", { windows: false }));
shape("fs.readFileSync(http URL)", () => fs.readFileSync(new URL("http://example.invalid/x")));
try {
  await fsp.readFile(new URL("http://example.invalid/x"));
  console.log("fsp.readFile(http URL): no throw");
} catch (e) {
  report("fsp.readFile(http URL)", e);
}
// Names the host platform (the same for both runtimes on one machine).
shape("fileURLToPath host (posix)", () => url.fileURLToPath("file://host/a", { windows: false }));

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-265-"));
try {
  const d = fs.opendirSync(dir);
  d.closeSync();
  shape("Dir.readSync after close", () => d.readSync());
  shape("Dir.closeSync twice", () => d.closeSync());
  const d2 = await fsp.opendir(dir);
  await d2.close();
  try { await d2.read(); } catch (e) { report("Dir.read() after close", e); }
  try { await d2.close(); } catch (e) { report("Dir.close() twice", e); }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
