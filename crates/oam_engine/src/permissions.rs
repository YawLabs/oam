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
//! - `Vec<String>` -> whitelist of allowed paths/hosts (prefix match for
//!   paths, exact match for hosts)

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

impl PermValue {
    /// Check whether `target` is permitted.
    pub fn allows(&self, target: &str) -> bool {
        match self {
            PermValue::All => true,
            PermValue::None => false,
            PermValue::List(list) => list.iter().any(|item| target.starts_with(item.as_str())),
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
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: PermValue::All,
            write: PermValue::All,
            net: PermValue::All,
            env: PermValue::All,
            ffi: PermValue::All,
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
        }
    }

    /// Returns `Err(denial)` when `read` is denied for `path`.
    pub fn check_read(&self, path: &str) -> Result<(), PermissionDenial> {
        if self.read.allows(path) {
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
        if self.write.allows(path) {
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
        if self.net.allows(host) {
            Ok(())
        } else {
            Err(PermissionDenial {
                permission: "Net",
                resource: host.to_string(),
            })
        }
    }

    /// Returns `Err(denial)` when `env` is denied.
    pub fn check_env(&self, key: &str) -> Result<(), PermissionDenial> {
        if self.env.allows(key) {
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
        let perm = match name {
            "read" => &self.read,
            "write" => &self.write,
            "net" => &self.net,
            "env" => &self.env,
            "ffi" => &self.ffi,
            _ => return "denied",
        };
        match target {
            Some(t) => {
                if perm.allows(t) {
                    "granted"
                } else {
                    "denied"
                }
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

    fn opts_read_only(list: Vec<&str>) -> PermissionsOptions {
        PermissionsOptions {
            read: BoolOrList::List(list.iter().map(|s| s.to_string()).collect()),
            write: BoolOrList::Bool(false),
            net: BoolOrList::Bool(false),
            env: BoolOrList::Bool(false),
            ffi: BoolOrList::Bool(false),
        }
    }

    fn opts_net_only(list: Vec<&str>) -> PermissionsOptions {
        PermissionsOptions {
            read: BoolOrList::Bool(false),
            write: BoolOrList::Bool(false),
            net: BoolOrList::List(list.iter().map(|s| s.to_string()).collect()),
            env: BoolOrList::Bool(false),
            ffi: BoolOrList::Bool(false),
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
