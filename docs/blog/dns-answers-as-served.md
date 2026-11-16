# Answering DNS the way the server answered it

A name lookup has two halves that look like one call. `dns.lookup()` asks the
operating system, which asks whatever `/etc/resolv.conf` or its Windows
equivalent points at. `dns.resolve4()` and the rest ask a name server
directly, over UDP, and report what came back. Node runs the second half
through c-ares, a DNS client library compiled into `node.exe`.

oam, a JavaScript and TypeScript runtime built on Rust and V8, has no libuv
and no c-ares. Its resolver is `hickory-resolver`. The work of making
`node:dns` answer like Node's landed across v0.19.2 and v0.19.3 (2026-11-03),
and almost none of it was about parsing DNS. It was about the difference
between a resolver that *knows* the answer and a resolver that *reports*
the answer. oam's knew too much.

None of this has been written up on this blog before; the divergence list in
`docs/node-divergences.md` is the only prior account.

## The rule: one query, and the server's words

Six behaviours, each of which oam got wrong in the direction of being
helpful:

- **A CNAME is not chased.** oam sent a second query for the target and
  answered with the target's records. Node sends one query and answers `[]`.
- **`localhost` and `*.invalid` are asked, not answered.** oam replied from
  local knowledge. Node asks, and `resolve4('localhost')` against a server
  with no such name is `ENOTFOUND`.
- **An answer holding only other record types is not a failure.** oam
  reported `ENODATA`. Node reports `[]` for TLSA, MX, TXT, SRV, CAA and
  NAPTR; `ENODATA` for NS and PTR; for A / AAAA, `[]` when a CNAME is among
  the records and `ENODATA` when none is; and for a CNAME, the name asked.
- **Nothing is cached.** oam kept an answer cache, so a repeated query --
  including an NXDOMAIN or a NODATA answer -- was served without asking the
  server again. Every query now goes out, as c-ares sends every query.
- **TTLs are as served.** oam clamped them to one day.
- **No search domain is tried.** A bare name was walked through the search
  list. Node's `resolve*` functions try none: c-ares calls `ares_query`, not
  `ares_search`.

None of these is a parser bug. Each is a resolver deciding it can improve on
the answer, and each improvement was a divergence from the runtime people
are porting code from.

## Why helpfulness is the bug

The cache is the clearest case. Caching is what a resolver is *for* from the
user's point of view: fewer round trips, faster answers. It is also exactly
what a test that asserts "the second lookup asks the server" fails on, and
what a program that expects an NXDOMAIN to arrive fresh -- a
dynamic-DNS client, anything watching a record flip -- silently does not get.

The CNAME case is sharper. Node answering `[]` looks like a bug in Node. It
is c-ares handing back the answer section it received, which holds a CNAME
and no address, and Node reporting that faithfully. An implementation that
follows the alias and returns the address produces the *correct* DNS answer
and the *wrong* Node answer. For a runtime whose contract is Node's
behaviour, the second one is the requirement.

The same shape recurs across `node:dns`, and two entries in it are worth
reading slowly because they look like regressions to file:

**A failed `reverse()` always reports `ENOTFOUND`.** Node's reverse goes through
c-ares' `ares_gethostbyaddr`, which reports *every* failed reverse lookup
that way. oam distinguished `ENODATA` (the name exists, has no PTR) from
`ENOTFOUND`, which is the better error -- and `dns.resolvePtr()` still does,
deliberately. The `reverse()` path reports what Node reports because that is
what a caller branching on `err.code === 'ENOTFOUND'` was written against.

**Asking two servers in parallel is the one where oam's old behaviour was
plainly worse.** oam took whichever answer arrived first, so an empty answer from a
broken second server could beat the real answer from the first. c-ares walks
the list, one server at a time, and moves on only when a server does not
answer in time or sends something unparseable. oam now does the same: 2 s
per server, doubling each round, or `resolv.conf`'s `timeout:`.

