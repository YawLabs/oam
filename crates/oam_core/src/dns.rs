use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts, ServerOrderingStrategy,
};
use hickory_resolver::net::NetError;
use hickory_resolver::proto::rr::{Name, RData, RecordType};

use super::OpOutcome;

// ------------------------------------------------------------- channels
//
// A "channel" is what c-ares calls one: an ordered nameserver list plus the
// try budget, and it is the unit node's dns API works in. Which channel a call
// uses depends on which API it came through (`Api`), as in node v22.22.2:
//
// * dns.setServers() builds a new channel and makes it the default for BOTH
//   the callback API and dns.promises (node's defaultResolverSetServers
//   rebinds both).
// * dns.promises.setServers() runs the promises module's own copy of that
//   function, which rebinds only dns.promises -- dns.getServers() keeps the
//   old list (measured).
// * every `new dns.Resolver()` reads the SYSTEM configuration, whatever
//   setServers did to the defaults.
//
// One divergence is left on purpose: node gives each Resolver instance its
// own channel (own failure counts), where oam's instances share the system
// channel. Only which server a query tries first can differ, and only after a
// server has failed.
//
// Each nameserver gets its own single-server hickory resolver, and the walk
// across them is oam's, not hickory's, because hickory's pool does not do what
// c-ares does (node v22.22.2 ships c-ares 1.34.6, measured against fake UDP
// servers on 127.0.0.1):
//
// * c-ares sends to ONE server at a time, best first, where best is fewest
//   consecutive failures and then configuration order. hickory defaults to two
//   servers in parallel, ordered by smoothed RTT, and takes whichever answers
//   first -- so with [good-but-50ms, broken-but-instant] node got the good
//   server's records and oam got the broken one's empty NOERROR (a CAA query
//   came back ENODATA where node had records).
// * c-ares moves on only when a server does not answer (timeout, network
//   error). node creates its channels with ARES_FLAG_NOCHECKRESP, so a
//   SERVFAIL or REFUSED answer is final: [SERVFAIL, good] is ESERVFAIL in node
//   with the good server never asked.
// * A server that failed is demoted for later queries: after one timeout on
//   [dead, good] the next query goes straight to the good server.
//
// hickory's UserProvidedOrder with one request at a time gets the first two
// points but not failover: its pool spends the whole timeout on the first
// server and then stops (the pool deadline IS the per-server timeout), and
// every retry starts at the same dead server again.
//
// The try budget is the system one: `attempts` retries after the first round,
// one round being every server once. For a single server that is exactly what
// hickory's own pool and retry handle did, so a dead lone nameserver takes as
// long as before.

/// One nameserver of a channel.
struct Server {
    resolver: TokioResolver,
    /// Consecutive failures to answer (timeouts, network errors); any answer
    /// resets it. The walk tries the server with the fewest first.
    failures: AtomicU32,
}

struct Channel {
    servers: Vec<Server>,
    /// What `getServers()` reports, in node's string form and config order.
    display: Vec<String>,
    /// Rounds after the first, as hickory's `ResolverOpts::attempts`.
    attempts: usize,
    ndots: usize,
}

/// The system configuration every channel is built from: search domains and
/// resolver options, read once. Only the nameserver list differs per channel.
struct Base {
    domain: Option<Name>,
    search: Vec<Name>,
    options: ResolverOpts,
    name_servers: Vec<NameServerConfig>,
}

fn base() -> &'static Base {
    static BASE: OnceLock<Base> = OnceLock::new();
    BASE.get_or_init(|| {
        let (config, options) = hickory_resolver::system_conf::read_system_conf()
            .expect("read system DNS configuration");
        let (domain, search, name_servers) = config.into_parts();
        Base {
            domain,
            search,
            options,
            name_servers,
        }
    })
}

/// The port a nameserver config queries: its first connection's (hickory gives
/// UDP and TCP the same one).
fn server_port(ns: &NameServerConfig) -> u16 {
    ns.connections.first().map_or(53, |c| c.port)
}

/// A nameserver in node's `dns.getServers()` string form: a plain address when
/// the port is the default 53 (IPv6 unbracketed), `ip:port` / `[ip]:port`
/// otherwise -- node's ResolverBase.getServers, measured on v22.22.2
/// (`[::1]` -> "::1", `[::1]:5353` -> "[::1]:5353").
fn format_server(ip: IpAddr, port: u16) -> String {
    match (ip, port) {
        (_, 53) => ip.to_string(),
        (IpAddr::V4(v4), _) => format!("{v4}:{port}"),
        (IpAddr::V6(v6), _) => format!("[{v6}]:{port}"),
    }
}

