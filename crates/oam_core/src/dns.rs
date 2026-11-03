use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ProtocolConfig, ResolverConfig, ResolverOpts,
    ServerOrderingStrategy,
};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::udp::UdpClientStream;
use hickory_resolver::net::xfer::DnsExchange;
use hickory_resolver::proto::rr::{Name, RData, RecordType};
use hickory_resolver::{ConnectionProvider, PoolContext, Resolver};

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
// * every `new dns.Resolver()` / `new dns.promises.Resolver()` gets its own
//   channel (`Api::Instance`), built from the SYSTEM configuration whatever
//   setServers did to the defaults, with its own failure counts, its own
//   timing (the constructor's `timeout` / `tries` / `maxTimeout`), its own
//   setServers() and setLocalAddress(), and a cancel() that fails only its own
//   pending queries (measured: an instance's setServers reaches neither the
//   defaults nor any other instance).
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
// The try budget and the per-server timeout are c-ares's as node configures
// it (`CaresTiming`): node always passes ARES_OPT_TRIES = 4, so a query runs
// four rounds, one round being every server once, and each send waits c-ares's
// server timeout -- 2 s, or resolv.conf's / RES_OPTIONS' `timeout:` -- doubled
// per round (`round_timeout`). hickory's own timeout (5 s, glibc's default)
// and attempts no longer decide anything.

/// hickory's connections, except that a UDP query is ONE datagram. hickory's
/// UdpClientStream resends an unanswered query on a timer (every
/// max(1.2 x SRTT, 333 ms), up to three copies) inside a single request; c-ares
/// sends once per try and the try's timeout is the only resend (measured on
/// node v22.22.2: `{timeout: 100, tries: 2}` against a silent server is asked
/// exactly twice, where oam asked it three times). The walk already owns the
/// tries, so the stream's own retransmission is turned off here; TCP and the
/// rest are hickory's unchanged.
#[derive(Clone, Default)]
struct CaresProvider(TokioRuntimeProvider);

impl ConnectionProvider for CaresProvider {
    type Conn = <TokioRuntimeProvider as ConnectionProvider>::Conn;
    type FutureConn = <TokioRuntimeProvider as ConnectionProvider>::FutureConn;
    type RuntimeProvider = TokioRuntimeProvider;

    fn new_connection(
        &self,
        ip: IpAddr,
        config: &ConnectionConfig,
        cx: &PoolContext,
    ) -> Result<Self::FutureConn, NetError> {
        if !matches!(config.protocol, ProtocolConfig::Udp) {
            return self.0.new_connection(ip, config, cx);
        }
        // hickory's own UDP arm (connection_provider.rs), plus max_retries 1.
        let stream = UdpClientStream::builder(SocketAddr::new(ip, config.port), self.0.clone())
            .with_timeout(Some(cx.options.timeout))
            .with_os_port_selection(cx.options.os_port_selection)
            .avoid_local_ports(cx.options.avoid_local_udp_ports.clone())
            .with_bind_addr(config.bind_addr)
            .with_max_retries(1);
        Ok(Box::pin(async move {
            let exchange: DnsExchange<TokioRuntimeProvider> = stream.exchange();
            Ok(exchange)
        }))
    }

    fn runtime_provider(&self) -> &Self::RuntimeProvider {
        &self.0
    }
}

/// The single-server resolver each channel server is.
type ServerResolver = Resolver<CaresProvider>;

/// One nameserver of a channel.
struct Server {
    resolver: ServerResolver,
    /// Consecutive failures to answer (timeouts, network errors); any answer
    /// resets it. The walk tries the server with the fewest first.
    failures: AtomicU32,
}

struct Channel {
    servers: Vec<Server>,
    /// What `getServers()` reports, in node's string form and config order.
    display: Vec<String>,
    timing: CaresTiming,
    ndots: usize,
}

