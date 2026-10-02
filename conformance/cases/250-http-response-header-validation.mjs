// What a node:http server response refuses as a header, measured on node
// v22.22.2 first. oam's ServerResponse stored anything: `res.setHeader('y',
// '€')` did not throw and the value went out as its UTF-8, a CR or LF went
// to hyper, which answered 500, appendHeader() wrote to the wrong store, and
// setHeaders() / addTrailers() did not exist.
//
//   - setHeader, appendHeader, setHeaders(Map | Headers) and writeHead()'s
//     headers -- an object, a flat [name, value, ...] list or a list of
//     pairs -- refuse a name that is not a token (ERR_INVALID_HTTP_TOKEN), an
//     undefined value (ERR_HTTP_INVALID_HEADER_VALUE), and a value holding a
//     control character or a code point above U+00FF (ERR_INVALID_CHAR),
//     each with node's text. U+00E9 and U+00FF are allowed: the head goes
//     out one byte per code point.
//   - writeHead refuses an odd flat list, a status outside 100-999 and a
//     status message with such a character, in node's order: headers given
//     to an untouched response are checked after the status message, and
//     none is kept when one is refused; after setHeader() they go through
//     setHeader() / appendHeader() and are checked first.
//   - addTrailers refuses with `Trailer name` / `trailer content`.
//   - Once the head is out, set / append / remove / write headers throw
//     ERR_HTTP_HEADERS_SENT. writeHead() puts it out: headersSent turns
//     true and a second writeHead() throws. oam's second writeHead() added
//     its headers to the first one's, and two content-lengths made hyper
//     panic.
//   - http.validateHeaderName / validateHeaderValue are the same checks.
//   - getHeader / hasHeader / removeHeader refuse a name that is not a
//     string (ERR_INVALID_ARG_TYPE), on a response, an OutgoingMessage and a
//     ClientRequest alike; getHeader(1) answered undefined and hasHeader(1)
//     false.
import http from "node:http";

const describe = (e) => `${e.constructor.name}${e.code ? `[${e.code}]` : ""}: ${e.message}`;
const shown = (value) => {
  if (value === undefined) return "undefined";
  if (Array.isArray(value)) return `[${value.map(shown).join(",")}]`;
  return JSON.stringify(value);
};
const attempt = (label, fn) => {
  try {
    const result = fn();
    console.log(label, "ok", result === undefined ? "" : shown(result));
  } catch (e) {
    console.log(label, describe(e));
  }
};

const values = {
  ascii: "plain",
  e9: "café",
  ff: "ÿ\u0080",
  euro: "€",
  pair: "😀",
  lone: "\ud800",
  tab: "a\tb",
  ctl: "a\u0001b",
  del: "a\u007fb",
  nul: "a\u0000b",
  cr: "a\rb",
  lf: "a\nb",
  crlf: "a\r\nx-injected: 1",
  spaces: " x ",
  number: 5,
  null: null,
  undefined: undefined,
  list: ["ok", "€"],
  listOk: ["a", "é"],
  object: { toString: () => "€" },
};
const names = {
  empty: "",
  space: "x y",
  euro: "x-€",
  e9: "x-é",
  colon: "a:b",
  number: 123,
  undefined: undefined,
  ok: "X-Ok",
};

const nameArgs = { number: 1, undefined: undefined, null: null, object: {}, symbol: Symbol("s"), string: "X-V" };

// Each case runs in its own request, on a fresh response.
const cases = [];
const run = (label, fn) => cases.push([label, fn]);

