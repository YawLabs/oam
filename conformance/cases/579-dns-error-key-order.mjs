// A dns failure's own keys come in node's order. node's DNSException assigns
// `errno` FIRST and always -- the libuv EAI_* number on the getaddrinfo path,
// an own `undefined` on the resolve* path (c-ares fails with a string code) --
// then `code`, `syscall`, `hostname`. So every dns error enumerates as
// errno, code, syscall, hostname, and `'errno' in err` is true even where its
// value is undefined (measured on v22.22.2).
//
// oam assigned `errno` last on the lookup path and not at all on the resolve*
// path, so `Object.keys(err)` read code, syscall, hostname, errno (lookup) or
// code, syscall, hostname (resolve4). Key order is observable to anything that
// snapshots, diffs or logs an error. Case 88 pins the values; this pins the
// order and the presence of the key.
//
// .invalid is reserved by RFC 2606 and can never resolve, so the failure is
// deterministic without depending on the network being down.
import dns from "node:dns";

const BAD = "no-such-host.invalid";

function keys(label, err) {
  console.log(label, JSON.stringify(Object.keys(err)), "errno own:", Object.hasOwn(err, "errno"));
}

await new Promise((resolve) => {
  dns.lookup(BAD, (err) => {
    keys("lookup", err);
    resolve();
  });
});

try {
  await dns.promises.lookup(BAD);
  console.log("promises.lookup UNEXPECTEDLY RESOLVED");
} catch (err) {
  keys("promises.lookup", err);
}

await new Promise((resolve) => {
  dns.resolve4(BAD, (err) => {
    keys("resolve4", err);
    resolve();
  });
});

await new Promise((resolve) => {
  new dns.Resolver().resolve4(BAD, (err) => {
    keys("Resolver.resolve4", err);
    resolve();
  });
});
