// fs.readdir's `encoding: "buffer"` and `recursive: true` together with it.
// - `encoding: "buffer"` lists raw name bytes: each name is a `Buffer` (its
//   UTF-8 bytes -- `Buffer.from(name)`), and a Dirent's `name` is that Buffer
//   too. `encoding: "utf8"`, no encoding, and a string options argument all
//   still decode to strings. oam returned plain strings for "buffer".
// - With `recursive: true` AND `"buffer"`, node joins the Buffer names into
//   paths and its path validation refuses the first join:
//   ERR_INVALID_ARG_TYPE "The \"path\" argument must be of type string.
//   Received an instance of Buffer" -- whenever anything would be joined, in
//   all three forms and with or without withFileTypes. An empty directory has
//   nothing to join and returns []. oam used to silently coerce.
// Both forms are measured on node v22.22.2.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-466-"));
const scrub = (s) => (typeof s === "string" ? s.split(made).join("<D>") : s);
const show = (label, f) => {
  try {
    const r = f();
    console.log(label + ": OK " + JSON.stringify(scrub(r)));
  } catch (e) {
    console.log(label + ": " + [e.name, e.code, scrub(e.message)].join(" | "));
  }
};
const settle = async (label, f) => {
  try {
    const r = await f();
    console.log(label + ": OK " + JSON.stringify(scrub(r)));
  } catch (e) {
    console.log(label + ": " + [e.name, e.code, scrub(e.message)].join(" | "));
  }
};
const viaCallback = (fn, ...args) => new Promise((res, rej) => fn(...args, (e, v) => (e ? rej(e) : res(v))));
// A Buffer's hex, so the pinned line shows the exact bytes.
const kinds = (list) => list.map((x) => (Buffer.isBuffer(x) ? "BUF:" + x.toString("hex") : "str:" + x)).sort();
const direntKinds = (list) =>
  list
    .map((d) => (Buffer.isBuffer(d.name) ? "BUF:" + d.name.toString("hex") : "str:" + d.name) + ":" + d.isFile())
    .sort();

fs.writeFileSync(path.join(made, "a.txt"), "x");
fs.writeFileSync(path.join(made, "\u00e9.txt"), "x");
fs.mkdirSync(path.join(made, "sub"));
const empty = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-466-empty-"));

show("sync buffer", () => kinds(fs.readdirSync(made, { encoding: "buffer" })));
show("sync buffer withFileTypes", () => direntKinds(fs.readdirSync(made, { encoding: "buffer", withFileTypes: true })));
show("sync utf8", () => kinds(fs.readdirSync(made, { encoding: "utf8" })));
show("sync none", () => kinds(fs.readdirSync(made)));
show("sync string option", () => kinds(fs.readdirSync(made, "buffer")));
show("sync buffer empty dir", () => fs.readdirSync(empty, { encoding: "buffer" }));
show("sync buffer recursive", () => fs.readdirSync(made, { encoding: "buffer", recursive: true }));
show("sync buffer recursive withFileTypes", () =>
  fs.readdirSync(made, { encoding: "buffer", recursive: true, withFileTypes: true }));
show("sync buffer recursive empty dir", () => fs.readdirSync(empty, { encoding: "buffer", recursive: true }));

await settle("promise buffer", async () => kinds(await fs.promises.readdir(made, { encoding: "buffer" })));
await settle("promise buffer withFileTypes", async () =>
  direntKinds(await fs.promises.readdir(made, { encoding: "buffer", withFileTypes: true })));
await settle("promise buffer recursive", () => fs.promises.readdir(made, { encoding: "buffer", recursive: true }));
await settle("callback buffer", async () => kinds(await viaCallback(fs.readdir, made, { encoding: "buffer" })));

// The callback + recursive + buffer combination is node's alone to pin: node
// throws the join failure UNCAUGHT from its scandir callback (an internal
// `node:fs` stack, exit 1) rather than handing it to the user callback, so its
// output cannot be diffed byte-for-byte and is left out. oam delivers the same
// ERR_INVALID_ARG_TYPE to the callback instead.

fs.rmSync(made, { recursive: true, force: true });
fs.rmSync(empty, { recursive: true, force: true });
