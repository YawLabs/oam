// The certificate chain a TLS endpoint sends.
//
// node builds a context's chain the way OpenSSL does when `cert` is a
// certificate on its own: from the context's store -- the `ca` option's
// certificates, or without one the bundled roots and NODE_EXTRA_CA_CERTS --
// it adds each issuer it holds, up to and including a self-signed one. A
// `cert` that carries its chain is sent as it is. So `{ cert: leaf, ca:
// intermediate }`, a common server configuration, sends the intermediate a
// client needs, and a client with `{ cert, ca: [intermediate, root] }` is
// accepted by a server that trusts only the root. tls.createServer,
// https.createServer and http2.createSecureServer serve the same chain.
//
// The server side is observed by a node client that trusts none of these
// certificates (rejectUnauthorized false): its verdict and
// getPeerCertificate(true) show exactly the chain that came over the wire.
// The client side is observed by a node server that trusts the root alone.
//
// Regression guard: oam sent the `cert` option's certificates only, so a
// client that did not already hold the intermediate failed with
// UNABLE_TO_VERIFY_LEAF_SIGNATURE against an oam server that node's own
// server would have satisfied, and oam's client certificate failed the same
// way against a server trusting the root.
//
// Fixtures: a throwaway P-256 root, an intermediate it signed, a localhost
// leaf and a client leaf the intermediate signed, and a localhost leaf the
// root signed (valid 2025-2125); the case 141 CA as a CA that signed none.
import tls from "node:tls";
import https from "node:https";
import http2 from "node:http2";
import { spawn } from "node:child_process";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

