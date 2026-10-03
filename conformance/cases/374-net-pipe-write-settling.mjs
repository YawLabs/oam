// When a write over a pipe settles (#219): a Windows named pipe, a Unix
// domain socket elsewhere.
//
// An allowHalfOpen server whose 'end' listener answers with write() +
// end() to a client that has gone: the write fails with EPIPE, and its
// callback, then 'error', then 'close' follow in that order. Regression
// guard: oam closed the socket inside the 'end' listener's end() --
// 'close' came first, the write callback after it, and the 'error' was
// never emitted.
//
// Only the server side's events are logged: the order between the two sides
// is the scheduler's, not node's contract.
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
