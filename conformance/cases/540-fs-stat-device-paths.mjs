// What stat, access, chmod, realpath, readdir and cpSync say of a Windows
// device path (#380, #383).
//
// libuv stats a path first with GetFileInformationByName, which on Windows
// 11 24H2 and later tells it the device type, and otherwise through a handle
// it asks NtQueryVolumeInformationFile and NtQueryInformationFile
// (FileAllInformation). NUL -- however it is spelled: \\.\NUL, \\?\NUL,
// \\.\Global\NUL, \\.\GLOBALROOT\Device\Null -- gets a fixed character
// device's stat, the shape fstat gives an open NUL: mode 0o20666, rdev
// FILE_DEVICE_NULL << 16, blksize 4096, nlink 1, every other number and time
// 0. Any other device reports what its query fails with: EINVAL for the pipe
// and mailslot namespaces and for KsecDD.
//
// access and chmod go through GetFileAttributesW (chmod through the CRT's
// _wchmod, which asks the same before SetFileAttributesW). A device answers
// that ERROR_INVALID_PARAMETER, EINVAL -- NUL included -- and a share root
// that is no disk ERROR_NOT_SUPPORTED, ENOTSUP. cpSync's C++ asks the same
// question of its source (MSVC's std::filesystem::status), and names
// `lstat`. readdir of KsecDD fails its directory query
// STATUS_INVALID_DEVICE_REQUEST, EISDIR.
//
// Regression guard: oam stat through std (GetFileInformationByHandle),
// whose ERROR_INVALID_FUNCTION on any device read as EISDIR, and access,
// chmod and cpSync asked a stat too.
//
// Windows only. The share rows run where the loopback share answers, as
// case 408's do; the console is left out (what it says depends on whether
// the process has one).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("Windows device paths: nothing to stat here");
} else {
  const drive = process.cwd()[0];
  const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-540-"));
  process.on("exit", () => {
    try {
      fs.rmSync(made, { recursive: true, force: true });
    } catch {}
  });
  const names = [
    "\\\\.\\NUL",
    "\\\\.\\nul",
    "\\\\?\\NUL",
    "\\\\.\\Global\\NUL",
    "\\\\.\\GLOBALROOT\\Device\\Null",
    "\\\\?\\GLOBALROOT\\Device\\Null",
    "\\\\.\\pipe\\",
    "\\\\.\\pipe",
    "\\\\.\\mailslot",
    "\\\\.\\mailslot\\",
    "\\\\?\\GLOBALROOT\\Device\\KsecDD",
    `\\\\.\\${drive}:`,
  ];
  if (fs.existsSync(`\\\\localhost\\${drive}$`)) {
    names.push(
      "\\\\localhost\\IPC$",
      "\\\\.\\UNC\\localhost\\IPC$",
      "\\\\?\\UNC\\localhost\\IPC$",
      "\\\\localhost\\pipe\\",
    );
  }
  const scrub = (s) => String(s).split(`${drive}:`).join("<D>:");
  const stats = (s) =>
    `mode ${s.mode.toString(8)} rdev ${s.rdev} blksize ${s.blksize} nlink ${s.nlink} ino ${s.ino} ` +
    `dev ${s.dev} size ${s.size} blocks ${s.blocks} uid ${s.uid} gid ${s.gid} ` +
    `times ${s.atimeMs}/${s.mtimeMs}/${s.ctimeMs}/${s.birthtimeMs} ` +
    `char ${s.isCharacterDevice()} file ${s.isFile()} dir ${s.isDirectory()}`;
  const brief = (e) => `${e.code}/${e.errno}`;
  const full = (e) => `${e.code}/${e.errno}/${e.syscall} ${scrub(e.message)} path ${JSON.stringify(scrub(e.path))}`;
  const sync = (f, ok = (v) => (v === undefined ? "ok" : JSON.stringify(scrub(v)))) => {
    try {
      return ok(f());
    } catch (e) {
      return full(e);
    }
  };
  const settle = async (p, ok = () => "ok") => {
    try {
      return ok(await p);
    } catch (e) {
      return brief(e);
    }
  };
  const cb = (f, ok = () => "ok") =>
    new Promise((resolve) => f((e, v) => resolve(e ? brief(e) : ok(v))));
  const mode = (s) => `mode ${s.mode.toString(8)}`;

  for (const p of names) {
    console.log(scrub(JSON.stringify(p)));
    console.log("  statSync", sync(() => fs.statSync(p), stats));
    console.log("  lstatSync", sync(() => fs.lstatSync(p), stats));
    console.log("  statSync bigint", sync(() => fs.statSync(p, { bigint: true }), (s) => `mode ${s.mode} rdev ${s.rdev}`));
    console.log("  stat", await cb((done) => fs.stat(p, done), mode));
    console.log("  lstat", await cb((done) => fs.lstat(p, done), mode));
    console.log("  promises.stat", await settle(fs.promises.stat(p), mode));
    console.log("  promises.lstat", await settle(fs.promises.lstat(p), mode));
    console.log("  accessSync", sync(() => fs.accessSync(p)));
    console.log("  accessSync W_OK", sync(() => fs.accessSync(p, fs.constants.W_OK)));
    console.log("  access", await cb((done) => fs.access(p, done)));
    console.log("  promises.access", await settle(fs.promises.access(p)));
    console.log("  existsSync", fs.existsSync(p));
    console.log("  chmodSync", sync(() => fs.chmodSync(p, 0o666)));
    console.log("  chmod", await cb((done) => fs.chmod(p, 0o666, done)));
    console.log("  promises.chmod", await settle(fs.promises.chmod(p, 0o666)));
    console.log("  realpathSync", sync(() => fs.realpathSync(p)));
    console.log("  realpath", await cb((done) => fs.realpath(p, done), (v) => JSON.stringify(scrub(v))));
    console.log("  realpathSync.native", sync(() => fs.realpathSync.native(p)));
    console.log("  readdirSync", sync(() => fs.readdirSync(p), (v) => (Array.isArray(v) ? "array" : typeof v)));
    console.log("  cpSync", sync(() => fs.cpSync(p, path.join(made, "copy"))));
    console.log("  cpSync recursive", sync(() => fs.cpSync(p, path.join(made, "copy"), { recursive: true })));
  }

  // The shape a path stat gives NUL is fstat's of an open NUL.
  const fd = fs.openSync("\\\\.\\NUL", "r");
  console.log("fstatSync of an open NUL", stats(fs.fstatSync(fd)));
  fs.closeSync(fd);
}
