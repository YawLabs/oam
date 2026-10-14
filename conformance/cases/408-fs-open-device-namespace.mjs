// On Windows libuv hands every handle its open gets to the C runtime's
// _open_osfhandle, which refuses one whose type GetFileType cannot name
// (#330). libuv reports the refusal as the thread's last error, or as
// UV_UNKNOWN where there is none:
//
// - the root of a share that is no disk, however it is spelled -- \\srv\IPC$,
//   \\?\UNC\srv\IPC$, \\.\UNC\srv\IPC$ -- is EINVAL (the last spelling opened
//   in oam until 0.19.1);
// - the mailslot namespace, \\.\mailslot, is UNKNOWN, errno -4094 (it
//   opened);
// - NUL and the pipe namespace, which GetFileType names, open.
//
// Every open form, and writeFile / appendFile of no data, which open too.
// The share rows run where the loopback share answers, as case 392's do.
import fs from "node:fs";

if (process.platform !== "win32") {
  console.log("Windows device namespaces: nothing to open here");
} else {
  const drive = process.cwd()[0];
  const names = ["\\\\.\\mailslot", "\\\\.\\mailslot\\", "\\\\.\\NUL", "\\\\.\\pipe\\"];
  if (fs.existsSync(`\\\\localhost\\${drive}$`)) {
    names.unshift(
      "\\\\.\\UNC\\localhost\\IPC$",
      "\\\\.\\UNC\\localhost\\IPC$\\",
      "\\\\localhost\\IPC$",
      "\\\\?\\UNC\\localhost\\IPC$",
    );
  }
  for (const p of names) {
    const row = [];
    for (const flag of ["r", "r+", "w", "w+", "a", "wx", "ax"]) {
      try {
        fs.closeSync(fs.openSync(p, flag));
        row.push(`${flag}:fd`);
      } catch (e) {
        row.push(`${flag}:${e.code}/${e.errno}/${e.syscall}`);
      }
    }
    const attempt = (f) => {
      try {
        f();
        return "ok";
      } catch (e) {
        return `${e.code}/${e.errno}`;
      }
    };
    const writes = attempt(() => fs.writeFileSync(p, ""));
    const appends = attempt(() => fs.appendFileSync(p, ""));
    let opens;
    try {
      await (await fs.promises.open(p, "r")).close();
      opens = "fd";
    } catch (e) {
      opens = `${e.code}/${e.errno}`;
    }
    const callback = await new Promise((resolve) =>
      fs.open(p, "r", (e, fd) => {
        if (e) resolve(`${e.code}/${e.errno}`);
        else fs.close(fd, () => resolve("fd"));
      }),
    );
    let message = "";
    try {
      fs.closeSync(fs.openSync(p, "r"));
    } catch (e) {
      message = ` | ${e.message} | path ${JSON.stringify(e.path)}`;
    }
    console.log(
      `${JSON.stringify(p)} ${row.join(" ")} | writeFileSync ${writes} | appendFileSync ${appends} | ` +
        `promises.open ${opens} | open ${callback}${message}`,
    );
  }
}
