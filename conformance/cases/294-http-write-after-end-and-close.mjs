// write() and end() on an http message that has ended or whose connection
// has closed, as node's OutgoingMessage#write_ answers them (measured on
// node v22.22.2):
// - the chunk is checked first: null is ERR_STREAM_NULL_VALUES, a number
//   ERR_INVALID_ARG_TYPE, whatever the message's state;
// - a write after end() returns false, and on the next tick its callback
//   and the message's 'error' get ERR_STREAM_WRITE_AFTER_END -- on a server
//   response and on a client request alike;
// - on a response whose connection closed, a write's callback gets
//   ERR_STREAM_DESTROYED, end('c', cb) builds no head (headersSent stays
//   false) and its callback, which waits for 'finish', is never called;
// - flushHeaders() on a destroyed request still renders the head.
// oam returned false silently or true with a null callback after end(),
// returned false where node throws for a bad chunk on a closed response,
// called end()'s callback with no error, and left a destroyed request's
// headersSent false.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

const serverCases = {
  "write after end": (res, log) => {
    res.end("a");
    log.push("write returned " + res.write("b", (e) => log.push("write callback " + (e && e.code))));
  },
  "end with a chunk after end": (res, log) => {
    res.end("a");
    res.end("b", (e) => log.push("end callback " + (e && e.code)));
  },
  "closed, write(number)": async (res, log) => {
    await wait(150);
    try { log.push("write returned " + res.write(123)); } catch (e) { log.push("throws " + e.code); }
  },
  "closed, write(null)": async (res, log) => {
    await wait(150);
    try { log.push("write returned " + res.write(null)); } catch (e) { log.push("throws " + e.code); }
  },
  "closed, write then end": async (res, log) => {
    res.write("a");
    await wait(150);
    log.push("write returned " + res.write("b", (e) => log.push("write callback " + (e && e.code))));
    res.end("c", (e) => log.push("end callback " + (e && e.code)));
  },
  "closed, end(chunk) before any head": async (res, log) => {
    await wait(150);
    res.end("c", (e) => log.push("end callback " + (e && e.code)));
    log.push(`headersSent ${res.headersSent} writableEnded ${res.writableEnded}`);
  },
};

for (const [label, handle] of Object.entries(serverCases)) {
  const log = [];
  let done;
  const finished = new Promise((r) => { done = r; });
  const server = http.createServer(async (req, res) => {
    req.on("error", () => {});
    res.on("error", (e) => log.push("response error " + e.code));
    res.on("finish", () => log.push("finish"));
    try {
      await handle(res, log);
    } catch (e) {
      log.push("throws " + e.code);
    }
    setTimeout(done, 200);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const client = net.connect(server.address().port, "127.0.0.1");
  client.on("error", () => {});
  client.on("data", () => {});
  client.write("GET / HTTP/1.1\r\nHost: x\r\n\r\n");
  if (label.startsWith("closed")) setTimeout(() => client.destroy(), 50);
  await finished;
  console.log(`server, ${label}: ${log.join(", ")}`);
  client.destroy();
  server.close();
}

// A client request written after end(), on its own transport, over an
// agent's socket and through createConnection.
{
  const server = http.createServer((req, res) => {
    req.resume();
    req.on("end", () => res.end("ok"));
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  for (const [path, extra] of [
    ["own transport", {}],
    ["agent", { agent: new http.Agent() }],
    ["createConnection", { agent: false, createConnection: () => net.connect(port, "127.0.0.1") }],
  ]) {
    const log = [];
    await new Promise((resolve) => {
      const req = http.request({ host: "127.0.0.1", port, method: "POST", ...extra }, (res) => {
        res.resume();
        res.on("end", () => setTimeout(resolve, 50));
      });
      req.on("error", (e) => log.push("error " + e.code));
      req.end("a");
      log.push("write returned " + req.write("x", (e) => log.push("write callback " + (e && e.code))));
    });
    console.log(`client (${path}), write after end: ${log.join(", ")}`);
  }
  server.close();
}

// flushHeaders() on a destroyed request.
for (const [path, extra] of [
  ["own transport", {}],
  ["agent", { agent: new http.Agent() }],
  ["createConnection", { createConnection: () => new net.Socket() }],
]) {
  const req = http.request({ host: "127.0.0.1", port: 1, ...extra });
  req.on("error", () => {});
  req.destroy();
  req.flushHeaders();
  console.log(`client (${path}), flushHeaders after destroy: headersSent ${req.headersSent} header ${typeof req._header}`);
}
