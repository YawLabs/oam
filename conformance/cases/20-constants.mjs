// Legacy node:constants -- the flat union of fs flags, signal numbers, and
// crypto constants. Regression guard for the gap where the whole module was
// unimplemented (broke graceful-fs -> playwright-core). Only cross-platform-
// stable, node-faithful keys are checked here: O_CREAT/O_APPEND/O_EXCL differ
// by platform and the libuv-style errno values are an oam-wide convention.
import C from "node:constants";

console.log("typeof", typeof C);
console.log("open", C.O_RDONLY, C.O_WRONLY, C.O_RDWR, C.O_TRUNC);
console.log("mode", C.S_IFMT, C.S_IFREG, C.S_IFDIR, C.S_IFLNK);
console.log("amode", C.F_OK, C.R_OK, C.W_OK, C.X_OK);
console.log("copy", C.COPYFILE_EXCL);
console.log("signals", C.SIGTERM, C.SIGKILL, C.SIGINT, C.SIGHUP, C.SIGSEGV);
console.log("crypto", C.RSA_PKCS1_PADDING, C.RSA_PKCS1_OAEP_PADDING);
