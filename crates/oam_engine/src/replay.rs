//! Record-replay substrate: capture non-deterministic I/O to a JSON Lines file
//! and replay it for deterministic re-execution.
//!
//! `ReplayState` lives in an isolate slot (single-threaded, no Mutex needed).
//! `Recorder` flushes its buffer to disk on `Drop` (clean exit and panics).
//! `Replayer` loads the file once at startup and drains events in order.
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

pub struct Recorder {
    path: PathBuf,
    events: Vec<ReplayEvent>,
    seq: u64,
}

impl Recorder {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            events: Vec::new(),
            seq: 0,
        }
    }

    pub fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    pub fn push(&mut self, event: ReplayEvent) {
        self.events.push(event);
    }

    fn flush_to_disk(&self) {
        use std::io::Write;
        match std::fs::File::create(&self.path) {
            Ok(mut f) => {
                for ev in &self.events {
                    if let Ok(line) = serde_json::to_string(ev) {
                        let _ = writeln!(f, "{line}");
                    }
                }
            }
            Err(e) => {
                eprintln!("oam record: cannot write {}: {e}", self.path.display());
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.flush_to_disk();
    }
}

// ---------------------------------------------------------------------------
// Replayer
// ---------------------------------------------------------------------------

pub struct Replayer {
    events: VecDeque<ReplayEvent>,
    seq: u64,
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
        Ok(Self { events, seq: 0 })
    }

    pub fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
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
    pub fn from_mode(mode: ReplayMode) -> Self {
        match mode {
            ReplayMode::Off => Self::default(),
            ReplayMode::Record(path) => Self {
                recorder: Some(Recorder::new(path)),
                replayer: None,
            },
            ReplayMode::Replay(path) => match Replayer::load(&path) {
                Ok(replayer) => Self {
                    recorder: None,
                    replayer: Some(replayer),
                },
                Err(e) => {
                    eprintln!("oam replay: {e}");
                    Self::default()
                }
            },
        }
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
    if completion.id == oam_core::SIGNAL_OP_ID {
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