## What Node actually pins, from the cases that pin it

`CONFORMANCE.md` lists twelve DNS cases as of oam 0.20.0, all passing
against Node v22.22.2: `228` (getaddrinfo hints), `579` (error key order),
`581` (reverse and server order), `583` (falsy hostname), `584` (resolver
instances), `587` (TLSA, TTL, no cache), `588` (result order), `590`
(lookupService), `591` (result order reaching `fetch`), `592` (answer
section as served), plus `88` and `90` (error shape and name shapes).

Two of them are worth naming because they pin behaviour that is not
observable without a controlled server. `587` and `592` cannot be asserted
against a public resolver: the resolver in front of you may cache on its
own, and then your runtime's cache is invisible. They run against a local
server the harness controls, which is the only way "every query is sent" is
a checkable claim rather than a sentence in a changelog.

Case `584` is the other structural one. `new dns.Resolver()` in Node is its
own channel: it starts from the system nameservers, its `setServers()`
reaches no other instance and not the default, its `timeout` / `tries` /
`maxTimeout` set its own timing, and `cancel()` fails its pending queries
with `ECANCELLED`. Up to v0.19.2, `dns.promises.Resolver` did not exist at
all and an instance's `setServers()` threw `ENOSYS`. That is not a subtle
divergence; it is a missing class. It landed as
<a href="https://github.com/YawLabs/oam/pull/463">#463</a>.

## Three things that were not DNS at all

**Windows delivered a query's answer to a different socket.** Each query's
random source port was bound without exclusive use, and Windows permits that
bind to share a port another socket already holds on `127.0.0.1`. The answer
went to the other socket; the query then waited out its full timeout. So the
symptom was a slow lookup, not a wrong one, and the fault was in the bind
rather than anywhere in the resolver. Fixed by binding exclusively and
picking another port when taken
(<a href="https://github.com/YawLabs/oam/pull/467">#467</a>).

**`os.hostname()` was read from the environment.** It read `COMPUTERNAME` /
`HOSTNAME`, so it answered `localhost` on macOS -- where neither variable is
set -- and the upper-case NetBIOS name on Windows, where Node reports the
case the system actually uses. The reverse-lookup and `lookupService` paths
are built on the host name, so this sat underneath the DNS work rather than
beside it.

**An unanswered UDP query was resent about every 340 ms within a single
wait.** A silent server therefore got up to three datagrams per try where
c-ares sends one. Under a harness that counts datagrams, that is three
times the traffic and a different test result; on a real network it is
retry noise pointed at a server that has already declined to answer.

## What remains

Three timing differences survive, none of which changes a result, and they
are listed as divergences rather than hidden: Node checks c-ares's timeouts
on a 1 s timer so its waits run up to a second longer than oam's nominal
ones (a dead first server costs oam 2.0 s and Node 2.1-2.9 s); c-ares
shortens a later server's timeout from the latency it measured on earlier
answers, which oam does not model; and c-ares resends with the same query ID
on each try where oam sends each try with a new one. One answer still
differs outright: a SOA query answered with records of other types only is
`EBADRESP` in Node and `ENODATA` in oam.

All of it is written down in divergence 30 of
<a href="https://github.com/YawLabs/oam/blob/main/docs/node-divergences.md"><code>docs/node-divergences.md</code></a>,
which is the honest place for it. A runtime that claims Node's DNS while
running a different client library will differ in the timing layer
regardless; the question is whether the differences change an answer. The
timing ones do not, and the SOA case is the one that does.

The larger point is the one that runs through oam's conformance work
generally: the reference is not the DNS specification, it is c-ares at the
version your Node vendors. Reading RFC 1035 would have told oam that
following a CNAME is correct. It would not have told it that Node answers
`[]`.

oam is beta: breaking changes before 1.0 are still possible, and there is no
LTS, so this is not the runtime for a service you are on call for.