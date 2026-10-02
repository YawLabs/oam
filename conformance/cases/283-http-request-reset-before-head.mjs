// req.socket.resetAndDestroy() on an http.request whose server has not
// answered yet. node's socket is connected by then: it is reset and
// destroyed at once, the server's connection closes, and the request fails
// with ECONNRESET "socket hang up" and then closes. On oam's own transport
// the req.socket stand-in has no connection until the response head, so
// resetAndDestroy() only queued itself behind a 'connect' that never came:
// nothing happened, and a late answer was then torn down mid-body. Measured
// on node v22.22.2. (Case 254 resets mid-response and after it.)
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

for (const [label, agentOf] of [
  ["keepAlive agent", () => new http.Agent({ keepAlive: true })],
  ["default agent", () => undefined],
  ["agent: false", () => false],
]) {
  let serverClosed = false;
  const server = http.createServer((req) => {
    // Never answers.
    req.socket.on("close", () => {
      serverClosed = true;
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const events = [];
  const agent = agentOf();
  const req = http.get({ host: "127.0.0.1", port: server.address().port, agent });
  req.on("error", (e) => events.push(`req error ${e.code} ${e.message}`));
  req.on("close", () => events.push("req close"));
  req.on("response", () => events.push("response?!"));
  const socket = await new Promise((r) => req.on("socket", r));
  // Long enough for the request to be on the server.
  await sleep(200);
  socket.resetAndDestroy();
  events.push(`after resetAndDestroy: destroyed ${socket.destroyed}`);
  await sleep(300);
  console.log(`${label}: ${events.join(", ")} | req.destroyed ${req.destroyed} | server connection closed ${serverClosed}`);
  if (agent) agent.destroy();
  server.closeAllConnections();
  await new Promise((r) => server.close(r));
}
