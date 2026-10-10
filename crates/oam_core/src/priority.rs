//! Process scheduling priority: node's `os.getPriority` / `os.setPriority`
//! (libuv's `uv_os_getpriority` / `uv_os_setpriority`), and the
//! `OAM_PRIORITY` / `OAM_ECOQOS` knobs `oam` applies to itself at startup.
//!
//! Priorities are on node's scale, -20 (highest) to 19 (lowest), the unix
//! nice range. On Windows libuv maps that scale onto the six priority
//! classes and back, and so does this file: setting 12 asks for
//! BELOW_NORMAL, and reading BELOW_NORMAL back answers 10.
//!
//! Errors are libuv's code names (`ESRCH`, `EPERM`, `EACCES`, ...), which the
//! JS side turns into node's `ERR_SYSTEM_ERROR`.

/// node's `os.constants.priority` (libuv's UV_PRIORITY_*).
pub const PRIORITY_LOW: i32 = 19;
pub const PRIORITY_BELOW_NORMAL: i32 = 10;
pub const PRIORITY_NORMAL: i32 = 0;
pub const PRIORITY_ABOVE_NORMAL: i32 = -7;
pub const PRIORITY_HIGH: i32 = -14;
pub const PRIORITY_HIGHEST: i32 = -20;

/// `uv_os_getpriority(pid)`. `pid` 0 is this process.
pub fn get_priority(pid: i32) -> Result<i32, &'static str> {
    imp::get_priority(pid)
}

/// `uv_os_setpriority(pid, priority)`. `pid` 0 is this process. Out-of-range
/// priorities are `EINVAL` (node validates them before they get here).
pub fn set_priority(pid: i32, priority: i32) -> Result<(), &'static str> {
    if !(PRIORITY_HIGHEST..=PRIORITY_LOW).contains(&priority) {
        return Err("EINVAL");
    }
    imp::set_priority(pid, priority)
}

/// An `OAM_PRIORITY` value on node's scale: a name (`normal`,
/// `below_normal`, `low`/`idle`) or an integer 0..=19. Only lowering is
/// offered -- the knob exists to make background runtimes yield to
/// interactive ones, and raising priority is what `os.setPriority` is for.
fn parse_priority_env(value: &str) -> Option<i32> {
    let v = value.trim().to_ascii_lowercase().replace('-', "_");
    match v.as_str() {
        "normal" => Some(PRIORITY_NORMAL),
        "below_normal" | "belownormal" => Some(PRIORITY_BELOW_NORMAL),
        "low" | "idle" => Some(PRIORITY_LOW),
        _ => v
            .parse::<i32>()
            .ok()
            .filter(|n| (PRIORITY_NORMAL..=PRIORITY_LOW).contains(n)),
    }
}

/// The priority `OAM_PRIORITY=requested` moves this process to, if any:
/// only ever a lower one (a higher number). A process its launcher already
/// lowered further -- `start /low`, `nice -n 19`, or an `oam` parent at idle
/// whose class a Windows child inherits -- stays where it is: Windows lets
/// any process raise its own class back to NORMAL, so applying the value
/// as given would silently undo the launcher's choice. An unreadable current
/// priority leaves it alone too.
fn lowered_to(requested: i32, current: Option<i32>) -> Option<i32> {
    current.filter(|&c| requested > c).map(|_| requested)
}