run("setHeader values", (res) => {
  for (const [k, v] of Object.entries(values)) {
    attempt(`  setHeader ${k}`, () => {
      res.setHeader("x-v", v);
      return res.getHeader("x-v");
    });
  }
  for (const [k, n] of Object.entries(names)) {
    attempt(`  setHeader name ${k}`, () => res.setHeader(n, "1") === res);
  }
});
run("appendHeader", (res) => {
  for (const [k, v] of Object.entries(values)) attempt(`  appendHeader ${k}`, () => res.appendHeader("x-a", v) === res);
  for (const [k, n] of Object.entries(names)) attempt(`  appendHeader name ${k}`, () => res.appendHeader(n, "1") === res);
  attempt("  x-a", () => res.getHeader("x-a"));
  res.setHeader("x-b", null);
  res.appendHeader("x-b", ["c", "d"]);
  attempt("  x-b", () => res.getHeader("x-b"));
});
run("setHeaders", (res) => {
  attempt("  Map euro", () => res.setHeaders(new Map([["x-m", "€"]])) === res);
  attempt("  Map bad name", () => res.setHeaders(new Map([["x m", "1"]])));
  attempt("  Map undefined", () => res.setHeaders(new Map([["x-m", undefined]])));
  attempt("  Map ok", () => res.setHeaders(new Map([["x-m", "café"], ["set-cookie", "a=1"]])) === res);
  attempt("  Headers ok", () => res.setHeaders(new Headers([["x-h", "ÿ"], ["set-cookie", "b=2"], ["set-cookie", "c=3"]])) === res);
  attempt("  stored", () => [res.getHeader("x-m"), res.getHeader("x-h"), res.getHeader("set-cookie")]);
  attempt("  object", () => res.setHeaders({ a: "1" }));
  attempt("  array", () => res.setHeaders([["a", "1"]]));
  attempt("  undefined", () => res.setHeaders(undefined));
  attempt("  null", () => res.setHeaders(null));
});
for (const [label, headers] of [
  ["object euro", { "x-v": "€" }],
  ["object bad name", { "x v": "1" }],
  ["object empty name", { "": "1" }],
  ["object undefined", { "x-v": undefined }],
  ["object list euro", { "x-v": ["a", "€"] }],
  ["object list undefined", { "x-v": ["a", undefined] }],
  ["object later refused", { "x-a": "1", "x-v": "€" }],
  ["flat euro", ["x-v", "€"]],
  ["flat bad name", ["x v", "1"]],
  ["flat undefined", ["x-v", undefined]],
  ["flat odd", ["x-v"]],
  ["flat empty name", ["", "1"]],
  ["pairs euro", [["x-v", "€"]]],
  ["pairs ok", [["x-v", "café"], ["x-v", "2"]]],
  ["object ok", { "x-v": "ÿ" }],
]) {
  // node keeps these out of getHeader(); what was kept shows on the wire.
  run(`writeHead ${label}`, (res) => {
    attempt("  fresh", () => res.writeHead(200, headers) === res);
  });
  run(`writeHead ${label} after setHeader`, (res) => {
    res.setHeader("x-s", "1");
    attempt("  progressive", () => {
      res.writeHead(200, headers);
      return [res.getHeader("x-a"), res.getHeader("x-v")];
    });
  });
}
run("writeHead status", (res) => {
  for (const status of [99, 1000, "abc", -1]) attempt(`  status ${shown(status)}`, () => res.writeHead(status));
  attempt("  status unchanged", () => res.statusCode);
});
run("writeHead message euro", (res) => {
  attempt("  euro", () => res.writeHead(200, "€"));
  attempt("  statusMessage kept", () => res.statusMessage);
  res.statusMessage = "OK";
});
run("writeHead message lf", (res) => {
  attempt("  lf", () => res.writeHead(200, "a\nb", {}));
  res.statusMessage = "OK";
});
run("writeHead message e9", (res) => {
  attempt("  e9", () => res.writeHead(200, "café") === res);
});
run("writeHead message default", (res) => {
  attempt("  default", () => {
    res.writeHead(404);
    return res.statusMessage;
  });
});
run("writeHead message and header both bad, fresh", (res) => {
  attempt("  fresh", () => res.writeHead(200, "€", { "x-v": "€" }));
  res.statusMessage = "OK";
});
run("writeHead message and header both bad, progressive", (res) => {
  res.setHeader("x-s", "1");
  attempt("  progressive", () => res.writeHead(200, "€", { "x-v": "€" }));
  res.statusMessage = "OK";
});
run("implicit head: bad status", (res) => {
  res.statusCode = 99;
  attempt("  end", () => res.end());
  res.statusCode = 200;
});
run("implicit head: bad message", (res) => {
  res.statusMessage = "a\u0001";
  attempt("  end", () => res.end());
  res.statusMessage = "OK";
});
run("addTrailers", (res) => {
  attempt("  euro", () => res.addTrailers({ "x-t": "€" }));
  attempt("  bad name", () => res.addTrailers({ "x t": "1" }));
  attempt("  undefined", () => res.addTrailers({ "x-t": undefined }));
  attempt("  pairs euro", () => res.addTrailers([["x-t", "€"]]));
  attempt("  list item lf", () => res.addTrailers({ "x-t": ["a", "b\n"] }));
  attempt("  ok", () => res.addTrailers({ "x-t": "café", "x-u": ["a", "b"] }));
  attempt("  null", () => res.addTrailers(null));
});
run("after the head is out", (res) => {
  res.write("a");
  attempt("  setHeader", () => res.setHeader("x-v", "1"));
  attempt("  appendHeader", () => res.appendHeader("x-v", "1"));
  attempt("  setHeaders", () => res.setHeaders(new Map([["x-v", "1"]])));
  attempt("  removeHeader", () => res.removeHeader("x-v"));
  attempt("  writeHead", () => res.writeHead(200));
  attempt("  setHeader invalid", () => res.setHeader("x v", "€"));
});