/// The try budget and server timeout of a channel, as c-ares 1.34.6 runs them
/// under node v22.22.2.
///
/// Measured against silent fake servers on 127.0.0.1 (node polls c-ares's
/// timeouts on a 1 s timer, so every measured wait is the real one rounded up
/// to that tick):
///
/// * tries: a lone silent server is asked 4 times before ETIMEOUT, and two
///   silent servers 4 times each. node's ChannelWrap always passes
///   ARES_OPT_TRIES (its `tries` option, default 4), and an explicit option
///   beats the system one, so `attempts:` in resolv.conf or RES_OPTIONS
///   changes nothing (measured: RES_OPTIONS="attempts:1" still asks 4 times).
/// * timeout: [silent, live] fails over after 2.9 s (a 2 s timeout); with
///   RES_OPTIONS="timeout:4" after 4.9 s, with "timeout:1" after 1.9 s, and
///   with "timeout:7" after 5.9 s -- c-ares clamps the server timeout to
///   250 ms..5 s when no maxTimeout is set (node sets none).
/// * doubling: round `r` waits `timeout << r`, cut by a random jitter to
///   between half and all of that, never below `timeout` (measured waits on
///   one silent server: 2.9, 3.0, 5.0, 11.1 s against nominal 2, 4, 8, 16).
///
/// Not modelled: c-ares also shortens a server's timeout from the latency it
/// measured on earlier answers (ares_metrics), so a server that answered fast
/// and then goes silent can fail over sooner in node than in oam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaresTiming {
    /// The round-0 wait for one server.
    timeout: Duration,
    /// Rounds over the whole server list.
    tries: usize,
    /// A Resolver's `maxTimeout` (c-ares ARES_OPT_MAXTIMEOUTMS): no single
    /// wait is longer. `None` for every channel built without one.
    max: Option<Duration>,
}

impl CaresTiming {
    /// c-ares's DEFAULT_TIMEOUT.
    const DEFAULT_TIMEOUT: Duration = Duration::from_millis(2000);
    /// node's ChannelWrap default `tries`, always passed as ARES_OPT_TRIES.
    const NODE_TRIES: usize = 4;
    /// The bounds c-ares puts on a server's timeout when the channel has no
    /// maxTimeout (measured upper bound; the lower one is c-ares's 250 ms).
    const MIN_TIMEOUT: Duration = Duration::from_millis(250);
    const MAX_TIMEOUT: Duration = Duration::from_millis(5000);

    /// The timing for a system whose `timeout:` option (seconds) is
    /// `timeout_secs`; `None` or 0 is "not set", as c-ares treats a zero.
    fn from_timeout_option(timeout_secs: Option<u64>) -> CaresTiming {
        let timeout = match timeout_secs {
            Some(secs) if secs > 0 => Duration::from_secs(secs),
            _ => Self::DEFAULT_TIMEOUT,
        };
        CaresTiming {
            timeout: timeout.clamp(Self::MIN_TIMEOUT, Self::MAX_TIMEOUT),
            tries: Self::NODE_TRIES,
            max: None,
        }
    }

    /// A `new dns.Resolver(options)` channel's timing, from the options as the
    /// JS side validated them (node's ResolverBase): `timeout_ms` in
    /// -1..=i32::MAX, where -1 (the default) or 0 keeps the system's server
    /// timeout (measured: both wait c-ares's 2 s); `tries` >= 1; and
    /// `max_timeout_ms` 0 for none.
    ///
    /// A set timeout gets the same 250 ms floor c-ares puts on every server
    /// timeout (measured: `{timeout: 50, tries: 1}` fails after about 270 ms,
    /// `{timeout: 100}` after about 305). Without a maxTimeout the 5 s ceiling
    /// applies as for the system channel; with one, the ceiling is the
    /// maxTimeout instead and every round's wait is cut to it.
    fn for_resolver(
        system: CaresTiming,
        timeout_ms: i32,
        tries: u32,
        max_timeout_ms: u32,
    ) -> CaresTiming {
        let max = (max_timeout_ms > 0).then(|| Duration::from_millis(u64::from(max_timeout_ms)));
        let timeout = match u64::try_from(timeout_ms) {
            Ok(ms) if ms > 0 => {
                let ceiling = max.map_or(Self::MAX_TIMEOUT, |m| m.max(Self::MIN_TIMEOUT));
                Duration::from_millis(ms).clamp(Self::MIN_TIMEOUT, ceiling)
            }
            _ => system.timeout,
        };
        CaresTiming {
            timeout,
            tries: usize::try_from(tries.max(1)).unwrap_or(Self::NODE_TRIES),
            max,
        }
    }

    /// How long round `round` waits for one server: `round_timeout`, cut to
    /// the maxTimeout when there is one.
    fn wait(&self, round: usize, jitter: u32) -> Duration {
        let wait = round_timeout(self.timeout, round, jitter);
        self.max.map_or(wait, |max| wait.min(max))
    }

