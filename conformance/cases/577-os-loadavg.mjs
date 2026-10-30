// os.loadavg(): three finite, non-negative numbers in a fresh array. libuv
// cannot compute a load average on Windows and reports zeros there; oam
// hard-coded the zeros on every platform (#441).
import os from "node:os";

const avg = os.loadavg();
console.log("length", Array.isArray(avg) && avg.length === 3);
console.log("numbers", avg.every((n) => typeof n === "number" && Number.isFinite(n) && n >= 0));
console.log("win32-zeros", process.platform !== "win32" || avg.every((n) => n === 0));
console.log("fresh-array", os.loadavg() !== avg);
