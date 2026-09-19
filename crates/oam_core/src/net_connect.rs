//! The outbound TCP connector `net.connect` and `tls.connect` share: node's
//! connect algorithm and node's error contract, in one place.
//!
//! Node v22.22.2 (`lib/net.js` `lookupAndConnect` / `lookupAndConnectMultiple`
//! / `internalConnectMultiple`, with libuv underneath) does this for a host
//! and a port:
//!
//! 1. An IP literal skips DNS and is ONE attempt. Its `address` in an error is
//!    the string as the caller wrote it (`0:0:0:0:0:0:0:1` stays that, not
//!    `::1`).
//! 2. Otherwise getaddrinfo, in the resolver's (verbatim) order. A resolver
//!    failure is a `DNSException` (`getaddrinfo ENOTFOUND host`), never
//!    aggregated.
//! 3. The addresses are grouped by family -- the family of the FIRST address
//!    is group 0 -- deduplicated per family, and interleaved g0[0], g1[0],
//!    g0[1], ... A list that is one address after that is a single attempt
//!    with a plain error.
//! 4. Several: sequential abandon-and-advance (NOT RFC 8305 racing). Every
//!    attempt but the last is raced against the attempt timeout
//!    (`net.getDefaultAutoSelectFamilyAttemptTimeout()`, 250 ms); an elapsed
//!    one is dropped and recorded as `ETIMEDOUT`. The last attempt has no
//!    timer. First success wins; all failed is `NodeAggregateError`, one child
//!    per attempt in attempt order.
//!
//! and each attempt is libuv's: a fresh non-blocking socket; on Windows an
//! unspecified target (0.0.0.0 / ::) dialled as loopback, a pre-bind to the
//! family's unspecified address, dual-stack IPv6, and no SYN retransmit to a
//! loopback peer; a failure `connect(2)` reports at once gets node's
//! ` - Local (addr:port)` detail, one that arrives later does not.
//!
//! Resolution stays on std's getaddrinfo (tokio `lookup_host`) with no
//! AI_ADDRCONFIG, where node passes it off Windows: on a POSIX host without a
//! routable IPv6 address node may resolve `localhost` to `127.0.0.1` alone
//! while oam also tries `::1` -- a narrowed divergence, kept because passing
//! AI_ADDRCONFIG means calling getaddrinfo by hand, through new unsafe. On
//! Windows node passes no flags either, so the two agree there.

use crate::{NodeSysError, OpOutcome, node_errno, node_error_code};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Node's `autoSelectFamilyAttemptTimeout` default (src/node_options.h).
pub const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(250);

/// The floor node raises a smaller attempt timeout to (lib/net.js: "if below
/// 10, 10").
pub const MIN_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(10);

/// The attempt timeout a JS caller passed (milliseconds, as a JS number): the
/// default for anything that is not a positive finite number, floored at
/// node's 10 ms. JS already validated and clamped it (node_compat.js
/// `setDefaultAutoSelectFamilyAttemptTimeout`); this only keeps a missing or
/// garbage argument from becoming a zero timeout that abandons every attempt.
pub fn attempt_timeout_from_ms(ms: Option<f64>) -> Duration {
    match ms {
        Some(ms) if ms.is_finite() && ms > 0.0 => {
            // A JS number far past any real timeout saturates rather than
            // wrapping; `from_secs_f64` would panic on an overflow.
            let ms = ms.min(u32::MAX as f64);
            Duration::from_secs_f64(ms / 1000.0).max(MIN_ATTEMPT_TIMEOUT)
        }
        _ => DEFAULT_ATTEMPT_TIMEOUT,
    }
}

/// Per-connect knobs. Built per call: the JS side holds the single source of
/// truth for the attempt timeout and passes it with every connect.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// How long every attempt but the last may take before it is abandoned
    /// with `ETIMEDOUT`. Floored at `MIN_ATTEMPT_TIMEOUT`.
    pub attempt_timeout: Duration,
    /// Pre-resolved addresses standing in for DNS: undici's `connect.lookup`
    /// for fetch, and for net / tls the `lookup` option, a replaced
    /// `dns.lookup` or a redeemed [`resolve`] ticket. Applies only when the
    /// connect's host is `pin.host`.
    pub pin: Option<Pin>,
    /// Where each attempt's socket is bound before it dials: node's
    /// `localAddress` / `localPort`.
    pub local: Option<LocalBind>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            attempt_timeout: DEFAULT_ATTEMPT_TIMEOUT,
            pin: None,
            local: None,
        }
    }
}

/// node's `localAddress` / `localPort` connect options (lib/net.js
/// `internalConnect`, `internalConnectMultiple`): with either set, the socket
/// is bound before it dials -- to `address`, or the unspecified address of
/// the target's family (`0.0.0.0` / `::`), and `port` (0: any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBind {
    /// As the caller spelled it; node's bind error names it that way. An
    /// address of the other family than the target's -- or one that is not
    /// an address at all -- fails that attempt with `bind EINVAL`, as
    /// libuv's `uv_ip4_addr` / `uv_ip6_addr` refuse it.
    pub address: Option<String>,
    pub port: u16,
}

/// Addresses a caller resolved itself for one host.
#[derive(Debug, Clone)]
pub struct Pin {
    /// Lowercased: fetch's URL host (no URI brackets), or net / tls's host as
    /// the caller spelled it.
    pub host: String,
    pub addrs: Vec<IpAddr>,
}

/// A connected stream and every address an attempt was made to, in order.
#[derive(Debug)]
pub struct Connected {
    pub stream: tokio::net::TcpStream,
    pub attempted: Vec<SocketAddr>,
}