/// Applies `OAM_PRIORITY` and `OAM_ECOQOS=1` to this process. Call it first
/// thing in `main`, before any thread exists: on Linux a nice value is
/// per-thread and new threads inherit their creator's, and on Windows the
/// BELOW_NORMAL and IDLE classes pass to child processes. Both variables are
/// in the environment every child inherits, so each `oam` in a fork tree
/// applies them to itself as well.
///
/// Lowering only (see `lowered_to`), so `normal` changes nothing unless the
/// process was started above normal. Best effort: a value it cannot parse is
/// reported once on stderr and ignored, and so is a refusal from the OS.
pub fn apply_env() {
    if let Ok(raw) = std::env::var("OAM_PRIORITY")
        && !raw.trim().is_empty()
    {
        match parse_priority_env(&raw) {
            Some(p) => {
                if let Some(p) = lowered_to(p, get_priority(0).ok()) {
                    let _ = set_priority(0, p);
                }
            }
            None => eprintln!(
                "oam: ignoring OAM_PRIORITY={raw:?}: expected normal, below_normal, idle, \
                 or a number from 0 to 19"
            ),
        }
    }
    if matches!(
        std::env::var("OAM_ECOQOS")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Ok("1" | "true" | "on")
    ) {
        imp::enable_ecoqos();
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, HANDLE};
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass,
        HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess,
        PROCESS_ACCESS_RIGHTS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, ProcessPowerThrottling,
        REALTIME_PRIORITY_CLASS, SetPriorityClass, SetProcessInformation,
    };

    /// The last Win32 error as libuv's code name.
    fn last_error() -> &'static str {
        crate::node_error_code(&std::io::Error::last_os_error())
    }

    /// The current-process pseudo-handle, `(HANDLE)-1` -- the documented,
    /// fixed value GetCurrentProcess returns. Always valid, never closed.
    fn current_process() -> HANDLE {
        -1isize as HANDLE
    }

    /// libuv's `uv__get_handle`: the current-process pseudo-handle for pid 0
    /// (never closed), else an opened handle owned by the second field. A
    /// pid OpenProcess rejects as ERROR_INVALID_PARAMETER -- one that does
    /// not exist -- is `ESRCH`, as libuv reports it.
    fn process_handle(
        pid: i32,
        access: PROCESS_ACCESS_RIGHTS,
    ) -> Result<(HANDLE, Option<OwnedHandle>), &'static str> {
        if pid == 0 {
            return Ok((current_process(), None));
        }
        // SAFETY: OpenProcess takes plain values; a null result is checked
        // before the handle is used, and a non-null one is owned (closed on
        // drop) by the returned OwnedHandle alone.
        unsafe {
            let raw = OpenProcess(access, 0, pid as u32);
            if raw.is_null() {
                let err = std::io::Error::last_os_error();
                return Err(
                    if err.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                        "ESRCH"
                    } else {
                        crate::node_error_code(&err)
                    },
                );
            }
            Ok((raw, Some(OwnedHandle::from_raw_handle(raw))))
        }
    }

    pub(super) fn get_priority(pid: i32) -> Result<i32, &'static str> {
        let (handle, _owned) = process_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        // SAFETY: `handle` is valid for the call: the current-process
        // pseudo-handle, or an open handle kept alive by `_owned`.
        let class = unsafe { GetPriorityClass(handle) };
        Ok(match class {
            0 => return Err(last_error()),
            REALTIME_PRIORITY_CLASS => PRIORITY_HIGHEST,
            HIGH_PRIORITY_CLASS => PRIORITY_HIGH,
            ABOVE_NORMAL_PRIORITY_CLASS => PRIORITY_ABOVE_NORMAL,
            NORMAL_PRIORITY_CLASS => PRIORITY_NORMAL,
            BELOW_NORMAL_PRIORITY_CLASS => PRIORITY_BELOW_NORMAL,
            _ => PRIORITY_LOW,
        })
    }

    pub(super) fn set_priority(pid: i32, priority: i32) -> Result<(), &'static str> {
        let class = if priority < PRIORITY_HIGH {
            REALTIME_PRIORITY_CLASS
        } else if priority < PRIORITY_ABOVE_NORMAL {
            HIGH_PRIORITY_CLASS
        } else if priority < PRIORITY_NORMAL {
            ABOVE_NORMAL_PRIORITY_CLASS
        } else if priority < PRIORITY_BELOW_NORMAL {
            NORMAL_PRIORITY_CLASS
        } else if priority < PRIORITY_LOW {
            BELOW_NORMAL_PRIORITY_CLASS
        } else {
            IDLE_PRIORITY_CLASS
        };
        let (handle, _owned) = process_handle(pid, PROCESS_SET_INFORMATION)?;
        // SAFETY: `handle` is valid for the call: the current-process
        // pseudo-handle, or an open handle kept alive by `_owned`.
        if unsafe { SetPriorityClass(handle, class) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// EcoQoS: opt this process into execution-speed power throttling, so
    /// the scheduler prefers efficiency cores and lower clocks for it. A
    /// Windows 10 1709+ API; on an older build the call fails and nothing
    /// changes.
    pub(super) fn enable_ecoqos() {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        // SAFETY: the current-process pseudo-handle is always valid, and the
        // call reads exactly size_of::<PROCESS_POWER_THROTTLING_STATE>()
        // bytes through a pointer to `state`, a live, initialized local of
        // that type.
        unsafe {
            SetProcessInformation(
                current_process(),
                ProcessPowerThrottling,
                (&raw const state).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
        }
    }
}

#[cfg(unix)]
mod imp {
    use rustix::process::{Pid, getpriority_process, setpriority_process};

    /// libuv passes the pid to getpriority/setpriority as given; 0 is the
    /// caller. rustix spells "the caller" None and has no negative pids, and
    /// the kernel finds no process for one, so that is `ESRCH` here.
    fn pid_arg(pid: i32) -> Result<Option<Pid>, &'static str> {
        if pid == 0 {
            return Ok(None);
        }
        Pid::from_raw(pid).map(Some).ok_or("ESRCH")
    }

    fn code(e: rustix::io::Errno) -> &'static str {
        crate::node_error_code(&crate::io_from_errno(e))
    }

    pub(super) fn get_priority(pid: i32) -> Result<i32, &'static str> {
        getpriority_process(pid_arg(pid)?).map_err(code)
    }

    pub(super) fn set_priority(pid: i32, priority: i32) -> Result<(), &'static str> {
        setpriority_process(pid_arg(pid)?, priority).map_err(code)
    }

    /// EcoQoS is a Windows scheduler feature; there is nothing to do here.
    pub(super) fn enable_ecoqos() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_env_names_and_numbers() {
        assert_eq!(parse_priority_env("normal"), Some(0));
        assert_eq!(parse_priority_env("below_normal"), Some(10));
        assert_eq!(parse_priority_env("Below-Normal"), Some(10));
        assert_eq!(parse_priority_env(" idle "), Some(19));
        assert_eq!(parse_priority_env("low"), Some(19));
        assert_eq!(parse_priority_env("15"), Some(15));
        assert_eq!(parse_priority_env("0"), Some(0));
        // Raising and nonsense are refused.
        assert_eq!(parse_priority_env("-5"), None);
        assert_eq!(parse_priority_env("high"), None);
        assert_eq!(parse_priority_env("20"), None);
        assert_eq!(parse_priority_env(""), None);
    }

    #[test]
    fn priority_env_never_raises() {
        assert_eq!(lowered_to(10, Some(0)), Some(10));
        assert_eq!(lowered_to(19, Some(10)), Some(19));
        assert_eq!(lowered_to(0, Some(-7)), Some(0));
        // Already that low, or lower: left alone.
        assert_eq!(lowered_to(10, Some(10)), None);
        assert_eq!(lowered_to(10, Some(19)), None);
        assert_eq!(lowered_to(0, Some(10)), None);
        assert_eq!(lowered_to(19, None), None);
    }

    #[test]
    fn set_priority_rejects_out_of_range() {
        assert_eq!(set_priority(0, 20), Err("EINVAL"));
        assert_eq!(set_priority(0, -21), Err("EINVAL"));
    }

    #[test]
    fn get_priority_of_self_is_on_the_scale() {
        let p = get_priority(0).unwrap();
        assert!((PRIORITY_HIGHEST..=PRIORITY_LOW).contains(&p));
        assert_eq!(get_priority(std::process::id() as i32), Ok(p));
    }

    #[test]
    fn missing_process_is_esrch() {
        assert_eq!(get_priority(i32::MAX), Err("ESRCH"));
        assert_eq!(set_priority(i32::MAX, PRIORITY_LOW), Err("ESRCH"));
    }
}
