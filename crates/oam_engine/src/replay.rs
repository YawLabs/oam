//! Record-replay substrate: capture non-deterministic I/O to a JSON Lines file
//! and replay it for deterministic re-execution.
//!
//! `ReplayState` lives in an isolate slot (single-threaded, no Mutex needed).
//! `Recorder` writes its buffer to disk on `Drop` (a run that returns) and
//! from a process exit hook (`process.exit()` and the other hard exits, which
//! drop nothing). A write failure is reported on stderr.
//! `Replayer` loads the file once at startup and drains events by type (and,
//! for ops, by id). The recorded `seq` is informational: replay does not
//! compare it, so a run whose events arrive in a different order is not
//! detected as such.
//!
//! Determinism scope (what record/replay reproduces): async op completions
//! (`fetch`, fs, dns, ...), `Math.random`, `Date.now`, and `performance.now`.
//! When the replay trace runs out of a given event type, the corresponding
//! native falls back to the live value rather than erroring -- replay degrades
//! gracefully instead of stranding a longer run.
//!
//! NOT yet captured (known limitations): `crypto.getRandomValues` /
//! `randomUUID` and other `OsRng`-backed crypto (see crypto_ops), and
//! wall-clock read inside timers. A program leaning on those will diverge on
//! replay; capturing crypto randomness is the next increment.
//!
//! Worker isolates are outside the trace too: only the main runtime is put
//! in record or replay mode (`JsRuntime::set_replay_mode`); a `Worker` or
//! `oam.fork()` isolate keeps the default Off state, so its op completions,
//! `Math.random` and clocks are neither recorded nor replayed.

use std::collections::VecDeque;
use std::path::PathBuf;

use oam_core::{OpCompletion, OpOutcome};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum ReplayEvent {
    Op {
        seq: u64,
        id: u64,
        outcome_json: String,
    },
    Rng {
        seq: u64,
        #[serde(with = "f64_bits")]
        value: f64,
    },
    DateNow {
        seq: u64,
        value: i64,
    },
    PerfNow {
        seq: u64,
        #[serde(with = "f64_bits")]
        value: f64,
    },
}

/// Serialize an f64 as its raw IEEE-754 bits (a `u64`) instead of a decimal.
///
/// Record/replay must be bit-exact -- the replayed value has to reproduce the
/// recorded run's output character-for-character. serde_json's DECIMAL f64
/// round-trip is NOT always exact: e.g. `0.10534155930636513` serializes to
/// that string but parses back 1 ULP lower (`...512`), which made
/// `Math.random()` / `performance.now()` replay diverge from the recorded run
/// for unlucky values. Integers round-trip exactly, so we store the bits.
mod f64_bits {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.to_bits())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        Ok(f64::from_bits(u64::deserialize(d)?))
    }
}

pub enum ReplayMode {
    Off,
    Record(PathBuf),
    Replay(PathBuf),
}

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

/// The trace being recorded. Shared (not owned by the isolate slot alone) so
/// the process exit hook can write it: `process.exit()`, the EPIPE bail and
/// the heap-cap exit all leave through `oam_core::exit_process`, which never
/// drops an isolate slot.
struct Trace {
    path: PathBuf,
    events: Vec<ReplayEvent>,
    /// `events.len()` at the last successful write; `None` before the first.
    /// Makes a second flush (the hook, then `Drop`, or the reverse) a no-op
    /// unless events arrived in between.
    written: Option<usize>,
}

impl Trace {
    fn flush_to_disk(&mut self) {
        if self.written == Some(self.events.len()) {
            return;
        }
        match self.write_all() {
            Ok(()) => self.written = Some(self.events.len()),
            // The first failure, once per attempt: a trace cut short by a
            // full disk must not look like a complete one.
            Err(e) => eprintln!("oam record: cannot write {}: {e}", self.path.display()),
        }
    }

    fn write_all(&self) -> std::io::Result<()> {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&self.path)?);
        for ev in &self.events {
            // serde_json::Error converts into io::Error.
            let line = serde_json::to_string(ev)?;
            writeln!(out, "{line}")?;
        }
        out.flush()
    }
}