/// Why a connect failed, in the shape node reports it.
#[derive(Debug, Clone)]
pub enum ConnectError {
    /// getaddrinfo failed or found nothing: node's `DNSException`.
    Resolve(Box<NodeSysError>),
    /// The one attempt failed: node's `ExceptionWithHostPort`.
    Single(Box<NodeSysError>),
    /// Every attempt of a multi-address connect failed: node's
    /// `NodeAggregateError`, one child per attempt in attempt order.
    Multi(Vec<NodeSysError>),
    /// Node's ERR_INVALID_IP_ADDRESS: an empty pinned address list (the JS
    /// pin resolver refuses one first; this is the backstop).
    Invalid(Box<NodeSysError>),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Resolve(e) | ConnectError::Single(e) | ConnectError::Invalid(e) => {
                f.write_str(&e.message)
            }
            // Node's aggregate has an empty message; a Display that says
            // nothing would be useless in a log, so name every attempt.
            ConnectError::Multi(errors) => {
                f.write_str("every address failed: ")?;
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    f.write_str(&e.message)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConnectError {}

impl ConnectError {
    /// The op outcome a connect op rejects with. By reference: a caller that
    /// holds the error behind a `&dyn Error` (a transport's source chain) can
    /// still produce it.
    pub fn to_outcome(&self) -> OpOutcome {
        match self {
            ConnectError::Resolve(e) | ConnectError::Single(e) | ConnectError::Invalid(e) => {
                OpOutcome::sys(e.as_ref().clone())
            }
            ConnectError::Multi(errors) => OpOutcome::NodeAggregateFailed {
                errors: errors.clone(),
            },
        }
    }
}

/// Connect to `host`:`port` the way node's net.connect does (see the module
/// docs). `host` is used exactly as the caller has it: net and tls pass the
/// user's string, brackets and all (Windows' getaddrinfo resolves `[::1]`,
/// glibc's does not -- as in node).
pub async fn connect(
    host: &str,
    port: u16,
    opts: &ConnectOptions,
) -> Result<Connected, ConnectError> {
    let (stream, attempted) = connect_with(host, port, opts, &SystemDialer).await?;
    Ok(Connected { stream, attempted })
}

/// Name resolution ahead of a connect: what node's default `dns.lookup` hands
/// `net.connect` (lib/net.js `lookupAndConnect`), so JS can emit the socket's
/// `'lookup'` events -- and let a listener veto the connect -- before anything
/// is dialled. getaddrinfo in the resolver's order, narrowed to `family` (4 or
/// 6; anything else keeps both); a failure, or an answer the family leaves
/// empty, is the error a connect to the same name reports (`getaddrinfo
/// ENOTFOUND host`).
pub async fn resolve(host: &str, family: Option<u8>) -> Result<Vec<IpAddr>, ConnectError> {
    resolve_with(host, host, family, &SystemDialer).await
}

/// [`resolve`], with getaddrinfo handed `name` where errors name `host`.
/// node's GetAddrInfo (src/cares_wrap.cc) runs the host through UTS #46
/// ToASCII (`ada::idna::to_ascii`) before libuv sees it, so a fullwidth
/// `localhost`, or one with a soft hyphen in it, resolves as `localhost`,
/// while the error still reads `getaddrinfo CODE <host as written>`; the
/// caller passes that mapping as `name`. An empty `name` -- ToASCII refused
/// the host, or mapped it to nothing -- is the `EINVAL` libuv's
/// `uv__idna_toascii` reports for it.
pub async fn resolve_as(
    host: &str,
    name: &str,
    family: Option<u8>,
) -> Result<Vec<IpAddr>, ConnectError> {
    resolve_with(host, name, family, &SystemDialer).await
}

/// The error a lookup of an empty name reports: libuv's `UV_EINVAL`,
/// `getaddrinfo EINVAL <host as written>`.
pub fn empty_name_error(host: &str) -> NodeSysError {
    dns_error(host, "EINVAL", fallback_errno("EINVAL").unwrap_or(-22))
}

pub(crate) async fn resolve_with<D: Dialer>(
    host: &str,
    name: &str,
    family: Option<u8>,
    dialer: &D,
) -> Result<Vec<IpAddr>, ConnectError> {
    if name.is_empty() {
        return Err(ConnectError::Resolve(Box::new(empty_name_error(host))));
    }
    let resolved = dialer
        .lookup(name, 0)
        .await
        .map_err(|error| ConnectError::Resolve(Box::new(resolve_error(host, &error))))?;
    let addrs: Vec<IpAddr> = resolved
        .iter()
        .map(SocketAddr::ip)
        .filter(|ip| match family {
            Some(4) => ip.is_ipv4(),
            Some(6) => ip.is_ipv6(),
            _ => true,
        })
        .collect();
    if addrs.is_empty() {
        return Err(ConnectError::Resolve(Box::new(dns_error(
            host,
            "ENOTFOUND",
            -3008,
        ))));
    }
    Ok(addrs)
}

/// Answers [`resolve`] handed to JS, by ticket, until the connect that dials
/// them redeems the ticket ([`redeem_answer`]) or JS drops it
/// ([`drop_answer`]). A connect trusts only what is filed here -- never
/// addresses JS hands back -- so a hostname grant under `--allow-net` keeps
/// working for the default resolver, while the addresses a user's `lookup`
/// hook answers are each checked against the grant (the engine's
/// `tcpConnect` / `tlsConnect`).
///
/// An entry lives until it is redeemed, dropped, or the runtime drops: the
/// same bound as a parked fetch continuation. JS redeems or drops every
/// ticket it is given.
pub type ResolvedAnswers =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, Resolved>>>;

/// One ticket's answer: the host it was resolved for and its addresses.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// [`pin_host_key`] of the resolved host.
    host: String,
    addrs: Vec<IpAddr>,
}

/// The spelling a ticket's host is compared in: lowercased, URI brackets
/// stripped.
fn pin_host_key(host: &str) -> String {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.to_ascii_lowercase()
}

fn lock_answers(
    answers: &ResolvedAnswers,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<u64, Resolved>> {
    answers.lock().unwrap_or_else(|e| e.into_inner())
}

/// File `addrs`, resolved for `host`, under `token`.
pub fn store_answer(answers: &ResolvedAnswers, token: u64, host: &str, addrs: Vec<IpAddr>) {
    lock_answers(answers).insert(
        token,
        Resolved {
            host: pin_host_key(host),
            addrs,
        },
    );
}

/// Drop the answer under `token` (a vetoed or destroyed connect). True if it
/// was there.
pub fn drop_answer(answers: &ResolvedAnswers, token: u64) -> bool {
    lock_answers(answers).remove(&token).is_some()
}

/// How many answers are waiting to be redeemed or dropped.
pub fn pending_answers(answers: &ResolvedAnswers) -> usize {
    lock_answers(answers).len()
}

/// Redeem `token` for a connect to `host`: the pin that stands in for that
/// connect's lookup. One-shot -- the entry is consumed whether or not it
/// matches -- and bound to the host it was resolved for, so an answer for one
/// name can never be dialled under another.
pub fn redeem_answer(answers: &ResolvedAnswers, token: u64, host: &str) -> Result<Pin, String> {
    let Some(resolved) = lock_answers(answers).remove(&token) else {
        return Err(format!("resolve ticket {token} is gone"));
    };
    if resolved.host != pin_host_key(host) {
        return Err(format!(
            "resolve ticket {token} was issued for another host"
        ));
    }
    Ok(Pin {
        // connect_with matches a pin against the connect's host as the
        // caller spelled it.
        host: host.to_ascii_lowercase(),
        addrs: resolved.addrs,
    })
}

/// How one attempt failed.
#[derive(Debug)]
pub(crate) struct DialFailure {
    pub(crate) error: std::io::Error,
    /// `Some("addr:port")` for a failure `connect(2)` reported synchronously:
    /// node appends ` - Local ({local})` to that message, from getsockname
    /// (`undefined:undefined` when getsockname fails). `None` for a failure
    /// that arrived through the event loop, which node reports bare.
    pub(crate) local: Option<String>,
    /// The attempt failed binding its socket to this local address and port
    /// (node's `ExceptionWithHostPort(err, 'bind', address, port)`), before
    /// anything was dialled.
    pub(crate) bind: Option<(String, u16)>,
}

/// The two effects the algorithm has, separated so the algorithm can be
/// driven by a script in tests.
pub(crate) trait Dialer {
    type Stream;
    /// getaddrinfo, in the resolver's order.
    async fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
    /// One connect attempt to one address, its socket bound to `local`
    /// first when that is set.
    async fn dial(
        &self,
        target: SocketAddr,
        local: Option<&LocalBind>,
    ) -> Result<Self::Stream, DialFailure>;
}

/// The real network.
pub(crate) struct SystemDialer;

impl Dialer for SystemDialer {
    type Stream = tokio::net::TcpStream;

    async fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }

    async fn dial(
        &self,
        target: SocketAddr,
        local: Option<&LocalBind>,
    ) -> Result<Self::Stream, DialFailure> {
        dial(target, local).await
    }
}

/// The algorithm, over any dialer. Returns the stream and every address an
/// attempt was made to.
pub(crate) async fn connect_with<D: Dialer>(
    host: &str,
    port: u16,
    opts: &ConnectOptions,
    dialer: &D,
) -> Result<(D::Stream, Vec<SocketAddr>), ConnectError> {
    // 1. An IP literal: no DNS, one attempt, the address as written.
    if let Ok(ip) = host.parse::<IpAddr>() {
        let target = SocketAddr::new(ip, port);
        return match dialer.dial(target, opts.local.as_ref()).await {
            Ok(stream) => Ok((stream, vec![target])),
            Err(failure) => Err(ConnectError::Single(Box::new(attempt_error(
                host, port, &failure,
            )))),
        };
    }

    // 2. A pin for this host stands in for the lookup; 3. else getaddrinfo.
    let pinned = opts
        .pin
        .as_ref()
        .filter(|pin| pin.host.eq_ignore_ascii_case(host));
    let resolved: Vec<SocketAddr> = match pinned {
        Some(pin) if pin.addrs.is_empty() => {
            return Err(ConnectError::Invalid(Box::new(NodeSysError {
                code: "ERR_INVALID_IP_ADDRESS".to_string(),
                message: "Invalid IP address: undefined".to_string(),
                errno: None,
                syscall: None,
                hostname: None,
                address: None,
                port: None,
            })));
        }
        Some(pin) => pin
            .addrs
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect(),
        None => match dialer.lookup(host, port).await {
            Ok(resolved) => resolved,
            Err(error) => return Err(ConnectError::Resolve(Box::new(resolve_error(host, &error)))),
        },
    };
    if resolved.is_empty() {
        return Err(ConnectError::Resolve(Box::new(dns_error(
            host,
            "ENOTFOUND",
            -3008,
        ))));
    }

    // 4. Group, dedup, interleave; one left is a single attempt.
    let order = interleave(&resolved);
    if let [target] = order[..] {
        let address = target.ip().to_string();
        return match dialer.dial(target, opts.local.as_ref()).await {
            Ok(stream) => Ok((stream, vec![target])),
            Err(failure) => Err(ConnectError::Single(Box::new(attempt_error(
                &address, port, &failure,
            )))),
        };
    }

    // 5. Sequential attempts; every one but the last on a timer.
    let attempt_timeout = opts.attempt_timeout.max(MIN_ATTEMPT_TIMEOUT);
    let last = order.len() - 1;
    let mut attempted = Vec::with_capacity(order.len());
    let mut errors = Vec::with_capacity(order.len());
    for (i, target) in order.into_iter().enumerate() {
        attempted.push(target);
        let address = target.ip().to_string();
        let outcome = if i < last {
            match tokio::time::timeout(attempt_timeout, dialer.dial(target, opts.local.as_ref()))
                .await
            {
                Ok(outcome) => outcome,
                Err(_elapsed) => {
                    // Dropping the dial future closed its socket.
                    errors.push(connect_error(
                        "ETIMEDOUT",
                        Some(UV_ETIMEDOUT),
                        &address,
                        port,
                        None,
                    ));
                    continue;
                }
            }
        } else {
            dialer.dial(target, opts.local.as_ref()).await
        };
        match outcome {
            Ok(stream) => return Ok((stream, attempted)),
            Err(failure) => errors.push(attempt_error(&address, port, &failure)),
        }
    }
    Err(ConnectError::Multi(errors))
}

/// lib/net.js `lookupAndConnectMultiple`: the first address's family is group
/// 0; each family keeps the first occurrence of an address; the result
/// alternates g0[0], g1[0], g0[1], g1[1], ... and then runs out the longer
/// group.
fn interleave(resolved: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = resolved.first() else {
        return Vec::new();
    };
    let first_is_v4 = first.is_ipv4();
    let (mut g0, mut g1): (Vec<SocketAddr>, Vec<SocketAddr>) = (Vec::new(), Vec::new());
    for addr in resolved {
        let group = if addr.is_ipv4() == first_is_v4 {
            &mut g0
        } else {
            &mut g1
        };
        if !group.iter().any(|seen| seen.ip() == addr.ip()) {
            group.push(*addr);
        }
    }
    let mut order = Vec::with_capacity(g0.len() + g1.len());
    let (mut a, mut b) = (g0.into_iter(), g1.into_iter());
    loop {
        match (a.next(), b.next()) {
            (None, None) => break,
            (x, y) => order.extend(x.into_iter().chain(y)),
        }
    }
    order
}

/// libuv's UV_ETIMEDOUT for this platform, stamped on an abandoned attempt.
/// A constant rather than anything derived from an `io::Error`: the timeout
/// is oam's, and an error built without an OS number would lose `errno` on
/// Linux and macOS (node always sets it -- `createConnectionError(req,
/// UV_ETIMEDOUT)`).
#[cfg(windows)]
const UV_ETIMEDOUT: i32 = -4039;
#[cfg(unix)]
const UV_ETIMEDOUT: i32 = -libc::ETIMEDOUT;

/// Node's errno for a connect failure whose `io::Error` carries no OS number
/// (one oam or a library built): libuv's value for the code on this platform.
fn fallback_errno(code: &str) -> Option<i32> {
    #[cfg(windows)]
    {
        // node_errno's Windows arm is a table by NAME; an error without a raw
        // code only matters when the name is not in it.
        node_errno(code, &std::io::Error::other(code))
    }
    #[cfg(unix)]
    {
        let raw = match code {
            "ETIMEDOUT" => libc::ETIMEDOUT,
            "ECONNREFUSED" => libc::ECONNREFUSED,
            "ECONNRESET" => libc::ECONNRESET,
            "ECONNABORTED" => libc::ECONNABORTED,
            "ENETUNREACH" => libc::ENETUNREACH,
            "EHOSTUNREACH" => libc::EHOSTUNREACH,
            "EADDRNOTAVAIL" => libc::EADDRNOTAVAIL,
            "EADDRINUSE" => libc::EADDRINUSE,
            "ENOTCONN" => libc::ENOTCONN,
            "EINVAL" => libc::EINVAL,
            _ => return None,
        };
        Some(-raw)
    }
}

/// One attempt's error: node's `ExceptionWithHostPort(err, 'connect', address,
/// port, details)`.
fn attempt_error(address: &str, port: u16, failure: &DialFailure) -> NodeSysError {
    let code = node_error_code(&failure.error);
    let errno = node_errno(code, &failure.error).or_else(|| fallback_errno(code));
    if let Some((local_address, local_port)) = &failure.bind {
        return bind_error(code, errno, local_address, *local_port);
    }
    connect_error(code, errno, address, port, failure.local.as_deref())
}

/// `ExceptionWithHostPort(err, 'bind', localAddress, localPort)`: `bind CODE
/// address`, then `:port` (and a `port` key) only for a non-zero port.
fn bind_error(code: &str, errno: Option<i32>, address: &str, port: u16) -> NodeSysError {
    let mut message = format!("bind {code} {address}");
    if port > 0 {
        message.push_str(&format!(":{port}"));
    }
    NodeSysError {
        code: code.to_string(),
        message,
        errno,
        syscall: Some("bind".to_string()),
        hostname: None,
        address: Some(address.to_string()),
        port: (port > 0).then_some(port),
    }
}

/// lib/internal/errors.js `ExceptionWithHostPort`: `connect CODE address`, then
/// `:port` only for a non-zero port (and no `port` key either), then
/// ` - Local (details)` for a synchronous failure. IPv6 is unbracketed
/// (`connect ECONNREFUSED ::1:9`).
fn connect_error(
    code: &str,
    errno: Option<i32>,
    address: &str,
    port: u16,
    local: Option<&str>,
) -> NodeSysError {
    let mut message = format!("connect {code} {address}");
    if port > 0 {
        message.push_str(&format!(":{port}"));
    }
    if let Some(local) = local {
        message.push_str(&format!(" - Local ({local})"));
    }
    NodeSysError {
        code: code.to_string(),
        message,
        errno,
        syscall: Some("connect".to_string()),
        hostname: None,
        address: Some(address.to_string()),
        port: (port > 0).then_some(port),
    }
}

/// lib/internal/errors.js `DNSException`: `getaddrinfo CODE host`.
fn dns_error(host: &str, code: &str, errno: i32) -> NodeSysError {
    NodeSysError {
        code: code.to_string(),
        message: format!("getaddrinfo {code} {host}"),
        errno: Some(errno),
        syscall: Some("getaddrinfo".to_string()),
        hostname: Some(host.to_string()),
        address: None,
        port: None,
    }
}

/// A getaddrinfo failure, classified the way libuv and node do.
///
/// Windows' getaddrinfo reports a WSA code, which libuv translates
/// (src/win/getaddrinfo.c `uv__getaddrinfo_translate_error`) and node then
/// renames (errors.js `DNSException`: EAI_NODATA and EAI_NONAME are
/// `ENOTFOUND`, keeping their errno). WSANO_DATA is kept as ENOTFOUND/-3008:
/// libuv has no row for it (its generic table would say ENOENT/-4058), but it
/// could not be triggered on the dev box to confirm what node shows, so this
/// row is unverified against node.
///
/// std builds a POSIX resolver failure with no OS number, only gai_strerror's
/// text (EAI_SYSTEM excepted, which carries errno), so there the text decides:
/// glibc's EAI_NONAME "Name or service not known" / macOS's "nodename nor
/// servname provided, or not known" are ENOTFOUND/-3008 (UV_EAI_NONAME),
/// glibc's EAI_NODATA "No address associated with hostname" is
/// ENOTFOUND/-3007 (UV_EAI_NODATA), "temporary failure" (either case) is
/// EAI_AGAIN/-3001, and anything else EAI_FAIL/-3004.
pub(crate) fn resolve_error(host: &str, error: &std::io::Error) -> NodeSysError {
    let (code, errno) = classify_resolve(error);
    dns_error(host, code, errno)
}

fn classify_resolve(error: &std::io::Error) -> (&'static str, i32) {
    #[cfg(windows)]
    if let Some(raw) = error.raw_os_error() {
        use windows_sys::Win32::Networking::WinSock::{
            WSAEAFNOSUPPORT, WSAEINVAL, WSAESOCKTNOSUPPORT, WSAHOST_NOT_FOUND, WSANO_DATA,
            WSANO_RECOVERY, WSATRY_AGAIN, WSATYPE_NOT_FOUND,
        };
        return match raw {
            WSAHOST_NOT_FOUND | WSANO_DATA => ("ENOTFOUND", -3008),
            WSATRY_AGAIN => ("EAI_AGAIN", -3001),
            WSANO_RECOVERY => ("EAI_FAIL", -3004),
            WSAEINVAL => ("EAI_BADFLAGS", -3002),
            WSAEAFNOSUPPORT => ("EAI_FAMILY", -3005),
            WSATYPE_NOT_FOUND => ("EAI_SERVICE", -3010),
            WSAESOCKTNOSUPPORT => ("EAI_SOCKTYPE", -3011),
            // libuv's default: the generic system-error translation.
            _ => {
                let code = node_error_code(error);
                (code, node_errno(code, error).unwrap_or(-3004))
            }
        };
    }
    #[cfg(not(windows))]
    if error.raw_os_error().is_some() {
        // EAI_SYSTEM: libuv reports the errno itself.
        let code = node_error_code(error);
        return (code, node_errno(code, error).unwrap_or(-3004));
    }
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("name or service not known") || text.contains("nodename nor servname") {
        ("ENOTFOUND", -3008)
    } else if text.contains("no address associated with hostname") {
        ("ENOTFOUND", -3007)
    } else if text.contains("temporary failure") {
        ("EAI_AGAIN", -3001)
    } else {
        ("EAI_FAIL", -3004)
    }
}

/// One real connect attempt, libuv's way (src/win/tcp.c `uv__tcp_try_connect`,
/// src/unix/tcp.c `uv__tcp_connect`).
async fn dial(
    target: SocketAddr,
    local: Option<&LocalBind>,
) -> Result<tokio::net::TcpStream, DialFailure> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};

    let dialled = dialled_address(target);
    let socket = match Socket::new(
        Domain::for_address(dialled),
        Type::STREAM,
        Some(Protocol::TCP),
    ) {
        Ok(socket) => socket,
        // No socket, so no getsockname: node's details read
        // `undefined:undefined`.
        Err(error) => {
            return Err(DialFailure {
                error,
                local: Some("undefined:undefined".to_string()),
                bind: None,
            });
        }
    };
    let sync_failure = |socket: &Socket, error: std::io::Error| DialFailure {
        error,
        local: Some(local_details(socket)),
        bind: None,
    };
    if let Err(error) = socket.set_nonblocking(true) {
        return Err(sync_failure(&socket, error));
    }

    // node's localAddress / localPort: bound before the connect, and a bind
    // that fails is the attempt's error, named for the bind.
    if let Some(bind) = local {
        bind_local(&socket, dialled, bind)?;
    }

    #[cfg(windows)]
    {
        // libuv binds every client socket before ConnectEx, to the family's
        // unspecified address (which is also what makes getsockname answer
        // for a synchronous failure), and an IPv6 one dual-stack (IPV6_V6ONLY
        // off, failure ignored as libuv ignores it) so `::ffff:127.0.0.1`
        // connects -- unless the caller's localAddress / localPort bound it.
        if local.is_none() {
            let unspecified = if dialled.is_ipv4() {
                SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
            } else {
                let _ = socket.set_only_v6(false);
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0)
            };
            if let Err(error) = socket.bind(&SockAddr::from(unspecified)) {
                return Err(sync_failure(&socket, error));
            }
        }
        if is_loopback(dialled.ip()) {
            fail_fast_on_loopback(&socket);
        }
    }

    match socket.connect(&SockAddr::from(dialled)) {
        Ok(()) => {}
        Err(error) if connect_in_progress(&error) => {}
        // libuv on unix defers a synchronous ECONNREFUSED (a loopback peer can
        // refuse inside connect(2)) to the connect callback: it is reported
        // like any refusal, with no local detail.
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {
            return Err(DialFailure {
                error,
                local: None,
                bind: None,
            });
        }
        Err(error) => return Err(sync_failure(&socket, error)),
    }

    let async_failure = |error| DialFailure {
        error,
        local: None,
        bind: None,
    };
    let stream = tokio::net::TcpStream::from_std(std::net::TcpStream::from(socket))
        .map_err(async_failure)?;
    // mio's documented protocol for a connecting socket: wait for writable,
    // then SO_ERROR, then peer_addr (a spurious wakeup answers NotConnected /
    // EINPROGRESS: clear the readiness and wait again).
    loop {
        let ready = stream
            .ready(tokio::io::Interest::WRITABLE)
            .await
            .map_err(async_failure)?;
        if let Some(error) = stream.take_error().map_err(async_failure)? {
            return Err(async_failure(error));
        }
        match stream.peer_addr() {
            Ok(_) => return Ok(stream),
            // Closed for writing with neither SO_ERROR nor a peer: the socket
            // is not connecting and never will be. tokio's clear_readiness
            // keeps closed states, so waiting again would return at once
            // forever -- an unbounded spin, where the error is the only
            // honest outcome.
            Err(error) if ready.is_write_closed() => return Err(async_failure(error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotConnected => {}
            Err(error) if connect_in_progress(&error) => {}
            Err(error) => return Err(async_failure(error)),
        }
        let _ = stream.try_io(tokio::io::Interest::WRITABLE, || {
            Err::<(), _>(std::io::ErrorKind::WouldBlock.into())
        });
    }
}

/// Bind `socket` (about to dial `dialled`) where `bind` says, libuv's way:
/// the address must be of the target's family (`uv_ip4_addr` /
/// `uv_ip6_addr` refuse the other: EINVAL); none means the family's
/// unspecified address; on unix SO_REUSEADDR first (`uv__tcp_bind`); an
/// IPv6 socket stays dual-stack, as libuv leaves it (IPV6_V6ONLY off). The
/// failure names the address as the caller spelled it (or node's `0.0.0.0` /
/// `::`) and the port.
fn bind_local(
    socket: &socket2::Socket,
    dialled: SocketAddr,
    bind: &LocalBind,
) -> Result<(), DialFailure> {
    let named = bind.address.clone().unwrap_or_else(|| {
        if dialled.is_ipv4() {
            "0.0.0.0".to_string()
        } else {
            "::".to_string()
        }
    });
    let failure = |error: std::io::Error| DialFailure {
        error,
        local: None,
        bind: Some((named.clone(), bind.port)),
    };
    let ip = match &bind.address {
        None if dialled.is_ipv4() => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        None => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        Some(text) => match text.parse::<IpAddr>() {
            Ok(ip) if ip.is_ipv4() == dialled.is_ipv4() => ip,
            _ => return Err(failure(einval())),
        },
    };
    if ip.is_ipv6() {
        let _ = socket.set_only_v6(false);
    }
    #[cfg(unix)]
    socket.set_reuse_address(true).map_err(failure)?;
    socket
        .bind(&socket2::SockAddr::from(SocketAddr::new(ip, bind.port)))
        .map_err(failure)
}

/// The OS's EINVAL, so the error classifies as node's `EINVAL` everywhere.
fn einval() -> std::io::Error {
    #[cfg(windows)]
    {
        // WSAEINVAL.
        std::io::Error::from_raw_os_error(10022)
    }
    #[cfg(unix)]
    {
        std::io::Error::from_raw_os_error(libc::EINVAL)
    }
}

/// `connect(2)` on a non-blocking socket started rather than finished:
/// WSAEWOULDBLOCK on Windows, and only EINPROGRESS on unix. A unix EAGAIN
/// (== EWOULDBLOCK, std's WouldBlock kind) is a real synchronous failure --
/// libuv's `uv__tcp_connect` (src/unix/tcp.c) and mio's unix `connect` both
/// accept EINPROGRESS alone -- so node reports it as `connect EAGAIN ... -
/// Local (...)`. Waiting on such a socket would wait on one that is not
/// connecting.
fn connect_in_progress(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        error.kind() == std::io::ErrorKind::WouldBlock
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EINPROGRESS)
    }
}

