// `fetch` and `http.request` refuse a response whose head is over node's
// 16 KiB maxHeaderSize, each counting it the way node does: fetch (undici)
// counts header names and values, http.request (node's parser) the status
// line's reason phrase as well, and both refuse at 16384. oam's transport
// used to accept response heads of hundreds of KiB.
//
// A raw server answers `HTTP/1.1 <status> <reason>` with one `X-A` header of
// the given size plus Content-Length and Connection (names + values = 33 +
// size; the reason `OK` adds 2 for http.request). fetch's rejection is
// printed in full; for http.request the code is read from the error or its
// cause, since which object carries it is the client's error mapping.
import http from "node:http";
import net from "node:net";

function rawServer(status, reason, size, extra = "") {
  return new Promise((resolve) => {
    const server = net.createServer((c) => {
      c.on("error", () => {});
      c.once("data", () => {
        c.end(
          `HTTP/1.1 ${status} ${reason}\r\nX-A: ${"a".repeat(size)}\r\n${extra}Content-Length: 2\r\nConnection: close\r\n\r\nok`,
        );
      });
    });
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

function viaHttp(port) {
  return new Promise((resolve) => {
    const req = http.get({ host: "127.0.0.1", port, path: "/", agent: false }, (res) => {
      let body = "";
      res.on("data", (d) => (body += d));
      res.on("end", () => resolve(`response ${res.statusCode} ${body}`));
    });
    req.on("error", (e) => resolve(`error ${e.code || (e.cause && e.cause.code)}`));
  });
}

async function viaFetch(port, path = "/") {
  try {
    const r = await fetch(`http://127.0.0.1:${port}${path}`);
    return `response ${r.status} ${await r.text()}`;
  } catch (e) {
    return `rejects ${e.name}: ${e.message}; cause ${e.cause && e.cause.code} ${JSON.stringify(e.cause && e.cause.message)}`;
  }
}

for (const size of [16340, 16348, 16349, 16350, 16351, 20000]) {
  const server = await rawServer(200, "OK", size);
  const port = server.address().port;
  console.log(`size ${size} | http.request | ${await viaHttp(port)}`);
  console.log(`size ${size} | fetch | ${await viaFetch(port)}`);
  server.close();
}

// A redirect's head is held to the same limit before it is followed.
{
  const target = await rawServer(200, "OK", 10);
  const hop = await rawServer(
    302,
    "Found",
    20000,
    `Location: http://127.0.0.1:${target.address().port}/\r\n`,
  );
  console.log(`oversized 302 | fetch | ${await viaFetch(hop.address().port)}`);
  hop.close();
  target.close();
}
