// The request's 'close' against the response's, as node orders them
// (measured on node v22.22.2). A connection the response does not keep open
// closes the request when its socket has closed, which is after the
// response's own 'end' and 'close'; a kept-alive one hands the socket back a
// tick after the response's 'end', so there the request closes first. A
// response destroyed by its reader closes first as well, and a request
// destroyed mid-response closes ahead of the response it aborted. oam
// emitted the request's 'close' first on every connection that was not kept
// (#194), so code that tears down per-request state on the request's 'close'
// and reads the response in the response's ran its halves in the opposite
// order.
//
// Only the order of events is printed.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const server = http.createServer((req, res) => {
  if (req.url === "/close") res.setHeader("connection", "close");
  if (req.url === "/slow") {
    // Never ends.
    res.write("part");
    return;
  }
  if (req.url === "/two-parts") {
    // The body's second part comes a moment after the first.
    res.write("line1\n");
    setTimeout(() => res.end("rest"), 5);
    return;
  }
  if (req.url === "/big") {
    res.end(Buffer.alloc(256 * 1024, 97));
    return;
  }
  res.end("ok");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const P = server.address().port;

// Sends the request over a socket the agent creates itself.
class WrappingAgent extends http.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}

async function run(label, options, drive) {
  const events = [];
  const req = http.request({ host: "127.0.0.1", port: P, path: "/", ...options });
  req.on("error", (e) => events.push(`req error ${e.code}`));
  await new Promise((resolve) => {
    req.on("close", () => {
      events.push("req close");
      setTimeout(resolve, 60);
    });
    req.on("response", (res) => {
      events.push("response");
      res.on("end", () => events.push("res end"));
      res.on("aborted", () => events.push("res aborted"));
      res.on("error", (e) => events.push(`res error ${e.code}`));
      res.on("close", () => events.push("res close"));
      if (drive) drive(req, res, events);
      else res.resume();
    });
    req.end();
  });
  await sleep(20);
  console.log(`${label}: ${events.join(", ")}`);
  if (options.agent && options.agent.destroy) options.agent.destroy();
}

const paths = [
  ["default agent", () => ({})],
  ["agent: false", () => ({ agent: false })],
  ["new Agent()", () => ({ agent: new http.Agent() })],
  ["keepAlive agent", () => ({ agent: new http.Agent({ keepAlive: true }) })],
  ["agent socket", () => ({ agent: new WrappingAgent() })],
  ["agent socket, keepAlive", () => ({ agent: new WrappingAgent({ keepAlive: true }) })],
];
for (const [name, options] of paths) {
  await run(`${name}, connection: close`, { ...options(), path: "/close" });
  await run(`${name}, left open`, options());
  await run(`${name}, HEAD`, { ...options(), path: "/close", method: "HEAD" });
  await run(`${name}, a large body`, { ...options(), path: "/big", headers: { connection: "close" } });
  await run(`${name}, response destroyed`, { ...options(), path: "/slow" }, (req, res, events) => {
    res.once("data", () => {
      events.push("res.destroy()");
      res.destroy();
    });
  });
  await run(`${name}, request destroyed`, { ...options(), path: "/slow" }, (req, res, events) => {
    res.once("data", () => {
      events.push("req.destroy()");
      req.destroy();
    });
  });
  await run(`${name}, request destroyed at the response's end`, { ...options(), path: "/close" }, (req, res, events) => {
    res.resume();
    res.on("end", () => {
      events.push("req.destroy()");
      req.destroy();
    });
  });
  await run(`${name}, read by an async iterator`, { ...options(), path: "/close" }, async (req, res, events) => {
    for await (const chunk of res) void chunk;
    events.push("iterated");
  });
  await run(`${name}, left open, read by an async iterator`, options(), async (req, res, events) => {
    for await (const chunk of res) void chunk;
    events.push("iterated");
  });
  // The whole body has arrived by the time the reader leaves, but its 'end'
  // has not been delivered: the request still closes. A kept-alive one on
  // oam's own transport, and any over an agent's socket, never did. (Over an
  // agent socket that is not kept, node closes the request when the server
  // closes the socket, before the reader has left: docs/node-divergences.md
  // entry 38, so that path is not compared here.)
  if (name !== "agent socket") {
    await run(`${name}, left open, an async iterator that breaks`, { ...options(), path: "/two-parts" }, async (req, res, events) => {
      for await (const chunk of res) {
        void chunk;
        await sleep(50);
        break;
      }
      events.push("broke");
    });
    await run(`${name}, left open, response destroyed after its body arrived`, { ...options(), path: "/two-parts" }, (req, res, events) => {
      res.once("data", () => {
        res.pause();
        setTimeout(() => {
          events.push("res.destroy()");
          res.destroy();
        }, 200);
      });
    });
  }
  // A request already done with ignores a destroy(err): no 'error' (which
  // with no listener would throw), as node's destroy() returns early.
  await run(`${name}, destroy(err) after the response's end`, options(), (req, res, events) => {
    res.resume();
    res.on("end", () => {
      setTimeout(() => {
        events.push(`destroyed=${req.destroyed}, destroy(err)`);
        req.removeAllListeners("error");
        req.destroy(new Error("late"));
      }, 20);
    });
  });
}
// Nobody listens for the response: it is dumped, and the request closes.
{
  const events = [];
  const req = http.request({ host: "127.0.0.1", port: P, path: "/close" });
  await new Promise((resolve) => {
    req.on("close", () => {
      events.push("req close");
      resolve();
    });
    req.end();
  });
  console.log(`no 'response' listener: ${events.join(", ")}`);
}

server.close();
server.closeAllConnections();
