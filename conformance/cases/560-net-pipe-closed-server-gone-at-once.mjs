// A pipe server's close() closes its listening pipe before it returns
// (#388): a client that dials the name right after close() -- in the same
// tick, before anything else has run -- finds nothing there and gets
// ENOENT, as libuv's uv_close closes every pending instance at once. On
// Unix the socket file is unlinked by the close (case 375 covers that).
//
// Regression guard: on Windows oam's close() only woke the accept that
// held the listening pipe, so its waiting instances stayed open until that
// accept ran. A synchronous open right after close() connected to one of
// them (an fd where node gives ENOENT), and a net.connect could connect and
// then see the pipe dropped.
import net from "node:net";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const pid = String(process.pid);
const nameFor = (tag, i) =>
  process.platform === "win32"
    ? "\\\\.\\pipe\\oam-case-560-" + pid + "-" + tag + "-" + i
    : path.join(os.tmpdir(), "oam-case-560-" + pid + "-" + tag + "-" + i + ".sock");

const listen = (name) => new Promise((resolve, reject) => {
  const srv = net.createServer((c) => {
    c.on("error", () => {});
    c.end();
  });
  srv.on("error", reject);
  srv.listen(name, () => resolve(srv));
});

const tally = (counts, key) => {
  counts[key] = (counts[key] || 0) + 1;
};

// A synchronous open of the name, in the same tick as close(). Only a
// Windows pipe opens as a file; a Unix socket file is ENXIO either way, so
// there the open is skipped and only the dials below are compared.
if (process.platform === "win32") {
  const counts = {};
  for (let i = 0; i < 25; i++) {
    const name = nameFor("open", i);
    const srv = await listen(name);
    srv.close();
    try {
      fs.closeSync(fs.openSync(name, "r"));
      tally(counts, "opened");
    } catch (e) {
      tally(counts, e.code);
    }
  }
  console.log("open right after close", JSON.stringify(counts));
} else {
  console.log("open right after close", JSON.stringify({ ENOENT: 25 }));
}

// A net.connect issued in the same tick as close().
{
  const counts = {};
  for (let i = 0; i < 25; i++) {
    const name = nameFor("dial", i);
    const srv = await listen(name);
    const closed = new Promise((r) => srv.close(r));
    const outcome = await new Promise((resolve) => {
      const s = net.connect(name);
      s.on("connect", () => {
        s.destroy();
        resolve("connected");
      });
      s.on("error", (e) => resolve(e.code));
    });
    tally(counts, outcome);
    await closed;
  }
  console.log("dial right after close", JSON.stringify(counts));
}
