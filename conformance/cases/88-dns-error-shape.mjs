// A dns failure carries more than `code`. Node's dnsException() stamps
// `syscall` and `hostname` on every resolver error, and on the getaddrinfo
// path an `errno` too -- the negative libuv EAI_* number, so
// util.getSystemErrorName(err.errno) round-trips to EAI_NONAME. oam returned
// the bare resolver rejection, leaving all three undefined, which made a
// handler logging the decoded name print "Unknown system error undefined" and
// made `err.syscall === 'getaddrinfo'` never match.
//
// The resolve* family deliberately has NO errno on either runtime (c-ares
// fails with a string code, not a libuv number), so this pins the asymmetry
// rather than assuming every dns error looks the same.
//
// .invalid is reserved by RFC 2606 and can never resolve, so the failure is
// deterministic without depending on the network being down.
import dns from "node:dns";
import util from "node:util";

const BAD = "no-such-host.invalid";

function shape(label, err) {
  console.log(
    label,
    "code=" + err.code,
    "errno=" + typeof err.errno,
    "syscall=" + err.syscall,
    "hostname=" + err.hostname,
  );
}

// 1) getaddrinfo path: the one that carries a libuv number.
await new Promise((resolve) => {
  dns.lookup(BAD, (err) => {
    shape("lookup", err);
    console.log("lookup errno decodes to", util.getSystemErrorName(err.errno));
    console.log("lookup message", err.message);
    resolve();
  });
});

// 2) promises form inherits the same shaping (it used to hand back the raw
//    op rejection).
try {
  await dns.promises.lookup(BAD);
  console.log("promises.lookup UNEXPECTEDLY RESOLVED");
} catch (err) {
  shape("promises.lookup", err);
}

// 3) resolver path: syscall/hostname present, errno absent on both runtimes.
await new Promise((resolve) => {
  dns.resolve4(BAD, (err) => {
    shape("resolve4", err);
    console.log("resolve4 message", err.message);
    resolve();
  });
});

// 4) the Resolver class routes through the same shaping.
const resolver = new dns.Resolver();
await new Promise((resolve) => {
  resolver.resolve4(BAD, (err) => {
    shape("Resolver.resolve4", err);
    resolve();
  });
});
