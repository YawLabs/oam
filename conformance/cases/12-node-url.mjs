// node:url conversions (platform-aware: outputs agree per-host).
import { fileURLToPath, pathToFileURL, urlToHttpOptions } from "node:url";
import path from "node:path";

// Round trip through import.meta.
console.log(fileURLToPath(import.meta.url) === import.meta.filename);
// Relative resolution keeps the cwd anchor (compare equality, not paths).
console.log(pathToFileURL("foo/bar.txt").href === pathToFileURL(path.resolve("foo/bar.txt")).href);
// Guards.
let smuggle = "";
try {
  fileURLToPath(`file:///${process.platform === "win32" ? "C:/" : ""}foo%2Fbar`);
} catch (e) {
  smuggle = e.code;
}
console.log(smuggle);
let scheme = "";
try {
  fileURLToPath("https://x.dev/a");
} catch (e) {
  scheme = e.code;
}
console.log(scheme);
// urlToHttpOptions node shape.
const o = urlToHttpOptions(new URL("http://us%65r:p%40ss@[::1]:8080/x/y?q=1#h"));
console.log(o.hostname, typeof o.port, o.port, o.auth, o.pathname, o.path);
const o2 = urlToHttpOptions(new URL("https://example.com/"));
console.log("port" in o2, "auth" in o2, o2.pathname);