    /// The system's timing: the last `timeout:` among resolv.conf's `options`
    /// lines (not on Windows, which has none), overridden by one in the
    /// RES_OPTIONS environment variable, which c-ares reads on every platform
    /// (measured on Windows).
    fn system() -> CaresTiming {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut timeout = None;
        #[cfg(unix)]
        if let Ok(conf) = std::fs::read_to_string("/etc/resolv.conf") {
            for line in conf.lines() {
                let mut words = line.split_whitespace();
                if words.next() == Some("options") {
                    timeout = timeout_option(words).or(timeout);
                }
            }
        }
        if let Ok(env) = std::env::var("RES_OPTIONS") {
            timeout = timeout_option(env.split_whitespace()).or(timeout);
        }
        Self::from_timeout_option(timeout)
    }

    /// The longest single wait: the last round's, before jitter.
    fn longest(&self) -> Duration {
        self.wait(self.tries.saturating_sub(1), 0)
    }
}

/// The last `timeout:N` among `options` words; an unparseable value is
/// skipped.
fn timeout_option<'a>(options: impl Iterator<Item = &'a str>) -> Option<u64> {
    options
        .filter_map(|o| o.strip_prefix("timeout:"))
        .filter_map(|v| v.parse::<u64>().ok())
        .last()
}

/// How long round `round` (0-based) waits for one server, as c-ares's
/// ares_calc_query_timeout computes it: round 0 waits `timeout`; a later round
/// doubles it per round and then takes off a jitter of up to half -- `jitter`
/// in 0..=1000 is that cut in thousandths of the half -- and never waits less
/// than `timeout`.
fn round_timeout(timeout: Duration, round: usize, jitter: u32) -> Duration {
    let factor = u32::try_from(round)
        .ok()
        .and_then(|r| 1u32.checked_shl(r))
        .unwrap_or(u32::MAX);
    let full = timeout.saturating_mul(factor);
    if round == 0 {
        return full;
    }
    let cut = (full / 2).mul_f64(f64::from(jitter.min(1000)) / 1000.0);
    full.saturating_sub(cut).max(timeout)
}

/// A random jitter for `round_timeout`. Without a random source it is 0, the
/// longest wait.
fn random_jitter() -> u32 {
    getrandom::u32().map_or(0, |r| r % 1001)
}

/// The system configuration every channel is built from: search domains,
/// resolver options and c-ares timing, read once. Only the nameserver list
/// differs per channel.
struct Base {
    domain: Option<Name>,
    search: Vec<Name>,
    options: ResolverOpts,
    name_servers: Vec<NameServerConfig>,
    timing: CaresTiming,
}

impl Base {
    /// The base for a system DNS configuration read. A host that has none
    /// (no resolv.conf, or one with no `nameserver` line -- hickory then
    /// errors "no nameservers found in config") gets an empty system server
    /// list and default options: every query on the system channel fails,
    /// and dns.setServers() still works, as in node. Panicking here aborted
    /// the process from inside a V8 callback.
    fn from_system<E>(
        conf: Result<(ResolverConfig, ResolverOpts), E>,
        timing: CaresTiming,
    ) -> Base {
        let (config, options) = conf.unwrap_or_else(|_| {
            (
                ResolverConfig::from_parts(None, Vec::new(), Vec::new()),
                ResolverOpts::default(),
            )
        });
        let (domain, search, name_servers) = config.into_parts();
        Base {
            domain,
            search,
            options,
            name_servers,
            timing,
        }
    }
}

