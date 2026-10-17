// tty.isatty and the tty stream constructors on descriptors that are not
// terminals (#379, #381): the null device opened with every flag fs.open
// takes, and descriptors asked again once they are closed. isatty answers
// false for all of them without throwing -- a closed descriptor included --
// and both constructors refuse each with ERR_TTY_INIT_FAILED from
// uv_tty_init.
//
// The console rows (a console opened by name is a TTY, and which of its
// opens uv_tty_init takes: EPERM for one whose access libuv cannot use) need
// a real console, which this harness never gives a case; they are the
// ConPTY tests' (`tty_streams_over_a_console_opened_by_name_init_as_node`,
// `fstat_of_a_console_opened_by_name_answers_as_node`).
//
// Platform rows: uv_tty_init's errno is EBADF on Windows for anything that
// is not a console and EINVAL on POSIX for the null device or a closed
// descriptor, so the errno prints on Windows only, as in case 418.
import tty from "node:tty";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const isWin = process.platform === "win32";
const nullDevice = isWin ? "\\\\.\\NUL" : "/dev/null";

const verdict = (C, fd) => {
  try {
    new C(fd);
    return "constructed";
  } catch (e) {
    if (e.code !== "ERR_TTY_INIT_FAILED") return `${e.name} ${e.code}`;
    return [e.code, e.syscall, e.info.syscall, isWin ? e.info.code : "(errno: windows-only)"].join(" ");
  }
};
const ask = (label, fd) => {
  let atty;
  try {
    atty = tty.isatty(fd);
  } catch (e) {
    atty = `threw ${e.code}`;
  }
  console.log(label, "isatty", atty);
  console.log(label, "WriteStream", verdict(tty.WriteStream, fd));
  console.log(label, "ReadStream", verdict(tty.ReadStream, fd));
};

for (const flag of ["r", "r+", "rs+", "w", "wx", "w+", "a", "a+", "as+"]) {
  let fd;
  try {
    fd = fs.openSync(nullDevice, flag);
  } catch (e) {
    console.log(`null device ${flag}`, "open", e.code);
    continue;
  }
  ask(`null device ${flag}`, fd);
  fs.closeSync(fd);
  // Asked straight after the close, before anything can reuse the number.
  ask(`null device ${flag} (closed)`, fd);
}

const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-550-")));
try {
  const fd = fs.openSync(path.join(dir, "file"), "w+");
  ask("file", fd);
  fs.closeSync(fd);
  ask("file (closed)", fd);
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
console.log("done");
