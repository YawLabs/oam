use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ProtocolConfig, ResolverConfig, ResolverOpts,
    ServerOrderingStrategy,
};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::{RuntimeProvider, TokioRuntimeProvider};
use hickory_resolver::net::udp::UdpClientStream;
use hickory_resolver::net::xfer::{DnsExchange, DnsHandle, FirstAnswer};
use hickory_resolver::proto::op::{DnsRequestOptions, Query, ResponseCode};
use hickory_resolver::proto::rr::{Name, RData, Record, RecordType};
use hickory_resolver::{ConnectionProvider, NameServerPool, PoolContext, Resolver, TlsConfig};

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
/// rest are hickory's unchanged. Its UDP sockets come from `CaresRuntime`.
#[derive(Clone, Default)]
struct CaresProvider(CaresRuntime);

impl ConnectionProvider for CaresProvider {
    type Conn = <CaresRuntime as ConnectionProvider>::Conn;
    type FutureConn = <CaresRuntime as ConnectionProvider>::FutureConn;
    type RuntimeProvider = CaresRuntime;

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
            let exchange: DnsExchange<CaresRuntime> = stream.exchange();
            Ok(exchange)
        }))
    }

    fn runtime_provider(&self) -> &Self::RuntimeProvider {
        &self.0
    }
}

/// tokio's runtime for hickory, except that on Windows a UDP socket is bound
/// with SO_EXCLUSIVEADDRUSE (`bind_udp_exclusive`).
///
/// hickory picks each query's source port itself, at random in 1024-65535,
/// and binds the wildcard address (or setLocalAddress's). Windows lets a plain
/// bind share a port that another socket holds on a more specific address, in
/// either order, and delivers a datagram to the most specific socket (measured:
/// with node:dgram on 127.0.0.1:P, a bind of 0.0.0.0:P succeeds and a datagram
/// to 127.0.0.1:P reaches node's socket). So whenever hickory's pick landed on
/// a port held on 127.0.0.1 -- one of the process's own dgram sockets, a local
/// forwarder's, anything's -- the answer went to that socket and the query
/// waited out its timeout (conformance case 581 caught it: a fake server was
/// "asked" a reply meant for a query to another one). With
/// SO_EXCLUSIVEADDRUSE the shared bind fails WSAEADDRINUSE, which hickory's
/// bind loop already answers by picking another random port, and no socket can
/// later bind the same port on a specific address while the query is out.
///
/// Letting the OS choose the port (hickory's os_port_selection, what c-ares
/// and so node do) would also avoid a held port, but Windows hands out
/// ephemeral UDP ports sequentially (measured: node's DNS queries came from
/// 56827, 56828, 56829, ...), and a predictable source port leaves only the
/// 16-bit query id against a spoofed answer. Linux and macOS refuse the shared
/// bind with EADDRINUSE, so hickory's plain bind already moves on there.
#[derive(Clone, Default)]
struct CaresRuntime(TokioRuntimeProvider);

impl RuntimeProvider for CaresRuntime {
    type Handle = <TokioRuntimeProvider as RuntimeProvider>::Handle;
    type Timer = <TokioRuntimeProvider as RuntimeProvider>::Timer;
    type Udp = tokio::net::UdpSocket;
    type Tcp = <TokioRuntimeProvider as RuntimeProvider>::Tcp;

    fn create_handle(&self) -> Self::Handle {
        self.0.create_handle()
    }

    fn connect_tcp(
        &self,
        server_addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn Send + Future<Output = io::Result<Self::Tcp>>>> {
        self.0.connect_tcp(server_addr, bind_addr, timeout)
    }

    #[cfg(windows)]
    fn bind_udp(
        &self,
        local_addr: SocketAddr,
        _server_addr: SocketAddr,
    ) -> Pin<Box<dyn Send + Future<Output = io::Result<Self::Udp>>>> {
        Box::pin(async move { bind_udp_exclusive(local_addr) })
    }

    #[cfg(not(windows))]
    fn bind_udp(
        &self,
        local_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> Pin<Box<dyn Send + Future<Output = io::Result<Self::Udp>>>> {
        self.0.bind_udp(local_addr, server_addr)
    }
}

/// A tokio UDP socket bound to `local` with SO_EXCLUSIVEADDRUSE set first, so
/// the bind fails WSAEADDRINUSE (`io::ErrorKind::AddrInUse`) when any socket
/// holds the port on any address, and no other socket can bind the port on
/// any address while this one lives. Otherwise what tokio's own bind does:
/// socket2 creates the socket non-inheritable and overlapped, as std does.
#[cfg(windows)]
fn bind_udp_exclusive(local: SocketAddr) -> io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET, setsockopt,
    };
    let socket = Socket::new(Domain::for_address(local), Type::DGRAM, Some(Protocol::UDP))?;
    let on: i32 = 1;
    // SAFETY: a synchronous setsockopt on a socket `socket` owns for the whole
    // call; the option value points at the live i32 `on` and the length is
    // that i32's size; nothing is read after the call returns.
    let rc = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    socket.bind(&local.into())?;
    socket.set_nonblocking(true)?;
    tokio::net::UdpSocket::from_std(socket.into())
}

/// The single-server resolver each channel server is.
type ServerResolver = Resolver<CaresProvider>;

/// One nameserver of a channel.
struct Server {
    /// For reverse() and a bare name's search-list walk.
    resolver: ServerResolver,
    /// The same server's connection pool asked directly (`raw_lookup`): a
    /// query and its response as served, without the resolver's answer
    /// handling -- c-ares hands node the answer section as it came.
    pool: Arc<NameServerPool<CaresProvider>>,
    /// The request options the resolver would send with.
    request: DnsRequestOptions,
    /// Consecutive failures to answer (timeouts, network errors); any answer
    /// resets it. The walk tries the server with the fewest first.
    failures: AtomicU32,
}