const ROOT = `-----BEGIN CERTIFICATE-----
MIIBbzCCARWgAwIBAgIBCzAKBggqhkjOPQQDAjAeMRwwGgYDVQQDDBNvYW0gY2hh
aW4gdGVzdCByb290MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAe
MRwwGgYDVQQDDBNvYW0gY2hhaW4gdGVzdCByb290MFkwEwYHKoZIzj0CAQYIKoZI
zj0DAQcDQgAE0nhjwFbnEztYVr8y1dSDKCEkU4MIkP+piRZ5XMYbmZ9V9h/DSdI6
vXny7ztREvpnZCM8W9UROrkhfqVKwVkD/6NCMEAwDwYDVR0TAQH/BAUwAwEB/zAO
BgNVHQ8BAf8EBAMCAQYwHQYDVR0OBBYEFJdj6PKqiwenVoPtxFDOzSB1OMpkMAoG
CCqGSM49BAMCA0gAMEUCIGeHjvxGlLulZ8zLFPykdn7yAqW9zwejyn+sZpbzzWLF
AiEApPFKNRlypD0jcdsUbWPwir6+R/24IuDgLpGFZJm3GqA=
-----END CERTIFICATE-----
`;
const INTERMEDIATE = `-----BEGIN CERTIFICATE-----
MIIBmTCCAT6gAwIBAgIBDDAKBggqhkjOPQQDAjAeMRwwGgYDVQQDDBNvYW0gY2hh
aW4gdGVzdCByb290MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAm
MSQwIgYDVQQDDBtvYW0gY2hhaW4gdGVzdCBpbnRlcm1lZGlhdGUwWTATBgcqhkjO
PQIBBggqhkjOPQMBBwNCAAST5X7K5jLLkFSvxbaXgU+S9JLq2CN7/qzWAwpy2NL6
43d3Uvaav8XM8pN4F5+I0qLymVY99CFCjMDX41XKg4lLo2MwYTAPBgNVHRMBAf8E
BTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUsqhJF2Evx32JhYJDphzU
4gf/0mAwHwYDVR0jBBgwFoAUl2Po8qqLB6dWg+3EUM7NIHU4ymQwCgYIKoZIzj0E
AwIDSQAwRgIhANqCuSTUrs9MkaO50wpawZCDH7JGy1OlM152b1yCEpN8AiEApNE3
9dHUrWXG03i8g7AVEh0Rsh65oCYmUf3PkZjqfnw=
-----END CERTIFICATE-----
`;
const LEAF = `-----BEGIN CERTIFICATE-----
MIIBuDCCAV6gAwIBAgIBDTAKBggqhkjOPQQDAjAmMSQwIgYDVQQDDBtvYW0gY2hh
aW4gdGVzdCBpbnRlcm1lZGlhdGUwIBcNMjUwMTAxMDAwMDAwWhgPMjEyNTAxMDEw
MDAwMDBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49
AwEHA0IABLiuWlpOCJTxRKlkpHGm3z40S64lr0uJT2r3srJrixwBriaShC/DrLhi
x9/hByHdMFXOEX0kgimSkfxH8r437iejgYwwgYkwCQYDVR0TBAIwADALBgNVHQ8E
BAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwGgYDVR0RBBMwEYIJbG9jYWxob3N0
hwR/AAABMB0GA1UdDgQWBBSTTj8L6WhDAiyPuZ1GOOHWDEIzTDAfBgNVHSMEGDAW
gBSyqEkXYS/HfYmFgkOmHNTiB//SYDAKBggqhkjOPQQDAgNIADBFAiBJWFIR6nwY
9q6wBgsh5r7f7TxNg+Bij/Zo6uEvdCAQKgIhAMBDAXfDPiYsgEp3EKoXH03U7Vqc
G6vdaoyEoNv2Y1cA
-----END CERTIFICATE-----
`;
const LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgrqNGNZL2MkXbFnhR
6rFhG1nfb/9waDt4KC23Yn28YmGhRANCAAS4rlpaTgiU8USpZKRxpt8+NEuuJa9L
iU9q97Kya4scAa4mkoQvw6y4Ysff4Qch3TBVzhF9JIIpkpH8R/K+N+4n
-----END PRIVATE KEY-----
`;
const ROOT_LEAF = `-----BEGIN CERTIFICATE-----
MIIBsDCCAVagAwIBAgIBDjAKBggqhkjOPQQDAjAeMRwwGgYDVQQDDBNvYW0gY2hh
aW4gdGVzdCByb290MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAU
MRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATJ
hRNftCAFqRpaxjUi3R60lBk+REt0Eb1B+ZV5oAgB3llt6uCCBK40Ge0H+wKaC7pI
dbQO1nvU1TNDbuXVMAETo4GMMIGJMAkGA1UdEwQCMAAwCwYDVR0PBAQDAgeAMBMG
A1UdJQQMMAoGCCsGAQUFBwMBMBoGA1UdEQQTMBGCCWxvY2FsaG9zdIcEfwAAATAd
BgNVHQ4EFgQUC+LyumRT8srjoCBR5KFAQ2kp7SswHwYDVR0jBBgwFoAUl2Po8qqL
B6dWg+3EUM7NIHU4ymQwCgYIKoZIzj0EAwIDSAAwRQIhAJINpqGswTyYozKfXXJ+
LyX4qwhUKgWjCiu6lof9HSZvAiBZe6+EFfp8/H6LLDhCL8gn2S8edapyc+b/uPkL
D9+LdA==
-----END CERTIFICATE-----
`;
const ROOT_LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgMe5Y5B0QtzTc9F6S
3ls6+Yw95M+eWjvqDQa2lUigS0WhRANCAATJhRNftCAFqRpaxjUi3R60lBk+REt0
Eb1B+ZV5oAgB3llt6uCCBK40Ge0H+wKaC7pIdbQO1nvU1TNDbuXVMAET
-----END PRIVATE KEY-----
`;
const CLIENT = `-----BEGIN CERTIFICATE-----
MIIBpTCCAUygAwIBAgIBDzAKBggqhkjOPQQDAjAmMSQwIgYDVQQDDBtvYW0gY2hh
aW4gdGVzdCBpbnRlcm1lZGlhdGUwIBcNMjUwMTAxMDAwMDAwWhgPMjEyNTAxMDEw
MDAwMDBaMCAxHjAcBgNVBAMMFW9hbSBjaGFpbiB0ZXN0IGNsaWVudDBZMBMGByqG
SM49AgEGCCqGSM49AwEHA0IABLtMoM0LKFl2TQsVYi8GHnepkZ22cLyfQsD/MNck
wlB/ZcwNSzcL2lfLXAUmb4QIvwE+FVZD/Vl369RNvur2ho2jbzBtMAkGA1UdEwQC
MAAwCwYDVR0PBAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMCMB0GA1UdDgQWBBQz
UIZC7OmXcJ1mu/97ah+4yExdKDAfBgNVHSMEGDAWgBSyqEkXYS/HfYmFgkOmHNTi
B//SYDAKBggqhkjOPQQDAgNHADBEAiB2ADvrtm3TgspUneilQjguhIWHqVQsn8bm
bSD0o3vlTQIgGg1YB0WP9B+4cOTk9n89TRThZALq7iDVqOiJJRSC2go=
-----END CERTIFICATE-----
`;
const CLIENT_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgBSsXc+C5CPaF7G2B
4sJvzxLRsw60HuX8jEy/jNEUAxyhRANCAAS7TKDNCyhZdk0LFWIvBh53qZGdtnC8
n0LA/zDXJMJQf2XMDUs3C9pXy1wFJm+ECL8BPhVWQ/1Zd+vUTb7q9oaN
-----END PRIVATE KEY-----
`;
const OTHER_CA = `-----BEGIN CERTIFICATE-----
MIIBmjCCAUGgAwIBAgIUHjF3aO/Nr2SNMEQNV9GNuumIljswCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAaMRgwFgYDVQQDDA9vYW0gaDJzIHRlc3QgQ0EwWTATBgcq
hkjOPQIBBggqhkjOPQMBBwNCAAR6EfahtynuI8VLuixWn6GiZ3BYWFdJEqP1FfLE
lCBVF/69Rm6fDrzSVP/GWO7qsNhAZmyIVWyRQJcQiBv55omto2MwYTAdBgNVHQ4E
FgQUOlIo6O4tIFNjD7vXJV51FU2DLQcwHwYDVR0jBBgwFoAUOlIo6O4tIFNjD7vX
JV51FU2DLQcwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZI
zj0EAwIDRwAwRAIgFFCfCAiuzT1cHBF7zAQEVxSrWsoco8cOD49S6whO4vsCIC/T
xtSxdoSsByDfaJz7qxOrhJzSD5lDwUdNMe3EoP9l
-----END CERTIFICATE-----
`;

// ---- node, the other end.
const NODE_CLIENT = `
import tls from "node:tls";
const ops = JSON.parse(process.argv[1]);
for (const op of ops) {
  const line = await new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port: op.port, servername: "localhost", rejectUnauthorized: false, ALPNProtocols: op.alpn }, () => {
      const chain = [];
      for (let p = s.getPeerCertificate(true), i = 0; p && p.subject && i < 5; p = p.issuerCertificate === p ? null : p.issuerCertificate, i++) {
        chain.push(p.subject.CN);
      }
      s.destroy();
      resolve("authorizationError=" + s.authorizationError + " chain=" + JSON.stringify(chain));
    });
    s.on("error", (e) => resolve("error " + e.code));
  });
  console.log(line);
}
`;
function nodeClients(ports, alpn) {
  return new Promise((resolve) => {
    const child = spawn("node", ["--input-type=module", "-e", NODE_CLIENT, JSON.stringify(ports.map((port) => ({ port, alpn })))], {
      stdio: ["ignore", "pipe", "inherit"],
    });
    let out = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { out += d; });
    child.on("close", () => resolve(out.trim().split("\n")));
  });
}

const NODE_SERVER = `
import tls from "node:tls";
import https from "node:https";
const ROOT = \`${ROOT}\`;
const LEAF = \`${ROOT_LEAF}\`;
const KEY = \`${ROOT_LEAF_KEY}\`;
const describe = (s) => {
  const chain = [];
  for (let p = s.getPeerCertificate(true), i = 0; p && p.subject && i < 5; p = p.issuerCertificate === p ? null : p.issuerCertificate, i++) {
    chain.push(p.subject.CN);
  }
  return "authorized=" + s.authorized + " authorizationError=" + s.authorizationError + " chain=" + JSON.stringify(chain);
};
const options = { key: KEY, cert: LEAF, ca: ROOT, requestCert: true, rejectUnauthorized: false };
const t = tls.createServer(options, (s) => { s.on("error", () => {}); s.end(describe(s)); });
t.on("tlsClientError", () => {});
const h = https.createServer(options, (q, s) => s.end(describe(q.socket)));
await new Promise((r) => t.listen(0, "127.0.0.1", r));
await new Promise((r) => h.listen(0, "127.0.0.1", r));
console.log(JSON.stringify({ tls: t.address().port, https: h.address().port }));
process.stdin.on("data", () => {});
process.stdin.on("end", () => process.exit(0));
`;

