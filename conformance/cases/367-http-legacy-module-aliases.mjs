// node's legacy http / tls module names are require()able builtins (and
// importable, `node:` prefix or not) that carry the public pieces of http and
// tls -- the very same objects -- and node:_http_common carries the parser
// node's http is built on. oam shipped none of them, so a program that
// reached for one failed to load (nock 14 at import, #207). Also: http.METHODS
// is every method node's parser takes, and a ClientRequest is an
// OutgoingMessage.
import { createRequire, builtinModules, isBuiltin } from "node:module";
import http from "node:http";
import tls from "node:tls";
import * as viaImport from "_http_common";
const require = createRequire(import.meta.url);

for (const name of [
  "_http_agent", "_http_client", "_http_common", "_http_incoming", "_http_outgoing",
  "_http_server", "_tls_common", "_tls_wrap",
]) {
  const mod = require(name);
  console.log(name, typeof mod, "listed=" + builtinModules.includes(name), "isBuiltin=" + isBuiltin(name),
    "prefixed=" + (require("node:" + name) === mod));
}
const same = (a, b) => (a === b ? "same" : "DIFFERENT");
console.log("_http_agent.Agent", same(require("_http_agent").Agent, http.Agent));
console.log("_http_agent.globalAgent", same(require("_http_agent").globalAgent, http.globalAgent));
console.log("_http_client.ClientRequest", same(require("_http_client").ClientRequest, http.ClientRequest));
console.log("_http_incoming.IncomingMessage", same(require("_http_incoming").IncomingMessage, http.IncomingMessage));
console.log("_http_outgoing.OutgoingMessage", same(require("_http_outgoing").OutgoingMessage, http.OutgoingMessage));
console.log("_http_outgoing.validateHeaderName", same(require("_http_outgoing").validateHeaderName, http.validateHeaderName));
console.log("_http_server.ServerResponse", same(require("_http_server").ServerResponse, http.ServerResponse));
console.log("_http_server.STATUS_CODES", same(require("_http_server").STATUS_CODES, http.STATUS_CODES));
console.log("_tls_wrap.TLSSocket", same(require("_tls_wrap").TLSSocket, tls.TLSSocket));
console.log("_tls_wrap.connect", same(require("_tls_wrap").connect, tls.connect));
console.log("_tls_common.createSecureContext", same(require("_tls_common").createSecureContext, tls.createSecureContext));
console.log("import _http_common", typeof viaImport.HTTPParser, typeof viaImport.default);

const outgoing = require("_http_outgoing");
const unique = outgoing.parseUniqueHeadersOption(["Set-Cookie", "X-A"]);
console.log("parseUniqueHeadersOption", typeof unique.has, [...unique].join(","),
  outgoing.parseUniqueHeadersOption("x"));

const common = require("_http_common");
console.log("tokens", common._checkIsHttpToken("Content-Type"), common._checkIsHttpToken("a b"),
  common._checkInvalidHeaderChar("ok\tvalue"), common._checkInvalidHeaderChar("bad\nvalue"));
console.log("CRLF", JSON.stringify(common.CRLF), common.chunkExpression.test("gzip, chunked"),
  common.continueExpression.test("100-continue"));
console.log("isLenient", common.isLenient());
const parser = common.parsers.alloc();
console.log("pooled parser", parser instanceof common.HTTPParser, parser.maxHeaderPairs, parser._url === "",
  Array.isArray(parser._headers), typeof parser[common.HTTPParser.kOnHeadersComplete]);
parser.initialize(common.HTTPParser.REQUEST, {});
common.freeParser(parser, null, null);
console.log("freed back", common.parsers.list.length >= 1);

// A pooled parser builds an IncomingMessage over its socket, as node's http
// does: headers, trailers, the body pushed in.
{
  const { PassThrough } = await import("node:stream");
  const socket = new PassThrough();
  const p = common.parsers.alloc();
  p.initialize(common.HTTPParser.RESPONSE, {});
  p.socket = socket;
  let got = null;
  p.onIncoming = (msg, keepAlive) => {
    got = msg;
    console.log("incoming", msg.statusCode, msg.statusMessage, msg.httpVersion, JSON.stringify(msg.headers),
      JSON.stringify(msg.rawHeaders), keepAlive, msg.socket === socket);
    return 0;
  };
  p.execute(Buffer.from(
    "HTTP/1.1 201 Made\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nX-Dup: 1\r\nX-Dup: 2\r\n" +
      "Content-Type: a\r\nContent-Type: b\r\nTransfer-Encoding: chunked\r\n\r\n" +
      "3\r\nabc\r\n0\r\nT-A: 1\r\n\r\n",
  ));
  const chunks = [];
  got.on("data", (c) => chunks.push(String(c)));
  await new Promise((r) => got.on("end", r));
  console.log("body", chunks.join(""), "complete", got.complete, JSON.stringify(got.trailers),
    JSON.stringify(got.rawTrailers));
}

console.log("METHODS", http.METHODS.length, http.METHODS.join(","));
const req = http.request({ host: "127.0.0.1", port: 9, agent: false });
req.on("error", () => {});
console.log("ClientRequest is an OutgoingMessage", req instanceof http.OutgoingMessage);
req.destroy();
