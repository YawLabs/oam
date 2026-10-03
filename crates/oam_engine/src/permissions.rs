//! Agent-context sandboxing: Deno-shaped permission descriptors.
//!
//! A `Permissions` value lives in the isolate slot (installed during
//! `JsRuntime::new_with_permissions`). The default is all-granted so
//! existing code and tests need zero changes.
//!
//! Gate shape: each op calls `check_read(path)` / `check_write(path)` /
//! `check_net(host)` at entry, returns `Err(denied_message)` on deny, and
//! the op translates that into a Node-shaped `ERR_ACCESS_DENIED` error.
//!
//! Permission values mirror Deno's `bool | Vec<String>`:
//! - `true` -> granted unconditionally
//! - `false` -> denied unconditionally
//! - `Vec<String>` -> whitelist of allowed paths/hosts
//!
//! MATCHING IS PER-CATEGORY, and the difference is the security boundary:
//!
//! - fs (`read`/`write`) is a PATH-PREFIX match anchored at a separator, over
//!   a lexically resolved target. `/box/allowed` grants `/box/allowed` and
//!   `/box/allowed/file`, and denies both `/box/allowed-evil` (no separator
//!   boundary) and `/box/allowed/../../secret` (resolved out of the subtree).
//!   As in node, a target with no root is resolved against the cwd of the
//!   moment, and a grant entry with no root against the cwd at startup
//!   (`resolve_against_cwd`, `fs_grant`).
//! - net and env are EXACT matches. A prefix there grants names an ATTACKER
//!   CAN REGISTER: `--allow-net=api.github.com` must not admit
//!   `api.github.com.attacker.net`, and `--allow-env=API` must not admit
//!   `API_SECRET`.
//!
//! Until 2026-09-09 every category shared one raw `starts_with`, so all four
//! of those escapes were live and confirmed against the shipped 0.14.0 binary.
//! This doc said "exact match for hosts" the whole time; the code did not do
//! it. If you add a category, pick its matcher deliberately.
//!
//! KNOWN LIMIT, deliberately not addressed here: resolution is LEXICAL, so a
//! symlink inside an allowed subtree that points outside it is still followed.
//! Fixing that needs the filesystem, and `canonicalize` cannot be used on the
//! write path because it errors on a not-yet-existing file. Treat the fs
//! allowlist as bounding PATH SHAPE, not the physical filesystem.

/// What is allowed for one permission category.
#[derive(Clone, Debug)]
pub enum PermValue {
    /// All instances allowed.
    All,
    /// All instances denied.
    None,
    /// Only these specific paths/hosts allowed (prefix-match for fs,
    /// exact-match for net).
    List(Vec<String>),
}

/// Lexically resolve a path for comparison: normalise separators, drop `.`
/// and empty segments, and pop a segment on `..` without touching the disk.
///
/// NOT `std::fs::canonicalize`: that hits the filesystem and errors on a path
/// that does not exist yet, which is exactly the case on every write gate.
/// The comparison has to work for a file we are about to create.
///
/// A leading `..` that would escape the root is DROPPED rather than kept, so a
/// target can never resolve to something above its own root and then re-enter
/// an allowed subtree by coincidence.
fn normalize_path(raw: &str) -> String {
    let unified = raw.replace('\\', "/");
    let absolute = unified.starts_with('/');
    // A Windows drive prefix (`C:`) is carried through as its own segment so
    // `C:/a` and `D:/a` can never compare equal.
    let mut out: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Resolve a path that has no root against the process's CURRENT working
/// directory, as node's permission model does before it matches a grant
/// (its C++ checks run `PathResolve` with the cwd of the moment, so a
/// `process.chdir` moves what a relative path names). A rooted path is
/// returned as given and left to `normalize_path`.
///
/// Without this a relative target could never match an absolute grant:
/// `--allow-fs-write=<cwd>` refused `writeFileSync("out.txt")`,
/// `mkdirSync("d")` and `mkdtempSync("tmp-")` where node allows all three.
///
/// "No root" rather than "not absolute": on Windows `\x` is rooted but not
/// absolute, and node resolves it onto the cwd's drive; keeping it lexical
/// (`/x`) can never match a drive-qualified grant, so it is denied either
/// way, and a rooted grant entry still matches a rooted target the same way
/// it always has. A drive-relative `C:x` has no root and resolves through
/// `GetFullPathNameW`, which is what the OS opens. The empty path names the
/// cwd itself, as node's `path.resolve("")` does. If the cwd cannot be read
/// (deleted under the process), the path stays as given and so matches only
/// a relative entry -- never more than before.
fn resolve_against_cwd(raw: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if std::path::Path::new(raw).has_root() {
        return Cow::Borrowed(raw);
    }
    let resolved = if raw.is_empty() {
        std::env::current_dir()
    } else {
        std::path::absolute(raw)
    };
    match resolved {
        Ok(path) => Cow::Owned(path.to_string_lossy().into_owned()),
        Err(_) => Cow::Borrowed(raw),
    }
}

/// Whether `path` is in the Windows named-pipe namespace,
/// `\\<server>\pipe\<name>` (`\\.\pipe\x`, `\\?\pipe\x`, `//./pipe/x`, a
/// remote `\\host\pipe\x`; `pipe` in any letter case): a name the OS hands
/// to the named-pipe file system, never a file on disk. Every other path a
/// pipe op is given is opened as a file. Unix has no such namespace.
///
/// A path with a component made only of dots and spaces (`.`, `..`, `...`,
/// `. `), anywhere and under any prefix, is NOT in the namespace: Win32
/// folds such components while it normalises a `\\.\` path (`..` drops the
/// component before it; trailing dots and spaces are trimmed), so
/// `\\.\pipe\..\C:\x` is opened as `\\.\C:\x`, a file. Such a path is judged
/// as one, by the fs grants as well as net. (The `.` server of the local
/// device root `\\.\` itself is the one such component taken as given; it
/// is never folded.) No pipe name needs such a component, and a path wrongly
/// judged "not a pipe" only has the fs grants asked as well -- the check
/// fails closed. Any other component the normaliser could alter (`pipe.`,
/// `pipe `) already fails the exact `pipe` match below.
///
/// A path with a NUL byte is not in the namespace either: the OS reads it
/// only up to the NUL, so `\\.\pipe\` + NUL + `x` opens the pipe file
/// system's root, not a pipe ([`Permissions::check_pipe`] refuses such a
/// path outright; this keeps the judgement closed on its own).
#[cfg(windows)]
fn is_named_pipe_namespace(path: &str) -> bool {
    if path.contains('\0') {
        return false;
    }
    let dots_and_spaces = |c: &str| !c.is_empty() && c.bytes().all(|b| b == b'.' || b == b' ');
    // The server of the local device root `\\.\` is the one `.` allowed.
    let mut components = path.split(['\\', '/']).enumerate();
    if components.any(|(i, c)| dots_and_spaces(c) && !(i == 2 && c == ".")) {
        return false;
    }
    let mut parts = path.split(['\\', '/']);
    // `\\` (two empty components), a server, `pipe`, and a non-empty name.
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(""), Some(""), Some(server), Some(pipe), Some(name))
            if !server.is_empty() && pipe.eq_ignore_ascii_case("pipe") && !name.is_empty()
    )
}
#[cfg(not(windows))]
fn is_named_pipe_namespace(_path: &str) -> bool {
    false
}

/// Windows path comparison is case-insensitive; POSIX is not.
#[cfg(windows)]
fn path_eq_fold(a: &str) -> String {
    a.to_ascii_lowercase()
}
#[cfg(not(windows))]
fn path_eq_fold(a: &str) -> String {
    a.to_string()
}

/// Split `host[:port]` into its host part, tolerating a bracketed IPv6
/// literal (`[::1]:8080`), whose host half itself contains colons.
///
/// The bracketed form is taken ONLY when the `]` ends the string or is
/// followed by `:<port>`. Anything else after it returns the target WHOLE, so
/// it can never equal a bare `[...]` entry. It used to split at the first `]`
/// and ignore the rest: `[::1].evil.example:80` read as `[::1]`, while the
/// connect ops hand the full name to getaddrinfo, so `--allow-net=[::1]`
/// admitted whatever a wildcard DNS name starting with `[::1].` resolves to
/// (on macOS `[::1].127.0.0.1.nip.io` connected to 127.0.0.1 under it).
fn host_of(hostport: &str) -> &str {
    if hostport.starts_with('[') {
        let Some(end) = hostport.find(']') else {
            return hostport;
        };
        let is_port = |p: &str| {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()
        };
        let rest = &hostport[end + 1..];
        let bracketed = rest.is_empty() || rest.strip_prefix(':').is_some_and(is_port);
        // `[::1]:8080` -> `[::1]`; `[::1].evil:80`, `[::1]x:80` -> whole.
        return if bracketed {
            &hostport[..=end]
        } else {
            hostport
        };
    }
    match hostport.rsplit_once(':') {
        // A bare IPv6 literal has several colons and no port; leave it whole.
        Some((head, _)) if !head.contains(':') => head,
        _ => hostport,
    }
}

/// A port as an entry spells it: its decimal digits exactly -- `8080`, not
/// `08080`, `+8080` or `8080 `, which the joined `host:port` string a connect
/// is checked as never equals either.
fn canonical_port(text: &str) -> Option<u16> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if digits && (text == "0" || !text.starts_with('0')) {
        text.parse().ok()
    } else {
        None
    }
}

/// An IPv6 address with a zone id, as node's `net.isIP` and oam's transport
/// read one ([`oam_core::net_connect::PinAddr`]): the address, then the whole
/// zone after `%`, `:` included. `None` for text that is not one.
fn parse_zoned(text: &str) -> Option<(std::net::Ipv6Addr, &str)> {
    let (_, zone) = text.split_once('%')?;
    match text.parse::<oam_core::net_connect::PinAddr>().ok()? {
        oam_core::net_connect::PinAddr {
            ip: std::net::IpAddr::V6(ip),
            zone: Some(_),
        } => Some((ip, zone)),
        _ => None,
    }
}