// ---- 1. the chain a server under test sends.
const configs = [
  ["leaf, ca intermediate", { key: LEAF_KEY, cert: LEAF, ca: INTERMEDIATE }],
  ["leaf, ca [intermediate, root]", { key: LEAF_KEY, cert: LEAF, ca: [INTERMEDIATE, ROOT] }],
  ["leaf, ca [root, intermediate]", { key: LEAF_KEY, cert: LEAF, ca: [ROOT, INTERMEDIATE] }],
  ["leaf, ca intermediate + root in one string", { key: LEAF_KEY, cert: LEAF, ca: INTERMEDIATE + ROOT }],
  ["leaf, ca [intermediate, intermediate]", { key: LEAF_KEY, cert: LEAF, ca: [INTERMEDIATE, INTERMEDIATE] }],
  ["leaf, ca root only", { key: LEAF_KEY, cert: LEAF, ca: ROOT }],
  ["leaf, ca another CA", { key: LEAF_KEY, cert: LEAF, ca: OTHER_CA }],
  ["leaf, no ca", { key: LEAF_KEY, cert: LEAF }],
  ["leaf as [cert], ca [intermediate]", { key: LEAF_KEY, cert: [LEAF], ca: [INTERMEDIATE] }],
  ["leaf + intermediate, ca root", { key: LEAF_KEY, cert: LEAF + INTERMEDIATE, ca: ROOT }],
  ["root's leaf, ca root", { key: ROOT_LEAF_KEY, cert: ROOT_LEAF, ca: ROOT }],
  ["root's leaf, no ca", { key: ROOT_LEAF_KEY, cert: ROOT_LEAF }],
];
for (const [kind, create] of [
  ["tls", (o) => tls.createServer(o, (s) => { s.on("error", () => {}); s.end(); })],
  ["https", (o) => https.createServer(o, (q, s) => s.end())],
]) {
  const servers = [];
  for (const [, options] of configs) {
    const server = create(options);
    server.on("tlsClientError", () => {});
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    servers.push(server);
  }
  const lines = await nodeClients(servers.map((s) => s.address().port));
  for (let i = 0; i < configs.length; i++) {
    console.log(kind + " | " + configs[i][0] + ": " + lines[i]);
    servers[i].close();
  }
}
{
  const server = http2.createSecureServer({ key: LEAF_KEY, cert: LEAF, ca: INTERMEDIATE });
  server.on("tlsClientError", () => {});
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const [line] = await nodeClients([server.address().port], ["h2"]);
  console.log("http2 | leaf, ca intermediate: " + line);
  server.close();
}
{
  // setSecureContext() builds the new chain the same way.
  const server = tls.createServer({ key: ROOT_LEAF_KEY, cert: ROOT_LEAF }, (s) => { s.on("error", () => {}); s.end(); });
  server.on("tlsClientError", () => {});
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  server.setSecureContext({ key: LEAF_KEY, cert: LEAF, ca: [INTERMEDIATE, ROOT] });
  const [line] = await nodeClients([server.address().port]);
  console.log("tls | setSecureContext leaf, ca [intermediate, root]: " + line);
  server.close();
}

