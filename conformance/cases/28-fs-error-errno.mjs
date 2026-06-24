// fs errors carry err.errno -- the negative libuv error NUMBER (platform-
// specific: ENOENT is -4058 on Windows, -2 on Unix; EBADF is -4083 / -9). oam
// left err.errno undefined. Differential vs node: the value must match on each
// platform (oam derives -errno on Unix and a libuv-win table on Windows).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

function grab(fn) {
  try {
    fn();
    return "NO-THROW";
  } catch (e) {
    return `${e.code} errno=${e.errno}`;
  }
}

// ENOENT: open a missing file.
console.log("open-missing", grab(() => fs.openSync(path.join(os.tmpdir(), `oam-no-such-${process.pid}`), "r")));

// EBADF: read a closed fd.
const tmp = path.join(os.tmpdir(), `oam-errno-${process.pid}.txt`);
fs.writeFileSync(tmp, "x");
const fd = fs.openSync(tmp, "r");
fs.closeSync(fd);
console.log("read-closed", grab(() => fs.readSync(fd, Buffer.alloc(1), 0, 1, null)));
fs.rmSync(tmp, { force: true });