/// Whether the `--allow-net` entry `item` admits the zone-id address `ip`
/// `%` `zone` on `port`. The entry is matched by its parts: `<ipv6>%<zone>`
/// is host-only and admits that address and zone on any port, and
/// `<ipv6>%<zone>:<digits>` is port-scoped and admits them on that port
/// alone. The address is compared parsed (any spelling of it), the zone as
/// written. An entry whose zone ends in `:` and digits is always the
/// port-scoped form -- a port that is not its canonical digits grants
/// nothing -- so a host-only entry never names a zone ending that way, and
/// such a zone is granted only by `--allow-net` with no list.
fn zoned_entry_admits(item: &str, ip: std::net::Ipv6Addr, zone: &str, port: u16) -> bool {
    let Some((entry_ip, entry_zone)) = parse_zoned(item) else {
        return false;
    };
    if entry_ip != ip {
        return false;
    }
    match entry_zone.rsplit_once(':') {
        Some((head, tail)) if is_port_scoped_zone(entry_zone) => {
            head == zone && canonical_port(tail) == Some(port)
        }
        _ => entry_zone == zone,
    }
}

/// Whether an entry's zone text is the port-scoped form: it ends in `:` and
/// decimal digits, which [`zoned_entry_admits`] always reads as a port.
fn is_port_scoped_zone(entry_zone: &str) -> bool {
    entry_zone
        .rsplit_once(':')
        .is_some_and(|(_, tail)| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
}

impl PermValue {
    /// Path-prefix match, anchored at a separator, over lexically resolved
    /// paths. Used by fs read/write ONLY -- see the module doc.
    pub fn allows_path(&self, target: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            PermValue::List(list) => {
                let t = path_eq_fold(&normalize_path(target));
                list.iter().any(|item| {
                    let entry = path_eq_fold(&normalize_path(item));
                    if entry.is_empty() {
                        return false;
                    }
                    if t == entry {
                        return true;
                    }
                    // The separator is what makes this a SUBTREE test rather
                    // than a string test: without it `/box/allowed` matches
                    // the unrelated sibling `/box/allowed-evil`.
                    let with_sep = if entry.ends_with('/') {
                        entry.clone()
                    } else {
                        format!("{entry}/")
                    };
                    t.starts_with(&with_sep)
                })
            }
        }
    }

    /// `allows_path` for a filesystem target as an fs op passes it: a target
    /// with no root is first resolved against the current cwd (see
    /// `resolve_against_cwd`). The cwd is read only for a `List` grant and a
    /// relative target, so an unrestricted run (`All`) pays nothing.
    pub fn allows_fs_path(&self, target: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            PermValue::List(_) => self.allows_path(&resolve_against_cwd(target)),
        }
    }

    /// Exact match on a pipe's path: a Windows named pipe (`\\.\pipe\name`)
    /// or a Unix domain socket (`/run/app.sock`). `path` is rooted -- the
    /// caller has resolved a relative one against the cwd of the moment
    /// ([`Permissions::check_pipe`]) -- and only a ROOTED entry can match it,
    /// component by component (`C:/a/x.sock` is `C:\a\x.sock`; `/a//x` is
    /// `/a/x`). No host:port reading -- `C:\app.sock` is not host `C` -- no
    /// prefix, and no relative entry: a relative name would name a different
    /// socket after every `process.chdir`, so `--allow-net=docker.sock`
    /// grants no pipe at all, and a grant for one pipe admits no other --
    /// never `\\.\pipe\docker_engine` or `/var/run/docker.sock`.
    pub fn allows_net_path(&self, path: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            PermValue::List(list) => {
                let target = std::path::Path::new(path);
                target.has_root()
                    && list.iter().any(|item| {
                        let entry = std::path::Path::new(item);
                        entry.has_root() && entry == target
                    })
            }
        }
    }

    /// Exact match on a `host[:port]`. An entry carrying a port matches only
    /// that port; a bare-host entry matches the host on any port.
    ///
    /// Used by net ONLY. `--allow-net=127.0.0.1:5432` must not admit `:54321`.
    ///
    /// A target with a `%` is a zone-id address, and is matched as an address
    /// and a zone by the rule [`Self::allows_net_target`] applies, never as
    /// text: the connect ops build their resource as `host:port` with the
    /// op's own port in decimal, so the text after the target's last `:` is
    /// that port and everything before it the host. So `net.connect`,
    /// `tls.connect`, their lookup-hook answers and `fetch` read one grant
    /// the same way -- a host-only `fe80::1%1` admits that address and zone
    /// on any port, `fe80::1%1:8080` on port 8080 alone, and the address is
    /// compared parsed (any spelling of it). A zoned target with no port (a
    /// `permissions.query` target) is admitted by a host-only entry alone.
    pub fn allows_net(&self, target: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            PermValue::List(list) if target.contains('%') => {
                if let Some((host, port)) = target
                    .rsplit_once(':')
                    .and_then(|(host, tail)| Some((host, canonical_port(tail)?)))
                {
                    return self.allows_net_target(host, port);
                }
                let Some((ip, zone)) = parse_zoned(target) else {
                    return false;
                };
                list.iter().any(|item| {
                    parse_zoned(item).is_some_and(|(entry_ip, entry_zone)| {
                        entry_ip == ip && entry_zone == zone && !is_port_scoped_zone(entry_zone)
                    })
                })
            }
            PermValue::List(list) => list.iter().any(|item| {
                if item == target {
                    return true;
                }
                // Entry has no port -> compare host halves exactly. Entry HAS
                // a port -> the exact test above was the only chance.
                if host_of(item) == item.as_str() {
                    return host_of(target) == item.as_str();
                }
                false
            }),
        }
    }

    /// [`Self::allows_net`] for a connection to `host` on `port`, judged on
    /// the two halves as given rather than on `host:port` re-split: a
    /// bare-host entry matches when it IS `host`, a port-scoped entry when it
    /// is `host` followed by `:<port>`, spelled as the port's decimal digits.
    ///
    /// The halves are never joined and split again because a host may itself
    /// contain colons. A `connect.lookup` answer with an IPv6 zone id
    /// (`fe80::1%1`) is not a URL host and stays unbracketed
    /// ([`lookup_answer_resource`]); joined, `fe80::1%1:8080` re-splits as one
    /// multi-colon host with no port, so no bare-host entry could match it,
    /// and only one port-scoped entry per port could grant the address.
    ///
    /// A zone-id host is matched as an address and a zone, never as text
    /// ([`zoned_entry_admits`]). node's zone grammar admits `:`
    /// (`net.isIP('fe80::1%1:8080')` is 6, and that answer dials the zone
    /// `1:8080` on the connect's own port), so the host's zone is all of its
    /// text after `%`, and an entry `fe80::1%1:8080` is always the address
    /// `fe80::1`, the zone `1` and the port 8080. A host with a `%` that is
    /// not such an address (which the transport would not dial) matches no
    /// entry.
    pub fn allows_net_target(&self, host: &str, port: u16) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            // The empty host (a URL that did not parse) is never granted, not
            // even by a blank list item (`--allow-net=a.example,`).
            PermValue::List(_) if host.is_empty() => false,
            PermValue::List(list) if host.contains('%') => match parse_zoned(host) {
                Some((ip, zone)) => list
                    .iter()
                    .any(|item| zoned_entry_admits(item, ip, zone, port)),
                None => false,
            },
            PermValue::List(list) => list.iter().any(|item| {
                if item == host {
                    return true;
                }
                item.strip_prefix(host)
                    .and_then(|rest| rest.strip_prefix(':'))
                    .is_some_and(|p| canonical_port(p) == Some(port))
            }),
        }
    }

    /// Exact match. Used by env, where a prefix turns `--allow-env=API` into a
    /// grant over `API_SECRET`.
    pub fn allows_exact(&self, target: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            PermValue::List(list) => list.iter().any(|item| item == target),
        }
    }

    /// Serialise to a `&'static str` state name for the JS query API.
    pub fn state(&self) -> &'static str {
        match self {
            PermValue::All => "granted",
            PermValue::None => "denied",
            // For a list we cannot know the target at query time; report
            // "granted" (the list IS a set of grants).
            PermValue::List(_) => "granted",
        }
    }
}

/// Runtime permission descriptor. Carried as an isolate slot.
/// Default: all categories granted (no-break for existing code).
#[derive(Clone, Debug)]
pub struct Permissions {
    pub read: PermValue,
    pub write: PermValue,
    pub net: PermValue,
    pub env: PermValue,
    pub ffi: PermValue,
    pub child: PermValue,
    /// Starting a worker_threads Worker or an `oam.fork()` isolate.
    ///
    /// Separate from `child` now that a child isolate INHERITS this
    /// permission set: while workers ran all-granted, `--allow-worker` had to
    /// imply `--allow-child-process` (a worker could just spawn its way out),
    /// which over-granted every caller that only wanted a worker.
    pub worker: PermValue,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: PermValue::All,
            write: PermValue::All,
            net: PermValue::All,
            env: PermValue::All,
            ffi: PermValue::All,
            child: PermValue::All,
            worker: PermValue::All,
        }
    }
}

impl Permissions {
    /// Build from an optional `RuntimeOptions::permissions` field.
    /// `None` -> default (all-granted).
    pub fn from_opts(opts: Option<PermissionsOptions>) -> Self {
        let Some(o) = opts else {
            return Self::default();
        };
        Self {
            read: fs_grant(o.read),
            write: fs_grant(o.write),
            net: from_bool_or_list(o.net),
            env: from_bool_or_list(o.env),
            ffi: from_bool_or_list(o.ffi),
            child: from_bool_or_list(o.child),
            worker: from_bool_or_list(o.worker),
        }
    }