// ---- 2. what a server under test makes of a client chain: the client
// sends its leaf alone, and getPeerCertificate(true) goes on through the
// issuers the server's store holds (node's GetLastIssuedCert).
{
  const CHAIN_CLIENT = `
import tls from "node:tls";
const [port, cert, key] = JSON.parse(process.argv[1]);
const s = tls.connect({ host: "127.0.0.1", port, servername: "localhost", rejectUnauthorized: false, cert, key });
let body = "";
s.setEncoding("utf8");
s.on("data", (d) => { body += d; });
s.on("error", () => {});
s.on("close", () => console.log(body));
`;
  for (const [label, ca] of [["ca [intermediate, root]", [INTERMEDIATE, ROOT]], ["ca root", [ROOT]]]) {
    const server = tls.createServer({ key: LEAF_KEY, cert: LEAF, ca, requestCert: true, rejectUnauthorized: false }, (s) => {
      const chain = [];
      for (let p = s.getPeerCertificate(true), i = 0; p && p.subject && i < 5; p = p.issuerCertificate === p ? null : p.issuerCertificate, i++) {
        chain.push(p.subject.CN);
      }
      s.on("error", () => {});
      s.end("authorized=" + s.authorized + " authorizationError=" + s.authorizationError + " chain=" + JSON.stringify(chain));
    });
    server.on("tlsClientError", () => {});
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    const line = await new Promise((resolve) => {
      const child = spawn("node", ["--input-type=module", "-e", CHAIN_CLIENT,
        JSON.stringify([server.address().port, CLIENT, CLIENT_KEY])], { stdio: ["ignore", "pipe", "inherit"] });
      let text = "";
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (d) => { text += d; });
      child.on("close", () => resolve(text.trim()));
    });
    console.log("server | a client leaf alone, " + label + ": " + line);
    server.close();
  }
}