pub struct Recorder {
    trace: std::sync::Arc<std::sync::Mutex<Trace>>,
    seq: u64,
}

impl Recorder {
    pub fn new(path: PathBuf) -> Self {
        let trace = std::sync::Arc::new(std::sync::Mutex::new(Trace {
            path,
            events: Vec::new(),
            written: None,
        }));
        // A hard exit skips `Drop`: write the trace from the exit path too.
        // Weak, so a recorder that was dropped (and flushed) costs nothing.
        let weak = std::sync::Arc::downgrade(&trace);
        oam_core::register_exit_hook(move || {
            if let Some(trace) = weak.upgrade() {
                trace
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .flush_to_disk();
            }
        });
        Self { trace, seq: 0 }
    }

    pub fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    pub fn push(&mut self, event: ReplayEvent) {
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .push(event);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.trace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush_to_disk();
    }
}

// ---------------------------------------------------------------------------
// Replayer
// ---------------------------------------------------------------------------

pub struct Replayer {
    events: VecDeque<ReplayEvent>,
}

impl Replayer {
    pub fn load(path: &PathBuf) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("replay: cannot read {}: {e}", path.display()))?;
        let mut events = VecDeque::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let ev: ReplayEvent = serde_json::from_str(line)
                .map_err(|e| format!("replay: malformed event '{line}': {e}"))?;
            events.push_back(ev);
        }
        Ok(Self { events })
    }

    pub fn next_op(&mut self, id: u64) -> Option<String> {
        // Fast path: check if the front is the right op.
        if let Some(ReplayEvent::Op { id: fid, .. }) = self.events.front()
            && *fid == id
            && let Some(ReplayEvent::Op { outcome_json, .. }) = self.events.pop_front()
        {
            return Some(outcome_json);
        }
        // Slow path: scan for any Op with matching id (out-of-order completions).
        let pos = self
            .events
            .iter()
            .position(|e| matches!(e, ReplayEvent::Op { id: eid, .. } if *eid == id))?;
        if let Some(ReplayEvent::Op { outcome_json, .. }) = self.events.remove(pos) {
            Some(outcome_json)
        } else {
            None
        }
    }

    pub fn next_rng(&mut self) -> Option<f64> {
        let pos = self
            .events
            .iter()
            .position(|e| matches!(e, ReplayEvent::Rng { .. }))?;
        if let Some(ReplayEvent::Rng { value, .. }) = self.events.remove(pos) {
            Some(value)
        } else {
            None
        }
    }

    pub fn next_date_now(&mut self) -> Option<i64> {
        let pos = self
            .events
            .iter()
            .position(|e| matches!(e, ReplayEvent::DateNow { .. }))?;
        if let Some(ReplayEvent::DateNow { value, .. }) = self.events.remove(pos) {
            Some(value)
        } else {
            None
        }
    }

    pub fn next_perf_now(&mut self) -> Option<f64> {
        let pos = self
            .events
            .iter()
            .position(|e| matches!(e, ReplayEvent::PerfNow { .. }))?;
        if let Some(ReplayEvent::PerfNow { value, .. }) = self.events.remove(pos) {
            Some(value)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// ReplayState: isolate slot
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct ReplayState {
    pub recorder: Option<Recorder>,
    pub replayer: Option<Replayer>,
}

impl ReplayState {
    /// `Err` when the replay trace cannot be read or parsed. The caller must
    /// not run the program then: the Off state would run it live, with the
    /// real I/O and randomness a replay was asked to stand in for.
    pub fn from_mode(mode: ReplayMode) -> Result<Self, String> {
        Ok(match mode {
            ReplayMode::Off => Self::default(),
            ReplayMode::Record(path) => Self {
                recorder: Some(Recorder::new(path)),
                replayer: None,
            },
            ReplayMode::Replay(path) => Self {
                recorder: None,
                replayer: Some(Replayer::load(&path)?),
            },
        })
    }

    pub fn mode_str(&self) -> &'static str {
        if self.recorder.is_some() {
            "record"
        } else if self.replayer.is_some() {
            "replay"
        } else {
            "off"
        }
    }

    pub fn is_replay(&self) -> bool {
        self.replayer.is_some()
    }
}

// ---------------------------------------------------------------------------
// Completion intercept: called from pump_event_loop
// ---------------------------------------------------------------------------

/// In record mode: serialize the outcome and push a ReplayEvent::Op, return
/// completion unchanged. In replay mode: replace outcome with recorded one.
/// In off mode: return unchanged (zero overhead -- no slot access on hot path).
pub fn intercept_completion(state: &mut ReplayState, completion: OpCompletion) -> OpCompletion {
    // OS signals are environmental, not part of the deterministic op stream:
    // never record or replay them. Pass through untouched in every mode.
    // Nor are V8 foreground-task wakes: they carry no outcome, only timing.
    if completion.id == oam_core::SIGNAL_OP_ID || completion.id == oam_core::PLATFORM_TASK_OP_ID {
        return completion;
    }
    if let Some(recorder) = &mut state.recorder {
        let outcome_json = serde_json::to_string(&completion.outcome).unwrap_or_default();
        let seq = recorder.next_seq();
        recorder.push(ReplayEvent::Op {
            seq,
            id: completion.id,
            outcome_json,
        });
        return completion;
    }
    if let Some(replayer) = &mut state.replayer
        && let Some(json) = replayer.next_op(completion.id)
        && let Ok(outcome) = serde_json::from_str::<OpOutcome>(&json)
    {
        return OpCompletion {
            id: completion.id,
            outcome,
        };
    }
    completion
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("oam-replay-{name}-{}-{nanos}", std::process::id()))
    }

    // The trace is on disk before the recorder drops (the hard-exit hook's
    // path), a second flush with nothing new rewrites nothing, and a later
    // event is still written by the drop.
    #[test]
    fn trace_flushes_without_a_drop_and_again_only_when_it_grew() {
        let path = scratch("flush");
        let mut recorder = Recorder::new(path.clone());
        recorder.push(ReplayEvent::DateNow { seq: 0, value: 7 });
        let flush = |r: &Recorder| r.trace.lock().unwrap().flush_to_disk();
        flush(&recorder);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        std::fs::remove_file(&path).unwrap();
        flush(&recorder);
        assert!(!path.exists(), "an unchanged trace is not rewritten");
        recorder.push(ReplayEvent::DateNow { seq: 1, value: 8 });
        drop(recorder);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // A trace that cannot be read or parsed is an error, not the Off state.
    #[test]
    fn unusable_replay_trace_is_an_error() {
        let missing = scratch("missing");
        assert!(ReplayState::from_mode(ReplayMode::Replay(missing)).is_err());
        let malformed = scratch("malformed");
        std::fs::write(&malformed, "not json\n").unwrap();
        assert!(ReplayState::from_mode(ReplayMode::Replay(malformed.clone())).is_err());
        let _ = std::fs::remove_file(&malformed);
        assert!(ReplayState::from_mode(ReplayMode::Off).is_ok());
    }

    // Regression: f64 ReplayEvents must round-trip through JSON bit-exactly, so
    // a replayed Math.random / performance.now reproduces the recorded run
    // character-for-character. 0.10534155930636513 is a value serde_json's
    // decimal float round-trip got wrong (came back 1 ULP low) before the
    // bits-based encoding.
    #[test]
    fn f64_events_roundtrip_bit_exact() {
        let values = [
            0.10534155930636513f64,
            0.10534155930636512f64,
            0.5,
            0.0,
            1.0,
            f64::MIN_POSITIVE,
            0.123_456_789_012_345_68,
        ];
        for v in values {
            for ev in [
                ReplayEvent::Rng { seq: 1, value: v },
                ReplayEvent::PerfNow { seq: 2, value: v },
            ] {
                let json = serde_json::to_string(&ev).unwrap();
                let back: ReplayEvent = serde_json::from_str(&json).unwrap();
                let got = match back {
                    ReplayEvent::Rng { value, .. } | ReplayEvent::PerfNow { value, .. } => value,
                    _ => unreachable!(),
                };
                assert_eq!(
                    v.to_bits(),
                    got.to_bits(),
                    "f64 must round-trip bit-exact; {v} -> {json} -> {got}"
                );
            }
        }
    }
}
