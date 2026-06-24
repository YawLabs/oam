// fs.constants O_* are the platform's fcntl/CRT values -- O_CREAT/O_EXCL/O_TRUNC/
// O_APPEND differ between Linux, Windows (MSVCRT), and macOS (BSD) -- and
// os.constants.errno is POSITIVE (POSIX), not libuv-negative. The differential
// (node vs oam on the same platform) verifies the per-platform tables match,
// and a numeric-flag open must round-trip identically on every platform.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// 1) per-platform O_* values must equal node's on this platform.
const O = ["O_RDONLY", "O_WRONLY", "O_RDWR", "O_CREAT", "O_EXCL", "O_TRUNC", "O_APPEND"];
console.log("O " + O.map((k) => `${k}=${fs.constants[k]}`).join(" "));

// 2) os.constants.errno is positive POSIX (stable codes, same on all 3 platforms).
const E = ["ENOENT", "EEXIST", "EACCES", "EBADF", "EPERM", "EINVAL", "EISDIR", "ENOTDIR"];
console.log("errno " + E.map((k) => `${k}=${os.constants.errno[k]}`).join(" "));

// 3) opening with NUMERIC O_ flags must create+write+read identically -- this
//    exercises the per-platform O_* values AND numericOpenFlags consistency, and
//    is differential-safe (same output regardless of the underlying values).
const tmp = path.join(os.tmpdir(), `oam-oflags-${process.pid}.txt`);
const fd = fs.openSync(tmp, fs.constants.O_CREAT | fs.constants.O_WRONLY | fs.constants.O_TRUNC);
fs.writeSync(fd, "oflags-ok");
fs.closeSync(fd);
console.log("numeric-open", fs.readFileSync(tmp, "utf8"));
const fd2 = fs.openSync(tmp, fs.constants.O_WRONLY | fs.constants.O_APPEND);
fs.writeSync(fd2, "+append");
fs.closeSync(fd2);
console.log("after-append", fs.readFileSync(tmp, "utf8"));
fs.rmSync(tmp, { force: true });
