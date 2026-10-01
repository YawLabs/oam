// fetch builds its request with its own Request class, not with whatever
// `globalThis.Request` is when it is called. Measured on node v22.22.2
// first: with the global replaced by another class, and then deleted, a
// fetch still works. oam's fetch built its request with the global, so it
// threw `Cannot read properties of undefined (reading 'method')` and then
// `globalThis.Request is not a constructor`.
//
// And whether a RequestInit "has a key" -- which decides if a Request
// input's headers are refilled from init and its referrer reset -- is asked
// of the converted dictionary, as undici asks it: a RequestInit member
// counts when it is not undefined, inherited ones too, and an unknown key
// never counts. oam counted the init object's own keys, so an init whose
// members were inherited (an `Object.create(defaults)`, a class instance)
// lost its headers, and `{ foo: 1 }` or `{ method: undefined }` reset a
// Request input's referrer.
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

class Init {
  get method() {
    return "POST";
  }
}
Init.prototype.headers = { x: "1" };
await show("init, inherited headers", async () =>
  (await fetch(U, Object.create({ headers: { authorization: "Bearer t" } }))).text(),
);
await show("init, class instance", async () => (await fetch(U, new Init())).text());
await show("Request, inherited headers", async () => {
  const r = new Request(U, Object.create({ headers: { authorization: "Bearer t" } }));
  return r.headers.get("authorization");
});
const input = () =>
  new Request(U, { referrer: "", referrerPolicy: "no-referrer", headers: { x: "in" } });
for (const [label, init] of [
  ["{}", {}],
  ["{ foo: 1 }", { foo: 1 }],
  ["{ method: undefined }", { method: undefined }],
  ["{ headers: undefined }", { headers: undefined }],
  ["{ method: 'GET' }", { method: "GET" }],
  ["inherited method", Object.create({ method: "GET" })],
  ["{ dispatcher: null }", { dispatcher: null }],
]) {
  await show(`Request(Request, ${label})`, async () => {
    const r = new Request(input(), init);
    return JSON.stringify([r.referrer, r.referrerPolicy, r.headers.get("x")]);
  });
}

srv.close();
