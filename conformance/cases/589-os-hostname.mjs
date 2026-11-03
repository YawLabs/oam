// os.hostname() is the system's host name, as libuv's uv_os_gethostname
// answers node (gethostname(2) on unix, the DNS host name on Windows).
// oam read COMPUTERNAME / HOSTNAME instead: the upper-case NetBIOS name on
// Windows, and "localhost" on macOS, where neither is set. Both runtimes run
// on the same host, so the name itself is compared.
import os from "node:os";

const name = os.hostname();
console.log("hostname", JSON.stringify(name));
console.log("type", typeof name, "non-empty", name.length > 0);
