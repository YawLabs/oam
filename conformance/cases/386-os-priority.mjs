// os.getPriority([pid]) / os.setPriority([pid,] priority), as node's lib/os.js
// and libuv's uv_os_{get,set}priority implement them:
//
//   * pid defaults to 0 (this process); a non-int32 pid is
//     ERR_INVALID_ARG_TYPE / ERR_OUT_OF_RANGE, and so is a priority outside
//     -20..19; setPriority(p) is setPriority(0, p);
//   * a pid with no process is ERR_SYSTEM_ERROR, a SystemError whose info is
//     libuv's { errno, code, message, syscall } naming uv_os_getpriority /
//     uv_os_setpriority;
//   * the value reads back on node's scale -- on Windows through libuv's
//     priority-class mapping, so 12 reads back as 10 (BELOW_NORMAL) there;
//   * a child process inherits the lowered priority.
//
// oam had only os.constants.priority: both functions were missing, so a
// program importing either failed to link.
//
// Only lowers priority (raising it needs privilege on unix), and prints no
// pids or absolute priorities except the ones this script set.
import { spawnSync } from "node:child_process";
import os from "node:os";

const show = (label, f) => {
  try {
    console.log(label, "->", JSON.stringify(f()));
  } catch (e) {
    console.log(label, "threw", e.name, e.code, e.message);
    if (e.code === "ERR_SYSTEM_ERROR") {
      console.log("  info", JSON.stringify(e.info));
      console.log("  errno", e.errno === e.info.errno, typeof e.errno, "syscall", e.syscall);
      console.log("  keys", Object.keys(e).join(","));
      console.log("  string", String(e));
    }
  }
};

console.log(typeof os.getPriority, os.getPriority.name, os.getPriority.length);
console.log(typeof os.setPriority, os.setPriority.name, os.setPriority.length);

const start = os.getPriority();
console.log("start is an int on the scale", Number.isInteger(start) && start >= -20 && start <= 19);
console.log("pid 0 and own pid agree", os.getPriority(0) === start, os.getPriority(process.pid) === start);

show("get('1')", () => os.getPriority("1"));
show("get(1.5)", () => os.getPriority(1.5));
show("get(null)", () => os.getPriority(null));
show("get(2**31)", () => os.getPriority(2 ** 31));
show("set()", () => os.setPriority());
show("set('x')", () => os.setPriority("x"));
show("set(0, 'x')", () => os.setPriority(0, "x"));
show("set(20)", () => os.setPriority(20));
show("set(-21)", () => os.setPriority(-21));
show("set(0, 19.5)", () => os.setPriority(0, 19.5));
show("set(undefined, 19)", () => os.setPriority(undefined, 19));

show("get(missing pid)", () => os.getPriority(2147483647));
show("set(missing pid, 19)", () => os.setPriority(2147483647, 19));

show("set(12)", () => os.setPriority(12));
const twelve = os.getPriority();
console.log("12 reads back as", os.platform() === "win32" ? twelve === 10 : twelve === 12);
show("set(own pid, 19)", () => os.setPriority(process.pid, 19));
show("get() after 19", () => os.getPriority());

const child = spawnSync(
  process.execPath,
  ["-e", "console.log(require('node:os').getPriority())"],
  { encoding: "utf8" },
);
console.log("child inherits", child.status, child.stdout.trim());