/// Builds a channel over `name_servers`, in order. A server listed twice (same
/// address AND port) is kept once, at its first position -- c-ares does the
/// same (setServers(['1.2.3.4', '1.2.3.4:53']) reports one server on node).
fn build_channel(base: &Base, name_servers: Vec<NameServerConfig>) -> Channel {
    let mut seen: Vec<(IpAddr, u16)> = Vec::new();
    let mut servers = Vec::new();
    let mut display = Vec::new();
    for ns in name_servers {
        let key = (ns.ip, server_port(&ns));
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        display.push(format_server(key.0, key.1));
        let config = ResolverConfig::from_parts(base.domain.clone(), base.search.clone(), vec![ns]);
        let mut builder = TokioResolver::builder_with_config(
            config,
            hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
        );
        let opts = builder.options_mut();
        *opts = base.options.clone();
        // Node parity: c-ares always advertises EDNS0 (1232-byte UDP payloads,
        // DNS flag day 2020). hickory's system-conf path mirrors glibc instead
        // -- EDNS only with an `options edns0` line in resolv.conf -- and some
        // forwarders (Tailscale MagicDNS) answer non-EDNS queries with
        // oversized TC=1 datagrams that then fail to parse, so every large
        // answer (TXT sets) errors where Node succeeds. try_tcp_on_error
        // covers the same forwarder shape for answers past the EDNS size.
        opts.edns0 = true;
        opts.try_tcp_on_error = true;
        // One server per resolver and one try per call: the retries and the
        // server walk are the channel's (`walk`), so they follow c-ares.
        opts.attempts = 0;
        opts.num_concurrent_reqs = 1;
        opts.server_ordering_strategy = ServerOrderingStrategy::UserProvidedOrder;
        servers.push(Server {
            resolver: builder.build().expect("build DNS resolver"),
            failures: AtomicU32::new(0),
        });
    }
    Channel {
        servers,
        display,
        attempts: base.options.attempts,
        ndots: base.options.ndots,
    }
}

fn system_channel() -> Arc<Channel> {
    static SYSTEM: OnceLock<Arc<Channel>> = OnceLock::new();
    SYSTEM
        .get_or_init(|| {
            let base = base();
            Arc::new(build_channel(base, base.name_servers.clone()))
        })
        .clone()
}

/// The API a dns call came through; it picks the channel (see above).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// The module-level callback functions (`dns.resolve4`, `dns.reverse`, ...).
    Callback,
    /// `dns.promises` / `node:dns/promises`.
    Promises,
    /// A `dns.Resolver` instance.
    Resolver,
}

impl Api {
    /// The JS side's number for an API: 0 callback, 1 promises, 2 Resolver.
    /// Anything else is the callback API, the default every caller had before
    /// the argument existed.
    pub fn from_index(index: u32) -> Api {
        match index {
            1 => Api::Promises,
            2 => Api::Resolver,
            _ => Api::Callback,
        }
    }
}

/// The callback and promises defaults when setServers replaced them; `None` is
/// "still the system channel".
static CALLBACK_DEFAULT: RwLock<Option<Arc<Channel>>> = RwLock::new(None);
static PROMISES_DEFAULT: RwLock<Option<Arc<Channel>>> = RwLock::new(None);

/// The channel a call through `api` goes to. A query holds its own `Arc`, so a
/// setServers() during it changes the NEXT query, never one in flight.
fn channel(api: Api) -> Arc<Channel> {
    let slot = match api {
        Api::Callback => &CALLBACK_DEFAULT,
        Api::Promises => &PROMISES_DEFAULT,
        Api::Resolver => return system_channel(),
    };
    let current = slot.read().unwrap_or_else(|e| e.into_inner()).clone();
    current.unwrap_or_else(system_channel)
}

/// `getServers()` through `api`: the channel's nameservers in node's string
/// format and config order.
///
/// The system list is taken from the same `ResolverConfig` the system channel
/// is built from, so what is reported is by construction what is queried.
pub fn dns_get_servers(api: Api) -> Vec<String> {
    channel(api).display.clone()
}

/// `setServers()` through `api`: replace the default channel with `servers`
/// (address, port), already validated and parsed by the JS side as node's
/// ResolverBase.setServers does. Through the callback API that is the default
/// of both APIs; through dns.promises, of dns.promises alone. An empty list is
/// accepted, as node accepts it; every query then fails (`no_servers`).
///
/// `Api::Resolver` is not a target: per-instance setServers is refused in JS.
pub fn dns_set_servers(api: Api, servers: &[(IpAddr, u16)]) {
    let name_servers = servers
        .iter()
        .map(|&(ip, port)| {
            let mut udp = ConnectionConfig::udp();
            udp.port = port;
            let mut tcp = ConnectionConfig::tcp();
            tcp.port = port;
            NameServerConfig::new(ip, true, vec![udp, tcp])
        })
        .collect();
    let channel = Arc::new(build_channel(base(), name_servers));
    let set = |slot: &RwLock<Option<Arc<Channel>>>| {
        *slot.write().unwrap_or_else(|e| e.into_inner()) = Some(channel.clone());
    };
    match api {
        Api::Callback => {
            set(&CALLBACK_DEFAULT);
            set(&PROMISES_DEFAULT);
        }
        Api::Promises => set(&PROMISES_DEFAULT),
        Api::Resolver => {}
    }
}