fn base() -> &'static Base {
    static BASE: OnceLock<Base> = OnceLock::new();
    BASE.get_or_init(|| {
        Base::from_system(
            hickory_resolver::system_conf::read_system_conf(),
            CaresTiming::system(),
        )
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

/// The local addresses a Resolver's queries are sent from
/// (`resolver.setLocalAddress()`), one per family; `None` lets the system pick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalAddrs {
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
}

impl LocalAddrs {
    /// The address, port 0, a socket to a server at `server` is bound to.
    fn bind_for(&self, server: IpAddr) -> Option<SocketAddr> {
        match server {
            IpAddr::V4(_) => self.v4.map(|ip| SocketAddr::new(IpAddr::V4(ip), 0)),
            IpAddr::V6(_) => self.v6.map(|ip| SocketAddr::new(IpAddr::V6(ip), 0)),
        }
    }
}

/// Builds a channel over `name_servers`, in order, with the system timing and
/// no local address. A server listed twice (same address AND port) is kept
/// once, at its first position -- c-ares does the same
/// (setServers(['1.2.3.4', '1.2.3.4:53']) reports one server on node).
fn build_channel(base: &Base, name_servers: Vec<NameServerConfig>) -> Channel {
    build_channel_with(base, name_servers, base.timing, LocalAddrs::default())
}

/// `build_channel` with a channel's own timing and local addresses.
fn build_channel_with(
    base: &Base,
    name_servers: Vec<NameServerConfig>,
    timing: CaresTiming,
    local: LocalAddrs,
) -> Channel {
    let mut seen: Vec<(IpAddr, u16)> = Vec::new();
    let mut servers = Vec::new();
    let mut display = Vec::new();
    for mut ns in name_servers {
        let key = (ns.ip, server_port(&ns));
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        display.push(format_server(key.0, key.1));
        let bind = local.bind_for(ns.ip);
        for connection in &mut ns.connections {
            connection.bind_addr = bind;
        }
        let config = ResolverConfig::from_parts(base.domain.clone(), base.search.clone(), vec![ns]);
        let mut builder = Resolver::builder_with_config(config, CaresProvider::default());
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
        // One server per resolver and one try per call: the retries, the
        // server walk and the timeout are the channel's (`walk`), so they
        // follow c-ares. hickory's own timeout is set past the longest round
        // so it never fires first; `walk` cuts each send at its round's.
        opts.attempts = 0;
        opts.timeout = timing.longest();
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
        timing,
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
    /// A `dns.Resolver` or `dns.promises.Resolver` instance, by the id
    /// `dns_resolver_new` gave it.
    Instance(u32),
}

impl Api {
    /// The JS side's number for an API: 0 callback, 1 promises, and an
    /// instance's id (`FIRST_INSTANCE` and up) for a Resolver.
    pub fn from_index(index: u32) -> Api {
        match index {
            0 => Api::Callback,
            1 => Api::Promises,
            id => Api::Instance(id),
        }
    }
}

/// One Resolver instance's state. A query takes the channel's `Arc` and a
/// cancel receiver when it is issued, so a later setServers /
/// setLocalAddress, or the instance being freed, never changes a query
/// already sent; only cancel() reaches one.
struct Instance {
    /// The servers as configured, kept so setLocalAddress can rebuild.
    name_servers: Vec<NameServerConfig>,
    timing: CaresTiming,
    local: LocalAddrs,
    channel: Arc<Channel>,
    /// cancel()'s epoch: a pending query fails ECANCELLED once it moves on.
    cancel: tokio::sync::watch::Sender<u64>,
}

/// The first instance id; 0 and 1 are the two default APIs.
const FIRST_INSTANCE: u32 = 2;
static NEXT_INSTANCE: AtomicU32 = AtomicU32::new(FIRST_INSTANCE);

/// Every live Resolver instance, process-wide like the defaults (a worker's
/// instances live here too; ids never repeat).
fn instances() -> &'static Mutex<HashMap<u32, Instance>> {
    static INSTANCES: OnceLock<Mutex<HashMap<u32, Instance>>> = OnceLock::new();
    INSTANCES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_instance<R>(id: u32, f: impl FnOnce(&mut Instance) -> R) -> Option<R> {
    let mut map = instances().lock().unwrap_or_else(|e| e.into_inner());
    map.get_mut(&id).map(f)
}

/// `new dns.Resolver(options)`: a channel over the system nameservers with the
/// options' timing (see `CaresTiming::for_resolver`). Returns its id.
pub fn dns_resolver_new(timeout_ms: i32, tries: u32, max_timeout_ms: u32) -> u32 {
    let base = base();
    let timing = CaresTiming::for_resolver(base.timing, timeout_ms, tries, max_timeout_ms);
    let name_servers = base.name_servers.clone();
    let local = LocalAddrs::default();
    let channel = Arc::new(build_channel_with(
        base,
        name_servers.clone(),
        timing,
        local,
    ));
    let id = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
    let (cancel, _) = tokio::sync::watch::channel(0);
    instances()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            Instance {
                name_servers,
                timing,
                local,
                channel,
                cancel,
            },
        );
    id
}

/// The instance was collected. A query still in flight keeps its channel and
/// finishes; nothing can cancel it any more, as nothing can reach the
/// instance.
pub fn dns_resolver_free(id: u32) {
    instances()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
}

/// `resolver.cancel()`: every query the instance has pending fails with
/// ECANCELLED; later ones are unaffected (measured: a query issued after
/// cancel() runs, and a second cancel() cancels it).
pub fn dns_cancel(api: Api) {
    if let Api::Instance(id) = api {
        with_instance(id, |instance| {
            instance.cancel.send_modify(|epoch| *epoch += 1);
        });
    }
}

/// `resolver.setLocalAddress()`: the addresses later queries are sent from,
/// as the JS side resolved node's two-argument form (each call sets both
/// families; the one not given goes back to "any").
pub fn dns_set_local_address(api: Api, local: LocalAddrs) {
    if let Api::Instance(id) = api {
        with_instance(id, |instance| {
            instance.local = local;
            instance.channel = Arc::new(build_channel_with(
                base(),
                instance.name_servers.clone(),
                instance.timing,
                local,
            ));
        });
    }
}

/// A query's cancel receiver and the epoch it was issued in.
type CancelToken = (tokio::sync::watch::Receiver<u64>, u64);

/// The cancel token of the instance `api` names, taken when a query is
/// issued; `None` for the default APIs, which have no cancel.
fn cancel_token(api: Api) -> Option<CancelToken> {
    let Api::Instance(id) = api else {
        return None;
    };
    with_instance(id, |instance| {
        let rx = instance.cancel.subscribe();
        let epoch = *rx.borrow();
        (rx, epoch)
    })
}

/// Runs `query`, unless the instance's cancel() moves past the token's epoch
/// first: then `on_cancel`. A freed instance's sender is gone, which disables
/// the cancel branch and lets the query finish.
async fn cancellable(
    token: Option<CancelToken>,
    query: impl Future<Output = OpOutcome>,
    on_cancel: impl FnOnce() -> OpOutcome,
) -> OpOutcome {
    let Some((mut rx, epoch)) = token else {
        return query.await;
    };
    tokio::pin!(query);
    tokio::select! {
        outcome = &mut query => outcome,
        Ok(_) = rx.wait_for(|now| *now != epoch) => on_cancel(),
    }
}

/// c-ares's ARES_ECANCELLED, as node reports it (measured: "queryA ECANCELLED
/// h.example", errno undefined; the JS side writes node's message).
fn cancelled(target: &str) -> OpOutcome {
    OpOutcome::node_failed("ECANCELLED", format!("ECANCELLED {target}"))
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
        // An id with no instance (one already freed) falls back to the system
        // channel; the JS side never issues one.
        Api::Instance(id) => {
            return with_instance(id, |instance| instance.channel.clone())
                .unwrap_or_else(system_channel);
        }
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
/// of both APIs; through dns.promises, of dns.promises alone; through an
/// instance, of that instance alone, keeping its timing and local addresses.
/// An empty list is accepted, as node accepts it; every query then fails
/// (`no_servers`).
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
        .collect::<Vec<_>>();
    let slot = match api {
        Api::Callback => None,
        Api::Promises => Some(&PROMISES_DEFAULT),
        Api::Instance(id) => {
            with_instance(id, |instance| {
                instance.channel = Arc::new(build_channel_with(
                    base(),
                    name_servers.clone(),
                    instance.timing,
                    instance.local,
                ));
                instance.name_servers = name_servers;
            });
            return;
        }
    };
    let channel = Arc::new(build_channel(base(), name_servers));
    let set = |slot: &RwLock<Option<Arc<Channel>>>| {
        *slot.write().unwrap_or_else(|e| e.into_inner()) = Some(channel.clone());
    };
    match slot {
        None => {
            set(&CALLBACK_DEFAULT);
            set(&PROMISES_DEFAULT);
        }
        Some(slot) => set(slot),
    }
}

/// True when `e` means the server gave no usable answer, so c-ares would try
/// the next one: no answer at all (timeout, network error), or a reply that
/// does not parse (hickory's `Proto`). Any DNS response -- an answer, NODATA,
/// NXDOMAIN, SERVFAIL, REFUSED -- is final (node's ARES_FLAG_NOCHECKRESP).
///
/// The malformed reply was measured on node v22.22.2 against fake servers
/// sending a truncated datagram and a three-byte one: [malformed, good] gets
/// the good server's records at once, the malformed server is demoted for the
/// next query, and a malformed server alone fails EBADRESP after node's 4
/// tries.
fn is_no_answer(e: &NetError) -> bool {
    matches!(
        e,
        NetError::Timeout
            | NetError::Io(_)
            | NetError::NoConnections
            | NetError::Busy
            | NetError::Proto(_)
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
/// best first, moving on only when a server gives no usable answer, for
/// `timing.tries` rounds, each send cut at its round's timeout. The caller
/// has checked the channel is not empty.
async fn walk<T, F, Fut>(channel: &Channel, mut send: F) -> Result<T, NetError>
where
    F: FnMut(ServerResolver) -> Fut,
    Fut: Future<Output = Result<T, NetError>>,
{
    let mut last = NetError::NoConnections;
    for round in 0..channel.timing.tries {
        let failures: Vec<u32> = channel
            .servers
            .iter()
            .map(|s| s.failures.load(Ordering::Relaxed))
            .collect();
        for i in try_order(&failures) {
            let server = &channel.servers[i];
            let wait = channel.timing.wait(round, random_jitter());
            let result = tokio::time::timeout(wait, send(server.resolver.clone()))
                .await
                .unwrap_or(Err(NetError::Timeout));
            match result {
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
    // Parse the name before any server is asked: a name that does not parse
    // is the caller's error, not a server's, so it must not walk the list --
    // as a `Proto` error it would, demoting every server on the way.
    let name = match hickory_resolver::proto::rr::IntoName::into_name(name) {
        Ok(name) => name,
        Err(e) => return Err(NetError::Msg(format!("invalid name: {e}"))),
    };
    walk(channel, |r| {
        let name = name.clone();
        async move { r.lookup(name, rtype).await }
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
/// `api` picks the channel (see `channel`). The channel and an instance's
/// cancel token are taken NOW, at the call, not when the future is first
/// polled: a setServers() or cancel() later in the same tick must find this
/// query already issued, as c-ares does.
pub fn dns_resolve(
    hostname: String,
    rrtype: String,
    api: Api,
) -> impl Future<Output = OpOutcome> + Send + 'static {
    let ch = channel(api);
    let token = cancel_token(api);
    let target = hostname.clone();
    cancellable(token, resolve_on(ch, hostname, rrtype), move || {
        cancelled(&target)
    })
}

async fn resolve_on(ch: Arc<Channel>, hostname: String, rrtype: String) -> OpOutcome {
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
///
/// A cancel() is the one failure that is not ENOTFOUND (measured:
/// "getHostByAddr ECANCELLED <ip>"). The channel and cancel token are taken
/// at the call, as for `dns_resolve`.
pub fn dns_reverse(ip_str: String, api: Api) -> impl Future<Output = OpOutcome> + Send + 'static {
    let ch = channel(api);
    let token = cancel_token(api);
    let target = ip_str.clone();
    cancellable(token, reverse_on(ch, ip_str), move || cancelled(&target))
}

async fn reverse_on(ch: Arc<Channel>, ip_str: String) -> OpOutcome {
    let ip: IpAddr = match ip_str.parse() {
        Ok(ip) => ip,
        Err(_) => {
            return OpOutcome::node_failed("EINVAL", format!("reverse EINVAL {ip_str}"));
        }
    };
    let not_found = |why: &dyn std::fmt::Display| {
        OpOutcome::node_failed("ENOTFOUND", format!("ENOTFOUND {ip_str}: {why}"))
    };

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
/// No answer at all is ETIMEOUT, a port that refuses ECONNREFUSED, and a
/// reply that does not parse EBADRESP (measured: a lone server sending a
/// truncated datagram).
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
        NetError::Proto(_) => return "EBADRESP",
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
        /// An A answer cut 6 bytes short: the header, ID and question are
        /// right, the record does not parse.
        Truncated,
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
            Reply::Truncated => {
                let mut full = respond(
                    query,
                    Reply::Answer {
                        octet: 9,
                        delay_ms: 0,
                    },
                );
                full.truncate(full.len() - 6);
                return full;
            }
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

    fn test_base(tries: usize) -> Base {
        Base {
            domain: None,
            search: Vec::new(),
            options: ResolverOpts::default(),
            name_servers: Vec::new(),
            timing: CaresTiming {
                timeout: Duration::from_millis(300),
                tries,
                max: None,
            },
        }
    }

    fn test_channel(fakes: &[&Fake], tries: usize) -> Channel {
        let base = test_base(tries);
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
        let ch = test_channel(&[&good, &broken], 2);
        let lookup = channel_lookup(&ch, "a.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(first_a(&lookup), "10.0.0.1");
        assert_eq!(broken.asked.load(Ordering::SeqCst), 0);

        // In the other order the broken server's NODATA is the answer.
        let ch = test_channel(&[&broken, &good], 2);
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
            let ch = test_channel(&[&failing, &good], 2);
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
        let ch = test_channel(&[&dead, &live], 1);
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
        // tries = 2: two rounds over both servers.
        let ch = test_channel(&[&a, &b], 2);
        let err = channel_lookup(&ch, "f.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "ETIMEOUT");
        assert_eq!(ch.servers[0].failures.load(Ordering::SeqCst), 2);
        assert_eq!(ch.servers[1].failures.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_malformed_reply_fails_over_and_alone_is_ebadresp() {
        let bad = fake(Reply::Truncated).await;
        let live = fake(Reply::Answer {
            octet: 3,
            delay_ms: 0,
        })
        .await;
        let ch = test_channel(&[&bad, &live], 4);
        let started = std::time::Instant::now();
        let lookup = channel_lookup(&ch, "m.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(first_a(&lookup), "10.0.0.3");
        // At once, not after the server timeout.
        assert!(started.elapsed() < Duration::from_millis(300));
        assert_eq!(bad.asked.load(Ordering::SeqCst), 1);
        // Demoted: the next query goes straight to the live server.
        channel_lookup(&ch, "n.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(bad.asked.load(Ordering::SeqCst), 1);
        assert_eq!(live.asked.load(Ordering::SeqCst), 2);

        // Alone: every try, then EBADRESP (node: 4 sends, "queryA EBADRESP").
        let alone = fake(Reply::Truncated).await;
        let ch = test_channel(&[&alone], 4);
        let err = channel_lookup(&ch, "o.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "EBADRESP");
        assert_eq!(alone.asked.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn a_name_that_does_not_parse_asks_no_server() {
        let live = fake(Reply::Answer {
            octet: 4,
            delay_ms: 0,
        })
        .await;
        let ch = test_channel(&[&live], 4);
        let err = channel_lookup(&ch, "a..b.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "ESERVFAIL");
        assert_eq!(live.asked.load(Ordering::SeqCst), 0);
        assert_eq!(ch.servers[0].failures.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn timing_follows_c_ares_under_node() {
        let secs = Duration::from_secs;
        // Default: 2 s and node's 4 tries.
        let default = CaresTiming::from_timeout_option(None);
        assert_eq!(default.timeout, secs(2));
        assert_eq!(default.tries, 4);
        // A zero is "not set"; the timeout is clamped to 250 ms..5 s.
        assert_eq!(CaresTiming::from_timeout_option(Some(0)), default);
        assert_eq!(CaresTiming::from_timeout_option(Some(1)).timeout, secs(1));
        assert_eq!(CaresTiming::from_timeout_option(Some(4)).timeout, secs(4));
        assert_eq!(CaresTiming::from_timeout_option(Some(7)).timeout, secs(5));
        // The last parseable timeout: wins; attempts: is not read at all.
        let opts = |s: &'static str| timeout_option(s.split_whitespace());
        assert_eq!(opts("edns0 timeout:3 attempts:1"), Some(3));
        assert_eq!(opts("timeout:3 timeout:4"), Some(4));
        assert_eq!(opts("timeout:3 timeout:x"), Some(3));
        assert_eq!(opts("attempts:5 rotate"), None);
    }

    #[test]
    fn each_round_doubles_the_wait_less_a_jitter_of_up_to_half() {
        let t = Duration::from_secs(2);
        assert_eq!(round_timeout(t, 0, 1000), t, "round 0 has no jitter");
        assert_eq!(round_timeout(t, 1, 0), t * 2);
        assert_eq!(round_timeout(t, 1, 1000), t, "never below the timeout");
        assert_eq!(round_timeout(t, 2, 1000), t * 2);
        assert_eq!(round_timeout(t, 3, 500), t * 6);
        assert_eq!(round_timeout(t, 3, 0), t * 8);
        // hickory's own timeout covers the longest round.
        assert_eq!(
            CaresTiming::from_timeout_option(None).longest(),
            Duration::from_secs(16)
        );
        // A huge round saturates instead of overflowing.
        assert!(round_timeout(t, 200, 0) >= t);
    }

    #[test]
    fn a_host_without_dns_config_gets_an_empty_system_channel() {
        let timing = CaresTiming::from_timeout_option(None);
        let base = Base::from_system::<std::io::Error>(
            Err(std::io::Error::other("no nameservers found in config")),
            timing,
        );
        assert!(base.name_servers.is_empty());
        assert!(base.search.is_empty());
        let ch = build_channel(&base, base.name_servers.clone());
        assert!(ch.servers.is_empty());
        assert!(ch.display.is_empty());
        assert_eq!(ch.timing, timing);
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

    #[test]
    fn resolver_options_map_to_cares_timing() {
        let system = CaresTiming::from_timeout_option(None);
        let t = |timeout, tries, max| CaresTiming::for_resolver(system, timeout, tries, max);
        // -1 and 0 keep the system timeout; tries is taken as given.
        assert_eq!(t(-1, 4, 0), system);
        assert_eq!(t(0, 2, 0).timeout, system.timeout);
        assert_eq!(t(0, 2, 0).tries, 2);
        // c-ares's 250 ms floor and, without a maxTimeout, its 5 s ceiling.
        assert_eq!(t(50, 1, 0).timeout, Duration::from_millis(250));
        assert_eq!(t(700, 1, 0).timeout, Duration::from_millis(700));
        assert_eq!(t(9000, 1, 0).timeout, Duration::from_secs(5));
        // A maxTimeout replaces the ceiling and cuts every round's wait.
        let capped = t(9000, 3, 7000);
        assert_eq!(capped.timeout, Duration::from_secs(7));
        assert_eq!(capped.wait(2, 0), Duration::from_secs(7));
        let small = t(100, 3, 150);
        assert_eq!(small.timeout, Duration::from_millis(250));
        assert_eq!(small.wait(1, 0), Duration::from_millis(150));
        assert_eq!(system.wait(1, 0), Duration::from_secs(4));
    }

    #[tokio::test]
    async fn a_silent_server_gets_one_datagram_per_try() {
        // hickory's UDP stream would resend inside a try (CaresProvider).
        let silent = fake(Reply::Silent).await;
        let mut ch = test_channel(&[&silent], 3);
        ch.timing.timeout = Duration::from_millis(400);
        let err = channel_lookup(&ch, "q.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "ETIMEOUT");
        assert_eq!(silent.asked.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn an_instance_has_its_own_servers_and_cancel() {
        let one = fake(Reply::Answer {
            octet: 1,
            delay_ms: 0,
        })
        .await;
        let silent = fake(Reply::Silent).await;
        let a = Api::Instance(dns_resolver_new(-1, 4, 0));
        let b = Api::Instance(dns_resolver_new(100, 1, 0));
        let system = dns_get_servers(Api::Instance(dns_resolver_new(-1, 4, 0)));
        dns_set_servers(a, &[("127.0.0.1".parse().unwrap(), one.port)]);
        dns_set_servers(b, &[("127.0.0.1".parse().unwrap(), silent.port)]);
        assert_eq!(dns_get_servers(a), vec![format!("127.0.0.1:{}", one.port)]);
        assert_eq!(
            dns_get_servers(b),
            vec![format!("127.0.0.1:{}", silent.port)]
        );
        assert_eq!(dns_get_servers(Api::Callback), system);
        match dns_resolve("x.example".into(), "A".into(), a).await {
            OpOutcome::Json(json) => assert_eq!(json, r#"["10.0.0.1"]"#),
            other => panic!("expected records, got {other:?}"),
        }

        // A cancel() after the query is issued -- before its future is even
        // polled -- fails it; the instance stays usable.
        let pending = dns_resolve("c.example".into(), "A".into(), b);
        dns_cancel(b);
        match pending.await {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ECANCELLED"),
            other => panic!("expected ECANCELLED, got {other:?}"),
        }
        let pending = dns_reverse("192.0.2.9".into(), b);
        dns_cancel(a); // another instance's cancel does not reach it
        match pending.await {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ENOTFOUND"),
            other => panic!("expected the timeout's ENOTFOUND, got {other:?}"),
        }

        // A freed instance's id falls back to the system channel.
        let Api::Instance(id) = a else { unreachable!() };
        dns_resolver_free(id);
        assert_eq!(dns_get_servers(a), system);
    }
}
