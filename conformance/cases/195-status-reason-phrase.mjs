// The reason phrase of a response is the one the server sent (measured on
// node v22.22.2): fetch's `statusText` and http.IncomingMessage's
// `statusMessage` report `Custom Reason` for `HTTP/1.1 200 Custom Reason`,
// the phrase of a status code that has no standard one, and the empty string
// for a status line with no phrase at all. oam's own transport reported the
// status code's canonical phrase instead -- `OK`, `Not Found`, and nothing
// for a non-standard code -- for fetch and for http.get alike (#160); only a
// request sent over an agent's socket read the wire.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// A raw server: the request path picks the status line it answers with.
const LINES = {
  "/custom": "HTTP/1.1 200 Custom Reason",
  "/standard": "HTTP/1.1 200 OK",
  "/nonstandard": "HTTP/1.1 299 Whatever",
  "/not-here": "HTTP/1.1 404 Nope Not Here",
  "/lowercase": "HTTP/1.1 404 not found",
  "/empty": "HTTP/1.1 200 ",
  "/none": "HTTP/1.1 200",
  "/punctuation": "HTTP/1.1 500 It's broken: sorry (again)",
  "/server-error": "HTTP/1.1 503 Service Unavailable",
};
const server = net.createServer((socket) => {
  let buffered = "";
  socket.on("error", () => {});
  socket.on("data", (chunk) => {
    buffered += chunk.toString("latin1");
    let end;
    while ((end = buffered.indexOf("\r\n\r\n")) !== -1) {
      const path = buffered.slice(0, end).split(" ")[1];
      buffered = buffered.slice(end + 4);
      socket.write(`${LINES[path]}\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok`);
    }
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// Sends the request over a socket the agent creates itself.
class WrappingAgent extends http.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}

const get = (path, options) => new Promise((resolve, reject) => {
  http.get({ host: "127.0.0.1", port, path, ...options }, (res) => {
    res.resume();
    res.on("end", () => resolve(`${res.statusCode} ${JSON.stringify(res.statusMessage)}`));
  }).on("error", reject);
});

for (const path of Object.keys(LINES)) {
  const res = await fetch(`http://127.0.0.1:${port}${path}`);
  await res.text();
  console.log(`${path} fetch: ${res.status} ${JSON.stringify(res.statusText)} ok=${res.ok}`);
  console.log(`${path} http.get: ${await get(path, {})}`);
  console.log(`${path} http.get, agent: false: ${await get(path, { agent: false })}`);
  console.log(`${path} http.get, agent socket: ${await get(path, { agent: new WrappingAgent() })}`);
}
server.close();