/// True when `e` means the server did not answer at all, so c-ares would try
/// the next one. Any DNS response -- an answer, NODATA, NXDOMAIN, SERVFAIL,
/// REFUSED -- is final (node's ARES_FLAG_NOCHECKRESP).
fn is_no_answer(e: &NetError) -> bool {
    matches!(
        e,
        NetError::Timeout | NetError::Io(_) | NetError::NoConnections | NetError::Busy
    )
}

/// The order to try `failures` in: fewest consecutive failures first, then
/// configuration order -- c-ares's server sort without ARES_FLAG_ROTATE.
fn try_order(failures: &[u32]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..failures.len()).collect();
    order.sort_by_key(|&i| (failures[i], i));
    order
}

/// Sends one query over `channel` the way c-ares does: one server at a time,
/// best first, moving on only when a server does not answer, for
/// `attempts + 1` rounds. The caller has checked the channel is not empty.
async fn walk<T, F, Fut>(channel: &Channel, mut send: F) -> Result<T, NetError>
where
    F: FnMut(TokioResolver) -> Fut,
    Fut: Future<Output = Result<T, NetError>>,
{
    let mut last = NetError::NoConnections;
    for _round in 0..=channel.attempts {
        let failures: Vec<u32> = channel
            .servers
            .iter()
            .map(|s| s.failures.load(Ordering::Relaxed))
            .collect();
        for i in try_order(&failures) {
            let server = &channel.servers[i];
            match send(server.resolver.clone()).await {
                Err(e) if is_no_answer(&e) => {
                    server.failures.fetch_add(1, Ordering::Relaxed);
                    last = e;
                }
                result => {
                    server.failures.store(0, Ordering::Relaxed);
                    return result;
                }
            }
        }
    }
    Err(last)
}

/// One `RecordType` query of `name` over `channel`.
async fn channel_lookup(
    channel: &Channel,
    name: &str,
    rtype: RecordType,
) -> Result<hickory_resolver::lookup::Lookup, NetError> {
    walk(channel, |r| {
        let name = name.to_owned();
        async move { r.lookup(name.as_str(), rtype).await }
    })
    .await
}

/// A query on a channel with no nameservers (`dns.setServers([])`). c-ares
/// fails it with ARES_ENOSERVER, a status node's error table predates, so node
/// v22.22.2 reports the code `UNKNOWN_ARES_ERROR` (measured: "queryA
/// UNKNOWN_ARES_ERROR x.example").
fn no_servers(query: &str) -> OpOutcome {
    OpOutcome::node_failed(
        "UNKNOWN_ARES_ERROR",
        format!("UNKNOWN_ARES_ERROR {query}: no nameservers configured"),
    )
}

/// A DNS name as node reports it: without the root label's trailing dot.
///
/// hickory's `Display`/`to_string` emits a fully-qualified name with the final
/// `.` (`dns.google.`); node strips it on every field that carries a name --
/// verified against node v22.22.2 for reverse, PTR, NS, CNAME, MX.exchange,
/// SOA.nsname, SOA.hostmaster and NAPTR.replacement. The ROOT name is `.` and
/// must stay `.` rather than becoming an empty string.
fn host(name: &hickory_resolver::proto::rr::Name) -> String {
    // to_ascii, NOT to_string/to_utf8: Display un-punycodes IDNA labels, while
    // c-ares hands node the wire bytes. Verified against node v22.22.2 --
    // resolveNs("xn--p1acf") gives node "ns1.nic.xn--p1acf" where Display gave
    // the decoded Cyrillic. Fixing only the dot would leave that mismatch.
    let s = name.to_ascii();
    // strip_suffix removes exactly ONE dot; trim_end_matches would eat an
    // escaped trailing sequence. The ROOT name is "." and correctly becomes ""
    // -- c-ares does the same, so an RFC 7505 null MX reports exchange:"" on
    // both runtimes (probe-verified against example.com).
    s.strip_suffix('.').map(str::to_owned).unwrap_or(s)
}

