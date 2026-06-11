// fs: sync + promises round trips, error codes, dirents. Output uses only
// relative names so the two runtimes agree byte-for-byte.
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { mkdir, writeFile, readFile, readdir, rm } from "node:fs/promises";

// Fixed pid-keyed scratch dir, pre-cleaned: identical control flow under
// both runtimes, no leftovers can skew the readdir output.
const dir = path.join(os.tmpdir(), `oam-conf-${process.pid}`);
fs.rmSync(dir, { recursive: true, force: true });
fs.mkdirSync(dir, { recursive: true });

fs.writeFileSync(path.join(dir, "a.txt"), "sync-write");
console.log(fs.readFileSync(path.join(dir, "a.txt"), "utf8"), fs.existsSync(path.join(dir, "a.txt")));
fs.writeFileSync(path.join(dir, "enc.bin"), "aGk=", { encoding: "base64" });
console.log(fs.readFileSync(path.join(dir, "enc.bin"), "utf8"), fs.readFileSync(path.join(dir, "enc.bin"), "hex"));
console.log(fs.statSync(path.join(dir, "a.txt")).isFile(), fs.statSync(dir).isDirectory());
await mkdir(path.join(dir, "deep", "deeper"), { recursive: true });
await writeFile(path.join(dir, "b.txt"), "async-write");
console.log(await readFile(path.join(dir, "b.txt"), "utf8"));
console.log((await readdir(dir)).sort().join(","));
const dirents = fs.readdirSync(dir, { withFileTypes: true }).sort((x, y) => (x.name < y.name ? -1 : 1));
console.log(dirents.map((d) => `${d.name}:${d.isDirectory() ? "d" : "f"}`).join(","));
let syncCode = "";
try {
  fs.readFileSync(path.join(dir, "nope.txt"));
} catch (e) {
  syncCode = e.code;
}
let asyncCode = "";
try {
  await readFile(path.join(dir, "nope.txt"));
} catch (e) {
  asyncCode = e.code;
}
let rmdirCode = "";
try {
  fs.rmdirSync(path.join(dir, "a.txt"));
} catch (e) {
  rmdirCode = e.code;
}
console.log(syncCode, asyncCode, rmdirCode, fs.existsSync(path.join(dir, "a.txt")));
await rm(dir, { recursive: true, force: true });
console.log(fs.existsSync(dir));
