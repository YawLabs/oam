// When a write over a pipe settles (#219): a Windows named pipe, a Unix
// domain socket elsewhere.
//
// 1. An allowHalfOpen server whose 'end' listener answers with write() +
//    end() to a client that has gone: the write fails with EPIPE, and its
//    callback, then 'error', then 'close' follow in that order. Regression
//    guard: oam closed the socket inside the 'end' listener's end() --
//    'close' came first, the write callback after it, and the 'error' was
//    never emitted.
// 2. Writes to a peer that reads nothing stay pending: no callback runs and
//    every byte is still counted in writableLength. Regression guard: oam's
//    named pipe took a whole 1 MiB write into a buffer of its own and called
//    it written, so its callback ran though the peer had read nothing.
//
// Only the server side's events are logged in 1: the order between the two
// sides is the scheduler's, not node's contract.
import net from "node:net";
import os from "node:os";
import path from "node:path";

const base = process.platform === "win32"
  ? String.raw`\\.\pipe\oam-case-374-` + process.pid
  : path.join(os.tmpdir(), "oam-case-374-" + process.pid);
let serial = 0;
const fresh = () => base + "-" + ++serial + (process.platform === "win32" ? "" : ".sock");

for (const how of ["destroy", "end"]) {
  const P = fresh();
  const log = [];
  await new Promise((done) => {
    const srv = net.createServer({ allowHalfOpen: true }, (s) => {
      s.on("end", () => {
        s.write("reply", (e) => log.push("write cb " + (e && e.code)));
        s.end();
      });
      s.on("error", (e) => log.push("error " + e.code));
      s.on("close", (hadError) => {
        log.push("close " + hadError);
        srv.close(done);
      });
    });
    srv.listen(P, () => {
      const c = net.connect(P, () => (how === "destroy" ? c.destroy() : c.end()));
      c.on("error", () => {});
      c.resume();
    });
  });
  console.log("half-open, client " + how + ":", log.join(", "));
}

{
  const P = fresh();
  const held = [];
  const srv = net.createServer((s) => {
    s.pause();
    held.push(s);
  });
  await new Promise((r) => srv.listen(P, r));
  const c = net.connect(P);
  await new Promise((r) => c.on("connect", r));
  let callbacks = 0;
  const returns = [];
  const chunk = Buffer.alloc(1 << 20, 1);
  for (let i = 0; i < 4; i++) returns.push(c.write(chunk, () => callbacks++));
  await new Promise((r) => setTimeout(r, 300));
  console.log("unread writes:", JSON.stringify({ returns, callbacks, writableLength: c.writableLength }));
  c.on("error", () => {});
  c.destroy();
  for (const s of held) s.destroy();
  srv.close();
}