// ---- 3. the chain a client under test sends, to node servers that trust
// the root alone.
const server = spawn("node", ["--input-type=module", "-e", NODE_SERVER], { stdio: ["pipe", "pipe", "inherit"] });
const ports = await new Promise((resolve) => {
  let buffered = "";
  server.stdout.on("data", (d) => {
    buffered += d;
    if (buffered.includes("\n")) resolve(JSON.parse(buffered));
  });
});
function connect(label, options) {
  return new Promise((resolve) => {
    let data = "";
    const s = tls.connect({ host: "127.0.0.1", port: ports.tls, servername: "localhost", ...options });
    s.setEncoding("utf8");
    s.on("data", (d) => { data += d; });
    s.on("error", (e) => { data = data || "error " + e.code; });
    s.on("close", () => {
      console.log("client | " + label + ": " + data);
      resolve();
    });
  });
}
await connect("cert, ca [intermediate, root]", { ca: [INTERMEDIATE, ROOT], cert: CLIENT, key: CLIENT_KEY });
await connect("cert, ca [root, intermediate]", { ca: [ROOT, INTERMEDIATE], cert: CLIENT, key: CLIENT_KEY });
await connect("cert, ca root", { ca: ROOT, cert: CLIENT, key: CLIENT_KEY });
await connect("cert + intermediate, ca root", { ca: ROOT, cert: CLIENT + INTERMEDIATE, key: CLIENT_KEY });
await connect("secureContext {cert, ca [intermediate, root]}", {
  secureContext: tls.createSecureContext({ ca: [INTERMEDIATE, ROOT], cert: CLIENT, key: CLIENT_KEY }),
});
await new Promise((resolve) => {
  https.get({
    host: "127.0.0.1", port: ports.https, servername: "localhost", agent: false, path: "/",
    ca: [INTERMEDIATE, ROOT], cert: CLIENT, key: CLIENT_KEY,
  }, (res) => {
    let body = "";
    res.on("data", (d) => { body += d; });
    res.on("end", () => {
      console.log("client | https cert, ca [intermediate, root]: " + res.statusCode + " " + body);
      resolve();
    });
  }).on("error", (e) => {
    console.log("client | https cert, ca [intermediate, root]: error " + e.code);
    resolve();
  });
});
server.stdin.end();
