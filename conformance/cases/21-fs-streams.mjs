// fs.ReadStream / fs.WriteStream as real Readable/Writable subclasses with a
// `.prototype` (graceful-fs does Object.create(fs.ReadStream.prototype)), and
// createReadStream/createWriteStream returning instances of them. Regression
// guard for the gap where fs.ReadStream was an arrow alias (no .prototype),
// which crashed graceful-fs -> playwright-core on oam.
import fs from "node:fs";
import { Readable, Writable } from "node:stream";
import os from "node:os";
import path from "node:path";

console.log("ctors", typeof fs.ReadStream, typeof fs.WriteStream, typeof fs.ReadStream.prototype);
console.log("object-create", typeof Object.create(fs.ReadStream.prototype));

const tmp = path.join(os.tmpdir(), "oam-conf-rs.txt");
const content = "stream class line\n".repeat(8);
fs.writeFileSync(tmp, content);

const rs = fs.createReadStream(tmp);
console.log("rs", rs instanceof fs.ReadStream, rs instanceof Readable);
let data = "";
rs.on("data", (c) => { data += c; });
rs.on("end", () => {
  console.log("read", data === content);
  const out = path.join(os.tmpdir(), "oam-conf-ws.txt");
  const ws = fs.createWriteStream(out);
  console.log("ws", ws instanceof fs.WriteStream, ws instanceof Writable);
  ws.on("finish", () => {
    console.log("write", fs.readFileSync(out, "utf8") === "written");
    fs.unlinkSync(tmp);
    fs.unlinkSync(out);
  });
  ws.end("written");
});