for (const [label, before, first, second] of [
  ["fresh", null, { "x-a": "1", "x-b": "1" }, { "x-a": "2", "x-c": "2" }],
  ["content-length", null, { "content-length": "2", "x-a": "1" }, { "content-length": "5", "x-a": "2" }],
  ["bare", null, undefined, undefined],
  ["flat", null, ["x-a", "1", "x-a", "2"], ["x-a", "3"]],
  ["after setHeader", "x-s", { "x-a": "1" }, { "x-a": "2", "x-s": "2" }],
]) {
  run(`writeHead twice, ${label}`, (res) => {
    if (before) res.setHeader(before, "1");
    attempt("  first", () => res.writeHead(200, first) === res);
    attempt("  headersSent", () => res.headersSent);
    attempt("  second", () => res.writeHead(201, second));
    attempt("  setHeader", () => res.setHeader("x-a", "3"));
    attempt("  appendHeader", () => res.appendHeader("x-a", "3"));
    attempt("  setHeaders", () => res.setHeaders(new Map([["x-a", "3"]])));
    attempt("  removeHeader", () => res.removeHeader("x-a"));
    attempt("  hasHeader string", () => typeof res.hasHeader("x-a"));
    attempt("  statusCode", () => res.statusCode);
    res.end("hi");
  });
}
run("writeHead refused, then end", (res) => {
  attempt("  refused", () => res.writeHead(200, { "x-v": "€" }));
  attempt("  headersSent", () => res.headersSent);
  attempt("  setHeader", () => res.setHeader("x-a", "1") === res);
  attempt("  writeHead", () => res.writeHead(202, { "x-b": "1" }) === res);
  attempt("  headersSent after", () => res.headersSent);
});

run("name argument types", (res) => {
  for (const [k, v] of Object.entries(nameArgs)) {
    attempt(`  getHeader ${k}`, () => res.getHeader(v));
    attempt(`  hasHeader ${k}`, () => res.hasHeader(v));
    attempt(`  removeHeader ${k}`, () => res.removeHeader(v));
  }
});

const server = http.createServer((req, res) => {
  const [label, fn] = cases[Number(req.url.slice(1))];
  console.log(label);
  fn(res);
  res.end();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
for (let i = 0; i < cases.length; i++) {
  const seen = await new Promise((resolve, reject) => {
    http
      .get({ port, host: "127.0.0.1", path: `/${i}` }, (res) => {
        res.resume();
        const sent = Object.entries(res.headers).filter(([name]) => name.startsWith("x-"));
        res.on("end", () => resolve(`${res.statusCode} ${JSON.stringify(sent)}`));
      })
      .on("error", reject);
  });
  console.log("  ->", seen);
}
server.close();

for (const [label, fn] of [
  ["validateHeaderName space", () => http.validateHeaderName("x y")],
  ["validateHeaderName empty", () => http.validateHeaderName("")],
  ["validateHeaderName number", () => http.validateHeaderName(1)],
  ["validateHeaderName label", () => http.validateHeaderName("x y", "Trailer name")],
  ["validateHeaderName ok", () => http.validateHeaderName("x-v")],
  ["validateHeaderValue euro", () => http.validateHeaderValue("x", "€")],
  ["validateHeaderValue lf", () => http.validateHeaderValue("x", "a\nb")],
  ["validateHeaderValue undefined", () => http.validateHeaderValue("x", undefined)],
  ["validateHeaderValue e9", () => http.validateHeaderValue("x", "café")],
  ["validateHeaderValue null", () => http.validateHeaderValue("x", null)],
]) {
  attempt(label, fn);
}

// The same name check on the other outgoing messages.
const outgoing = new http.OutgoingMessage();
const request = http.request({ host: "127.0.0.1", port, path: "/" });
request.on("error", () => {});
for (const [label, message] of [["OutgoingMessage", outgoing], ["ClientRequest", request]]) {
  for (const [k, v] of Object.entries(nameArgs)) {
    attempt(`${label} getHeader ${k}`, () => message.getHeader(v));
    attempt(`${label} hasHeader ${k}`, () => message.hasHeader(v));
    attempt(`${label} removeHeader ${k}`, () => message.removeHeader(v));
  }
}
request.destroy();

// The errors themselves are node's internal NodeErrors: the per-code
// prototype (not TypeError.prototype), node's own-key order, and the code in
// toString and in the stack header. oam built them as plain TypeErrors with
// a code assigned.
for (const [label, fn] of [
  ["setHeader value with LF", () => new http.OutgoingMessage().setHeader("x", "a\nb")],
  ["validateHeaderName('bad name')", () => http.validateHeaderName("bad name")],
  ["setHeader('x') with no value", () => new http.OutgoingMessage().setHeader("x")],
  ["validateHeaderValue('x', undefined)", () => http.validateHeaderValue("x", undefined)],
]) {
  try {
    fn();
    console.log(label, "no throw");
  } catch (e) {
    console.log(
      label,
      e.code,
      Reflect.ownKeys(e).filter((k) => typeof k === "string").join(","),
      "TypeError.prototype:",
      Object.getPrototypeOf(e) === TypeError.prototype,
      JSON.stringify(String(e)),
      JSON.stringify(String(e.stack).split("\n")[0]),
    );
  }
}
