// DEP0123: node v22.23.5's tls.connect (lib/internal/tls/wrap.js) warns
// "Setting the TLS ServerName to an IP address is not permitted by RFC 6066"
// when the `servername` option is truthy and an IP address -- v4 or v6 --
// after it has started the connect, through process.emitWarning (not
// util.deprecate): once per process, its flag set even while
// --no-deprecation swallows the warning. Over a given socket it warns too.
// An agent's own name never draws it (https strips an IP host from the
// server name it computes); an explicit IP `servername` passed to
// https.request or http2.connect reaches tls.connect and does. The socket's
// own connect() -- net's, in node -- never warns. Then setServername's
// validateString(name, 'name') throws ERR_INVALID_ARG_TYPE out of
// tls.connect for a truthy servername that is not a string, the connect
// already started. A throw before the connect (no port) warns nothing.
//
// The printing goes to stderr, which the harness does not compare, so each
// scenario runs as a child and both its stdout and its warning lines are
// printed, with the pid and the program name in the one-time hint taken out.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-648-")));

// A server that hangs up on every connection: each TLS socket fails with an
// error this script swallows (the warning, not the handshake, is the point).
const prelude = `import tls from "node:tls";
import net from "node:net";
import https from "node:https";
import http2 from "node:http2";
process.on("warning", (w) => console.log("heard", w.name, w.code, JSON.stringify(w.message)));
const quiet = (s) => { s.on("error", () => {}); return s; };
const srv = net.createServer((s) => s.destroy());
await new Promise((r) => srv.listen(0, "127.0.0.1", r));
const port = srv.address().port;
const tick = () => new Promise((r) => setTimeout(r, 50));
`;
const epilogue = `await tick();\nsrv.close();\nconsole.log("done");\n`;

const scenarios = {
  "ipv4-once": `console.log("first");
quiet(tls.connect({ port, host: "127.0.0.1", servername: "127.0.0.1" }));
console.log("after the first, synchronously");
await tick();
console.log("second, v6");
quiet(tls.connect({ port, host: "127.0.0.1", servername: "::1" }));
await tick();
console.log("third, positional");
quiet(tls.connect(port, "127.0.0.1", { servername: "10.1.2.3" }));
`,
  "ipv6": `quiet(tls.connect({ port, host: "127.0.0.1", servername: "::ffff:127.0.0.1" }));\n`,
  "not-an-ip": `for (const servername of ["localhost", "127.1", "1.2.3.4.5", "", "[::1]", "0x7f.0.0.1"]) {
  quiet(tls.connect({ port, host: "127.0.0.1", servername }));
  console.log("connected with", JSON.stringify(servername));
}
`,
  "over-a-socket": `const s = quiet(net.connect(port, "127.0.0.1"));
quiet(tls.connect({ socket: s, servername: "192.168.0.1" }));
console.log("returned");
`,
  "no-port-throws-first": `try { tls.connect({ host: "127.0.0.1", servername: "127.0.0.1" }); } catch (e) { console.log("threw", e.code); }\n`,
  "non-string": `// The socket a throwing tls.connect leaves behind still fails, unheard.
process.on("uncaughtException", (e) => console.log("left behind:", e.code));
for (const servername of [5, true, { a: 1 }, ["127.0.0.1"], 0, null, false]) {
  try {
    quiet(tls.connect({ port, host: "127.0.0.1", servername }));
    console.log("connected with", JSON.stringify(servername));
  } catch (e) { console.log("threw", e.code, e.message); }
}
`,
  "non-string-over-a-socket": `const s = quiet(net.connect(port, "127.0.0.1"));
try { tls.connect({ socket: s, servername: 7 }); } catch (e) { console.log("threw", e.code); }
s.destroy();
`,
  "socket-connect": `const t = quiet(new tls.TLSSocket(null, { servername: "127.0.0.1" }));
t.connect(port, "127.0.0.1");
console.log("TLSSocket#connect returned");
`,
  "https": `quiet(https.get({ host: "127.0.0.1", port, path: "/" }));
await tick();
console.log("then an explicit servername");
quiet(https.get({ host: "127.0.0.1", port, path: "/", servername: "127.0.0.1" }));
`,
  "http2": `const a = http2.connect("https://127.0.0.1:" + port);
a.on("error", () => {});
await tick();
console.log("then an explicit servername");
const b = http2.connect("https://127.0.0.1:" + port, { servername: "127.0.0.1" });
b.on("error", () => {});
`,
};

for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(dir, name + ".mjs"), prelude + body + epilogue);
}

const run = (name, flags = []) => {
  const child = spawnSync(process.execPath, [...flags, path.join(dir, name + ".mjs")], {
    encoding: "utf8",
    cwd: dir,
  });
  console.log(`--- ${name}${flags.length ? " " + flags.join(" ") : ""}: status ${child.status}`);
  for (const line of child.stdout.split(/\r?\n/)) if (line) console.log("  out:", line);
  // The warning lines, and the head of a --throw-deprecation crash (its
  // stack and banner are each runtime's own).
  for (const line of child.stderr.split(/\r?\n/)) {
    if (!/^\((node|oam):|^\(Use |^DeprecationWarning|^  code:/.test(line)) continue;
    const shown = line.replace(/\((node|oam):\d+\)/g, "(node:PID)").replace(/^\(Use `(node|oam) /, "(Use `node ");
    console.log("  err:", shown);
  }
};

for (const name of Object.keys(scenarios)) run(name);
run("ipv4-once", ["--no-deprecation"]);
run("ipv4-once", ["--throw-deprecation"]);
run("over-a-socket", ["--no-deprecation"]);

fs.rmSync(dir, { recursive: true, force: true });
