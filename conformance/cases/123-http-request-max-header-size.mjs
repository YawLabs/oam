// http.request refuses a response head over node's maxHeaderSize -- 16 KiB by
// default, or the request's own `maxHeaderSize` -- counted as node's parser
// counts it for a response (the reason phrase, every header name and value)
// and refused at a count >= the limit, with node's error: `Parse Error:
// Header overflow`, code HPE_HEADER_OVERFLOW, reason `Header overflow`.
// `insecureHTTPParser` does not lift the limit. Both options are validated
// in the constructor with node's errors. Measured on node v22.22.2.
//
// The requests here go over an agent's socket (a custom agent), whose parser
// is oam's own; the transport behind a plain request applies the same limit
// in the Rust fetch op. A raw server answers `HTTP/1.1 200 OK` with one
// `X-A` header of the given size plus Content-Length and Connection (counted:
// 35 + size).
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

function rawServer(size) {
  return new Promise((resolve) => {
    const server = net.createServer((c) => {
      c.on("error", () => {});
      c.once("data", () => {
        c.end(`HTTP/1.1 200 OK\r\nX-A: ${"a".repeat(size)}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok`);
      });
    });
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

class OwnSocket extends http.Agent {
  createConnection(options, cb) {
    return net.createConnection(options, cb);
  }
}

function get(port, options = {}) {
  return new Promise((resolve) => {
    let req;
    try {
      req = http.get({ host: "127.0.0.1", port, agent: new OwnSocket(), ...options }, (res) => {
        let body = "";
        res.on("data", (d) => (body += d));
        res.on("end", () => resolve(`response ${res.statusCode} ${body}`));
      });
    } catch (e) {
      resolve(`throws ${e.constructor.name} ${e.code}: ${e.message}`);
      return;
    }
    req.on("error", (e) => resolve(`error ${e.code} ${JSON.stringify(e.message)} reason=${JSON.stringify(e.reason)}`));
  });
}

for (const size of [16348, 16349, 20000]) {
  const server = await rawServer(size);
  console.log(`default, size ${size}: ${await get(server.address().port)}`);
  server.close();
}
for (const [size, options] of [
  [964, { maxHeaderSize: 1000 }],
  [965, { maxHeaderSize: 1000 }],
  [965, { maxHeaderSize: 1000, insecureHTTPParser: true }],
  [16349, { maxHeaderSize: 0 }],
  [20000, { maxHeaderSize: 40000 }],
]) {
  const server = await rawServer(size);
  console.log(`${JSON.stringify(options)}, size ${size}: ${await get(server.address().port, options)}`);
  server.close();
}

// Validation, in the constructor.
const server = await rawServer(10);
const port = server.address().port;
for (const options of [
  { maxHeaderSize: "1000" },
  { maxHeaderSize: 1.5 },
  { maxHeaderSize: -1 },
  { insecureHTTPParser: "yes" },
]) {
  console.log(`${JSON.stringify(options)}: ${await get(port, options)}`);
}
{
  const req = http.get({ host: "127.0.0.1", port, agent: new OwnSocket(), maxHeaderSize: 1234, insecureHTTPParser: false });
  console.log(`properties: maxHeaderSize=${req.maxHeaderSize} insecureHTTPParser=${req.insecureHTTPParser}`);
  await new Promise((r) => req.on("response", (res) => { res.resume(); res.on("end", r); }));
}
server.close();
