// dgram: a socket binds once. Back-to-back send()s on an unbound socket share
// ONE implicit bind (one 'listening', both datagrams from the same port), and
// a second bind() throws ERR_SOCKET_ALREADY_BOUND whether the first is still
// in flight or done. A close() behind a queued send waits for the bind
// ('listening', then 'close') and the send is dropped without a callback; a
// close() during an explicit bind leaves no 'listening'; and a closed
// socket's send() and bind() throw ERR_SOCKET_DGRAM_NOT_RUNNING.
import dgram from "node:dgram";

const code = (fn) => {
  try {
    fn();
    return "no throw";
  } catch (e) {
    return e.code;
  }
};
const once = (emitter, event) => new Promise((done) => emitter.once(event, done));
const pause = (ms) => new Promise((done) => setTimeout(done, ms));

const receiver = dgram.createSocket("udp4");
const got = [];
let onMessage = () => {};
receiver.on("message", (msg, rinfo) => {
  got.push({ text: msg.toString(), port: rinfo.port });
  onMessage();
});
const received = (count) =>
  new Promise((done) => {
    onMessage = () => {
      if (got.length >= count) done();
    };
    onMessage();
  });
await new Promise((done) => receiver.bind(0, "127.0.0.1", done));
const port = receiver.address().port;

// 1. Two sends before the implicit bind settles.
const sender = dgram.createSocket("udp4");
let listening = 0;
sender.on("listening", () => listening++);
const sent = [];
sender.send("a", port, "127.0.0.1", (err) => sent.push(err ? err.code : "a sent"));
sender.send("b", port, "127.0.0.1", (err) => sent.push(err ? err.code : "b sent"));
// The implicit bind is in flight: an explicit one is a second bind.
console.log("bind while binding:", code(() => sender.bind(0)));
await received(2);
await pause(50);
console.log("listening events:", listening);
console.log("send callbacks:", sent.sort().join(", "));
console.log("datagrams:", got.map((m) => m.text).sort().join(", "));
console.log("one source port:", got[0].port === got[1].port, got[0].port === sender.address().port);
console.log("bind when bound:", code(() => sender.bind(0)));

// 2. close() behind a queued send: the bind completes, the socket closes,
//    and the send is dropped -- no datagram, no callback.
got.length = 0;
const brief = dgram.createSocket("udp4");
const briefEvents = [];
brief.on("listening", () => briefEvents.push("listening"));
brief.on("close", () => briefEvents.push("close"));
brief.on("error", (err) => briefEvents.push(`error ${err.code}`));
brief.send("c", port, "127.0.0.1", (err) => briefEvents.push(`callback ${err ? err.code : "ok"}`));
brief.close();
await once(brief, "close");
await pause(200);
console.log("send then close:", briefEvents.join(", "), "| datagrams:", got.length);

// 3. close() while an explicit bind is in flight: 'close', no 'listening'.
const never = dgram.createSocket("udp4");
const neverEvents = [];
never.on("listening", () => neverEvents.push("listening"));
never.on("close", () => neverEvents.push("close"));
never.bind(0, "127.0.0.1");
never.close();
await once(never, "close");
await pause(100);
console.log("bind then close:", neverEvents.join(", "));

// 4. A closed socket is not running.
console.log("send after close:", code(() => never.send("x", port, "127.0.0.1")));
console.log("bind after close:", code(() => never.bind(0)));

sender.close();
receiver.close();
