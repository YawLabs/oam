// An open with UV_FS_O_FILEMAP of a device on Windows (#384).
//
// With UV_FS_O_FILEMAP libuv's fs__open asks the handle it opened for its
// standard information (GetFileInformationByHandleEx(FileStandardInfo)) and
// fails the open with that call's error: a console, which keeps no file
// information, ERROR_INVALID_FUNCTION -- EISDIR -- and the pipe namespace
// ERROR_INVALID_PARAMETER -- EINVAL. NUL answers it, and opens. Without the
// flag the same opens give a descriptor (case 408).
//
// Regression guard: oam accepted the flag and ignored it, so every one of
// these opened.
//
// Windows only: UV_FS_O_FILEMAP is 0 elsewhere. A console row needs the
// process to have a console; without one its open fails before the flag
// matters, the same under both runtimes.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const { O_RDONLY, O_RDWR, O_WRONLY, UV_FS_O_FILEMAP } = fs.constants;
if (process.platform !== "win32") {
  console.log("UV_FS_O_FILEMAP", UV_FS_O_FILEMAP);
} else {
  const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-543-"));
  process.on("exit", () => {
    try {
      fs.rmSync(made, { recursive: true, force: true });
    } catch {}
  });
  const file = path.join(made, "f.txt");
  fs.writeFileSync(file, "abc");
  const empty = path.join(made, "empty.txt");
  fs.writeFileSync(empty, "");
  const names = [
    ["\\\\.\\CONOUT$", "\\\\.\\CONOUT$"],
    ["\\\\.\\CONIN$", "\\\\.\\CONIN$"],
    ["\\\\.\\CON", "\\\\.\\CON"],
    ["\\\\.\\pipe\\", "\\\\.\\pipe\\"],
    ["\\\\.\\pipe", "\\\\.\\pipe"],
    ["\\\\.\\NUL", "\\\\.\\NUL"],
    ["\\\\.\\mailslot\\", "\\\\.\\mailslot\\"],
    ["a file", file],
    ["an empty file", empty],
    ["a directory", made],
  ];
  const scrub = (s) => (typeof s === "string" ? s.split(made).join("<D>") : s);
  for (const [label, p] of names) {
    const row = [];
    for (const [name, access] of [["O_RDONLY", O_RDONLY], ["O_RDWR", O_RDWR], ["O_WRONLY", O_WRONLY]]) {
      try {
        const fd = fs.openSync(p, access | UV_FS_O_FILEMAP);
        fs.closeSync(fd);
        row.push(`${name}:fd`);
      } catch (e) {
        row.push(`${name}:${e.code}/${e.errno}/${e.syscall}`);
      }
    }
    let message = "";
    try {
      fs.closeSync(fs.openSync(p, O_RDONLY | UV_FS_O_FILEMAP));
    } catch (e) {
      message = ` | ${scrub(e.message)} | path ${JSON.stringify(scrub(e.path))}`;
    }
    const opened = await new Promise((resolve) =>
      fs.open(p, O_RDONLY | UV_FS_O_FILEMAP, (e, fd) => {
        if (e) resolve(`${e.code}/${e.errno}`);
        else fs.close(fd, () => resolve("fd"));
      }),
    );
    let promised;
    try {
      await (await fs.promises.open(p, O_RDONLY | UV_FS_O_FILEMAP)).close();
      promised = "fd";
    } catch (e) {
      promised = `${e.code}/${e.errno}`;
    }
    console.log(`${label}: ${row.join(" ")} | open ${opened} | promises.open ${promised}${message}`);
  }
  // A file opened with the flag reads as any other.
  const fd = fs.openSync(file, O_RDONLY | UV_FS_O_FILEMAP);
  console.log("read", JSON.stringify(fs.readFileSync(fd, "utf8")));
  fs.closeSync(fd);
}