    /// Returns `Err(denial)` when `read` is denied for `path`.
    pub fn check_read(&self, path: &str) -> Result<(), PermissionDenial> {
        if self.read.allows_fs_path(path) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "FileSystemRead",
                resource: path.to_string(),
            })
        }
    }

    /// Returns `Err(denial)` when `write` is denied for `path`.
    pub fn check_write(&self, path: &str) -> Result<(), PermissionDenial> {
        if self.write.allows_fs_path(path) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "FileSystemWrite",
                resource: path.to_string(),
            })
        }
    }

    /// Returns `Err(denial)` when `net` is denied for `host`.
    pub fn check_net(&self, host: &str) -> Result<(), PermissionDenial> {
        if self.net.allows_net(host) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "Net",
                resource: host.to_string(),
            })
        }
    }

    /// The gate of a pipe a net socket connects to or a net server listens
    /// on (`net.connect(path)`, `server.listen(path)`): a Windows named pipe
    /// or a Unix domain socket. Returns the path the op must then use, or
    /// `Err(denial)`.
    ///
    /// - `net`: unrestricted, or an entry that is exactly this pipe
    ///   ([`PermValue::allows_net_path`]).
    /// - `read` AND `write` too when the path is a filesystem path: on Unix
    ///   every socket path is one (a connect opens the socket file, a listen
    ///   creates it and the server's close unlinks it), and on Windows every
    ///   path outside the named-pipe namespace (`\\<server>\pipe\...`) is,
    ///   since the dial opens it with CreateFileW read-write -- were that
    ///   left to the net grant alone, the error of a dial to a file outside
    ///   the fs grant (ENOTSOCK, ENOENT, EPERM) would tell the script what
    ///   the fs gate refuses to: whether it exists, and what it is.
    ///
    /// A relative path is resolved against the cwd of the moment, as the fs
    /// checks resolve one, and when any check depended on that the RESOLVED
    /// path is returned: the op dials or binds exactly what was checked, not
    /// whatever the name means after a `process.chdir` made before the op
    /// runs. With nothing restricted the path is returned as given, at no
    /// cost. The denial names the path as given, as an fs op's does.
    ///
    /// A path with a NUL byte in it is refused whenever anything is
    /// restricted, before it is judged at all: the OS reads a pipe name only
    /// up to its first NUL (CreateFileW takes a NUL-terminated string, and a
    /// `sun_path` is one too), so the path judged would not be the path
    /// opened -- `<dir>\a.txt\0\..\box\x.sock` names `box\x.sock` to the
    /// grants and `<dir>\a.txt` to the OS. node's libuv refuses such a name
    /// with EINVAL (the JS raises that first, and the op refuses it again),
    /// so no working path is lost. The denial names the first restricted
    /// permission of net, read and write.
    pub fn check_pipe<'a>(
        &self,
        path: &'a str,
    ) -> Result<std::borrow::Cow<'a, str>, PermissionDenial> {
        let denied = |permission| PermissionDenial {
            permission,
            resource: path.to_string(),
        };
        if path.contains('\0') {
            return match (&self.net, &self.read, &self.write) {
                (PermValue::All, PermValue::All, PermValue::All) => {
                    Ok(std::borrow::Cow::Borrowed(path))
                }
                (PermValue::All, PermValue::All, _) => Err(denied("FileSystemWrite")),
                (PermValue::All, _, _) => Err(denied("FileSystemRead")),
                _ => Err(denied("Net")),
            };
        }
        let fs_path = !is_named_pipe_namespace(path);
        let fs_restricted = fs_path
            && !(matches!(self.read, PermValue::All) && matches!(self.write, PermValue::All));
        if matches!(self.net, PermValue::All) && !fs_restricted {
            return Ok(std::borrow::Cow::Borrowed(path));
        }
        let resolved = resolve_against_cwd(path);
        if !self.net.allows_net_path(&resolved) {
            return Err(denied("Net"));
        }
        if fs_path {
            if !self.read.allows_fs_path(&resolved) {
                return Err(denied("FileSystemRead"));
            }
            if !self.write.allows_fs_path(&resolved) {
                return Err(denied("FileSystemWrite"));
            }
        }
        Ok(resolved)
    }

    /// Returns `Err(denial)` when `net` is denied for a connection to `host`
    /// on `port`: [`Self::check_net`] on the resource `host:port`, the one
    /// `net.connect` asks about, so the HTTP clients -- `fetch` (each redirect
    /// hop and `connect.lookup` answer too), `http.request`, `undici.request`
    /// -- and `WebSocket` are judged by the raw socket's rule. A bare-host
    /// entry admits the host on any port; a port-scoped entry
    /// (`127.0.0.1:8080`) admits exactly that port. `host` is a URL host,
    /// IPv6 bracketed, so `[::1]` on 8080 is matched by a `[::1]` or a
    /// `[::1]:8080` entry.
    ///
    /// The two halves are matched apart ([`PermValue::allows_net_target`]),
    /// so a host with colons of its own (a zone-id lookup answer such as
    /// `fe80::1%1`) is matched by its bare-host entry too -- by its address
    /// and zone, the zone being all of its text after `%`. The empty host (a
    /// URL that did not parse) matches no list entry.
    pub fn check_net_target(&self, host: &str, port: u16) -> Result<(), PermissionDenial> {
        if self.net.allows_net_target(host, port) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "Net",
                resource: format!("{host}:{port}"),
            })
        }
    }

    /// Returns `Err(denial)` when starting a worker / forked isolate is
    /// denied. The child inherits this same permission set, so a worker is
    /// not a way around the parent's restrictions.
    pub fn check_worker(&self, resource: &str) -> Result<(), PermissionDenial> {
        if self.worker.allows_path(resource) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "WorkerThreads",
                resource: resource.to_string(),
            })
        }
    }

    /// Returns `Err(denial)` when spawning or replacing the process is denied.
    pub fn check_child(&self, resource: &str) -> Result<(), PermissionDenial> {
        if self.child.allows_path(resource) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "ChildProcess",
                resource: resource.to_string(),
            })
        }
    }

    /// Returns `Err(denial)` when loading a native addon is denied.
    ///
    /// `Addon` is Node's own name for this permission (`--allow-addons`);
    /// oam stores it under the Deno-shaped `ffi` field, so the field name
    /// and the reported name deliberately differ.
    ///
    /// This is the SANDBOX verdict and it is independent of
    /// `OAM_ENABLE_NATIVE_ADDONS`. That variable is the alpha opt-in for a
    /// subsystem that can deadlock the loader; it is not a grant, and an
    /// environment variable must never be able to widen a permission the
    /// caller withheld.
    pub fn check_ffi(&self, resource: &str) -> Result<(), PermissionDenial> {
        if self.ffi.allows_path(resource) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "Addon",
                resource: resource.to_string(),
            })
        }
    }

    /// Returns `Err(denial)` when `env` is denied.
    pub fn check_env(&self, key: &str) -> Result<(), PermissionDenial> {
        if self.env.allows_exact(key) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "Environment",
                resource: key.to_string(),
            })
        }
    }

    /// Query the state of a named permission, optionally scoped to a
    /// path/host.  Returns ("granted" | "denied").
    pub fn query_state(&self, name: &str, target: Option<&str>) -> &'static str {
        // `worker` was missing here while being a real field on Permissions,
        // so `permissions.query({name:'worker'})` answered "denied" even on a
        // default all-granted run -- a query API that lies about its own
        // permission set. Any field added to Permissions needs an arm here;
        // the test below asserts every one is answerable.
        let perm = match name {
            "read" => &self.read,
            "write" => &self.write,
            "net" => &self.net,
            "env" => &self.env,
            "child" => &self.child,
            "ffi" => &self.ffi,
            "worker" => &self.worker,
            _ => return "denied",
        };
        match target {
            // Scoped queries must answer with the SAME matcher the gate uses,
            // or query() and the op disagree about the same argument.
            Some(t) => {
                let granted = match name {
                    "read" | "write" => perm.allows_fs_path(t),
                    "net" => perm.allows_net(t),
                    _ => perm.allows_exact(t),
                };
                if granted { "granted" } else { "denied" }
            }
            None => perm.state(),
        }
    }
}

// ----------------------------------------------------------------- public API

/// A denied permission check. Node reports these as an `ERR_ACCESS_DENIED`
/// Error carrying `permission` (the subsystem) and `resource` (what was asked
/// for); both are surfaced as own properties on the thrown error.
#[derive(Debug, Clone)]
pub struct PermissionDenial {
    /// Node's permission name. `FileSystemRead` / `FileSystemWrite` are Node's
    /// own; `Net` / `Environment` cover oam checks Node has no equivalent for.
    pub permission: &'static str,
    pub resource: String,
}

/// Node's message for `ERR_ACCESS_DENIED`.
pub const ACCESS_DENIED_MESSAGE: &str = "Access to this API has been restricted";

impl std::fmt::Display for PermissionDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ACCESS_DENIED_MESSAGE)
    }
}

impl From<PermissionDenial> for oam_core::AccessDenial {
    fn from(denial: PermissionDenial) -> Self {
        oam_core::AccessDenial {
            permission: denial.permission.to_string(),
            resource: denial.resource,
        }
    }
}