/// dns.lookup: getaddrinfo of `name`, the UTS #46 ToASCII form of `hostname`
/// node's GetAddrInfo hands libuv; errors name `hostname` as written. An
/// empty `name` is libuv's EINVAL (see `net_connect::resolve_as`).
///
/// `flags` are node's `hints` as getaddrinfo takes them (AI_ADDRCONFIG,
/// AI_V4MAPPED, AI_ALL; the platform's values), handed to the platform
/// resolver with the family and SOCK_STREAM, as libuv's uv_getaddrinfo hands
/// node's (`net_connect::getaddrinfo`, the resolver a connect uses too). The
/// answer is then the resolver's own, duplicates included: glibc answers a
/// name with `::1` and `127.0.0.1` lines in /etc/hosts under AI_V4MAPPED |
/// AI_ALL with the mapped `::ffff:127.0.0.1` twice, and node reports both
/// (the 0.18.0 linux release leg, conformance case 228). Emulating the flags
/// over an unflagged lookup answered it once. A failure is node's
/// `DNSException` for the resolver's own code (`getaddrinfo EAI_AGAIN host`
/// for a temporary failure, not `ENOTFOUND` for everything).
pub async fn dns_lookup(
    hostname: String,
    name: String,
    family: i32,
    all: bool,
    flags: i32,
) -> OpOutcome {
    if name.is_empty() {
        return OpOutcome::sys(crate::net_connect::empty_name_error(&hostname));
    }
    let family_hint = match family {
        4 => Some(4),
        6 => Some(6),
        _ => None,
    };
    let addrs: Vec<std::net::SocketAddr> =
        match crate::net_connect::getaddrinfo(&name, 0, family_hint, flags).await {
            Ok(addrs) => addrs,
            Err(error) => {
                return OpOutcome::sys(crate::net_connect::resolve_error(&hostname, &error));
            }
        };

    if addrs.is_empty() {
        return OpOutcome::node_failed("ENOTFOUND", format!("getaddrinfo ENOTFOUND {hostname}"));
    }

    let filtered: Vec<_> = addrs
        .iter()
        .filter(|a| match family {
            4 => matches!(a.ip(), IpAddr::V4(_)),
            6 => matches!(a.ip(), IpAddr::V6(_)),
            _ => true,
        })
        .collect();

    if filtered.is_empty() {
        return OpOutcome::node_failed("ENOTFOUND", format!("getaddrinfo ENOTFOUND {hostname}"));
    }

    if all {
        let results: Vec<serde_json::Value> = filtered
            .iter()
            .map(|a| {
                let fam = if a.ip().is_ipv4() { 4 } else { 6 };
                serde_json::json!({ "address": a.ip().to_string(), "family": fam })
            })
            .collect();
        OpOutcome::Json(serde_json::json!(results).to_string())
    } else {
        let addr = filtered[0];
        let fam = if addr.ip().is_ipv4() { 4 } else { 6 };
        OpOutcome::Json(
            serde_json::json!({ "address": addr.ip().to_string(), "family": fam }).to_string(),
        )
    }
}

