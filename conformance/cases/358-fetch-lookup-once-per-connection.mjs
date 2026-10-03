// A replaced dns.lookup is asked once per connection fetch() opens, and the
// connections it opened are reused by later fetches (#179). node's fetch
// dials through net.connect, which calls dns.lookup as it is at that moment,
// once for each new socket of the global dispatcher's pool -- so twenty
// sequential fetches to one origin ask it about as often as they connect,
// which is far less than twenty. oam ran every hooked fetch on a client of
// its own, which opened a connection and asked the hook for every fetch.
//
// How MANY connections a run opens is undici's timing, not a contract: node
// has not yet released a response's socket when the next request is
// dispatched (sequential fetches alternate between two), where oam reuses the
// one. What both define is the relation printed here: one lookup per
// connection, fewer connections than requests, the name a connection was
// opened for never lent to another name, and an IP literal never looked up.
// Measured on node v22.22.2.
import http from "node:http";
import dns from "node:dns";

const server = http.createServer((req, res) => {
  if (req.url === "/hop") {
    res.writeHead(302, { location: "/end" });
    res.end();
    return;
  }
  res.end("ok");
});
let connections = 0;
server.on("connection", () => connections++);
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();

const original = dns.lookup;
const asked = [];
dns.lookup = (host, opts, cb) => {
  asked.push(host);
  cb(null, [{ address: "127.0.0.1", family: 4 }]);
};

function report(label, fetches, before) {
  const opened = connections - before.connections;
  const lookups = asked.length - before.asked;
  console.log(label, JSON.stringify({
    oneLookupPerConnection: lookups === opened,
    reused: opened < fetches,
    hosts: [...new Set(asked.slice(before.asked))],
  }));
}
const mark = () => ({ connections, asked: asked.length });

{
  const before = mark();
  for (let i = 0; i < 20; i++) await (await fetch(`http://pooled.test:${port}/`)).text();
  report("sequential", 20, before);
}
{
  const before = mark();
  for (let i = 0; i < 5; i++) await (await fetch(`http://pooled.test:${port}/hop`)).text();
  report("redirects", 10, before);
}
{
  // Another name on the same address and port: asked about, and given a
  // connection of its own.
  const before = mark();
  await (await fetch(`http://other.test:${port}/`)).text();
  const opened = connections - before.connections;
  console.log("other name", JSON.stringify({ asked: asked.slice(before.asked), opened }));
}
{
  // An IP literal is never looked up.
  const before = asked.length;
  await (await fetch(`http://127.0.0.1:${port}/`)).text();
  console.log("ip literal", JSON.stringify({ asked: asked.slice(before) }));
}
dns.lookup = original;
server.close();
server.closeAllConnections();