/// The `--permission` net check a fetch applies to every host it dials: the
/// URL JS passed in (at the op, synchronously) and every redirect hop (in the
/// transport's loop, before the hop parks for a `connect.lookup` hook or
/// dials). `None` when the grant covers every host -- no `--permission`, or a
/// bare `--allow-net` -- so an unrestricted run pays nothing per hop.
///
/// Both call sites go through this one closure, so the initial URL and a
/// redirect cannot be judged by different rules. The rule is
/// [`Permissions::check_net_target`]: the URL's host and the port the hop
/// will be dialled on, as the `host:port` resource `net.connect` checks:
///
/// - The host is the WHATWG serialization (see `oam_core::http_client::
///   NetTarget`): case, percent-encoding and non-canonical IP spellings are
///   folded by the URL parser before the grant is asked, so `LOCALHOST`,
///   `%6c%6fcalhost`, `0x7f.1` and `[0:0::1]` are judged as `localhost`,
///   `localhost`, `127.0.0.1` and `[::1]`. Userinfo is never part of it.
/// - A trailing dot is NOT folded: `localhost.` is a different string from a
///   `localhost` grant, so it is refused. That fails closed; granting it takes
///   an entry spelled with the dot.
/// - An IPv6 literal is checked in brackets, so a grant names it `[::1]`.
/// - The port is the URL's own, or the scheme's default (80 / 443). A
///   bare-host entry admits the host on any port; a port-scoped entry
///   (`example.com:443`) admits that port alone -- `https://example.com/`,
///   but not `http://example.com/` or a redirect to `example.com:8443`. A
///   denial names `host:port`. Up to 0.17.1 the port was not part of the
///   resource, so a port-scoped entry admitted no fetch at all.
pub fn fetch_net_check(
    permissions: &std::sync::Arc<Permissions>,
) -> Option<oam_core::http_client::NetCheck> {
    if matches!(permissions.net, PermValue::All) {
        return None;
    }
    let permissions = std::sync::Arc::clone(permissions);
    Some(std::sync::Arc::new(
        move |target: &oam_core::http_client::NetTarget<'_>| {
            permissions
                .check_net_target(target.host, target.port)
                .map_err(oam_core::AccessDenial::from)
        },
    ))
}

/// The port a request to `url` is dialled on, as the net grant is asked about
/// it: the URL's own, or the scheme's default -- 80 for `http:` and `ws:`,
/// 443 for `https:` and `wss:` (the WHATWG parser drops a default port, so
/// `port()` is empty for those) -- and 0 for any other scheme, which no
/// port-scoped entry names. The synchronous gates (`op_fetch`,
/// `op_ws_connect`) read it here; the transport's loop gets the same answer
/// from the `url` crate's `port_or_known_default`.
pub fn url_net_port(url: &ada_url::Url) -> u16 {
    match url.port() {
        "" => match url.protocol() {
            "http:" | "ws:" => 80,
            "https:" | "wss:" => 443,
            _ => 0,
        },
        explicit => explicit.parse().unwrap_or_default(),
    }
}

/// The net resource a `connect.lookup` hook's answer is checked as: the host
/// a URL naming that address serializes to, so one grant spelling covers the
/// address whether a script names it in a URL or a hook answers with it.
///
/// IPv4 is dotted-quad. IPv6 is bracketed and in the URL parser's canonical
/// form -- `::1`, `0:0:0:0:0:0:0:1` and `::0:1` are all `[::1]`, and
/// `::ffff:127.0.0.1` is `[::ffff:7f00:1]` (the WHATWG form; Rust's own
/// Display keeps the dotted tail, which a URL check never produces). It was
/// the raw answer, so `--allow-net=[::1]` (what `http://[::1]/` needs) refused
/// a hook answering `::1`, and only the unbracketed `::1`, which no URL check
/// ever matches, admitted it. An answer that is not an address is checked
/// as given; the transport refuses to dial it anyway. So is an IPv6 answer
/// with a zone id (`fe80::1%1`), which the transport does dial: a URL cannot
/// name one, so there is no URL spelling to match, and its grant stays the
/// spelling it always was (`--allow-net=fe80::1%1`, or `fe80::1%1:8080`). `op_fetch_continue`
/// checks it with the parked hop's port
/// ([`Permissions::check_net_target`]), as the hop's host was, which matches
/// it as an address and a zone ([`PermValue::allows_net_target`]).
pub fn lookup_answer_resource(answer: &str) -> String {
    match answer.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Ok(std::net::IpAddr::V6(v6)) => {
            let bracketed = format!("[{v6}]");
            ada_url::Url::parse(&format!("http://{bracketed}/"), None)
                .map(|url| url.hostname().to_string())
                .unwrap_or(bracketed)
        }
        Err(_) => answer.to_string(),
    }
}

/// Caller-facing descriptor for `JsRuntime::with_permissions`.
/// Each field is `None` (default: granted) | `Some(true)` (granted) |
/// `Some(false)` (denied) | list of strings (whitelist).
#[derive(Debug, Default, Clone)]
pub struct PermissionsOptions {
    pub read: BoolOrList,
    pub write: BoolOrList,
    pub net: BoolOrList,
    pub env: BoolOrList,
    pub ffi: BoolOrList,
    /// Spawning, or REPLACING, this process. Node gates both behind
    /// `--allow-child-process`, because either one hands control to a binary
    /// the permission set does not describe: once the image is replaced, no
    /// restriction here applies to what runs next.
    pub child: BoolOrList,
    /// Starting a worker / forked isolate. A child inherits the parent's
    /// permissions, so this no longer has to imply `child`.
    pub worker: BoolOrList,
}

/// `true` | `false` | list of allowed strings.
#[derive(Debug, Clone)]
pub enum BoolOrList {
    Bool(bool),
    List(Vec<String>),
}

impl Default for BoolOrList {
    fn default() -> Self {
        BoolOrList::Bool(true)
    }
}

fn from_bool_or_list(v: BoolOrList) -> PermValue {
    match v {
        BoolOrList::Bool(true) => PermValue::All,
        BoolOrList::Bool(false) => PermValue::None,
        BoolOrList::List(list) => {
            if list.is_empty() {
                PermValue::None
            } else {
                PermValue::List(list)
            }
        }
    }
}

