// Windows named-pipe names (#219): libuv hands the name to the OS as
// written, so `\\.\pipe\x`, `\\?\pipe\x`, `//./pipe/x` and a different
// letter case all name one pipe, and address() answers the name as the
// server was given it. A name that is not a pipe's cannot be listened on
// (`listen EACCES`), and dialling one fails as libuv's CreateFileW does: a
// regular file is ENOTSOCK, a directory EPERM, a missing name ENOENT.
// Elsewhere these are Unix domain socket paths, covered by case 371.
//
// Regression guard: oam had no pipe client or server at all (see 371).
import net from "node:net";
import { fileURLToPath } from "node:url";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("named pipes are Windows only");
} else {
  const pid = String(process.pid);
  const scrub = (s) => (typeof s === "string" ? s.split(pid).join("PID") : s);
  const here = fileURLToPath(import.meta.url);
  const shape = (e) => JSON.stringify({
    keys: Object.keys(e),
    message: scrub(e.message).split(here).join("FILE").split(path.dirname(here)).join("DIR"),
    code: e.code,
    port: e.port,
  });
  const listen = (name) => new Promise((resolve) => {
    const srv = net.createServer((c) => {
      c.on("error", () => {});
      c.end("x");
    });
    srv.on("error", (e) => resolve({ srv: null, err: e }));
    srv.listen(name, () => resolve({ srv, err: null }));
  });
  const dial = (name) => new Promise((resolve) => {
    const s = net.connect(name);
    s.on("data", (d) => resolve("data " + d));
    s.on("error", (e) => resolve("error " + shape(e)));
  });
  const names = [
    ["\\\\.\\pipe\\", "\\\\?\\pipe\\"],
    ["\\\\?\\pipe\\", "\\\\.\\pipe\\"],
    ["//./pipe/", "\\\\.\\pipe\\"],
    ["\\\\.\\pipe\\", "\\\\.\\PIPE\\"],
  ];
  let n = 0;
  for (const [listenAt, dialAt] of names) {
    const leaf = "oam-case-372-" + pid + "-" + ++n;
    const { srv, err } = await listen(listenAt + leaf);
    if (err) {
      console.log(scrub(listenAt + leaf), "listen error", shape(err));
      continue;
    }
    console.log("listening", scrub(srv.address()));
    const dialName = dialAt + (dialAt.includes("PIPE") ? leaf.toUpperCase() : leaf);
    console.log("  dial", scrub(dialName), await dial(dialName));
    await new Promise((r) => srv.close(r));
  }
  for (const name of ["oam-case-372-" + pid, "C:\\oam-case-372-" + pid + ".sock", "\\\\.\\pipe\\"]) {
    const { srv, err } = await listen(name);
    if (srv) srv.close();
    console.log(scrub(name), "listen", err ? shape(err) : "listening");
  }
  console.log("a file", await dial(here));
  console.log("a directory", await dial(path.dirname(here)));
  console.log("no such pipe", await dial("\\\\.\\pipe\\oam-case-372-missing-" + pid));
}
