// fetch builds its request with its own Request class, not with whatever
// `globalThis.Request` is when it is called. Measured on node v22.22.2
// first: with the global replaced by another class, and then deleted, a
// fetch still works. oam's fetch built its request with the global, so it
// threw `Cannot read properties of undefined (reading 'method')` and then
// `globalThis.Request is not a constructor`.
import http from "node:http";

const srv = http.createServer((req, res) => {
  res.end(JSON.stringify([req.method, req.headers.authorization ?? null, req.headers.x ?? null]));
});
await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
const U = `http://127.0.0.1:${srv.address().port}/`;

const show = async (label, run) => {
  let out;
  try {
    out = await run();
  } catch (e) {
    out = `${e.name}: ${e.message}`;
  }
  console.log(label.padEnd(40), out);
};

const Saved = globalThis.Request;
globalThis.Request = class NotARequest {};
await show("global Request replaced", async () => (await fetch(U)).text());
delete globalThis.Request;
await show("global Request deleted", async () => (await fetch(U, { method: "POST" })).text());
globalThis.Request = Saved;
await show("global Request restored", async () => (await fetch(U)).text());

srv.close();
