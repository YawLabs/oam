// An http server enforces node's maxHeaderSize: 16 KiB unless the server's
// `maxHeaderSize` option (or --max-http-header-size) says otherwise, counted
// as node counts it -- the request target plus every header name and value --
// and answered `431` without running a handler. oam's server used to accept
// heads of hundreds of KiB, ignore the option, and report a constant for
// `http.maxHeaderSize`.
//
// Also node's validation of the two parser options on http and https
// servers, and what the servers store.
import http from "node:http";
import https from "node:https";
import net from "node:net";

const show = (label, fn) => {
  try {
    console.log(label, "->", JSON.stringify(fn()));
  } catch (e) {
    console.log(label, "-> throws", e.name, e.code, JSON.stringify(e.message));
  }
};

show("http.maxHeaderSize", () => http.maxHeaderSize);
show("maxHeaderSize is a getter", () => typeof Object.getOwnPropertyDescriptor(http, "maxHeaderSize").get);
for (const v of [0, 1000, 2 ** 32, -1, 1.5, NaN, Infinity, "100", null, true]) {
  show(`createServer({maxHeaderSize: ${String(v)}}).maxHeaderSize`, () => http.createServer({ maxHeaderSize: v }).maxHeaderSize);
}
for (const v of [true, false, "yes", 1, null]) {
  show(`createServer({insecureHTTPParser: ${String(v)}}).insecureHTTPParser`, () => http.createServer({ insecureHTTPParser: v }).insecureHTTPParser);
}
show("createServer().maxHeaderSize", () => http.createServer().maxHeaderSize);
show("createServer().insecureHTTPParser", () => http.createServer().insecureHTTPParser);
show("new http.Server({maxHeaderSize: 7}).maxHeaderSize", () => new http.Server({ maxHeaderSize: 7 }).maxHeaderSize);
show("createServer('x')", () => http.createServer("x"));
show("createServer(null) is a server", () => http.createServer(null) instanceof http.Server);
show("https.createServer({maxHeaderSize: 1234}).maxHeaderSize", () => https.createServer({ maxHeaderSize: 1234 }).maxHeaderSize);
show("https.createServer({maxHeaderSize: -1})", () => https.createServer({ maxHeaderSize: -1 }));

function serve(options) {
  const seen = [];
  const server = http.createServer(options, (req, res) => {
    seen.push(req.url.slice(0, 12));
    res.end("ok");
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, seen })));
}

function status(port, bytes) {
  return new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1");
    let raw = "";
    socket.on("data", (d) => (raw += d.toString("latin1")));
    socket.on("close", () => resolve((/HTTP\/1\.1 \d{3}[^\r\n]*/.exec(raw) || ["(nothing)"])[0]));
    socket.on("error", () => {});
    socket.write(bytes);
  });
}

// Counted bytes: target "/" (1) + Host (4) + x (1) + Connection (10) +
// close (5) + X-A (3) = 24, plus the value.
const oneHeader = (n) => `GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A: ${"a".repeat(n)}\r\n\r\n`;
// Target "/" + n (1 + n) + Host, x, Connection, close (20) = 21 + n.
const longUrl = (n) => `GET /${"u".repeat(n)} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n`;
// Trailing whitespace in a value counts; leading whitespace does not.
const trailing = (n) => `GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A: v${" ".repeat(n)}\r\n\r\n`;
const leading = (n) => `GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A:${" ".repeat(n)}v\r\n\r\n`;

async function boundary(label, options, probes) {
  const { server, seen } = await serve(options);
  for (const [name, bytes] of probes) {
    const before = seen.length;
    const line = await status(server.address().port, bytes);
    console.log(`${label} | ${name} | ${line} | handler ${seen.length > before ? "ran" : "not run"}`);
  }
  await new Promise((r) => server.close(r));
}

await boundary("default", {}, [
  ["header, counted 16383", oneHeader(16359)],
  ["header, counted 16384", oneHeader(16360)],
  ["header, counted 20024", oneHeader(20000)],
  ["url, counted 16383", longUrl(16362)],
  ["url, counted 16384", longUrl(16363)],
  ["trailing ws, counted 16383", trailing(16358)],
  ["trailing ws, counted 16384", trailing(16359)],
  ["leading ws 20000, counted 26", leading(20000)],
]);
await boundary("maxHeaderSize 1000", { maxHeaderSize: 1000 }, [
  ["header, counted 999", oneHeader(975)],
  ["header, counted 1000", oneHeader(976)],
]);
await boundary("maxHeaderSize 0 (the default)", { maxHeaderSize: 0 }, [
  ["header, counted 16383", oneHeader(16359)],
  ["header, counted 16384", oneHeader(16360)],
]);
await boundary("maxHeaderSize 32768", { maxHeaderSize: 32768 }, [
  ["header, counted 20024", oneHeader(20000)],
  ["header, counted 32768", oneHeader(32744)],
]);
await boundary("insecureHTTPParser", { insecureHTTPParser: true }, [
  ["header, counted 16384", oneHeader(16360)],
]);

// A second request on a kept-alive connection is measured on its own.
{
  const { server, seen } = await serve({});
  const socket = net.connect(server.address().port, "127.0.0.1");
  let raw = "";
  await new Promise((resolve) => {
    socket.on("data", (d) => {
      raw += d.toString("latin1");
      if (/HTTP\/1\.1 200/.test(raw) && !socket.sentSecond) {
        socket.sentSecond = true;
        socket.write(oneHeader(16360).replace("GET / ", "GET /second "));
      }
    });
    socket.on("close", resolve);
    socket.on("error", () => {});
    socket.write("GET /first HTTP/1.1\r\nHost: x\r\n\r\n");
  });
  const statuses = [...raw.matchAll(/HTTP\/1\.1 \d{3}[^\r\n]*/g)].map((m) => m[0]);
  console.log("keep-alive | statuses", JSON.stringify(statuses), "| handler saw", JSON.stringify(seen));
  await new Promise((r) => server.close(r));
}
