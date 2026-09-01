//! Deferred loader warnings.
//!
//! The loader runs deep inside resolution and transpilation, with no idea
//! whether the process is in `--json` mode -- so it must never print. A
//! condition it cannot honor but should not fail on (a tsconfig that does
//! not parse, an `extends` that names a package) is queued here as a
//! `Severity::Warning` ODIF diagnostic, and the CLI drains the queue through
//! its normal diagnostic renderer (`oam_loader::take_warnings`), which keeps
//! stderr pure JSONL under `--json`.
//!
//! Emitters dedupe before queueing (once per tsconfig path per code, see
//! `tsconfig::warn_once`): the same tsconfig is consulted once per resolve,
//! and a warning per resolve would drown the program's own output.

use std::sync::{Mutex, OnceLock};

use oam_diagnostics::Diagnostic;

fn sink() -> &'static Mutex<Vec<Diagnostic>> {
    static SINK: OnceLock<Mutex<Vec<Diagnostic>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(Vec::new()))
}

/// Queue a warning for the next `take_warnings` call.
pub(crate) fn push(diagnostic: Diagnostic) {
    sink()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(diagnostic);
}

/// Drain every warning the loader has queued since the last call, oldest
/// first. Process-wide: the loader has no per-command scope, and a warning
/// raised while one thread resolves belongs to whichever command is running.
/// Callers print through their own diagnostic renderer.
pub fn take_warnings() -> Vec<Diagnostic> {
    std::mem::take(&mut *sink().lock().unwrap_or_else(|p| p.into_inner()))
}

/// Tests that DRAIN the process-wide sink must not interleave: one test's
/// `take_warnings` would swallow another's just-pushed entry (the harness
/// runs a crate's tests in parallel threads of one process). Every
/// sink-consuming test holds this lock across its push -> drain window.
#[cfg(test)]
pub(crate) fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oam_diagnostics::{Origin, Severity};

    #[test]
    fn take_warnings_drains_in_order_and_empties() {
        // Serialize against the other sink-consuming tests, then drain
        // leftovers so this test only observes its own entries.
        let _serial = test_serial();
        let _ = take_warnings();
        push(Diagnostic::new(
            "OAM-TEST9001",
            Severity::Warning,
            Origin::Resolve,
            "first",
        ));
        push(Diagnostic::new(
            "OAM-TEST9002",
            Severity::Warning,
            Origin::Resolve,
            "second",
        ));
        let drained: Vec<String> = take_warnings()
            .into_iter()
            .filter(|d| d.code.starts_with("OAM-TEST900"))
            .map(|d| d.code)
            .collect();
        assert_eq!(drained, vec!["OAM-TEST9001", "OAM-TEST9002"]);
        assert!(
            take_warnings()
                .iter()
                .all(|d| !d.code.starts_with("OAM-TEST900")),
            "a second drain must not replay the same warnings"
        );
    }
}