/// Node's ` - Local (...)` detail: getsockname as `address:port`, IPv6
/// unbracketed, `undefined:undefined` when getsockname fails.
fn local_details(socket: &socket2::Socket) -> String {
    match socket.local_addr().ok().and_then(|addr| addr.as_socket()) {
        Some(local) => format!("{}:{}", local.ip(), local.port()),
        None => "undefined:undefined".to_string(),
    }
}

/// What the socket actually dials. On Windows an unspecified target is
/// dialled as loopback (libuv `uv__convert_to_localhost_if_unspecified`, as
/// Linux and macOS kernels do by themselves): node's fetch, http.get and
/// net.connect to 0.0.0.0 reach a 127.0.0.1 listener, where a plain Winsock
/// connect fails with WSAEADDRNOTAVAIL. The error still names the address as
/// the caller gave it.
fn dialled_address(target: SocketAddr) -> SocketAddr {
    #[cfg(windows)]
    {
        let ip = match target.ip() {
            IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        let mut dialled = target;
        dialled.set_ip(ip);
        dialled
    }
    #[cfg(not(windows))]
    {
        target
    }
}

/// libuv's `uv__is_loopback`: 127.0.0.0/8 or exactly `::1`. A v4-mapped
/// `::ffff:127.0.0.1` is deliberately not included, as it is not in libuv.
#[cfg(windows)]
fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Ask the stack not to retransmit this socket's SYN, so a loopback peer that
/// answers RST fails the connect immediately instead of after the ~2 s SYN
/// retransmit cycle every other Windows connect path waits (#137). libuv does
/// exactly this in `uv__tcp_try_connect` (src/win/tcp.c -- SIO_TCP_INITIAL_RTO
/// with `MaxSynRetransmissions = TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS`,
/// loopback only, on the address it actually dials, result ignored), so node
/// reports ECONNREFUSED on a closed 127.0.0.1 port in single-digit
/// milliseconds. Windows 10 1709+; older kernels reject the ioctl, which is
/// ignored exactly as libuv ignores it.
#[cfg(windows)]
fn fail_fast_on_loopback(socket: &socket2::Socket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SIO_TCP_INITIAL_RTO, SOCKET, TCP_INITIAL_RTO_DEFAULT_RTT,
        TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS, TCP_INITIAL_RTO_PARAMETERS, WSAIoctl,
    };
    // mstcpip.h types TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS as `(UCHAR)-2` for
    // the UCHAR field; windows-sys publishes it widened to u16, hence the cast
    // back. TCP_INITIAL_RTO_DEFAULT_RTT (0) keeps the kernel's RTT estimate.
    let params = TCP_INITIAL_RTO_PARAMETERS {
        Rtt: TCP_INITIAL_RTO_DEFAULT_RTT as u16,
        MaxSynRetransmissions: TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS as u8,
    };
    let mut returned: u32 = 0;
    // SAFETY: a synchronous WSAIoctl on a socket `socket` owns for the whole
    // call: the input buffer points at `params`, a live, correctly sized
    // TCP_INITIAL_RTO_PARAMETERS; there is no output buffer (null, 0); the
    // byte count goes to the live `returned`; no OVERLAPPED and no completion
    // routine are passed, so nothing is touched after the call returns.
    let _ = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as SOCKET,
            SIO_TCP_INITIAL_RTO,
            (&params as *const TCP_INITIAL_RTO_PARAMETERS).cast(),
            std::mem::size_of::<TCP_INITIAL_RTO_PARAMETERS>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Mutex;

    /// What a scripted attempt does.
    #[derive(Clone, Copy)]
    enum Outcome {
        Connect,
        /// Refused through the event loop (no local detail).
        Refuse,
        /// Failed inside connect(2) itself, with this getsockname detail.
        SyncFail(io::ErrorKind, &'static str),
        /// Never completes.
        Hang,
    }

    #[derive(Clone, Copy)]
    struct Dial {
        delay: Duration,
        outcome: Outcome,
    }

    const fn now(outcome: Outcome) -> Dial {
        Dial {
            delay: Duration::ZERO,
            outcome,
        }
    }

    struct Script {
        /// None: the lookup fails with gai_strerror text `lookup_error`.
        resolved: Option<Vec<IpAddr>>,
        lookup_error: &'static str,
        dials: Vec<(IpAddr, Dial)>,
        lookups: Mutex<Vec<String>>,
        dialled: Mutex<Vec<SocketAddr>>,
    }

    impl Script {
        fn new(resolved: &[&str], dials: &[(&str, Dial)]) -> Script {
            Script {
                resolved: Some(resolved.iter().map(|ip| ip.parse().unwrap()).collect()),
                lookup_error: "unused",
                dials: dials
                    .iter()
                    .map(|(ip, dial)| (ip.parse().unwrap(), *dial))
                    .collect(),
                lookups: Mutex::new(Vec::new()),
                dialled: Mutex::new(Vec::new()),
            }
        }

        fn failing_lookup(error: &'static str) -> Script {
            Script {
                resolved: None,
                lookup_error: error,
                ..Script::new(&[], &[])
            }
        }

        fn dialled(&self) -> Vec<String> {
            self.dialled
                .lock()
                .unwrap()
                .iter()
                .map(|addr| addr.ip().to_string())
                .collect()
        }
    }

    impl Dialer for Script {
        /// The address that answered.
        type Stream = SocketAddr;

        async fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.lookups.lock().unwrap().push(host.to_string());
            match &self.resolved {
                Some(ips) => Ok(ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect()),
                None => Err(gai(self.lookup_error)),
            }
        }

        async fn dial(
            &self,
            target: SocketAddr,
            _local: Option<&LocalBind>,
        ) -> Result<SocketAddr, DialFailure> {
            self.dialled.lock().unwrap().push(target);
            let dial = self
                .dials
                .iter()
                .find(|(ip, _)| *ip == target.ip())
                .map(|(_, dial)| *dial)
                .unwrap_or(now(Outcome::Refuse));
            tokio::time::sleep(dial.delay).await;
            match dial.outcome {
                Outcome::Connect => Ok(target),
                Outcome::Refuse => Err(DialFailure {
                    error: io::ErrorKind::ConnectionRefused.into(),
                    local: None,
                    bind: None,
                }),
                Outcome::SyncFail(kind, local) => Err(DialFailure {
                    error: kind.into(),
                    local: Some(local.to_string()),
                    bind: None,
                }),
                Outcome::Hang => std::future::pending().await,
            }
        }
    }

    fn opts(attempt_ms: u64) -> ConnectOptions {
        ConnectOptions {
            attempt_timeout: Duration::from_millis(attempt_ms),
            pin: None,
            local: None,
        }
    }

    /// libuv's number for a code on this platform.
    fn uv(code: &str) -> i32 {
        let (win, linux, mac) = match code {
            "ECONNREFUSED" => (-4078, -111, -61),
            "ETIMEDOUT" => (-4039, -110, -60),
            "EADDRNOTAVAIL" => (-4090, -99, -49),
            other => panic!("no fixture for {other}"),
        };
        if cfg!(windows) {
            win
        } else if cfg!(target_os = "macos") {
            mac
        } else {
            linux
        }
    }

    fn messages(errors: &[NodeSysError]) -> Vec<String> {
        errors.iter().map(|e| e.message.clone()).collect()
    }

    fn gai(text: &'static str) -> io::Error {
        io::Error::other(format!("failed to lookup address information: {text}"))
    }

    #[tokio::test]
    async fn addresses_are_grouped_by_the_first_family_deduplicated_and_interleaved() {
        // Measured on node: [127.0.0.1, 127.0.0.2, ::1, ::1] is attempted as
        // 127.0.0.1, ::1, 127.0.0.2 -- the duplicate ::1 dropped, v4 first
        // because the first address is v4.
        let script = Script::new(&["127.0.0.1", "127.0.0.2", "::1", "::1"], &[]);
        let Err(ConnectError::Multi(errors)) =
            connect_with("multi.example", 8080, &opts(250), &script).await
        else {
            panic!("expected an aggregate");
        };
        assert_eq!(script.dialled(), ["127.0.0.1", "::1", "127.0.0.2"]);
        assert_eq!(
            messages(&errors),
            [
                "connect ECONNREFUSED 127.0.0.1:8080",
                "connect ECONNREFUSED ::1:8080",
                "connect ECONNREFUSED 127.0.0.2:8080",
            ]
        );
        assert_eq!(
            errors[1],
            NodeSysError {
                code: "ECONNREFUSED".to_string(),
                message: "connect ECONNREFUSED ::1:8080".to_string(),
                errno: Some(uv("ECONNREFUSED")),
                syscall: Some("connect".to_string()),
                hostname: None,
                address: Some("::1".to_string()),
                port: Some(8080),
            }
        );

        // A v6-first list puts v6 in group 0; the longer group runs out last.
        let script = Script::new(&["::1", "127.0.0.1", "127.0.0.2", "127.0.0.3"], &[]);
        let _ = connect_with("multi.example", 1, &opts(250), &script).await;
        assert_eq!(
            script.dialled(),
            ["::1", "127.0.0.1", "127.0.0.2", "127.0.0.3"]
        );
    }

    #[tokio::test]
    async fn the_first_success_wins_and_reports_what_was_attempted() {
        let script = Script::new(
            &["::1", "127.0.0.1"],
            &[("127.0.0.1", now(Outcome::Connect))],
        );
        let (answered, attempted) = connect_with("localhost", 80, &opts(250), &script)
            .await
            .expect("connects through the second address");
        assert_eq!(answered.ip().to_string(), "127.0.0.1");
        let attempted: Vec<String> = attempted.iter().map(|a| a.to_string()).collect();
        assert_eq!(attempted, ["[::1]:80", "127.0.0.1:80"]);
    }

    #[tokio::test]
    async fn a_list_that_dedups_to_one_address_is_a_single_plain_attempt() {
        let script = Script::new(&["127.0.0.1", "127.0.0.1"], &[]);
        let Err(ConnectError::Single(error)) =
            connect_with("one.example", 9, &opts(250), &script).await
        else {
            panic!("expected a plain error");
        };
        assert_eq!(error.message, "connect ECONNREFUSED 127.0.0.1:9");
        assert_eq!(script.dialled(), ["127.0.0.1"]);
    }

    #[tokio::test]
    async fn an_abandoned_attempt_is_etimedout_with_the_platform_errno() {
        let script = Script::new(&["192.0.2.1", "::1"], &[("192.0.2.1", now(Outcome::Hang))]);
        let started = std::time::Instant::now();
        let Err(ConnectError::Multi(errors)) =
            connect_with("blackhole.example", 81, &opts(10), &script).await
        else {
            panic!("expected an aggregate");
        };
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            messages(&errors),
            [
                "connect ETIMEDOUT 192.0.2.1:81",
                "connect ECONNREFUSED ::1:81"
            ]
        );
        assert_eq!(errors[0].code, "ETIMEDOUT");
        // Never derived from an io::Error: present on every platform.
        assert_eq!(errors[0].errno, Some(uv("ETIMEDOUT")));
        assert_eq!(errors[0].port, Some(81));
    }

    #[tokio::test]
    async fn the_last_attempt_has_no_timer() {
        // The last attempt outlives the attempt timeout many times over and
        // still reports its own outcome, not ETIMEDOUT.
        let slow = |outcome| Dial {
            delay: Duration::from_millis(150),
            outcome,
        };
        let script = Script::new(
            &["::1", "127.0.0.1"],
            &[("127.0.0.1", slow(Outcome::Refuse))],
        );
        let Err(ConnectError::Multi(errors)) =
            connect_with("localhost", 5, &opts(10), &script).await
        else {
            panic!("expected an aggregate");
        };
        assert_eq!(
            messages(&errors),
            [
                "connect ECONNREFUSED ::1:5",
                "connect ECONNREFUSED 127.0.0.1:5"
            ]
        );

        let script = Script::new(
            &["::1", "127.0.0.1"],
            &[("127.0.0.1", slow(Outcome::Connect))],
        );
        assert!(
            connect_with("localhost", 5, &opts(10), &script)
                .await
                .is_ok()
        );

        // A single attempt has no timer either.
        let script = Script::new(&["127.0.0.1"], &[("127.0.0.1", slow(Outcome::Connect))]);
        assert!(
            connect_with("localhost", 5, &opts(10), &script)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_attempt_timeout_is_floored_at_ten_ms() {
        // A zero timeout would abandon an attempt that answers in 2 ms.
        let quick = Dial {
            delay: Duration::from_millis(2),
            outcome: Outcome::Connect,
        };
        let script = Script::new(&["::1", "127.0.0.1"], &[("::1", quick)]);
        let (answered, _) = connect_with("localhost", 5, &opts(0), &script)
            .await
            .expect("the first attempt is not abandoned");
        assert_eq!(answered.ip().to_string(), "::1");

        assert_eq!(attempt_timeout_from_ms(None), DEFAULT_ATTEMPT_TIMEOUT);
        assert_eq!(
            attempt_timeout_from_ms(Some(f64::NAN)),
            DEFAULT_ATTEMPT_TIMEOUT
        );
        assert_eq!(attempt_timeout_from_ms(Some(-1.0)), DEFAULT_ATTEMPT_TIMEOUT);
        assert_eq!(attempt_timeout_from_ms(Some(5.0)), MIN_ATTEMPT_TIMEOUT);
        assert_eq!(
            attempt_timeout_from_ms(Some(300.0)),
            Duration::from_millis(300)
        );
        assert!(attempt_timeout_from_ms(Some(f64::MAX)) > Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_pin_stands_in_for_the_lookup_of_its_own_host_only() {
        let pin = Pin {
            host: "pinned.example".to_string(),
            addrs: vec!["127.0.0.2".parse().unwrap(), "127.0.0.1".parse().unwrap()],
        };
        let pinned = ConnectOptions {
            attempt_timeout: Duration::from_millis(250),
            pin: Some(pin.clone()),
            local: None,
        };
        let script = Script::new(&["::1"], &[("127.0.0.1", now(Outcome::Connect))]);
        let (answered, attempted) = connect_with("Pinned.Example", 443, &pinned, &script)
            .await
            .expect("connects through the pinned list");
        assert_eq!(answered.ip().to_string(), "127.0.0.1");
        assert_eq!(attempted.len(), 2);
        assert!(
            script.lookups.lock().unwrap().is_empty(),
            "no DNS for a pinned host"
        );

        // Another host resolves normally.
        let script = Script::new(&["::1"], &[]);
        let _ = connect_with("other.example", 443, &pinned, &script).await;
        assert_eq!(*script.lookups.lock().unwrap(), ["other.example"]);
        assert_eq!(script.dialled(), ["::1"]);

        // An empty pinned list is node's ERR_INVALID_IP_ADDRESS.
        let empty = ConnectOptions {
            pin: Some(Pin {
                addrs: Vec::new(),
                ..pin
            }),
            ..pinned
        };
        let script = Script::new(&["::1"], &[]);
        let Err(ConnectError::Invalid(error)) =
            connect_with("pinned.example", 443, &empty, &script).await
        else {
            panic!("expected ERR_INVALID_IP_ADDRESS");
        };
        assert_eq!(error.code, "ERR_INVALID_IP_ADDRESS");
        assert_eq!(error.message, "Invalid IP address: undefined");
        assert!(script.dialled().is_empty());
    }

    #[tokio::test]
    async fn an_ip_literal_skips_dns_and_is_named_as_written() {
        let script = Script::new(&["127.0.0.1"], &[]);
        for (host, message) in [
            ("0:0:0:0:0:0:0:1", "connect ECONNREFUSED 0:0:0:0:0:0:0:1:9"),
            (
                "::FFFF:127.0.0.1",
                "connect ECONNREFUSED ::FFFF:127.0.0.1:9",
            ),
            ("127.0.0.1", "connect ECONNREFUSED 127.0.0.1:9"),
        ] {
            let Err(ConnectError::Single(error)) = connect_with(host, 9, &opts(250), &script).await
            else {
                panic!("expected a plain error for {host}");
            };
            assert_eq!(error.message, message);
            assert_eq!(error.address.as_deref(), Some(host));
        }
        assert!(script.lookups.lock().unwrap().is_empty());
        // A pin never applies to an IP literal.
        let pinned = ConnectOptions {
            pin: Some(Pin {
                host: "127.0.0.1".to_string(),
                addrs: vec!["::1".parse().unwrap()],
            }),
            ..opts(250)
        };
        let _ = connect_with("127.0.0.1", 9, &pinned, &script).await;
        assert_eq!(
            script.dialled().last().map(String::as_str),
            Some("127.0.0.1")
        );
    }

    #[tokio::test]
    async fn port_zero_has_no_port_and_a_synchronous_failure_names_the_local_end() {
        // Measured on node (Windows): net.connect({host:'127.0.0.1', port:0})
        // -> `connect EADDRNOTAVAIL 127.0.0.1 - Local (0.0.0.0:55035)`, keys
        // errno, code, syscall, address.
        let sync = now(Outcome::SyncFail(
            io::ErrorKind::AddrNotAvailable,
            "0.0.0.0:55035",
        ));
        let script = Script::new(&[], &[("127.0.0.1", sync)]);
        let Err(ConnectError::Single(error)) =
            connect_with("127.0.0.1", 0, &opts(250), &script).await
        else {
            panic!("expected a plain error");
        };
        assert_eq!(
            *error,
            NodeSysError {
                code: "EADDRNOTAVAIL".to_string(),
                message: "connect EADDRNOTAVAIL 127.0.0.1 - Local (0.0.0.0:55035)".to_string(),
                errno: Some(uv("EADDRNOTAVAIL")),
                syscall: Some("connect".to_string()),
                hostname: None,
                address: Some("127.0.0.1".to_string()),
                port: None,
            }
        );
        // Inside an aggregate too; an asynchronous failure has no detail.
        let sync = now(Outcome::SyncFail(
            io::ErrorKind::AddrNotAvailable,
            ":::55036",
        ));
        let script = Script::new(&["::1", "127.0.0.1"], &[("::1", sync)]);
        let Err(ConnectError::Multi(errors)) =
            connect_with("localhost", 0, &opts(250), &script).await
        else {
            panic!("expected an aggregate");
        };
        assert_eq!(
            messages(&errors),
            [
                "connect EADDRNOTAVAIL ::1 - Local (:::55036)",
                "connect ECONNREFUSED 127.0.0.1"
            ]
        );
        assert!(errors.iter().all(|e| e.port.is_none()));
    }

    #[tokio::test]
    async fn resolver_failures_are_dns_exceptions() {
        let cases = [
            ("Name or service not known", "ENOTFOUND", -3008),
            (
                "nodename nor servname provided, or not known",
                "ENOTFOUND",
                -3008,
            ),
            ("No address associated with hostname", "ENOTFOUND", -3007),
            ("temporary failure in name resolution", "EAI_AGAIN", -3001),
            (
                "Non-recoverable failure in name resolution",
                "EAI_FAIL",
                -3004,
            ),
        ];
        for (error, code, errno) in cases {
            let script = Script::failing_lookup(error);
            let Err(ConnectError::Resolve(e)) =
                connect_with("nowhere.invalid", 80, &opts(250), &script).await
            else {
                panic!("expected a resolver error for {code}");
            };
            assert_eq!(
                *e,
                NodeSysError {
                    code: code.to_string(),
                    message: format!("getaddrinfo {code} nowhere.invalid"),
                    errno: Some(errno),
                    syscall: Some("getaddrinfo".to_string()),
                    hostname: Some("nowhere.invalid".to_string()),
                    address: None,
                    port: None,
                }
            );
            assert!(script.dialled().is_empty());
        }

        // Zero addresses is ENOTFOUND too.
        let script = Script::new(&[], &[]);
        let Err(ConnectError::Resolve(e)) =
            connect_with("empty.invalid", 80, &opts(250), &script).await
        else {
            panic!("expected a resolver error");
        };
        assert_eq!((e.code.as_str(), e.errno), ("ENOTFOUND", Some(-3008)));
    }

    /// Only the platform's own in-progress code means a connect is under way.
    /// A unix EAGAIN is a synchronous failure (libuv, mio); treating it as in
    /// progress waited on a socket that was never connecting.
    #[test]
    fn only_the_platform_in_progress_code_means_connecting() {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Networking::WinSock::{WSAECONNREFUSED, WSAEWOULDBLOCK};
            assert!(connect_in_progress(&io::Error::from_raw_os_error(
                WSAEWOULDBLOCK
            )));
            assert!(!connect_in_progress(&io::Error::from_raw_os_error(
                WSAECONNREFUSED
            )));
        }
        #[cfg(unix)]
        {
            assert!(connect_in_progress(&io::Error::from_raw_os_error(
                libc::EINPROGRESS
            )));
            assert!(!connect_in_progress(&io::Error::from_raw_os_error(
                libc::EAGAIN
            )));
            assert!(!connect_in_progress(&io::Error::from_raw_os_error(
                libc::ECONNREFUSED
            )));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolver_codes_follow_libuv() {
        use windows_sys::Win32::Networking::WinSock::{
            WSAHOST_NOT_FOUND, WSANO_DATA, WSANO_RECOVERY, WSATRY_AGAIN,
        };
        for (raw, code, errno) in [
            (WSAHOST_NOT_FOUND, "ENOTFOUND", -3008),
            (WSANO_DATA, "ENOTFOUND", -3008),
            (WSATRY_AGAIN, "EAI_AGAIN", -3001),
            (WSANO_RECOVERY, "EAI_FAIL", -3004),
        ] {
            let e = resolve_error("host.invalid", &io::Error::from_raw_os_error(raw));
            assert_eq!((e.code.as_str(), e.errno), (code, Some(errno)));
            assert_eq!(e.message, format!("getaddrinfo {code} host.invalid"));
        }
    }

    #[test]
    fn outcomes_carry_the_whole_shape() {
        let single = ConnectError::Single(Box::new(connect_error(
            "ECONNREFUSED",
            Some(-4078),
            "127.0.0.1",
            8080,
            None,
        )));
        match single.to_outcome() {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                path,
                errno,
                hostname,
                address,
                port,
            } => {
                assert_eq!(code, "ECONNREFUSED");
                assert_eq!(message, "connect ECONNREFUSED 127.0.0.1:8080");
                assert_eq!(syscall.as_deref(), Some("connect"));
                assert_eq!((path, hostname), (None, None));
                assert_eq!(errno, Some(-4078));
                assert_eq!((address.as_deref(), port), (Some("127.0.0.1"), Some(8080)));
            }
            other => panic!("expected NodeFailed, got {other:?}"),
        }
        assert_eq!(single.to_string(), "connect ECONNREFUSED 127.0.0.1:8080");

        let children = vec![
            connect_error("ECONNREFUSED", Some(-4078), "::1", 80, None),
            connect_error("ECONNREFUSED", Some(-4078), "127.0.0.1", 80, None),
        ];
        let multi = ConnectError::Multi(children.clone());
        assert!(matches!(
            multi.to_outcome(),
            OpOutcome::NodeAggregateFailed { errors } if errors == children
        ));
        assert_eq!(
            multi.to_string(),
            "every address failed: connect ECONNREFUSED ::1:80; connect ECONNREFUSED 127.0.0.1:80"
        );

        let resolve = ConnectError::Resolve(Box::new(dns_error("x.invalid", "ENOTFOUND", -3008)));
        assert!(matches!(
            resolve.to_outcome(),
            OpOutcome::NodeFailed { hostname: Some(h), address: None, port: None, .. } if h == "x.invalid"
        ));
        // Downcastable out of a boxed error chain (the fetch connector's path).
        let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(resolve);
        assert!(boxed.downcast_ref::<ConnectError>().is_some());
    }

    // ------------------------------------------------------------ real sockets

    /// A port that was listening a moment ago and is closed now.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn a_refused_loopback_connect_is_a_prompt_plain_econnrefused() {
        let port = closed_port().await;
        let started = std::time::Instant::now();
        let Err(ConnectError::Single(error)) = connect("127.0.0.1", port, &opts(250)).await else {
            panic!("expected a plain refusal");
        };
        // #137: no ~2 s SYN retransmit cycle on Windows.
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(error.code, "ECONNREFUSED");
        assert_eq!(error.errno, Some(uv("ECONNREFUSED")));
        assert_eq!(
            error.message,
            format!("connect ECONNREFUSED 127.0.0.1:{port}")
        );
    }

    #[tokio::test]
    async fn real_connects_reach_a_loopback_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = tokio::spawn(async move {
            for _ in 0..2 {
                let _ = listener.accept().await.unwrap();
            }
        });
        let connected = connect("127.0.0.1", port, &opts(250))
            .await
            .expect("connects");
        assert_eq!(connected.stream.peer_addr().unwrap().port(), port);
        assert_eq!(
            connected.attempted,
            [SocketAddr::from(([127, 0, 0, 1], port))]
        );
        // An unspecified target reaches the loopback listener (libuv rewrites
        // it on Windows; the kernel does elsewhere).
        let connected = connect("0.0.0.0", port, &opts(250))
            .await
            .expect("connects");
        assert_eq!(
            connected.stream.peer_addr().unwrap().ip(),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        accept.await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_port_zero_fails_synchronously_with_the_local_detail() {
        let Err(ConnectError::Single(error)) = connect("127.0.0.1", 0, &opts(250)).await else {
            panic!("expected a plain error");
        };
        assert_eq!(error.code, "EADDRNOTAVAIL");
        assert_eq!(error.port, None);
        let prefix = "connect EADDRNOTAVAIL 127.0.0.1 - Local (0.0.0.0:";
        assert!(error.message.starts_with(prefix), "{}", error.message);
    }

    /// A real blackhole: depends on the network (a host with no default route
    /// fails at once with ENETUNREACH instead), so it only runs on request.
    #[tokio::test]
    #[ignore]
    async fn probe_blackhole_then_refused_loopback() {
        let port = closed_port().await;
        let pinned = ConnectOptions {
            attempt_timeout: Duration::from_millis(250),
            pin: Some(Pin {
                host: "blackhole.example".to_string(),
                addrs: vec!["192.0.2.1".parse().unwrap(), "127.0.0.1".parse().unwrap()],
            }),
            local: None,
        };
        let Err(ConnectError::Multi(errors)) = connect("blackhole.example", port, &pinned).await
        else {
            panic!("expected an aggregate");
        };
        assert_eq!(errors[0].code, "ETIMEDOUT");
        assert_eq!(errors[1].code, "ECONNREFUSED");
    }

    #[tokio::test]
    async fn resolve_keeps_the_resolver_order_and_filters_by_family() {
        let script = Script::new(&["::1", "127.0.0.1", "::2"], &[]);
        let all = resolve_with("dual.example", "dual.example", None, &script)
            .await
            .unwrap();
        let all: Vec<String> = all.iter().map(|ip| ip.to_string()).collect();
        assert_eq!(all, ["::1", "127.0.0.1", "::2"]);
        // A family that is neither 4 nor 6 keeps both, as dns.lookup does
        // for family 0.
        let zero = resolve_with("dual.example", "dual.example", Some(0), &script)
            .await
            .unwrap();
        assert_eq!(zero.len(), 3);
        let v4 = resolve_with("dual.example", "dual.example", Some(4), &script)
            .await
            .unwrap();
        assert_eq!(v4, ["127.0.0.1".parse::<IpAddr>().unwrap()]);
        let v6 = resolve_with("dual.example", "dual.example", Some(6), &script)
            .await
            .unwrap();
        assert_eq!(v6.len(), 2);
        assert!(script.dialled().is_empty(), "resolving never dials");
    }

    /// localAddress / localPort: the socket is bound before it dials, so the
    /// peer sees the connection come from there; a bind that fails is the
    /// attempt's error in node's `bind CODE address[:port]` shape.
    #[tokio::test]
    async fn a_local_bind_is_where_the_connection_comes_from() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_port = free.local_addr().unwrap().port();
        drop(free);
        let bound = |address: Option<&str>, port: u16| ConnectOptions {
            local: Some(LocalBind {
                address: address.map(str::to_string),
                port,
            }),
            ..ConnectOptions::default()
        };

        let connected = connect("127.0.0.1", port, &bound(Some("127.0.0.1"), local_port))
            .await
            .unwrap();
        let (_accepted, peer) = listener.accept().await.unwrap();
        assert_eq!(peer.port(), local_port);
        assert_eq!(connected.stream.local_addr().unwrap().port(), local_port);

        // Not an address of this host.
        let Err(ConnectError::Single(error)) =
            connect("127.0.0.1", port, &bound(Some("192.0.2.1"), 0)).await
        else {
            panic!("expected the bind to fail");
        };
        assert_eq!(error.code, "EADDRNOTAVAIL");
        assert_eq!(error.syscall.as_deref(), Some("bind"));
        assert_eq!(error.message, "bind EADDRNOTAVAIL 192.0.2.1");
        assert_eq!(error.address.as_deref(), Some("192.0.2.1"));
        assert_eq!(error.port, None);

        // The other family than the target's: libuv's EINVAL, with the port.
        let Err(ConnectError::Single(error)) =
            connect("127.0.0.1", port, &bound(Some("::1"), 40123)).await
        else {
            panic!("expected EINVAL");
        };
        assert_eq!(error.code, "EINVAL");
        assert_eq!(error.message, "bind EINVAL ::1:40123");
        assert_eq!(error.port, Some(40123));
        #[cfg(windows)]
        assert_eq!(error.errno, Some(-4071));
        #[cfg(unix)]
        assert_eq!(error.errno, Some(-libc::EINVAL));
    }

    #[tokio::test]
    async fn resolve_looks_up_the_mapped_name_and_reports_the_host_as_written() {
        // getaddrinfo is handed the ToASCII form; errors name the host.
        let script = Script::new(&["127.0.0.1"], &[]);
        let found = resolve_with("LOC\u{AD}ALHOST", "localhost", None, &script)
            .await
            .unwrap();
        assert_eq!(found, ["127.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(*script.lookups.lock().unwrap(), ["localhost"]);

        let script = Script::failing_lookup("Name or service not known");
        let Err(ConnectError::Resolve(error)) = resolve_with(
            "b\u{FC}cher.invalid",
            "xn--bcher-kva.invalid",
            None,
            &script,
        )
        .await
        else {
            panic!("expected a resolver error");
        };
        assert_eq!(error.message, "getaddrinfo ENOTFOUND b\u{FC}cher.invalid");
        assert_eq!(error.hostname.as_deref(), Some("b\u{FC}cher.invalid"));

        // ToASCII refused the host (or mapped it to nothing): libuv's EINVAL,
        // and nothing is looked up.
        let script = Script::new(&["127.0.0.1"], &[]);
        let Err(ConnectError::Resolve(error)) = resolve_with("\u{AD}", "", None, &script).await
        else {
            panic!("expected EINVAL");
        };
        assert_eq!(error.code, "EINVAL");
        assert_eq!(error.message, "getaddrinfo EINVAL \u{AD}");
        #[cfg(windows)]
        assert_eq!(error.errno, Some(-4071));
        #[cfg(unix)]
        assert_eq!(error.errno, Some(-libc::EINVAL));
        assert!(script.lookups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_fails_as_a_connect_to_the_same_name_would() {
        // A family the name has no address in: getaddrinfo ENOTFOUND.
        let script = Script::new(&["127.0.0.1"], &[]);
        let Err(ConnectError::Resolve(error)) =
            resolve_with("v4only.example", "v4only.example", Some(6), &script).await
        else {
            panic!("expected a resolver error");
        };
        assert_eq!(error.code, "ENOTFOUND");
        assert_eq!(error.errno, Some(-3008));
        assert_eq!(error.message, "getaddrinfo ENOTFOUND v4only.example");
        assert_eq!(error.hostname.as_deref(), Some("v4only.example"));

        // A resolver failure is classified exactly as connect classifies it.
        let script = Script::failing_lookup("Name or service not known");
        let Err(ConnectError::Resolve(resolved)) =
            resolve_with("nx.example", "nx.example", None, &script).await
        else {
            panic!("expected a resolver error");
        };
        let Err(ConnectError::Resolve(connected)) =
            connect_with("nx.example", 80, &opts(250), &script).await
        else {
            panic!("expected a resolver error");
        };
        assert_eq!(resolved, connected);
    }

    fn answers() -> ResolvedAnswers {
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    #[test]
    fn a_ticket_is_redeemed_once_for_its_own_host() {
        let answers = answers();
        let addrs: Vec<IpAddr> = vec!["::1".parse().unwrap(), "127.0.0.1".parse().unwrap()];
        store_answer(&answers, 7, "Guard.Test", addrs.clone());
        assert_eq!(pending_answers(&answers), 1);
        // Case-insensitive, and the pin carries the connect's spelling.
        let pin = redeem_answer(&answers, 7, "GUARD.test").expect("redeems");
        assert_eq!(pin.host, "guard.test");
        assert_eq!(pin.addrs, addrs);
        assert_eq!(pending_answers(&answers), 0);
        // One-shot.
        let again = redeem_answer(&answers, 7, "guard.test").unwrap_err();
        assert_eq!(again, "resolve ticket 7 is gone");
    }

    #[test]
    fn a_ticket_for_one_host_is_consumed_and_refused_for_another() {
        let answers = answers();
        store_answer(&answers, 3, "a.test", vec!["127.0.0.1".parse().unwrap()]);
        let err = redeem_answer(&answers, 3, "b.test").unwrap_err();
        assert_eq!(err, "resolve ticket 3 was issued for another host");
        // The mismatch consumed it: a retry with the right host finds nothing.
        assert!(redeem_answer(&answers, 3, "a.test").is_err());
        assert_eq!(pending_answers(&answers), 0);
    }

    #[test]
    fn a_bracketed_host_matches_its_bare_spelling() {
        let answers = answers();
        store_answer(&answers, 1, "[::1]", vec!["::1".parse().unwrap()]);
        let pin = redeem_answer(&answers, 1, "::1").unwrap();
        assert_eq!(pin.host, "::1");
        store_answer(&answers, 2, "::1", vec!["::1".parse().unwrap()]);
        assert!(redeem_answer(&answers, 2, "[::1]").is_ok());
    }

    #[test]
    fn a_dropped_ticket_cannot_be_redeemed() {
        let answers = answers();
        store_answer(&answers, 9, "a.test", vec!["127.0.0.1".parse().unwrap()]);
        assert!(drop_answer(&answers, 9));
        assert!(!drop_answer(&answers, 9));
        assert!(redeem_answer(&answers, 9, "a.test").is_err());
    }

    #[tokio::test]
    async fn a_redeemed_ticket_drives_the_connect_like_a_lookup() {
        // The ticket's list goes through the same grouping and interleaving
        // a getaddrinfo answer would.
        let answers = answers();
        store_answer(
            &answers,
            5,
            "ticket.example",
            vec![
                "127.0.0.1".parse().unwrap(),
                "127.0.0.2".parse().unwrap(),
                "::1".parse().unwrap(),
            ],
        );
        let pin = redeem_answer(&answers, 5, "ticket.example").unwrap();
        let pinned = ConnectOptions {
            attempt_timeout: Duration::from_millis(250),
            pin: Some(pin),
            local: None,
        };
        let script = Script::new(&["10.9.9.9"], &[]);
        let _ = connect_with("ticket.example", 80, &pinned, &script).await;
        assert!(script.lookups.lock().unwrap().is_empty());
        assert_eq!(script.dialled(), ["127.0.0.1", "::1", "127.0.0.2"]);

        // A one-address ticket is a single attempt with a plain error.
        store_answer(
            &answers,
            6,
            "one.example",
            vec!["127.0.0.1".parse().unwrap()],
        );
        let pinned = ConnectOptions {
            attempt_timeout: Duration::from_millis(250),
            pin: Some(redeem_answer(&answers, 6, "one.example").unwrap()),
            local: None,
        };
        let script = Script::new(&[], &[]);
        let Err(ConnectError::Single(error)) =
            connect_with("one.example", 9, &pinned, &script).await
        else {
            panic!("expected a plain error");
        };
        assert_eq!(error.message, "connect ECONNREFUSED 127.0.0.1:9");
    }
}
