// node's `https` module exports exactly six names -- Agent, globalAgent, Server,
// createServer, get, request -- and not the nine that `http` carries but node's
// `https` does not. oam built `https` by copying every export of `http`, so
// those nine (STATUS_CODES, METHODS, ClientRequest, IncomingMessage,
// OutgoingMessage, ServerResponse, maxHeaderSize, validateHeaderName,
// validateHeaderValue) linked here and fail to link on node, before a line of
// the program runs. Printed: https's own keys, and the type of each name node's
// https lacks. Measured on node v22.22.2.
import https from "node:https";
import * as httpsNs from "node:https";

console.log("default keys:", Object.keys(https).join(" "));
console.log("namespace keys:", Object.keys(httpsNs).filter((k) => k !== "default").sort().join(" "));
for (const name of [
  "STATUS_CODES", "METHODS", "ClientRequest", "IncomingMessage", "OutgoingMessage",
  "ServerResponse", "maxHeaderSize", "validateHeaderName", "validateHeaderValue",
]) {
  console.log(name + ": " + typeof https[name]);
}
// The six that MUST be present still work.
console.log("createServer:", typeof https.createServer, " request:", typeof https.request,
  " get:", typeof https.get, " Agent:", typeof https.Agent, " Server:", typeof https.Server,
  " globalAgent:", typeof https.globalAgent);