/// dns.resolve(hostname, rrtype) -- query specific DNS record types.
///
/// `api` picks the channel (see `channel`).
pub async fn dns_resolve(hostname: String, rrtype: String, api: Api) -> OpOutcome {
    let ch = channel(api);
    if ch.servers.is_empty() {
        return no_servers(&hostname);
    }
    // Query names with enough dots ABSOLUTELY, so a NODATA answer is final.
    //
    // c-ares (and therefore node) treats a name carrying at least `ndots`
    // dots -- 1 by default -- as already qualified: it asks once and reports
    // that answer, NODATA included. hickory instead walks the search list on
    // any "no records" outcome (its own docs, resolver.rs: "Anything else will
    // always incur the cost of querying the ResolverConfig::domain and
    // ResolverConfig::search"), so the LAST attempt decides the error.
    //
    // That is invisible on a host with no search domains and wrong on one with
    // them: on a Mac with `search tailddd0f3.ts.net lan`, resolveTxt of a name
    // that exists but has no TXT returned NOERROR/no-answers, hickory then
    // tried `<name>.tailddd0f3.ts.net` and `<name>.lan`, and the NXDOMAIN from
    // those bogus names surfaced as ENOTFOUND where node reports ENODATA.
    // Appending the root dot makes hickory issue exactly one query.
    //
    // A name with FEWER than ndots dots (a bare `myhost`) is left alone, so the
    // search list still does its job for the case it exists for.
    let ndots = ch.ndots;
    let hostname = if hostname.ends_with('.') || hostname.matches('.').count() < ndots {
        hostname
    } else {
        format!("{hostname}.")
    };
    match rrtype.as_str() {
        "A" => match channel_lookup(&ch, &hostname, RecordType::A).await {
            Ok(lookup) => {
                let addrs: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::A(a) => Some(serde_json::json!(a.0.to_string())),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(addrs).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "AAAA" => match channel_lookup(&ch, &hostname, RecordType::AAAA).await {
            Ok(lookup) => {
                let addrs: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::AAAA(a) => Some(serde_json::json!(a.0.to_string())),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(addrs).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "CNAME" => match channel_lookup(&ch, &hostname, RecordType::CNAME).await {
            Ok(lookup) => {
                let names: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::CNAME(c) => Some(serde_json::json!(host(&c.0))),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "MX" => match channel_lookup(&ch, &hostname, RecordType::MX).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        // Key order is JS property-insertion order now that
                        // serde_json preserves it: node (c-ares) inserts
                        // exchange before priority, and the differential
                        // compares JSON.stringify byte-for-byte.
                        RData::MX(mx) => Some(serde_json::json!({
                            "exchange": host(&mx.exchange),
                            "priority": mx.preference,
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "TXT" => match channel_lookup(&ch, &hostname, RecordType::TXT).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::TXT(txt) => {
                            let parts: Vec<String> = txt
                                .txt_data
                                .iter()
                                .map(|b| String::from_utf8_lossy(b).into_owned())
                                .collect();
                            Some(serde_json::json!(parts))
                        }
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "NS" => match channel_lookup(&ch, &hostname, RecordType::NS).await {
            Ok(lookup) => {
                let names: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::NS(ns) => Some(serde_json::json!(host(&ns.0))),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "SRV" => match channel_lookup(&ch, &hostname, RecordType::SRV).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        // node's insertion order (verified live v22.22.2:
                        // _matrix._tcp.matrix.org): name, port, priority, weight.
                        RData::SRV(srv) => Some(serde_json::json!({
                            "name": host(&srv.target),
                            "port": srv.port,
                            "priority": srv.priority,
                            "weight": srv.weight,
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "SOA" => match channel_lookup(&ch, &hostname, RecordType::SOA).await {
            Ok(lookup) => {
                if let Some(soa) = lookup.answers().iter().find_map(|rec| match &rec.data {
                    RData::SOA(soa) => Some(soa),
                    _ => None,
                }) {
                    OpOutcome::Json(
                        serde_json::json!({
                            "nsname": host(&soa.mname),
                            "hostmaster": host(&soa.rname),
                            "serial": soa.serial,
                            "refresh": soa.refresh,
                            "retry": soa.retry,
                            "expire": soa.expire,
                            "minttl": soa.minimum,
                        })
                        .to_string(),
                    )
                } else {
                    OpOutcome::Json("null".to_string())
                }
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "PTR" => match channel_lookup(&ch, &hostname, RecordType::PTR).await {
            Ok(lookup) => {
                let names: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::PTR(p) => Some(serde_json::json!(host(&p.0))),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "CAA" => match channel_lookup(&ch, &hostname, RecordType::CAA).await {
            Ok(lookup) => {
                // node's shape is `{critical, <tag>: <value>}` -- the TAG is the
                // KEY. This used to emit `{critical, issue: <tag>, value: <value>}`,
                // which put the tag name where the value belongs, hardcoded the
                // key as `issue` no matter the real tag, and added a `value` key
                // node does not have. Measured against node v22.22.2:
                // wikipedia.org returns an `iodef` record and two `issue`
                // records, each keyed by its own tag.
                //
                // Built as text rather than through json!/Map because
                // serde_json's default Map is a BTreeMap and would sort the keys
                // -- node always puts `critical` first, and RFC 9495's
                // `contactemail` / `contactphone` tags sort BEFORE it.
                let objects: Vec<String> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::CAA(caa) => {
                            // The wire flags octet, which is what node reports --
                            // not a 0/1 bool. RFC 6844 puts Issuer Critical in bit
                            // 0 (0x80), so a critical record reads 128, matching
                            // node's own documented example. hickory splits the
                            // octet into the bool plus the remaining bits, so this
                            // reassembles it rather than inventing a value.
                            let flags =
                                if caa.issuer_critical { 0x80u8 } else { 0 } | caa.reserved_flags;
                            let tag = serde_json::json!(caa.tag.as_str());
                            let value =
                                serde_json::json!(String::from_utf8_lossy(&caa.value).into_owned());
                            Some(format!("{{\"critical\":{flags},{tag}:{value}}}"))
                        }
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(format!("[{}]", objects.join(",")))
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "NAPTR" => match channel_lookup(&ch, &hostname, RecordType::NAPTR).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        // node's insertion order (cares_wrap.cc ParseNaptrReply):
                        // flags, service, regexp, replacement, order, preference.
                        RData::NAPTR(naptr) => Some(serde_json::json!({
                            "flags": String::from_utf8_lossy(&naptr.flags).into_owned(),
                            "service": String::from_utf8_lossy(&naptr.services).into_owned(),
                            "regexp": String::from_utf8_lossy(&naptr.regexp).into_owned(),
                            "replacement": host(&naptr.replacement),
                            "order": naptr.order,
                            "preference": naptr.preference,
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        other => OpOutcome::Failed(format!("dns.resolve: unsupported rrtype '{other}'")),
    }
}

/// dns.reverse(ip) -- PTR lookup for an IP address.
///
/// Node's reverse is c-ares's ares_gethostbyaddr, not a PTR query, and that
/// reports EVERY failed lookup as ENOTFOUND: when the DNS step fails it falls
/// back to the hosts file, and a miss there is ARES_ENOTFOUND whatever the DNS
/// step said. Measured on node v22.22.2 against fake servers: a NOERROR answer
/// with no PTR (NODATA), NXDOMAIN, SERVFAIL, REFUSED, a server that never
/// answers, a closed port and an empty server list are all `getHostByAddr
/// ENOTFOUND <ip>`. resolvePtr() is a plain query and keeps the split
/// (NODATA is ENODATA there), so the mapping is reverse()'s alone.
pub async fn dns_reverse(ip_str: String, api: Api) -> OpOutcome {
    let ip: IpAddr = match ip_str.parse() {
        Ok(ip) => ip,
        Err(_) => {
            return OpOutcome::node_failed("EINVAL", format!("reverse EINVAL {ip_str}"));
        }
    };
    let not_found = |why: &dyn std::fmt::Display| {
        OpOutcome::node_failed("ENOTFOUND", format!("ENOTFOUND {ip_str}: {why}"))
    };

    let ch = channel(api);
    if ch.servers.is_empty() {
        return not_found(&"no nameservers configured");
    }
    match walk(&ch, |r| async move { r.reverse_lookup(ip).await }).await {
        Ok(lookup) => {
            let names: Vec<serde_json::Value> = lookup
                .answers()
                .iter()
                .filter_map(|rec| match &rec.data {
                    RData::PTR(p) => Some(serde_json::json!(host(&p.0))),
                    _ => None,
                })
                .collect();
            if names.is_empty() {
                not_found(&"no PTR records")
            } else {
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
        }
        Err(e) => not_found(&e),
    }
}

/// The c-ares status node reports for a failed query, as its code string.
///
/// Measured on node v22.22.2 (c-ares 1.34.6) against fake servers: an answer's
/// rcode decides it -- NXDOMAIN is ENOTFOUND, NOERROR with no record of the
/// type is ENODATA, SERVFAIL ESERVFAIL, REFUSED EREFUSED, NOTIMP ENOTIMP,
/// FORMERR EFORMERR, and any other rcode (YXDOMAIN, ...) leaves c-ares's
/// status at success with nothing to parse, which node reports as ENODATA.
/// No answer at all is ETIMEOUT, and a port that refuses ECONNREFUSED.
fn query_error_code(e: &NetError) -> &'static str {
    use hickory_resolver::net::DnsError;
    use hickory_resolver::proto::op::ResponseCode;
    // "no records" is TWO different node errors, and collapsing them to
    // ENOTFOUND made a name that exists look like one that does not.
    // hickory's NoRecords carries the response code and documents the split:
    // NXDomain means the name itself does not exist, NoError means it does but
    // carries no record of the requested type.
    let rcode = match e {
        NetError::Dns(DnsError::NoRecordsFound(no_records)) => no_records.response_code,
        NetError::Dns(DnsError::ResponseCode(code)) => *code,
        NetError::Timeout => return "ETIMEOUT",
        NetError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return "ECONNREFUSED";
        }
        _ => return "ESERVFAIL",
    };
    match rcode {
        ResponseCode::NXDomain => "ENOTFOUND",
        ResponseCode::ServFail => "ESERVFAIL",
        ResponseCode::Refused => "EREFUSED",
        ResponseCode::NotImp => "ENOTIMP",
        ResponseCode::FormErr => "EFORMERR",
        _ => "ENODATA",
    }
}

fn dns_err(query: &str, e: &NetError) -> OpOutcome {
    let code = query_error_code(e);
    OpOutcome::node_failed(code.to_string(), format!("{code} {query}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    #[test]
    fn servers_format_as_node_reports_them() {
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(format_server(v4, 53), "192.0.2.1");
        assert_eq!(format_server(v4, 5353), "192.0.2.1:5353");
        // IPv6 is bracketed only when a port follows.
        assert_eq!(format_server(v6, 53), "2001:db8::1");
        assert_eq!(format_server(v6, 5353), "[2001:db8::1]:5353");
    }

    #[test]
    fn try_order_is_fewest_failures_then_config_order() {
        assert_eq!(try_order(&[0, 0, 0]), vec![0, 1, 2]);
        assert_eq!(try_order(&[1, 0, 0]), vec![1, 2, 0]);
        assert_eq!(try_order(&[2, 1, 0]), vec![2, 1, 0]);
        assert_eq!(try_order(&[1, 1, 0]), vec![2, 0, 1]);
        assert!(try_order(&[]).is_empty());
    }

    #[test]
    fn only_a_missing_answer_moves_to_the_next_server() {
        use hickory_resolver::net::DnsError;
        use hickory_resolver::proto::op::ResponseCode;
        assert!(is_no_answer(&NetError::Timeout));
        assert!(is_no_answer(&NetError::NoConnections));
        assert!(is_no_answer(&NetError::Busy));
        assert!(is_no_answer(&NetError::Io(Arc::new(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        )))));
        // ARES_FLAG_NOCHECKRESP: an answer is final, whatever its rcode.
        assert!(!is_no_answer(&NetError::Dns(DnsError::ResponseCode(
            ResponseCode::ServFail
        ))));
        assert!(!is_no_answer(&NetError::Dns(DnsError::ResponseCode(
            ResponseCode::Refused
        ))));
    }

    #[test]
    fn query_errors_take_c_ares_codes() {
        use hickory_resolver::net::DnsError;
        use hickory_resolver::proto::op::ResponseCode;
        let rcode = |code| query_error_code(&NetError::Dns(DnsError::ResponseCode(code)));
        assert_eq!(rcode(ResponseCode::ServFail), "ESERVFAIL");
        assert_eq!(rcode(ResponseCode::Refused), "EREFUSED");
        assert_eq!(rcode(ResponseCode::NotImp), "ENOTIMP");
        assert_eq!(rcode(ResponseCode::FormErr), "EFORMERR");
        // Measured: node reports an unlisted rcode (YXDOMAIN) as ENODATA.
        assert_eq!(rcode(ResponseCode::YXDomain), "ENODATA");
        assert_eq!(query_error_code(&NetError::Timeout), "ETIMEOUT");
        assert_eq!(
            query_error_code(&NetError::Io(Arc::new(std::io::Error::from(
                std::io::ErrorKind::ConnectionRefused
            )))),
            "ECONNREFUSED"
        );
    }

    // ---- fake nameservers -------------------------------------------------

    #[derive(Clone, Copy)]
    enum Reply {
        /// An A record (a PTR for a PTR query) after `delay_ms`.
        Answer { octet: u8, delay_ms: u64 },
        /// A response with this rcode and no records.
        Empty(u8),
        /// Never answer.
        Silent,
    }

    struct Fake {
        port: u16,
        asked: Arc<AtomicUsize>,
    }

    fn encode_name(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    fn respond(query: &[u8], reply: Reply) -> Vec<u8> {
        // The question ends after the name's root label, QTYPE and QCLASS.
        let mut off = 12;
        while query[off] != 0 {
            off += query[off] as usize + 1;
        }
        let qtype = u16::from_be_bytes([query[off + 1], query[off + 2]]);
        let qend = off + 5;
        let (rcode, answer) = match reply {
            Reply::Answer { octet, .. } if qtype == 1 => (0u8, Some((1u16, vec![10, 0, 0, octet]))),
            Reply::Answer { .. } if qtype == 12 => (0, Some((12, encode_name("host.example")))),
            Reply::Answer { .. } => (0, None),
            Reply::Empty(rcode) => (rcode, None),
            Reply::Silent => unreachable!("a silent server never responds"),
        };
        let mut out = Vec::new();
        out.extend_from_slice(&query[0..2]);
        out.extend_from_slice(&(0x8180u16 | u16::from(rcode)).to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&u16::from(answer.is_some()).to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&query[12..qend]);
        if let Some((rtype, rdata)) = answer {
            out.extend_from_slice(&[0xc0, 0x0c]);
            out.extend_from_slice(&rtype.to_be_bytes());
            out.extend_from_slice(&1u16.to_be_bytes());
            out.extend_from_slice(&60u32.to_be_bytes());
            out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            out.extend_from_slice(&rdata);
        }
        out
    }

    async fn fake(reply: Reply) -> Fake {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let port = socket.local_addr().unwrap().port();
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                counter.fetch_add(1, Ordering::SeqCst);
                if let Reply::Silent = reply {
                    continue;
                }
                let out = respond(&buf[..n], reply);
                let socket = socket.clone();
                tokio::spawn(async move {
                    if let Reply::Answer { delay_ms, .. } = reply {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    let _ = socket.send_to(&out, from).await;
                });
            }
        });
        Fake { port, asked }
    }

    fn test_channel(fakes: &[&Fake], attempts: usize) -> Channel {
        let mut options = ResolverOpts::default();
        options.timeout = Duration::from_millis(300);
        options.attempts = attempts;
        let base = Base {
            domain: None,
            search: Vec::new(),
            options,
            name_servers: Vec::new(),
        };
        let servers = fakes
            .iter()
            .map(|f| {
                let mut udp = ConnectionConfig::udp();
                udp.port = f.port;
                let mut tcp = ConnectionConfig::tcp();
                tcp.port = f.port;
                NameServerConfig::new("127.0.0.1".parse().unwrap(), true, vec![udp, tcp])
            })
            .collect();
        build_channel(&base, servers)
    }

    fn first_a(lookup: &hickory_resolver::lookup::Lookup) -> String {
        lookup
            .answers()
            .iter()
            .find_map(|r| match &r.data {
                RData::A(a) => Some(a.0.to_string()),
                _ => None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn the_first_server_is_asked_alone_even_when_a_later_one_is_faster() {
        // The shape behind the bug: a correct first server that takes 50 ms
        // and a broken second one that answers NOERROR-empty at once. Racing
        // them took the broken answer; c-ares asks the first server only.
        let good = fake(Reply::Answer {
            octet: 1,
            delay_ms: 50,
        })
        .await;
        let broken = fake(Reply::Empty(0)).await;
        let ch = test_channel(&[&good, &broken], 1);
        let lookup = channel_lookup(&ch, "a.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(first_a(&lookup), "10.0.0.1");
        assert_eq!(broken.asked.load(Ordering::SeqCst), 0);

        // In the other order the broken server's NODATA is the answer.
        let ch = test_channel(&[&broken, &good], 1);
        let err = channel_lookup(&ch, "b.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "ENODATA");
        assert_eq!(
            good.asked.load(Ordering::SeqCst),
            1,
            "only the first lookup reached it"
        );
    }

    #[tokio::test]
    async fn a_servfail_or_refused_answer_is_final() {
        let good = fake(Reply::Answer {
            octet: 1,
            delay_ms: 0,
        })
        .await;
        for (rcode, code) in [(2u8, "ESERVFAIL"), (5, "EREFUSED")] {
            let failing = fake(Reply::Empty(rcode)).await;
            let ch = test_channel(&[&failing, &good], 1);
            let err = channel_lookup(&ch, "c.example.", RecordType::A)
                .await
                .unwrap_err();
            assert_eq!(query_error_code(&err), code);
            // One query: no retry of the same server and no failover.
            assert_eq!(failing.asked.load(Ordering::SeqCst), 1);
        }
        assert_eq!(good.asked.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_silent_server_fails_over_and_is_demoted() {
        let dead = fake(Reply::Silent).await;
        let live = fake(Reply::Answer {
            octet: 2,
            delay_ms: 0,
        })
        .await;
        let ch = test_channel(&[&dead, &live], 0);
        let lookup = channel_lookup(&ch, "d.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(first_a(&lookup), "10.0.0.2");
        let dead_after_first = dead.asked.load(Ordering::SeqCst);
        assert!(dead_after_first >= 1);
        assert_eq!(ch.servers[0].failures.load(Ordering::SeqCst), 1);

        // The next query goes straight to the live server.
        let lookup = channel_lookup(&ch, "e.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(first_a(&lookup), "10.0.0.2");
        assert_eq!(dead.asked.load(Ordering::SeqCst), dead_after_first);
        assert_eq!(live.asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn every_server_silent_is_a_timeout_after_every_round() {
        let a = fake(Reply::Silent).await;
        let b = fake(Reply::Silent).await;
        // attempts = 1: two rounds over both servers.
        let ch = test_channel(&[&a, &b], 1);
        let err = channel_lookup(&ch, "f.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "ETIMEOUT");
        assert_eq!(ch.servers[0].failures.load(Ordering::SeqCst), 2);
        assert_eq!(ch.servers[1].failures.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reverse_reports_nodata_as_enotfound_where_resolve_ptr_keeps_enodata() {
        // Through the promises slot, which no other test touches.
        let nodata = fake(Reply::Empty(0)).await;
        dns_set_servers(
            Api::Promises,
            &[("127.0.0.1".parse().unwrap(), nodata.port)],
        );
        assert_eq!(
            dns_get_servers(Api::Promises),
            vec![format!("127.0.0.1:{}", nodata.port)]
        );
        let code = |outcome: OpOutcome| match outcome {
            OpOutcome::NodeFailed { code, .. } => code,
            _ => "not a NodeFailed".to_string(),
        };
        assert_eq!(
            code(dns_reverse("192.0.2.5".into(), Api::Promises).await),
            "ENOTFOUND"
        );
        assert_eq!(
            code(dns_resolve("5.2.0.192.in-addr.arpa".into(), "PTR".into(), Api::Promises).await),
            "ENODATA"
        );

        // An empty list: a query is c-ares's ENOSERVER, a reverse ENOTFOUND.
        dns_set_servers(Api::Promises, &[]);
        assert!(dns_get_servers(Api::Promises).is_empty());
        assert_eq!(
            code(dns_reverse("192.0.2.5".into(), Api::Promises).await),
            "ENOTFOUND"
        );
        assert_eq!(
            code(dns_resolve("g.example".into(), "A".into(), Api::Promises).await),
            "UNKNOWN_ARES_ERROR"
        );
    }
}
