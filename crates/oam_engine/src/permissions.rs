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
fn host_of(hostport: &str) -> &str {
    if let Some(end) = hostport.strip_prefix('[').and_then(|_| hostport.find(']')) {
        // `[::1]:8080` -> `[::1]`
        return &hostport[..=end];
    }
    match hostport.rsplit_once(':') {
        // A bare IPv6 literal has several colons and no port; leave it whole.
        Some((head, _)) if !head.contains(':') => head,
        _ => hostport,
    }
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

    /// Exact match on a `host[:port]`. An entry carrying a port matches only
    /// that port; a bare-host entry matches the host on any port.
    ///
    /// Used by net ONLY. `--allow-net=127.0.0.1:5432` must not admit `:54321`.
    pub fn allows_net(&self, target: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
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
            read: from_bool_or_list(o.read),
            write: from_bool_or_list(o.write),
            net: from_bool_or_list(o.net),
            env: from_bool_or_list(o.env),
            ffi: from_bool_or_list(o.ffi),
            child: from_bool_or_list(o.child),
            worker: from_bool_or_list(o.worker),
        }
    }

    /// Returns `Err(denial)` when `read` is denied for `path`.
    pub fn check_read(&self, path: &str) -> Result<(), PermissionDenial> {
        if self.read.allows_path(path) {
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
        if self.write.allows_path(path) {
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
                    "read" | "write" => perm.allows_path(t),
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

impl std::fmt::Display for PermissionDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Access to this API has been restricted")
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