struct Channel {
    servers: Vec<Server>,
    /// What `getServers()` reports, in node's string form and config order.
    display: Vec<String>,
    timing: CaresTiming,
    /// Why a configured server is missing from `servers`: its resolver could
    /// not be built (`build_channel_with`). The first failure's text.
    setup_error: Option<String>,
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
///
/// It runs inside V8 callbacks (a Resolver's constructor, setServers,
/// setLocalAddress), where a panic aborts the process, so a server whose
/// resolver cannot be built is left out instead: it stays in `display`
/// (getServers() reports what was configured), and with no server left every
/// query fails as on an empty channel, naming `setup_error`.
fn build_channel_with(
    base: &Base,
    name_servers: Vec<NameServerConfig>,
    timing: CaresTiming,
    local: LocalAddrs,
) -> Channel {
    let mut seen: Vec<(IpAddr, u16)> = Vec::new();
    let mut servers = Vec::new();
    let mut display = Vec::new();
    let mut setup_error: Option<String> = None;
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
        let pool_ns = ns.clone();
        let config = ResolverConfig::from_parts(base.domain.clone(), base.search.clone(), vec![ns]);
        let mut builder = Resolver::builder_with_config(config, CaresProvider::default());
        let opts = builder.options_mut();
        *opts = base.options.clone();
        // EDNS0 with 1232-byte UDP payloads. NOT node's: node v22.22.2's
        // channels send no OPT record (measured: an 800-byte answer is TC over
        // UDP and then TCP in node, one UDP datagram here; the answer is the
        // same; docs/node-divergences.md #30). Kept on purpose: some
        // forwarders (Tailscale MagicDNS) answer a non-EDNS query with an
        // oversized TC=1 datagram that hickory fails to parse, where c-ares
        // retries over TCP, so large answers (TXT sets) failed without it.
        // try_tcp_on_error covers the same forwarder shape past the EDNS size.
        opts.edns0 = true;
        opts.try_tcp_on_error = true;
        // c-ares keeps no answer cache: every query reaches the server
        // (measured on node v22.22.2: five resolve4 calls, five datagrams,
        // NXDOMAIN and NODATA included). hickory's cache answered repeats
        // without asking, and with TTLs decremented by the time elapsed.
        opts.cache_size = 0;
        // ...and node reports a record's TTL as served, any u32 (measured:
        // 100000, 2^31 and 2^32-1 come through), where hickory clamps every
        // answer's TTL to its 86400 s cache ceiling even uncached.
        opts.positive_max_ttl = Some(Duration::from_secs(u64::from(u32::MAX)));
        // One server per resolver and one try per call: the retries, the
        // server walk and the timeout are the channel's (`walk`), so they
        // follow c-ares. hickory's own timeout is set past the longest round
        // so it never fires first; `walk` cuts each send at its round's.
        opts.attempts = 0;
        opts.timeout = timing.longest();
        opts.num_concurrent_reqs = 1;
        opts.server_ordering_strategy = ServerOrderingStrategy::UserProvidedOrder;
        let pool_opts = builder.options_mut().clone();
        let mut request = DnsRequestOptions::default();
        request.recursion_desired = pool_opts.recursion_desired;
        request.use_edns = pool_opts.edns0;
        request.edns_payload_len = pool_opts.edns_payload_len;
        request.case_randomization = pool_opts.case_randomization;
        let built = TlsConfig::new().and_then(|tls| Ok((tls, builder.build()?)));
        let (tls, resolver) = match built {
            Ok(parts) => parts,
            Err(e) => {
                setup_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        let pool = NameServerPool::from_config(
            [pool_ns],
            Arc::new(PoolContext::new(pool_opts, tls)),
            CaresProvider::default(),
        );
        servers.push(Server {
            resolver,
            pool: Arc::new(pool),
            request,
            failures: AtomicU32::new(0),
        });
    }
    Channel {
        servers,
        display,
        timing,
        setup_error,
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

/// One runtime's live Resolver instances (`CoreRuntime::dns_instances`). It
/// is per runtime, not process-wide, so a worker's instances go with the
/// worker: the FinalizationRegistry that frees an instance never runs for an
/// isolate that is torn down with its instances still reachable, and a
/// process-wide map kept every one of them (measured: 5000 Resolvers per
/// worker left about 425 MB behind per worker). Ids come from one process-wide
/// counter, so they never repeat across runtimes.
///
/// It also holds the runtime's two default channels (`DnsState`): node's
/// default resolver belongs to the environment, so a worker's
/// dns.setServers() reaches neither the main thread nor another worker
/// (measured on node v22.22.2).
#[derive(Clone, Default)]
pub struct DnsInstances(Arc<DnsState>);

#[derive(Default)]
struct DnsState {
    instances: Mutex<HashMap<u32, Instance>>,
    /// The callback and promises defaults when setServers replaced them;
    /// `None` is "still the system channel".
    callback_default: RwLock<Option<Arc<Channel>>>,
    promises_default: RwLock<Option<Arc<Channel>>>,
}

impl DnsInstances {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Instance>> {
        self.0.instances.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with<R>(&self, id: u32, f: impl FnOnce(&mut Instance) -> R) -> Option<R> {
        let mut map = self.map();
        map.get_mut(&id).map(f)
    }
}

/// `new dns.Resolver(options)`: a channel over the system nameservers with the
/// options' timing (see `CaresTiming::for_resolver`). Returns its id.
pub fn dns_resolver_new(
    instances: &DnsInstances,
    timeout_ms: i32,
    tries: u32,
    max_timeout_ms: u32,
) -> u32 {
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
    instances.map().insert(
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
pub fn dns_resolver_free(instances: &DnsInstances, id: u32) {
    instances.map().remove(&id);
}

/// `resolver.cancel()`: every query the instance has pending fails with
/// ECANCELLED; later ones are unaffected (measured: a query issued after
/// cancel() runs, and a second cancel() cancels it).
pub fn dns_cancel(instances: &DnsInstances, api: Api) {
    if let Api::Instance(id) = api {
        instances.with(id, |instance| {
            instance.cancel.send_modify(|epoch| *epoch += 1);
        });
    }
}

/// `resolver.setLocalAddress()`: the addresses later queries are sent from,
/// as the JS side resolved node's two-argument form (each call sets both
/// families; the one not given goes back to "any").
pub fn dns_set_local_address(instances: &DnsInstances, api: Api, local: LocalAddrs) {
    if let Api::Instance(id) = api {
        instances.with(id, |instance| {
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
fn cancel_token(instances: &DnsInstances, api: Api) -> Option<CancelToken> {
    let Api::Instance(id) = api else {
        return None;
    };
    instances.with(id, |instance| {
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

/// The channel a call through `api` goes to. A query holds its own `Arc`, so a
/// setServers() during it changes the NEXT query, never one in flight.
fn channel(instances: &DnsInstances, api: Api) -> Arc<Channel> {
    let slot = match api {
        Api::Callback => &instances.0.callback_default,
        Api::Promises => &instances.0.promises_default,
        // An id with no instance (one already freed) falls back to the system
        // channel; the JS side never issues one.
        Api::Instance(id) => {
            return instances
                .with(id, |instance| instance.channel.clone())
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
pub fn dns_get_servers(instances: &DnsInstances, api: Api) -> Vec<String> {
    channel(instances, api).display.clone()
}

/// `setServers()` through `api`: replace the default channel with `servers`
/// (address, port), already validated and parsed by the JS side as node's
/// ResolverBase.setServers does. Through the callback API that is the default
/// of both APIs; through dns.promises, of dns.promises alone; through an
/// instance, of that instance alone, keeping its timing and local addresses.
/// An empty list is accepted, as node accepts it; every query then fails
/// (`no_servers`).
pub fn dns_set_servers(instances: &DnsInstances, api: Api, servers: &[(IpAddr, u16)]) {
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
        Api::Promises => Some(&instances.0.promises_default),
        Api::Instance(id) => {
            instances.with(id, |instance| {
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
            set(&instances.0.callback_default);
            set(&instances.0.promises_default);
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
    F: FnMut(&Server) -> Fut,
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
            let result = tokio::time::timeout(wait, send(server))
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

/// A query's answer section, as the server sent it, and the question name
/// its response carried (node's c-ares reads names from the response).
#[derive(Debug)]
struct Answers {
    records: Vec<Record>,
    question: Option<Name>,
}

impl Answers {
    fn answers(&self) -> &[Record] {
        &self.records
    }
}

/// The text a `NetError::Msg` starts with when the name asked did not parse
/// (`channel_lookup`); `query_error_code` reads it as EBADNAME.
const INVALID_NAME: &str = "invalid name: ";

/// One `RecordType` query of `name` over `channel`.
async fn channel_lookup(
    channel: &Channel,
    name: &str,
    rtype: RecordType,
) -> Result<Answers, NetError> {
    // Parse the name before any server is asked: a name that does not parse
    // is the caller's error, not a server's, so it must not walk the list --
    // as a `Proto` error it would, demoting every server on the way.
    let name = match hickory_resolver::proto::rr::IntoName::into_name(name) {
        Ok(name) => name,
        Err(e) => return Err(NetError::Msg(format!("{INVALID_NAME}{e}"))),
    };
    // ONE query for the name as given, its answer section as served, as
    // c-ares's ares_query sends it for node's resolve* (not ares_search: no
    // search domain is tried, measured on node v22.22.2 on a host with two --
    // `localhost` is asked of the server as it is): the resolver would walk
    // the search list, chase a CNAME with a second query, answer RFC 6761
    // names (localhost, *.invalid) itself without asking, and drop an answer
    // holding no record of the type asked (node: [] or an error by type --
    // `empty_of_type`).
    let mut name = name;
    name.set_fqdn(true);
    raw_walk(channel, name, rtype).await
}

/// The walk over `channel` of one query for `name`.
async fn raw_walk(channel: &Channel, name: Name, rtype: RecordType) -> Result<Answers, NetError> {
    walk(channel, |server| {
        let pool = server.pool.clone();
        let query = Query::query(name.clone(), rtype);
        let request = server.request;
        async move { raw_lookup(&pool, query, request).await }
    })
    .await
}

/// One query to one server's pool. An empty answer is the pool's own
/// `NoRecords` (ENODATA / ENOTFOUND by its rcode, as before); an NXDOMAIN
/// that carries records (a CNAME to a name that does not exist) is
/// ENOTFOUND, as c-ares reports the rcode.
async fn raw_lookup(
    pool: &NameServerPool<CaresProvider>,
    query: Query,
    request: DnsRequestOptions,
) -> Result<Answers, NetError> {
    let response = pool.lookup(query.clone(), request).first_answer().await?;
    if response.metadata.response_code == ResponseCode::NXDomain {
        return Err(NetError::Dns(
            hickory_resolver::net::DnsError::NoRecordsFound(hickory_resolver::net::NoRecords::new(
                query,
                ResponseCode::NXDomain,
            )),
        ));
    }
    let message = response.into_message();
    Ok(Answers {
        question: message.queries.first().map(|q| q.name().clone()),
        records: message.answers,
    })
}

/// [`empty_of_type`]'s outcome when `answers` holds records but none of
/// `rtype`.
fn no_record_of_type(rtype: RecordType, hostname: &str, answers: &Answers) -> Option<OpOutcome> {
    let records = answers.answers();
    if records.is_empty() || records.iter().any(|r| r.record_type() == rtype) {
        return None;
    }
    // node names the question as the response carries it, not as typed.
    let asked = answers
        .question
        .as_ref()
        .map_or_else(|| hostname.to_string(), host);
    empty_of_type(rtype, &asked, records)
}

/// What node reports for a query whose answer section holds records, none
/// of the type asked (measured on node v22.22.2 against a fake server, one
/// datagram each): `None` is the type's empty array (TLSA, MX, TXT, SRV, CAA,
/// NAPTR, and A / AAAA when a CNAME is among the records), `Some` the
/// outcome otherwise -- ENODATA for A / AAAA without a CNAME, NS and PTR;
/// EBADRESP for SOA; for CNAME, the name asked.
fn empty_of_type(rtype: RecordType, hostname: &str, answers: &[Record]) -> Option<OpOutcome> {
    let nodata = || {
        OpOutcome::node_failed(
            "ENODATA",
            format!("ENODATA {hostname}: no record of the type asked"),
        )
    };
    match rtype {
        RecordType::A | RecordType::AAAA => {
            let cname = answers.iter().any(|r| r.record_type() == RecordType::CNAME);
            (!cname).then(nodata)
        }
        RecordType::NS | RecordType::PTR => Some(nodata()),
        RecordType::SOA => Some(OpOutcome::node_failed(
            "EBADRESP",
            format!("EBADRESP {hostname}: no SOA record in the answer"),
        )),
        RecordType::CNAME => Some(OpOutcome::Json(
            serde_json::json!([hostname.strip_suffix('.').unwrap_or(hostname)]).to_string(),
        )),
        _ => None,
    }
}

/// A query on a channel with no nameservers (`dns.setServers([])`). c-ares
/// fails it with ARES_ENOSERVER, a status node's error table predates, so node
/// v22.22.2 reports the code `UNKNOWN_ARES_ERROR` (measured: "queryA
/// UNKNOWN_ARES_ERROR x.example").
///
/// A channel whose servers could not be set up (`Channel::setup_error`) has
/// none either, and fails the same way with the reason in the text.
fn no_servers(query: &str, setup_error: Option<&str>) -> OpOutcome {
    let why = match setup_error {
        Some(error) => format!("nameserver setup failed: {error}"),
        None => "no nameservers configured".to_string(),
    };
    OpOutcome::node_failed(
        "UNKNOWN_ARES_ERROR",
        format!("UNKNOWN_ARES_ERROR {query}: {why}"),
    )
}

/// A DNS character-string as node builds it: one character per byte
/// (cares_wrap's OneByteString), not UTF-8. Measured on node v22.22.2
/// against a fake server: a TXT chunk and a CAA value of bytes 61 c3 a9 ff 62
/// are the five characters U+0061 U+00C3 U+00A9 U+00FF U+0062.
fn one_byte_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// True when c-ares takes `bytes` as a NAPTR flags / service / regexp
/// string: printable ASCII only. Measured on node v22.22.2 (c-ares 1.34.6)
/// against a fake server, one record per answer: 0x20 and 0x7e come through
/// in regexp; a 0x00, 0x01, 0x09, 0x7f or 0x80 there, and bytes c3 a9 ff in
/// each of the three fields alone, are "queryNaptr EBADRESP".
fn naptr_string_ok(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| (0x20..=0x7e).contains(b))
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

/// dns's result order (`dns.setDefaultResultOrder`, `--dns-result-order`, a
/// lookup's `order` / `verbatim`): the resolver's own order, or a stable
/// partition by family (measured on node v22.22.2: each family keeps its
/// relative order).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResultOrder {
    #[default]
    Verbatim,
    Ipv4First,
    Ipv6First,
}

impl ResultOrder {
    /// node's names; anything else (an absent argument) is verbatim. The JS
    /// side has validated the name.
    pub fn parse(name: &str) -> ResultOrder {
        match name {
            "ipv4first" => ResultOrder::Ipv4First,
            "ipv6first" => ResultOrder::Ipv6First,
            _ => ResultOrder::Verbatim,
        }
    }

    /// Reorders `items` in place, `is_v4` telling the families apart.
    pub fn apply<T>(self, items: &mut [T], is_v4: impl Fn(&T) -> bool) {
        match self {
            ResultOrder::Verbatim => {}
            // sort_by_key is stable: equal keys keep their order.
            ResultOrder::Ipv4First => items.sort_by_key(|item| !is_v4(item)),
            ResultOrder::Ipv6First => items.sort_by_key(|item| is_v4(item)),
        }
    }
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

/// dns.lookupService(address, port): getnameinfo (`net_connect::getnameinfo`).
/// A failure is node's DNSException for the resolver's code, the address as
/// written its hostname; the JS side names the syscall getnameinfo.
pub async fn dns_lookup_service(address: String, port: u16) -> OpOutcome {
    match crate::net_connect::getnameinfo(&address, port).await {
        Ok((hostname, service)) => OpOutcome::Json(
            serde_json::json!({ "hostname": hostname, "service": service }).to_string(),
        ),
        Err(error) => OpOutcome::sys(crate::net_connect::resolve_error(&address, &error)),
    }
}

/// dns.resolve(hostname, rrtype) -- query specific DNS record types.
///
/// `api` picks the channel (see `channel`). The channel and an instance's
/// cancel token are taken NOW, at the call, not when the future is first
/// polled: a setServers() or cancel() later in the same tick must find this
/// query already issued, as c-ares does.
pub fn dns_resolve(
    instances: &DnsInstances,
    hostname: String,
    rrtype: String,
    api: Api,
) -> impl Future<Output = OpOutcome> + Send + 'static {
    let ch = channel(instances, api);
    let token = cancel_token(instances, api);
    let target = hostname.clone();
    cancellable(token, resolve_on(ch, hostname, rrtype), move || {
        cancelled(&target)
    })
}

async fn resolve_on(ch: Arc<Channel>, hostname: String, rrtype: String) -> OpOutcome {
    if ch.servers.is_empty() {
        return no_servers(&hostname, ch.setup_error.as_deref());
    }
    let unsupported = || OpOutcome::Failed(format!("dns.resolve: unsupported rrtype '{rrtype}'"));
    let rtype = match rrtype.as_str() {
        "A" => RecordType::A,
        "AAAA" => RecordType::AAAA,
        "CNAME" => RecordType::CNAME,
        "MX" => RecordType::MX,
        "TXT" => RecordType::TXT,
        "NS" => RecordType::NS,
        "SRV" => RecordType::SRV,
        "SOA" => RecordType::SOA,
        "PTR" => RecordType::PTR,
        "CAA" => RecordType::CAA,
        "NAPTR" => RecordType::NAPTR,
        "TLSA" => RecordType::TLSA,
        _ => return unsupported(),
    };
    // Every name is one query, as given (`channel_lookup`): node's resolve*
    // is c-ares's ares_query, which tries no search domain, whatever the
    // name's dots. (Up to 0.19.2 a name with dots was qualified here so
    // hickory would not walk the search list; a bare one was walked.)
    let lookup = match channel_lookup(&ch, &hostname, rtype).await {
        Ok(lookup) => lookup,
        Err(e) => return dns_err(&hostname, &e),
    };
    // Every type's rule for an answer with records but none of the type.
    if let Some(outcome) = no_record_of_type(rtype, &hostname, &lookup) {
        return outcome;
    }
    match rtype {
        // With its TTL: the JS side keeps it for node's { ttl: true } and
        // otherwise drops it.
        RecordType::A => json_list(&lookup, |rec| match &rec.data {
            RData::A(a) => Some(serde_json::json!({
                "address": a.0.to_string(),
                "ttl": rec.ttl,
            })),
            _ => None,
        }),
        RecordType::AAAA => json_list(&lookup, |rec| match &rec.data {
            RData::AAAA(a) => Some(serde_json::json!({
                "address": a.0.to_string(),
                "ttl": rec.ttl,
            })),
            _ => None,
        }),
        RecordType::CNAME => json_list(&lookup, |rec| match &rec.data {
            RData::CNAME(c) => Some(serde_json::json!(host(&c.0))),
            _ => None,
        }),
        RecordType::MX => json_list(&lookup, |rec| match &rec.data {
            // Key order is JS property-insertion order now that
            // serde_json preserves it: node (c-ares) inserts
            // exchange before priority, and the differential
            // compares JSON.stringify byte-for-byte.
            RData::MX(mx) => Some(serde_json::json!({
                "exchange": host(&mx.exchange),
                "priority": mx.preference,
            })),
            _ => None,
        }),
        RecordType::TXT => json_list(&lookup, |rec| match &rec.data {
            RData::TXT(txt) => {
                let parts: Vec<String> = txt.txt_data.iter().map(|b| one_byte_string(b)).collect();
                Some(serde_json::json!(parts))
            }
            _ => None,
        }),
        RecordType::NS => json_list(&lookup, |rec| match &rec.data {
            RData::NS(ns) => Some(serde_json::json!(host(&ns.0))),
            _ => None,
        }),
        RecordType::SRV => json_list(&lookup, |rec| match &rec.data {
            // node's insertion order (verified live v22.22.2:
            // _matrix._tcp.matrix.org): name, port, priority, weight.
            RData::SRV(srv) => Some(serde_json::json!({
                "name": host(&srv.target),
                "port": srv.port,
                "priority": srv.priority,
                "weight": srv.weight,
            })),
            _ => None,
        }),
        RecordType::SOA => {
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
        RecordType::PTR => json_list(&lookup, |rec| match &rec.data {
            RData::PTR(p) => Some(serde_json::json!(host(&p.0))),
            _ => None,
        }),
        RecordType::CAA => {
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
                        let value = serde_json::json!(one_byte_string(&caa.value));
                        Some(format!("{{\"critical\":{flags},{tag}:{value}}}"))
                    }
                    _ => None,
                })
                .collect();
            OpOutcome::Json(format!("[{}]", objects.join(",")))
        }
        RecordType::NAPTR => {
            // c-ares refuses a NAPTR string that is not printable ASCII, and
            // the answer with it (`naptr_string_ok`); hickory decodes it.
            let refused = lookup.answers().iter().any(|rec| match &rec.data {
                RData::NAPTR(naptr) => {
                    !(naptr_string_ok(&naptr.flags)
                        && naptr_string_ok(&naptr.services)
                        && naptr_string_ok(&naptr.regexp))
                }
                _ => false,
            });
            if refused {
                return OpOutcome::node_failed(
                    "EBADRESP",
                    format!("EBADRESP {hostname}: NAPTR string outside printable ASCII"),
                );
            }
            json_list(&lookup, |rec| match &rec.data {
                // node's insertion order (cares_wrap.cc ParseNaptrReply):
                // flags, service, regexp, replacement, order, preference.
                RData::NAPTR(naptr) => Some(serde_json::json!({
                    "flags": one_byte_string(&naptr.flags),
                    "service": one_byte_string(&naptr.services),
                    "regexp": one_byte_string(&naptr.regexp),
                    "replacement": host(&naptr.replacement),
                    "order": naptr.order,
                    "preference": naptr.preference,
                })),
                _ => None,
            })
        }
        RecordType::TLSA => {
            // One entry per answer record, null for a record of another
            // type: node's array keeps the position, a hole where c-ares
            // skipped a record (measured: [A, TLSA] answers a two-slot
            // array with a hole first; [TLSA, A] one slot).
            let records: Vec<Option<&hickory_resolver::proto::rr::rdata::TLSA>> = lookup
                .answers()
                .iter()
                .map(|rec| match &rec.data {
                    RData::TLSA(tlsa) => Some(tlsa),
                    _ => None,
                })
                .collect();
            // c-ares refuses a TLSA record with no certificate data, and
            // the whole answer with it (measured: "queryTlsa EBADRESP"
            // even beside a well-formed record); hickory decodes it.
            if records
                .iter()
                .flatten()
                .any(|tlsa| tlsa.cert_data.is_empty())
            {
                return OpOutcome::node_failed(
                    "EBADRESP",
                    format!("EBADRESP {hostname}: TLSA record without certificate data"),
                );
            }
            // node's keys in its order (certUsage, selector, match,
            // data); `data` is the bytes, made an ArrayBuffer in JS.
            let records: Vec<serde_json::Value> = records
                .into_iter()
                .map(|tlsa| match tlsa {
                    Some(tlsa) => serde_json::json!({
                        "certUsage": u8::from(tlsa.cert_usage),
                        "selector": u8::from(tlsa.selector),
                        "match": u8::from(tlsa.matching),
                        "data": tlsa.cert_data,
                    }),
                    None => serde_json::Value::Null,
                })
                .collect();
            OpOutcome::Json(serde_json::json!(records).to_string())
        }
        // `rtype` is one of the twelve above.
        _ => unsupported(),
    }
}

/// A resolve*'s JSON array: one entry per answer record `entry` takes, in
/// the order served. A record of another type (a CNAME before the A records
/// it leads to) gives `None` and is left out.
fn json_list(answers: &Answers, entry: impl Fn(&Record) -> Option<serde_json::Value>) -> OpOutcome {
    let entries: Vec<serde_json::Value> = answers.answers().iter().filter_map(entry).collect();
    OpOutcome::Json(serde_json::json!(entries).to_string())
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
pub fn dns_reverse(
    instances: &DnsInstances,
    ip_str: String,
    api: Api,
) -> impl Future<Output = OpOutcome> + Send + 'static {
    let ch = channel(instances, api);
    let token = cancel_token(instances, api);
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
    match walk(&ch, |server| {
        let resolver = server.resolver.clone();
        async move { resolver.reverse_lookup(ip).await }
    })
    .await
    {
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
/// truncated datagram). A socket that cannot be bound to a Resolver's
/// setLocalAddress() is ECONNREFUSED too, c-ares's code for any socket it
/// cannot open (measured: setLocalAddress('192.0.2.77'), an address the host
/// does not have).
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
        // hickory's word for "no connection to the server could be made":
        // a UDP socket that cannot be bound to a setLocalAddress() address
        // (measured).
        NetError::NoConnections => return "ECONNREFUSED",
        NetError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::AddrNotAvailable
                    | std::io::ErrorKind::AddrInUse
            ) =>
        {
            return "ECONNREFUSED";
        }
        NetError::Proto(_) => return "EBADRESP",
        // A name that is not one (`channel_lookup`): c-ares's ARES_EBADNAME.
        // Measured on node v22.22.2: "queryA EBADNAME a..b.example.", and
        // the same for a leading dot, two trailing dots, a 64-byte label
        // and a name past 255 bytes.
        NetError::Msg(text) if text.starts_with(INVALID_NAME) => return "EBADNAME",
        // Anything hickory reports that is none of the above. No c-ares
        // status is known to match, so it reads as a failure to resolve.
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
        // A local address that cannot be bound (setLocalAddress): hickory
        // reports the failed socket as NoConnections, or as the io error.
        assert_eq!(query_error_code(&NetError::NoConnections), "ECONNREFUSED");
        assert_eq!(
            query_error_code(&NetError::Io(Arc::new(std::io::Error::from(
                std::io::ErrorKind::AddrNotAvailable
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
        test_channel_with(fakes, test_base(tries))
    }

    fn test_channel_with(fakes: &[&Fake], base: Base) -> Channel {
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

    fn first_a(lookup: &Answers) -> String {
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
        // A server timeout far above the bound below, so that only a wait
        // for the malformed server's timeout can cross it, not a busy host.
        let slow = || {
            let mut base = test_base(4);
            base.timing.timeout = Duration::from_secs(20);
            base
        };
        let ch = test_channel_with(&[&bad, &live], slow());
        let started = std::time::Instant::now();
        let lookup = channel_lookup(&ch, "m.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(first_a(&lookup), "10.0.0.3");
        // At once, not after the server timeout.
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(bad.asked.load(Ordering::SeqCst), 1);
        // Demoted: the next query goes straight to the live server.
        channel_lookup(&ch, "n.example.", RecordType::A)
            .await
            .unwrap();
        assert_eq!(bad.asked.load(Ordering::SeqCst), 1);
        assert_eq!(live.asked.load(Ordering::SeqCst), 2);

        // Alone: every try, then EBADRESP (node: 4 sends, "queryA EBADRESP").
        let alone = fake(Reply::Truncated).await;
        let ch = test_channel_with(&[&alone], slow());
        let err = channel_lookup(&ch, "o.example.", RecordType::A)
            .await
            .unwrap_err();
        assert_eq!(query_error_code(&err), "EBADRESP");
        assert_eq!(alone.asked.load(Ordering::SeqCst), 4);
    }

    /// Windows lets a wildcard UDP bind share a port another socket holds on
    /// 127.0.0.1, and delivers the reply to that other socket. With hickory
    /// choosing source ports from 1024-65535, about 4000 squatters on
    /// 127.0.0.1 took ~6% of its picks, so 100 clean queries happened about
    /// 0.2% of the time (the shape of conformance case 581's flake). An
    /// exclusive bind (`CaresRuntime`) fails on a held port and hickory picks
    /// again. Windows only: Linux and macOS refuse the shared bind anyway, and
    /// their default open-file limits do not fit the squatters.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_reply_never_lands_on_another_local_socket() {
        let squatters: Vec<std::net::UdpSocket> = (0..4000)
            .filter_map(|_| std::net::UdpSocket::bind("127.0.0.1:0").ok())
            .collect();
        assert!(squatters.len() > 3000, "bound {}", squatters.len());
        let live = fake(Reply::Answer {
            octet: 5,
            delay_ms: 0,
        })
        .await;
        // One try: a reply that went to a squatter is this query's timeout.
        let ch = test_channel(&[&live], 1);
        let mut failed = Vec::new();
        for i in 0..100 {
            if let Err(e) = channel_lookup(&ch, &format!("s{i}.example."), RecordType::A).await {
                failed.push(format!("s{i}: {}", query_error_code(&e)));
            }
        }
        let mut misdelivered = 0;
        let mut buf = [0u8; 1500];
        for s in &squatters {
            s.set_nonblocking(true).unwrap();
            while s.recv_from(&mut buf).is_ok() {
                misdelivered += 1;
            }
        }
        assert_eq!(misdelivered, 0, "replies delivered to squatters");
        assert!(
            failed.is_empty(),
            "queries that lost their reply: {failed:?}"
        );
        assert_eq!(live.asked.load(Ordering::SeqCst), 100);
    }

    /// Both directions of the Windows overlap: the exclusive wildcard bind
    /// fails on a port held on 127.0.0.1, and while it lives no socket can
    /// take its port on 127.0.0.1.
    #[cfg(windows)]
    #[tokio::test]
    async fn an_exclusive_bind_shares_its_port_with_no_socket() {
        let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        // A plain bind shares it (the bug's precondition)...
        assert!(std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok());
        // ...the exclusive one does not.
        let err = bind_udp_exclusive(SocketAddr::from(([0, 0, 0, 0], port))).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        let ours = bind_udp_exclusive(SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
        let port = ours.local_addr().unwrap().port();
        // Windows refuses a bind beside an exclusive socket with WSAEACCES.
        let err = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
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
        // node v22.22.2, measured: "queryA EBADNAME a..b.example.".
        assert_eq!(query_error_code(&err), "EBADNAME");
        assert_eq!(live.asked.load(Ordering::SeqCst), 0);
        assert_eq!(ch.servers[0].failures.load(Ordering::SeqCst), 0);
        // Only the unparsed name is EBADNAME, not every message error.
        assert_eq!(
            query_error_code(&NetError::Msg("something else".to_string())),
            "ESERVFAIL"
        );
    }

    #[test]
    fn character_strings_are_one_character_per_byte() {
        // node v22.22.2, measured: bytes 61 c3 a9 ff 62 in a TXT chunk and
        // a CAA value are five characters, not UTF-8.
        assert_eq!(
            one_byte_string(&[0x61, 0xc3, 0xa9, 0xff, 0x62]),
            "a\u{c3}\u{a9}\u{ff}b"
        );
        assert_eq!(one_byte_string(b""), "");
        // NAPTR's strings are printable ASCII or the answer is refused.
        assert!(naptr_string_ok(b" ~!^.*$!sip:info@example!"));
        assert!(naptr_string_ok(b""));
        for byte in [0x00, 0x01, 0x09, 0x7f, 0x80, 0xff] {
            assert!(!naptr_string_ok(&[b'a', byte]), "{byte:#x}");
        }
    }

    #[tokio::test]
    async fn each_runtime_has_its_own_default_servers() {
        // node v22.22.2, measured: a worker's dns.setServers() and
        // dns.promises.setServers() leave the main thread's lists as they
        // were.
        let main = DnsInstances::default();
        let worker = DnsInstances::default();
        let before = (
            dns_get_servers(&main, Api::Callback),
            dns_get_servers(&main, Api::Promises),
        );
        dns_set_servers(
            &worker,
            Api::Callback,
            &[("127.0.0.1".parse().unwrap(), 5399)],
        );
        assert_eq!(dns_get_servers(&worker, Api::Callback), ["127.0.0.1:5399"]);
        assert_eq!(dns_get_servers(&worker, Api::Promises), ["127.0.0.1:5399"]);
        dns_set_servers(
            &worker,
            Api::Promises,
            &[("127.0.0.1".parse().unwrap(), 5398)],
        );
        assert_eq!(dns_get_servers(&worker, Api::Callback), ["127.0.0.1:5399"]);
        assert_eq!(dns_get_servers(&worker, Api::Promises), ["127.0.0.1:5398"]);
        assert_eq!(
            (
                dns_get_servers(&main, Api::Callback),
                dns_get_servers(&main, Api::Promises),
            ),
            before
        );
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
        // Through the promises default of this test's own registry.
        let reg = DnsInstances::default();
        let nodata = fake(Reply::Empty(0)).await;
        dns_set_servers(
            &reg,
            Api::Promises,
            &[("127.0.0.1".parse().unwrap(), nodata.port)],
        );
        assert_eq!(
            dns_get_servers(&reg, Api::Promises),
            vec![format!("127.0.0.1:{}", nodata.port)]
        );
        let code = |outcome: OpOutcome| match outcome {
            OpOutcome::NodeFailed { code, .. } => code,
            _ => "not a NodeFailed".to_string(),
        };
        assert_eq!(
            code(dns_reverse(&reg, "192.0.2.5".into(), Api::Promises).await),
            "ENOTFOUND"
        );
        assert_eq!(
            code(
                dns_resolve(
                    &reg,
                    "5.2.0.192.in-addr.arpa".into(),
                    "PTR".into(),
                    Api::Promises
                )
                .await
            ),
            "ENODATA"
        );

        // An empty list: a query is c-ares's ENOSERVER, a reverse ENOTFOUND.
        dns_set_servers(&reg, Api::Promises, &[]);
        assert!(dns_get_servers(&reg, Api::Promises).is_empty());
        assert_eq!(
            code(dns_reverse(&reg, "192.0.2.5".into(), Api::Promises).await),
            "ENOTFOUND"
        );
        assert_eq!(
            code(dns_resolve(&reg, "g.example".into(), "A".into(), Api::Promises).await),
            "UNKNOWN_ARES_ERROR"
        );
    }

    #[test]
    fn result_order_is_a_stable_partition_by_family() {
        let mixed = ["6a", "4a", "6b", "4b", "6c"];
        let v4 = |s: &&str| s.starts_with('4');
        let mut verbatim = mixed;
        ResultOrder::parse("verbatim").apply(&mut verbatim, v4);
        assert_eq!(verbatim, mixed);
        let mut first4 = mixed;
        ResultOrder::parse("ipv4first").apply(&mut first4, v4);
        assert_eq!(first4, ["4a", "4b", "6a", "6b", "6c"]);
        let mut first6 = mixed;
        ResultOrder::parse("ipv6first").apply(&mut first6, v4);
        assert_eq!(first6, ["6a", "6b", "6c", "4a", "4b"]);
        assert_eq!(ResultOrder::parse(""), ResultOrder::Verbatim);
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
    async fn every_query_reaches_the_server_with_its_ttl() {
        // No answer cache (c-ares keeps none): a repeat is a datagram too, and
        // the record's TTL comes through as served (the fake serves 60).
        let reg = DnsInstances::default();
        let one = fake(Reply::Answer {
            octet: 1,
            delay_ms: 0,
        })
        .await;
        let api = Api::Instance(dns_resolver_new(&reg, -1, 4, 0));
        dns_set_servers(&reg, api, &[("127.0.0.1".parse().unwrap(), one.port)]);
        for _ in 0..3 {
            match dns_resolve(&reg, "repeat.example".into(), "A".into(), api).await {
                OpOutcome::Json(json) => assert_eq!(json, r#"[{"address":"10.0.0.1","ttl":60}]"#),
                other => panic!("expected records, got {other:?}"),
            }
        }
        assert_eq!(one.asked.load(Ordering::SeqCst), 3);
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
        let reg = DnsInstances::default();
        let one = fake(Reply::Answer {
            octet: 1,
            delay_ms: 0,
        })
        .await;
        let silent = fake(Reply::Silent).await;
        let a = Api::Instance(dns_resolver_new(&reg, -1, 4, 0));
        let b = Api::Instance(dns_resolver_new(&reg, 100, 1, 0));
        let system = dns_get_servers(&reg, Api::Instance(dns_resolver_new(&reg, -1, 4, 0)));
        dns_set_servers(&reg, a, &[("127.0.0.1".parse().unwrap(), one.port)]);
        dns_set_servers(&reg, b, &[("127.0.0.1".parse().unwrap(), silent.port)]);
        assert_eq!(
            dns_get_servers(&reg, a),
            vec![format!("127.0.0.1:{}", one.port)]
        );
        assert_eq!(
            dns_get_servers(&reg, b),
            vec![format!("127.0.0.1:{}", silent.port)]
        );
        assert_eq!(dns_get_servers(&reg, Api::Callback), system);
        match dns_resolve(&reg, "x.example".into(), "A".into(), a).await {
            OpOutcome::Json(json) => assert_eq!(json, r#"[{"address":"10.0.0.1","ttl":60}]"#),
            other => panic!("expected records, got {other:?}"),
        }

        // A cancel() after the query is issued -- before its future is even
        // polled -- fails it; the instance stays usable.
        let pending = dns_resolve(&reg, "c.example".into(), "A".into(), b);
        dns_cancel(&reg, b);
        match pending.await {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ECANCELLED"),
            other => panic!("expected ECANCELLED, got {other:?}"),
        }
        let pending = dns_reverse(&reg, "192.0.2.9".into(), b);
        dns_cancel(&reg, a); // another instance's cancel does not reach it
        match pending.await {
            OpOutcome::NodeFailed { code, .. } => assert_eq!(code, "ENOTFOUND"),
            other => panic!("expected the timeout's ENOTFOUND, got {other:?}"),
        }

        // A freed instance's id falls back to the system channel.
        let Api::Instance(id) = a else { unreachable!() };
        dns_resolver_free(&reg, id);
        assert_eq!(dns_get_servers(&reg, a), system);
    }
}
