// Which path an fs failure names. node's rule: the error reports the path its
// JS layer handed the binding. fs namespaces every path it is given (on Windows
// `toNamespacedPath`: resolved against the cwd, `\\?\` prefixed), and the
// error shows it with the prefix taken back off, so `readFileSync("x")` fails
// `open 'C:\cwd\x'` there and `open 'x'` elsewhere -- for relative, absolute,
// forward-slash, dotted, `\\?\`, Buffer and file: URL inputs alike, in the
// sync, callback and promise APIs, and in `dest` for two-path calls.
// `process.loadEnvFile()` with no path is the exception: its binding opens its
// own ".env" untouched and the error says '.env' (node's
// test-process-load-env-file); a path given to it is namespaced like fs's.
// Runs in a fresh temp directory; every absolute path is printed relative to
// it, as <cwd>.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const home = process.cwd();
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-354-"));
process.chdir(dir);
const cwd = process.cwd();

const scrub = (v) => {
  if (typeof v !== "string") return v;
  return v.split("\\\\?\\" + cwd).join("<ns-cwd>").split(cwd).join("<cwd>");
};
const show = (name, e) => {
  if (!e) return console.log(name, "no error");
  console.log(name, JSON.stringify({
    name: e.name,
    code: e.code,
    errno: typeof e.errno,
    syscall: e.syscall,
    path: scrub(e.path),
    dest: scrub(e.dest),
    message: scrub(e.message),
  }));
};
const sync = (name, f) => {
  try {
    f();
    show(name);
  } catch (e) {
    show(name, e);
  }
};
const cb = (name, f) => new Promise((res) => f((e) => (show(name, e), res())));
const prom = async (name, f) => {
  try {
    await f();
    show(name);
  } catch (e) {
    show(name, e);
  }
};

const rel = "missing.txt";
const relDir = "missing-dir/sub";
const relDots = "./a/../missing.txt";
const relSlash = "missing-dir/x.txt";
const abs = path.join(cwd, rel);
const absFwd = abs.split("\\").join("/");
const nsAbs = process.platform === "win32" ? "\\\\?\\" + abs : abs;
const dest = "dest.txt";

try {
  sync("readFileSync rel", () => fs.readFileSync(rel));
  sync("readFileSync dots", () => fs.readFileSync(relDots));
  sync("readFileSync slash", () => fs.readFileSync(relSlash));
  sync("readFileSync abs", () => fs.readFileSync(abs));
  sync("readFileSync abs forward", () => fs.readFileSync(absFwd));
  sync("readFileSync namespaced", () => fs.readFileSync(nsAbs));
  sync("readFileSync buffer", () => fs.readFileSync(Buffer.from(rel)));
  sync("readFileSync url", () => fs.readFileSync(pathToFileURL(abs)));
  sync("openSync", () => fs.openSync(relSlash));
  sync("statSync", () => fs.statSync(rel));
  sync("lstatSync", () => fs.lstatSync(rel));
  sync("readdirSync", () => fs.readdirSync(rel));
  sync("mkdirSync", () => fs.mkdirSync(relDir));
  sync("rmSync", () => fs.rmSync(rel));
  sync("rmdirSync", () => fs.rmdirSync(rel));
  sync("unlinkSync", () => fs.unlinkSync(rel));
  sync("renameSync", () => fs.renameSync(rel, dest));
  sync("copyFileSync", () => fs.copyFileSync(rel, dest));
  sync("accessSync", () => fs.accessSync(rel));
  sync("realpathSync", () => fs.realpathSync(rel));
  sync("realpathSync.native", () => fs.realpathSync.native(rel));
  sync("readlinkSync", () => fs.readlinkSync(rel));
  sync("chmodSync", () => fs.chmodSync(rel, 0o644));

  sync("loadEnvFile()", () => process.loadEnvFile());
  sync("loadEnvFile(undefined)", () => process.loadEnvFile(undefined));
  sync("loadEnvFile(null)", () => process.loadEnvFile(null));
  sync("loadEnvFile('')", () => process.loadEnvFile(""));
  sync("loadEnvFile('.env')", () => process.loadEnvFile(".env"));
  sync("loadEnvFile rel", () => process.loadEnvFile(relSlash));
  sync("loadEnvFile abs", () => process.loadEnvFile(abs));
  sync("loadEnvFile buffer", () => process.loadEnvFile(Buffer.from(rel)));
  sync("loadEnvFile url", () => process.loadEnvFile(pathToFileURL(abs)));
  sync("loadEnvFile(42)", () => process.loadEnvFile(42));
  sync("loadEnvFile directory", () => process.loadEnvFile("."));

  await cb("readFile cb", (k) => fs.readFile(rel, k));
  await cb("readFile cb abs", (k) => fs.readFile(abs, k));
  await cb("open cb namespaced", (k) => fs.open(nsAbs, k));
  await cb("stat cb", (k) => fs.stat(rel, k));
  await cb("access cb", (k) => fs.access(rel, k));
  await cb("mkdir cb", (k) => fs.mkdir(relDir, k));
  await cb("rm cb", (k) => fs.rm(rel, k));
  await cb("rename cb", (k) => fs.rename(rel, dest, k));
  await cb("copyFile cb", (k) => fs.copyFile(rel, dest, k));
  await cb("realpath cb", (k) => fs.realpath(rel, k));
  await cb("readdir cb", (k) => fs.readdir(rel, k));
  await cb("createReadStream", (k) => fs.createReadStream(rel).on("error", k));

  await prom("readFile p", () => fsp.readFile(rel));
  await prom("readFile p url", () => fsp.readFile(pathToFileURL(abs)));
  await prom("open p", () => fsp.open(rel));
  await prom("stat p", () => fsp.stat(rel));
  await prom("access p", () => fsp.access(rel));
  await prom("mkdir p", () => fsp.mkdir(relDir));
  await prom("rm p", () => fsp.rm(rel));
  await prom("readdir p", () => fsp.readdir(rel));
  await prom("rename p", () => fsp.rename(rel, dest));
  await prom("copyFile p", () => fsp.copyFile(rel, dest));
  await prom("realpath p", () => fsp.realpath(rel));
} finally {
  process.chdir(home);
  fs.rmSync(dir, { recursive: true, force: true });
}