/// `from_bool_or_list` for an fs category: a grant entry with no root is
/// resolved against the cwd ONCE, here, when the permission set is built --
/// node resolves `--allow-fs-write=.` or `=../out` at startup, so a later
/// `process.chdir` does not move the grant (it moves only relative targets).
/// A worker inherits the already-resolved set. An EMPTY entry stays empty
/// (and so grants nothing, see `allows_path`): resolving it would turn a
/// blank list item into a grant over the whole cwd.
fn fs_grant(v: BoolOrList) -> PermValue {
    match from_bool_or_list(v) {
        PermValue::List(list) => PermValue::List(
            list.into_iter()
                .map(|entry| {
                    if entry.is_empty() {
                        entry
                    } else {
                        resolve_against_cwd(&entry).into_owned()
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------- allowlist escape regressions
    //
    // Every test in this block was written as its INVERSE first and watched to
    // pass against the shipped matcher, i.e. each one is a hole that was open
    // on 0.14.0 and confirmed by running it. They are grouped because they
    // share one cause: a single raw `starts_with` served every category.

    fn perms(read: PermValue, net: PermValue, env: PermValue) -> Permissions {
        Permissions {
            read,
            write: PermValue::None,
            net,
            env,
            ffi: PermValue::None,
            child: PermValue::None,
            worker: PermValue::None,
        }
    }

    #[test]
    fn fs_allowlist_does_not_leak_to_a_sibling_with_a_shared_prefix() {
        // `/box/allowed-evil.txt` is not inside `/box/allowed`; only a string
        // test says otherwise.
        let p = perms(
            PermValue::List(vec!["/box/allowed".to_string()]),
            PermValue::None,
            PermValue::None,
        );
        assert!(p.check_read("/box/allowed-evil.txt").is_err());
        assert!(p.check_read("/box/allowedother").is_err());
    }

    #[test]
    fn fs_allowlist_does_not_admit_a_traversal_out_of_the_subtree() {
        let p = perms(
            PermValue::List(vec!["/box/allowed".to_string()]),
            PermValue::None,
            PermValue::None,
        );
        assert!(p.check_read("/box/allowed/../../secret.txt").is_err());
        assert!(p.check_read("/box/allowed/../allowed-evil.txt").is_err());
    }

    #[test]
    fn fs_allowlist_still_admits_the_subtree_and_the_entry_itself() {
        // The escapes above must close without breaking what the flag is FOR.
        let p = perms(
            PermValue::List(vec!["/box/allowed".to_string()]),
            PermValue::None,
            PermValue::None,
        );
        assert!(p.check_read("/box/allowed").is_ok());
        assert!(p.check_read("/box/allowed/file.txt").is_ok());
        assert!(p.check_read("/box/allowed/deep/nested/file.txt").is_ok());
        // A traversal that stays inside resolves and is allowed.
        assert!(p.check_read("/box/allowed/sub/../file.txt").is_ok());
    }

    #[test]
    fn net_allowlist_does_not_admit_an_attacker_registerable_suffix() {
        // The whole point: `api.github.com.attacker.net` is a domain someone
        // else can register, and a prefix match hands it the grant.
        let p = perms(
            PermValue::None,
            PermValue::List(vec!["api.github.com".to_string()]),
            PermValue::None,
        );
        assert!(p.check_net("api.github.com.attacker.net").is_err());
        assert!(p.check_net("api.github.com.evil.example").is_err());
    }

    /// A pipe gate with unrestricted fs, so only the net half decides.
    fn pipe_perms(net: PermValue) -> Permissions {
        Permissions {
            read: PermValue::All,
            write: PermValue::All,
            ..perms(PermValue::None, net, PermValue::None)
        }
    }

    #[test]
    fn a_pipe_path_is_granted_only_by_an_entry_that_is_exactly_it() {
        let p = pipe_perms(PermValue::List(vec![
            r"\\.\pipe\app".to_string(),
            "/run/app.sock".to_string(),
            "C".to_string(),
        ]));
        assert!(p.check_pipe(r"\\.\pipe\app").is_ok());
        assert!(p.check_pipe("/run/app.sock").is_ok());
        // The same path spelt with other separators, or a doubled one.
        assert!(p.check_pipe("/run//app.sock").is_ok());
        // No prefix: another pipe, a longer name, a socket under the path.
        assert!(p.check_pipe(r"\\.\pipe\app2").is_err());
        assert!(p.check_pipe(r"\\.\pipe\docker_engine").is_err());
        assert!(p.check_pipe("/run/app.sock.d/x").is_err());
        // No host:port reading: `C:\x.sock` is not the host `C`.
        assert!(p.check_pipe(r"C:\x.sock").is_err());
        let denial = p.check_pipe(r"\\.\pipe\other").unwrap_err();
        assert_eq!(
            (denial.permission, denial.resource.as_str()),
            ("Net", r"\\.\pipe\other")
        );
        let all = pipe_perms(PermValue::All);
        assert!(all.check_pipe(r"\\.\pipe\anything").is_ok());
        let none = pipe_perms(PermValue::None);
        assert!(none.check_pipe(r"\\.\pipe\app").is_err());
    }

    /// Regression guard: the pipe grant compared the raw string while the
    /// OS resolves a relative socket path against the cwd at connect time,
    /// so `--allow-net=docker.sock` plus `process.chdir('/var/run')` dialled
    /// /var/run/docker.sock. A relative entry grants no pipe; a relative
    /// target is resolved against the cwd of the moment, and that resolved
    /// path is what the op is handed to use.
    #[test]
    fn a_relative_pipe_path_is_resolved_and_a_relative_entry_grants_nothing() {
        let cwd = std::env::current_dir().unwrap();
        let here = cwd.join("app.sock").to_string_lossy().into_owned();
        let p = pipe_perms(PermValue::List(vec![
            "docker.sock".to_string(),
            here.clone(),
        ]));
        assert_eq!(
            p.check_pipe("docker.sock").unwrap_err().permission,
            "Net",
            "a relative entry grants no pipe"
        );
        assert_eq!(p.check_pipe("app.sock").unwrap(), here.as_str());
        assert_eq!(p.check_pipe(&here).unwrap(), here.as_str());
        // Nothing restricted: the path as given, untouched.
        let all = pipe_perms(PermValue::All);
        assert!(matches!(
            all.check_pipe("app.sock"),
            Ok(std::borrow::Cow::Borrowed("app.sock"))
        ));
    }

    /// Regression guard: a pipe op asked the net grant only, but a path
    /// outside the Windows pipe namespace is a file the dial opens
    /// read-write (and on Unix a socket file that listen creates and close
    /// unlinks), so under `--allow-net` alone the dial's error code
    /// (ENOTSOCK, ENOENT, EPERM) answered "does this exist, and is it a
    /// file?" for paths the fs grant refused. Such a path needs the fs read
    /// and write grants too; a named pipe needs only net.
    #[test]
    fn a_pipe_path_that_is_a_file_needs_the_fs_grants_too() {
        let cwd = std::env::current_dir().unwrap();
        let inside = cwd.join("box");
        let file = inside.join("x.sock").to_string_lossy().into_owned();
        let outside = cwd.join("secret.txt").to_string_lossy().into_owned();
        let grant = PermValue::List(vec![inside.to_string_lossy().into_owned()]);
        let p = Permissions {
            read: grant.clone(),
            write: grant.clone(),
            ..perms(PermValue::None, PermValue::All, PermValue::None)
        };
        assert!(p.check_pipe(&file).is_ok());
        let denial = p.check_pipe(&outside).unwrap_err();
        assert_eq!(
            (denial.permission, denial.resource.as_str()),
            ("FileSystemRead", outside.as_str())
        );
        let read_only = Permissions {
            write: PermValue::None,
            ..p.clone()
        };
        assert_eq!(
            read_only.check_pipe(&file).unwrap_err().permission,
            "FileSystemWrite"
        );
        // The OS reads a pipe name only up to a NUL: the first path would be
        // judged as `box\x.sock` and open `secret.txt`, the second judged a
        // pipe and open the pipe file system's root. Refused whatever the
        // rest of it says, whenever anything is restricted.
        let sep = std::path::MAIN_SEPARATOR;
        let truncated = format!("{outside}\0{sep}..{sep}box{sep}x.sock");
        for name in [truncated.as_str(), "\\\\.\\pipe\\\0x"] {
            assert!(!is_named_pipe_namespace(name), "{name:?}");
            let denial = p.check_pipe(name).unwrap_err();
            assert_eq!(
                (denial.permission, denial.resource.as_str()),
                ("FileSystemRead", name),
            );
            assert_eq!(
                read_only.check_pipe(name).unwrap_err().permission,
                "FileSystemRead"
            );
            let write_restricted = Permissions {
                read: PermValue::All,
                ..p.clone()
            };
            assert_eq!(
                write_restricted.check_pipe(name).unwrap_err().permission,
                "FileSystemWrite"
            );
            let net_listed = Permissions {
                read: PermValue::All,
                write: PermValue::All,
                net: PermValue::List(vec![name.to_string(), file.clone()]),
                ..p.clone()
            };
            assert_eq!(
                net_listed.check_pipe(name).unwrap_err().permission,
                "Net",
                "{name:?}: not even an entry that is exactly it"
            );
            // Nothing restricted: nothing to judge, the op refuses it.
            let all = Permissions {
                read: PermValue::All,
                write: PermValue::All,
                ..p.clone()
            };
            assert!(all.check_pipe(name).is_ok(), "{name:?}");
        }
        if cfg!(windows) {
            // The named-pipe namespace is no file: net alone decides.
            for name in [
                r"\\.\pipe\app",
                r"\\?\pipe\app",
                "//./pipe/app",
                r"\\.\PIPE\app",
            ] {
                assert!(read_only.check_pipe(name).is_ok(), "{name}");
            }
            // A device or drive path is not the pipe namespace.
            for name in [r"\\.\C:\x", r"\\?\C:\x", r"\\.\pipe\", r"\\.\pipe"] {
                assert!(read_only.check_pipe(name).is_err(), "{name}");
            }
            // Win32 folds a `.`/`..` (or dots-and-spaces) component while it
            // normalises a `\\.\` path, so `\\.\pipe\..\C:\x` opens
            // `\\.\C:\x`: such a path is judged as a file, by the fs grants.
            let in_cwd = format!(r"\\.\pipe\..\..\{}", outside);
            for name in [
                r"\\.\pipe\..\C:\x",
                r"\\.\pipe\x\..\..\C:\x",
                "//./pipe/../C:/x",
                r"\\.\pipe/x\..\..\C:\x",
                r"\\.\pipe\.\x",
                r"\\.\pipe\x\.",
                r"\\.\pipe\x\...",
                r"\\.\pipe\x\.. ",
                r"\\..\pipe\x",
                r"\\?\pipe\..\x",
                in_cwd.as_str(),
            ] {
                assert!(!is_named_pipe_namespace(name), "{name}");
                assert_eq!(
                    p.check_pipe(name).unwrap_err().permission,
                    "FileSystemRead",
                    "{name}"
                );
            }
            // With fs grants covering what it opens, it passes: the gate is
            // the fs one, not a refusal of the spelling.
            let fs_all = Permissions {
                read: PermValue::All,
                write: PermValue::All,
                ..p.clone()
            };
            assert!(fs_all.check_pipe(r"\\.\pipe\..\C:\x").is_ok());
        } else {
            // Every socket path is a file.
            assert!(read_only.check_pipe(r"\\.\pipe\app").is_err());
        }
    }

    #[test]
    fn net_allowlist_pins_the_port_when_the_entry_names_one() {
        let p = perms(
            PermValue::None,
            PermValue::List(vec!["127.0.0.1:5432".to_string()]),
            PermValue::None,
        );
        assert!(p.check_net("127.0.0.1:5432").is_ok());
        // `:54321` merely starts with `:5432`.
        assert!(p.check_net("127.0.0.1:54321").is_err());
        assert!(p.check_net("127.0.0.1:5433").is_err());
    }

    #[test]
    fn net_allowlist_entry_without_a_port_admits_any_port_on_that_host() {
        let p = perms(
            PermValue::None,
            PermValue::List(vec!["api.github.com".to_string()]),
            PermValue::None,
        );
        assert!(p.check_net("api.github.com").is_ok());
        assert!(p.check_net("api.github.com:443").is_ok());
        assert!(p.check_net("api.github.com:8443").is_ok());
    }

    #[test]
    fn net_allowlist_handles_a_bracketed_ipv6_literal() {
        // A bare rsplit on ':' would cut an IPv6 address in half.
        let p = perms(
            PermValue::None,
            PermValue::List(vec!["[::1]".to_string()]),
            PermValue::None,
        );
        assert!(p.check_net("[::1]:8080").is_ok());
        assert!(p.check_net("[::2]:8080").is_err());
    }

    #[test]
    fn net_allowlist_bracketed_entry_does_not_admit_a_name_that_merely_starts_with_it() {
        // The connect ops check `{host}:{port}` and dial `host` through
        // getaddrinfo, so a target whose host only BEGINS with `[::1]` is a
        // DNS name (`[::1].127.0.0.1.nip.io` resolves to 127.0.0.1 on macOS)
        // and must never be read as the bracketed literal.
        let p = perms(
            PermValue::None,
            PermValue::List(vec!["[::1]".to_string()]),
            PermValue::None,
        );
        for target in [
            "[::1].evil.example:80",
            "[::1].127.0.0.1.nip.io:443",
            "[::1]x:80",
            "[::1]]:80",
            "[::1]:80:90",
            "[::1]:",
            "[::1]:99999",
            "[::1]:+80",
            "[::1].evil.example",
            "[::1",
        ] {
            assert!(p.check_net(target).is_err(), "{target} must be refused");
            assert_eq!(p.query_state("net", Some(target)), "denied", "{target}");
        }
        // The literal itself, with and without a port, is still the grant.
        assert!(p.check_net("[::1]").is_ok());
        assert!(p.check_net("[::1]:0").is_ok());
        assert!(p.check_net("[::1]:65535").is_ok());
        // A port-scoped bracketed entry is still pinned to its port.
        let scoped = perms(
            PermValue::None,
            PermValue::List(vec!["[::1]:8080".to_string()]),
            PermValue::None,
        );
        assert!(scoped.check_net("[::1]:8080").is_ok());
        assert!(scoped.check_net("[::1]:8081").is_err());
        assert!(scoped.check_net("[::1]:8080.evil.example:80").is_err());
    }

    // ------------------------------------------- fetch: every hop, one rule

    fn target(host: &str, port: u16) -> oam_core::http_client::NetTarget<'_> {
        oam_core::http_client::NetTarget { host, port }
    }

    #[test]
    fn fetch_net_check_costs_nothing_when_every_host_is_granted() {
        // No --permission, and a bare --allow-net: no closure at all, so the
        // redirect loop does no per-hop work.
        let unrestricted = std::sync::Arc::new(Permissions::default());
        assert!(fetch_net_check(&unrestricted).is_none());
        let bare = std::sync::Arc::new(Permissions::from_opts(Some(PermissionsOptions {
            net: BoolOrList::Bool(true),
            ..opts_net_only(vec![])
        })));
        assert!(fetch_net_check(&bare).is_none());
        // Denied outright, or an empty list: a check that refuses.
        for net in [BoolOrList::Bool(false), BoolOrList::List(vec![])] {
            let p = std::sync::Arc::new(Permissions::from_opts(Some(PermissionsOptions {
                net,
                ..opts_net_only(vec![])
            })));
            let check = fetch_net_check(&p).expect("a restricted grant must check");
            assert!(check(&target("127.0.0.1", 80)).is_err());
        }
    }

    #[test]
    fn fetch_net_check_is_check_net_on_host_and_port() {
        let p = std::sync::Arc::new(Permissions::from_opts(Some(opts_net_only(vec![
            "127.0.0.1",
            "[::1]",
            "granted.test",
            "10.0.0.1:5432",
            "[::2]:8443",
            "scoped.test:443",
        ]))));
        let check = fetch_net_check(&p).unwrap();
        // A bare-host entry admits the host on any port.
        assert!(check(&target("127.0.0.1", 80)).is_ok());
        assert!(check(&target("127.0.0.1", 65535)).is_ok());
        assert!(check(&target("[::1]", 8080)).is_ok());
        assert!(check(&target("granted.test", 443)).is_ok());
        // The refusal names `host:port`, the resource net.connect's names.
        let denial = check(&target("localhost", 80)).unwrap_err();
        assert_eq!(
            denial,
            oam_core::AccessDenial {
                permission: "Net".to_string(),
                resource: "localhost:80".to_string(),
            }
        );
        // A trailing dot is its own name, and a suffix is not the grant.
        assert!(check(&target("granted.test.", 80)).is_err());
        assert!(check(&target("granted.test.evil.example", 80)).is_err());
        // An unbracketed IPv6 host never matches the bracketed entry.
        assert!(check(&target("::1", 80)).is_err());
        // A port-scoped entry admits a fetch to its own port, as it admits
        // net.connect there -- one rule -- and no other port.
        for (host, port) in [("10.0.0.1", 5432), ("[::2]", 8443), ("scoped.test", 443)] {
            assert!(p.check_net_target(host, port).is_ok(), "{host}:{port}");
            assert!(check(&target(host, port)).is_ok(), "{host}:{port}");
        }
        for (host, port) in [("10.0.0.1", 5433), ("[::2]", 443), ("scoped.test", 80)] {
            let denial = check(&target(host, port)).unwrap_err();
            assert_eq!(denial.resource, format!("{host}:{port}"));
        }
        // The empty host (a URL that did not parse) is never granted, not
        // even by a blank list item.
        assert!(check(&target("", 80)).is_err());
        let blank = Permissions::from_opts(Some(opts_net_only(vec!["", ":80", "a.test"])));
        assert!(blank.check_net_target("", 80).is_err());
        // A port-scoped entry is its port's decimal digits, exactly.
        let spelled = Permissions::from_opts(Some(opts_net_only(vec![
            "p.test:08080",
            "q.test:+80",
            "r.test:0",
            "s.test:",
        ])));
        assert!(spelled.check_net_target("p.test", 8080).is_err());
        assert!(spelled.check_net_target("q.test", 80).is_err());
        assert!(spelled.check_net_target("r.test", 0).is_ok());
        assert!(spelled.check_net_target("s.test", 0).is_err());
    }

    /// The synchronous gates' port matches the transport loop's
    /// (`url::Url::port_or_known_default`) for every scheme they see, so the
    /// initial URL and a redirect hop name the same `host:port`.
    #[test]
    fn url_net_port_is_the_port_the_transport_dials() {
        for (raw, port) in [
            ("http://example.com/", 80),
            ("http://example.com:80/", 80),
            ("https://example.com/", 443),
            ("https://example.com:443/", 443),
            ("http://example.com:8443/", 8443),
            ("https://example.com:80/", 80),
            ("ws://example.com/", 80),
            ("wss://example.com/", 443),
            ("wss://example.com:9001/", 9001),
            ("http://127.0.0.1:65535/", 65535),
        ] {
            let by_ada = ada_url::Url::parse(raw, None).unwrap();
            assert_eq!(url_net_port(&by_ada), port, "{raw}");
            let by_url = url::Url::parse(raw).unwrap();
            assert_eq!(by_url.port_or_known_default(), Some(port), "{raw}");
        }
        let other = ada_url::Url::parse("data:text/plain,x", None).unwrap();
        assert_eq!(url_net_port(&other), 0);
    }

    /// The initial URL's host is read by ada (`op_fetch`), a redirect hop's
    /// by the `url` crate (the transport's loop, which is what dials). Both
    /// are WHATWG parsers; if they ever disagreed on a host, the same URL
    /// would get one verdict as the URL a script passed in and another as a
    /// redirect target. Each spelling here is one a grant could be probed
    /// with, and the host both must read is the one the grant is asked about.
    ///
    /// The URLs are assembled at run time so the published-URLs gate
    /// (`xtask/tests/published_urls.rs`) does not read these fixtures as
    /// endpoints the binary talks to.
    #[test]
    fn the_two_fetch_gates_read_the_same_host() {
        for (scheme, authority, host) in [
            ("http", "LOCALHOST:1", "localhost"),
            ("http", "LocalHost.", "localhost."),
            ("http", "%6c%6fcalhost", "localhost"),
            ("http", "[0:0:0:0:0:0:0:1]:8080", "[::1]"),
            ("http", "[::FFFF:127.0.0.1]", "[::ffff:7f00:1]"),
            ("http", "0x7f.1", "127.0.0.1"),
            ("http", "2130706433", "127.0.0.1"),
            ("http", "0177.0.0.1", "127.0.0.1"),
            ("http", "127.1", "127.0.0.1"),
            ("http", "B%C3%BCcher.test", "xn--bcher-kva.test"),
            ("http", "b\u{fc}cher.test", "xn--bcher-kva.test"),
            ("http", "u:p@127.0.0.1@localhost", "localhost"),
            ("http", "127.0.0.1:80@localhost:81", "localhost"),
            ("http", r"granted.test\@localhost", "granted.test"),
            ("https", "granted.test:443", "granted.test"),
            ("http", "granted.test#@localhost", "granted.test"),
            ("http", "granted.test?@localhost", "granted.test"),
        ] {
            let raw = format!("{scheme}://{authority}/");
            let by_ada = ada_url::Url::parse(&raw, None)
                .ok()
                .map(|u| u.hostname().to_string());
            let by_url = url::Url::parse(&raw)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string));
            assert_eq!(by_ada.as_deref(), Some(host), "ada: {raw}");
            assert_eq!(by_url.as_deref(), Some(host), "url: {raw}");
        }
    }

    /// A hook's answer is checked as the host a URL naming that address
    /// serializes to, so it matches what `op_fetch` checks for the URL.
    #[test]
    fn a_lookup_answer_is_checked_as_a_url_naming_it_would_be() {
        for (answer, resource) in [
            ("127.0.0.1", "127.0.0.1"),
            ("10.9.9.9", "10.9.9.9"),
            ("::1", "[::1]"),
            ("0:0:0:0:0:0:0:1", "[::1]"),
            ("::0:1", "[::1]"),
            ("FE80::1", "[fe80::1]"),
            ("::ffff:127.0.0.1", "[::ffff:7f00:1]"),
            ("2001:db8:0:0:1:0:0:1", "[2001:db8::1:0:0:1]"),
            // Not an address: checked as given (the transport will not dial it).
            ("not-an-ip", "not-an-ip"),
        ] {
            assert_eq!(lookup_answer_resource(answer), resource, "{answer}");
            if resource.starts_with('[') {
                let url = format!("http://{resource}/");
                assert_eq!(
                    ada_url::Url::parse(&url, None).unwrap().hostname(),
                    resource,
                    "{answer}: a URL naming it reads the same host"
                );
            }
        }
        let p = std::sync::Arc::new(Permissions::from_opts(Some(opts_net_only(vec![
            "granted.test",
            "[::1]",
        ]))));
        // `[::1]` is the grant a URL needs, and now a hook answer too.
        assert!(p.check_net(&lookup_answer_resource("::1")).is_ok());
        assert!(p.check_net(&lookup_answer_resource("0::1")).is_ok());
        assert!(p.check_net(&lookup_answer_resource("::2")).is_err());
        // A zone-id answer is not a URL host, so it is checked as given, on
        // the hop's port. Its bare-host entry admits it on any port and its
        // port-scoped entry on that port alone. Joined into one string
        // (`fe80::1%1:8080`) and split again it read as a portless host, so
        // only the port-scoped entry could grant it.
        assert_eq!(lookup_answer_resource("fe80::1%1"), "fe80::1%1");
        let zoned = std::sync::Arc::new(Permissions::from_opts(Some(opts_net_only(vec![
            "fe80::1%1",
            "fe80::2%1:8080",
        ]))));
        for port in [80, 8080, 65535] {
            let r = zoned.check_net_target(&lookup_answer_resource("fe80::1%1"), port);
            assert!(r.is_ok(), "bare zone-id entry, port {port}");
        }
        let scoped = lookup_answer_resource("fe80::2%1");
        assert!(zoned.check_net_target(&scoped, 8080).is_ok());
        let denial = zoned.check_net_target(&scoped, 8081).unwrap_err();
        assert_eq!(denial.resource, "fe80::2%1:8081");
        // Another zone, or the address without its zone, is another host.
        assert!(zoned.check_net_target("fe80::1%2", 80).is_err());
        assert!(
            zoned
                .check_net_target(&lookup_answer_resource("fe80::1"), 80)
                .is_err()
        );
        let unbracketed =
            std::sync::Arc::new(Permissions::from_opts(Some(opts_net_only(vec!["::1"]))));
        assert!(
            unbracketed
                .check_net(&lookup_answer_resource("::1"))
                .is_err()
        );
    }

    /// A zone-id answer is matched as an address and a zone, never as text.
    /// node's grammar lets a zone hold `:` (`net.isIP('fe80::2%1:8080')` is
    /// 6, and `net.connect` dials it with the zone `1:8080` on its own port),
    /// so an entry `<zone-id IPv6>:<digits>` reads only as a port-scoped
    /// grant -- address, zone, port -- and an answer's zone is its whole text
    /// after `%`, never split into a port.
    #[test]
    fn a_zone_id_answer_is_matched_as_an_address_and_a_zone() {
        let net = |entries: Vec<&str>| Permissions::from_opts(Some(opts_net_only(entries)));
        let check = |p: &Permissions, answer: &str, port: u16| {
            p.check_net_target(&lookup_answer_resource(answer), port)
                .is_ok()
        };

        // Port-scoped: address fe80::2, zone 1, port 8080.
        let scoped = net(vec!["fe80::2%1:8080"]);
        assert!(check(&scoped, "fe80::2%1", 8080));
        assert!(!check(&scoped, "fe80::2%1", 80));
        // The answer's zone is `1:8080`, not the entry's `1`, on any port.
        for port in [80, 8080, 443] {
            assert!(!check(&scoped, "fe80::2%1:8080", port), "port {port}");
        }
        let denial = scoped
            .check_net_target(&lookup_answer_resource("fe80::2%1:8080"), 80)
            .unwrap_err();
        assert_eq!(denial.resource, "fe80::2%1:8080:80");
        // The address is compared parsed, so any spelling of it matches; the
        // zone is compared as written.
        assert!(check(&scoped, "FE80:0::2%1", 8080));
        assert!(!check(&scoped, "fe80::2%01", 8080));
        assert!(!check(&scoped, "fe80::3%1", 8080));
        assert!(!check(&scoped, "fe80::2", 8080));

        // Host-only: any port, and only that zone.
        let host_only = net(vec!["fe80::2%1"]);
        for port in [0, 80, 8080, 65535] {
            assert!(check(&host_only, "fe80::2%1", port), "port {port}");
            assert!(!check(&host_only, "fe80::2%1:8080", port), "port {port}");
            assert!(!check(&host_only, "fe80::2%2", port), "port {port}");
        }
        // A zone whose text after its last `:` is not digits stays one zone,
        // host-only; one whose tail is digits is a zone and a port.
        let colon_zone = net(vec!["fe80::2%a:b", "fe80::2%eth0:1"]);
        assert!(check(&colon_zone, "fe80::2%a:b", 80));
        assert!(check(&colon_zone, "fe80::2%eth0", 1));
        assert!(!check(&colon_zone, "fe80::2%eth0", 2));
        assert!(!check(&colon_zone, "fe80::2%eth0:1", 1));
        // A port that is not its canonical decimal digits grants nothing.
        let spelled = net(vec!["fe80::2%1:08080", "fe80::2%2:99999"]);
        for (answer, port) in [
            ("fe80::2%1", 8080),
            ("fe80::2%1:08080", 8080),
            ("fe80::2%2", 99),
            ("fe80::2%2:99999", 80),
        ] {
            assert!(!check(&spelled, answer, port), "{answer} on {port}");
        }
        // Text with a `%` that is not a zone-id address is never granted, not
        // even by an entry spelled the same.
        let malformed = net(vec!["fe80::2%", "fe80::2%1%2", "127.0.0.1%1", "x%1"]);
        for answer in ["fe80::2%", "fe80::2%1%2", "127.0.0.1%1", "x%1"] {
            assert!(!check(&malformed, answer, 80), "{answer}");
        }

        // Bare hosts and URL hosts are unchanged.
        let plain = net(vec!["a.test", "b.test:8080", "[::1]", "10.0.0.1:5432"]);
        assert!(plain.check_net_target("a.test", 1).is_ok());
        assert!(plain.check_net_target("b.test", 8080).is_ok());
        assert!(plain.check_net_target("b.test", 80).is_err());
        assert!(plain.check_net_target("[::1]", 9).is_ok());
        assert!(plain.check_net_target("10.0.0.1", 5432).is_ok());
        assert!(plain.check_net_target("10.0.0.1", 5433).is_err());
        assert!(plain.check_net_target("", 80).is_err());
        // A zoned entry grants no zoneless host, and a plain entry no zone.
        assert!(scoped.check_net_target("[fe80::2]", 8080).is_err());
        assert!(!check(&net(vec!["[fe80::2]"]), "fe80::2%1", 80));

        // net.connect's joined `host:port` agrees: its host is a zone-id
        // address and its port the connect's own.
        assert!(scoped.check_net("fe80::2%1:8080").is_ok());
        assert!(scoped.check_net("fe80::2%1:8080:80").is_err());
        assert!(scoped.check_net("fe80::2%1:8080:8080").is_err());
    }

    #[test]
    fn net_connect_resource_matches_a_zone_id_host_as_fetch_does() {
        // net.connect, tls.connect and their lookup-hook answers ask about
        // `host:port` (check_net); fetch asks about the two halves
        // (check_net_target). For a zone-id host both read one grant the same
        // way: the address parsed, the zone as written, the port the op's own.
        let net = |list: Vec<&str>| {
            perms(
                PermValue::None,
                PermValue::List(list.into_iter().map(String::from).collect()),
                PermValue::None,
            )
        };
        let grants = [
            net(vec!["fe80::2%1:8080"]),
            net(vec!["fe80::2%1"]),
            net(vec!["::1%1"]),
            net(vec!["fe80::2%a:b", "fe80::2%eth0:1"]),
            net(vec!["fe80::2%1:08080", "fe80::2%2:99999"]),
            net(vec!["fe80::2%", "fe80::2%1%2", "127.0.0.1%1", "x%1"]),
            net(vec!["granted.invalid", "[fe80::2]", "fe80::2"]),
        ];
        let hosts = [
            "fe80::2%1",
            "FE80:0::2%1",
            "fe80:0:0:0:0:0:0:2%1",
            "fe80::2%01",
            "fe80::2%2",
            "fe80::3%1",
            "fe80::2%1:8080",
            "fe80::2%a:b",
            "fe80::2%eth0",
            "fe80::2%eth0:1",
            "::1%1",
            "0::1%1",
            "fe80::2%",
            "fe80::2%1%2",
            "127.0.0.1%1",
            "x%1",
        ];
        for grant in &grants {
            for host in hosts {
                for port in [0u16, 1, 80, 8080, 65535] {
                    assert_eq!(
                        grant.check_net(&format!("{host}:{port}")).is_ok(),
                        grant.check_net_target(host, port).is_ok(),
                        "{:?}: {host} on {port}",
                        grant.net
                    );
                }
            }
        }

        // A host-only zone entry admits its address, in any spelling, on any
        // port; a port-scoped one on its port alone.
        let host_only = net(vec!["::1%1", "fe80::2%a:b"]);
        for port in [80, 8080] {
            assert!(host_only.check_net(&format!("::1%1:{port}")).is_ok());
            assert!(host_only.check_net(&format!("0:0::1%1:{port}")).is_ok());
            assert!(host_only.check_net(&format!("fe80::2%a:b:{port}")).is_ok());
            assert!(host_only.check_net(&format!("::1%2:{port}")).is_err());
        }
        let scoped = net(vec!["fe80::2%1:8080"]);
        assert!(scoped.check_net("FE80:0::2%1:8080").is_ok());
        assert!(scoped.check_net("FE80:0::2%1:8081").is_err());

        // A zoned query target with no port is admitted by a host-only entry
        // alone, and query() answers as the gate does.
        assert!(host_only.check_net("::1%1").is_ok());
        assert!(host_only.check_net("0::1%1").is_ok());
        assert!(host_only.check_net("fe80::2%a:b").is_ok());
        assert!(scoped.check_net("fe80::2%1").is_err());
        assert!(scoped.check_net("fe80::2%1:08080").is_err());
        assert!(host_only.check_net("x%1").is_err());
        for target in ["::1%1:80", "0::1%1", "::1%2:80", "x%1:80"] {
            let expected = if host_only.check_net(target).is_ok() {
                "granted"
            } else {
                "denied"
            };
            assert_eq!(
                host_only.query_state("net", Some(target)),
                expected,
                "{target}"
            );
        }
    }

    #[test]
    fn env_allowlist_does_not_admit_a_longer_variable_sharing_the_prefix() {
        // `--allow-env=API` granting `API_SECRET` is the same bug wearing a
        // different hat, and secrets are exactly what lives behind that prefix.
        let p = perms(
            PermValue::None,
            PermValue::None,
            PermValue::List(vec!["API".to_string()]),
        );
        assert!(p.check_env("API").is_ok());
        assert!(p.check_env("API_SECRET").is_err());
        assert!(p.check_env("API_KEY").is_err());
    }

    #[test]
    fn query_state_answers_for_every_permission_field() {
        // `worker` was absent from the match and fell through to "denied", so
        // the query API reported a permission the runtime had actually granted
        // as denied. Enumerated rather than spot-checked so a NEW field cannot
        // be added without either an arm here or a red test.
        let granted = Permissions::default();
        for name in ["read", "write", "net", "env", "child", "ffi", "worker"] {
            assert_eq!(
                granted.query_state(name, None),
                "granted",
                "query_state has no arm for `{name}`"
            );
        }
        assert_eq!(granted.query_state("not-a-permission", None), "denied");
    }

    #[test]
    fn query_state_scoped_answers_agree_with_the_gate() {
        // A scoped query that used a different matcher than the gate would let
        // query() promise access the op then refuses -- or the reverse.
        let p = perms(
            PermValue::List(vec!["/box/allowed".to_string()]),
            PermValue::List(vec!["api.github.com".to_string()]),
            PermValue::List(vec!["API".to_string()]),
        );
        for target in ["/box/allowed/file.txt", "/box/allowed-evil.txt"] {
            let expected = if p.check_read(target).is_ok() {
                "granted"
            } else {
                "denied"
            };
            assert_eq!(
                p.query_state("read", Some(target)),
                expected,
                "read {target}"
            );
        }
        for target in ["api.github.com:443", "api.github.com.attacker.net"] {
            let expected = if p.check_net(target).is_ok() {
                "granted"
            } else {
                "denied"
            };
            assert_eq!(p.query_state("net", Some(target)), expected, "net {target}");
        }
        for target in ["API", "API_SECRET"] {
            let expected = if p.check_env(target).is_ok() {
                "granted"
            } else {
                "denied"
            };
            assert_eq!(p.query_state("env", Some(target)), expected, "env {target}");
        }
    }

    fn opts_read_only(list: Vec<&str>) -> PermissionsOptions {
        PermissionsOptions {
            read: BoolOrList::List(list.iter().map(|s| s.to_string()).collect()),
            write: BoolOrList::Bool(false),
            net: BoolOrList::Bool(false),
            env: BoolOrList::Bool(false),
            ffi: BoolOrList::Bool(false),
            child: BoolOrList::Bool(false),
            worker: BoolOrList::Bool(false),
        }
    }

    fn opts_net_only(list: Vec<&str>) -> PermissionsOptions {
        PermissionsOptions {
            read: BoolOrList::Bool(false),
            write: BoolOrList::Bool(false),
            net: BoolOrList::List(list.iter().map(|s| s.to_string()).collect()),
            env: BoolOrList::Bool(false),
            ffi: BoolOrList::Bool(false),
            child: BoolOrList::Bool(false),
            worker: BoolOrList::Bool(false),
        }
    }

    // -------------------------------------------------------------- fs read

    #[test]
    fn read_denied_when_false() {
        let p = Permissions::from_opts(Some(PermissionsOptions {
            read: BoolOrList::Bool(false),
            ..Default::default()
        }));
        assert!(p.check_read("/tmp/foo").is_err());
    }

    #[test]
    fn read_granted_when_true() {
        let p = Permissions::from_opts(Some(PermissionsOptions {
            read: BoolOrList::Bool(true),
            ..Default::default()
        }));
        assert!(p.check_read("/tmp/foo").is_ok());
    }

    #[test]
    fn read_granted_for_listed_prefix() {
        let p = Permissions::from_opts(Some(opts_read_only(vec!["/tmp/allowed"])));
        assert!(p.check_read("/tmp/allowed/file.txt").is_ok());
    }

    #[test]
    fn read_denied_outside_listed_prefix() {
        let p = Permissions::from_opts(Some(opts_read_only(vec!["/tmp/allowed"])));
        assert!(p.check_read("/etc/passwd").is_err());
    }

    // -------------------------------------------------------------- fs write

    #[test]
    fn write_denied_when_false() {
        let p = Permissions::from_opts(Some(PermissionsOptions {
            write: BoolOrList::Bool(false),
            ..Default::default()
        }));
        assert!(p.check_write("/tmp/out.txt").is_err());
    }

    #[test]
    fn write_granted_when_true() {
        let p = Permissions::default();
        assert!(p.check_write("/tmp/out.txt").is_ok());
    }

    // ----------------------------------------------------------------- net

    #[test]
    fn net_denied_when_false() {
        let p = Permissions::from_opts(Some(PermissionsOptions {
            net: BoolOrList::Bool(false),
            ..Default::default()
        }));
        assert!(p.check_net("example.com").is_err());
    }

    #[test]
    fn net_granted_for_listed_host() {
        let p = Permissions::from_opts(Some(opts_net_only(vec!["example.com"])));
        assert!(p.check_net("example.com").is_ok());
    }

    #[test]
    fn net_denied_for_unlisted_host() {
        let p = Permissions::from_opts(Some(opts_net_only(vec!["example.com"])));
        assert!(p.check_net("evil.com").is_err());
    }

    // --------------------------------------------------------------- query

    #[test]
    fn query_state_all() {
        let p = Permissions::default();
        assert_eq!(p.query_state("read", None), "granted");
        assert_eq!(p.query_state("net", None), "granted");
    }

    #[test]
    fn query_state_none() {
        let p = Permissions::from_opts(Some(PermissionsOptions {
            read: BoolOrList::Bool(false),
            net: BoolOrList::Bool(false),
            ..Default::default()
        }));
        assert_eq!(p.query_state("read", None), "denied");
        assert_eq!(p.query_state("net", None), "denied");
    }

    #[test]
    fn query_state_list_scoped() {
        let p = Permissions::from_opts(Some(opts_read_only(vec!["/tmp/a"])));
        assert_eq!(p.query_state("read", Some("/tmp/a/file")), "granted");
        assert_eq!(p.query_state("read", Some("/etc/passwd")), "denied");
    }

    #[test]
    fn default_permissions_allow_all() {
        let p = Permissions::default();
        assert!(p.check_read("/any/path").is_ok());
        assert!(p.check_write("/any/path").is_ok());
        assert!(p.check_net("any.host").is_ok());
        assert!(p.check_env("ANY_VAR").is_ok());
    }

    // ------------------------------------------- relative paths, node's way
    //
    // These read the cwd but never change it: tests share one process.

    fn cwd() -> String {
        std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn opts_write_only(list: Vec<String>) -> PermissionsOptions {
        PermissionsOptions {
            write: BoolOrList::List(list),
            ..opts_read_only(vec![])
        }
    }

    #[test]
    fn relative_target_resolves_against_the_cwd_before_matching() {
        // node: --allow-fs-write=<cwd> admits writeFileSync("out.txt") and
        // mkdtempSync("tmp-") (template "tmp-XXXXXX"); oam denied both.
        let p = Permissions::from_opts(Some(opts_write_only(vec![cwd()])));
        assert!(p.check_write("out.txt").is_ok());
        assert!(p.check_write("tmp-XXXXXX").is_ok());
        assert!(p.check_write("./sub/tmp-XXXXXX").is_ok());
        assert!(p.check_write("").is_ok(), "the empty path names the cwd");
        // Resolution does not widen the grant: up and out is still out.
        assert!(p.check_write("../outside").is_err());
        assert!(p.check_write("sub/../../outside").is_err());
        assert_eq!(p.query_state("write", Some("out.txt")), "granted");
        assert_eq!(p.query_state("write", Some("../outside")), "denied");
        // The denial names the path as passed, not the resolved one.
        assert_eq!(
            p.check_write("../outside").unwrap_err().resource,
            "../outside"
        );
    }

    #[test]
    fn relative_grant_resolves_against_the_cwd_when_built() {
        let here = std::path::PathBuf::from(cwd());
        let name = here.file_name().unwrap().to_string_lossy().into_owned();
        for grant in [".".to_string(), format!("../{name}")] {
            let p = Permissions::from_opts(Some(opts_write_only(vec![grant.clone()])));
            assert!(p.check_write("out.txt").is_ok(), "grant {grant}");
            assert!(
                p.check_write(&here.join("out.txt").to_string_lossy())
                    .is_ok(),
                "grant {grant} covers the absolute spelling too"
            );
            assert!(p.check_write("../sibling").is_err(), "grant {grant}");
        }
    }

    #[test]
    fn blank_grant_entry_still_grants_nothing() {
        let p = Permissions::from_opts(Some(opts_write_only(vec![String::new()])));
        assert!(p.check_write("out.txt").is_err());
        assert!(p.check_write("").is_err());
    }

    #[test]
    fn rooted_targets_are_not_moved_onto_the_cwd() {
        // A rooted target is matched as given, so `/box/allowed` keeps
        // meaning the same subtree whatever the cwd is.
        let p = Permissions::from_opts(Some(opts_write_only(vec!["/box/allowed".into()])));
        assert!(p.check_write("/box/allowed/f").is_ok());
        assert!(p.check_write("f").is_err());
    }

    #[test]
    fn denial_carries_node_permission_and_resource() {
        let p = Permissions::from_opts(Some(PermissionsOptions {
            read: BoolOrList::Bool(false),
            ..Default::default()
        }));
        let denial = p.check_read("/etc/passwd").unwrap_err();
        assert_eq!(denial.permission, "FileSystemRead");
        assert_eq!(denial.resource, "/etc/passwd");
        assert_eq!(denial.to_string(), "Access to this API has been restricted");
    }
}
