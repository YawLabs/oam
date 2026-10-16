// fs.ReadStream.prototype.open and fs.WriteStream.prototype.open are
// node's deprecated no-ops (DEP0135), and a stream whose `open` is not
// one of them opens by calling it, as node's does (#356): the stream
// counts as constructed at the first 'open' it emits, or failed at the
// first 'error', and its own open is all that runs -- no options.fs.open,
// no fs.open.
//
// Regression guard: oam's streams had no `open` method, and opened
// natively whatever a subclass or an assignment put there.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-533-")));
process.chdir(root);
const warnings = [];
process.on("warning", (w) => warnings.push(`${w.name} ${w.code} ${w.message}`));
const closed = (s) => new Promise((resolve) => s.on("close", resolve));

const kinds = [fs.ReadStream, fs.WriteStream];
for (const K of kinds) {
  const name = K.name;
  console.log(name, typeof K.prototype.open, K.prototype.open.length, K.prototype.open.name);
}
// Calling the deprecated one does nothing but warn, once per code.
{
  const ws = fs.createWriteStream("w.txt");
  console.log("prototype open returns", ws.open(), fs.ReadStream.prototype.open.call({}));
  ws.end("w");
  await closed(ws);
}

fs.writeFileSync("r.txt", "data");
// A subclass with its own open: it opens, and emits 'open' with a fd.
class MyRead extends fs.ReadStream {
  open() {
    this.log.push(`open called ${typeof this.fd}`);
    fs.open(this.path, "r", (err, fd) => {
      if (err) { this.emit("error", err); return; }
      this.fd = fd;
      this.emit("open", fd);
    });
  }
}
{
  MyRead.prototype.log = [];
  const rs = new MyRead("r.txt", { fs: { open() { MyRead.prototype.log.push("options.fs.open"); }, read: fs.read, close: fs.close } });
  rs.on("ready", () => rs.log.push("ready"));
  rs.on("open", () => rs.log.push("'open'"));
  const got = [];
  rs.on("data", (c) => got.push(String(c)));
  await closed(rs);
  console.log("subclass open", JSON.stringify(rs.log), JSON.stringify(got));
}
// An open that fails by emitting 'error'.
{
  const log = [];
  const ws = fs.createWriteStream("x.txt");
  ws.open = function () { log.push("own open"); this.emit("error", Object.assign(new Error("own"), { code: "EOWN" })); };
  // The assignment comes after the constructor: construction is already
  // queued, and it is the open seen at that point that counts.
  ws.on("error", (e) => log.push(`error ${e.code}`));
  await closed(ws);
  console.log("assigned open fails", JSON.stringify(log), fs.existsSync("x.txt"));
}
// An instance open assigned before construction runs replaces the open.
{
  const log = [];
  const ws = new fs.WriteStream("y.txt");
  ws.open = function () {
    log.push("own open");
    fs.open("y.txt", "w", (err, fd) => { this.fd = fd; this.emit("open", fd); });
  };
  ws.on("open", () => log.push("'open'"));
  ws.on("ready", () => log.push("ready"));
  ws.end("y");
  await closed(ws);
  console.log("assigned open", JSON.stringify(log), fs.readFileSync("y.txt", "utf8"));
}
await new Promise((resolve) => setImmediate(resolve));
console.log("warnings", JSON.stringify(warnings));

process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
