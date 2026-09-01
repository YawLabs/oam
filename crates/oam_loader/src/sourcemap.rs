//! Runtime source-map registry: generated-position -> source-position
//! lookup for transpiled files, so stack traces and uncaught-error reports
//! cite the .ts/.tsx/.cts line the user wrote instead of the oxc codegen
//! line (codegen reflows; Node's strip-only pipeline never did).
//!
//! Shape: a process-global table `file -> map` populated by whoever
//! prepares a transpiled source for execution (the ESM host, the CJS
//! require path, both cache hits and fresh transpiles), consulted lazily by
//! every surface that formats a V8 position (the engine's fatal/diagnostic
//! paths in Rust, `Error.prepareStackTrace` in JS via the
//! `__oam.mapPosition` op). Process-global rather than per-isolate because
//! the ESM preparer (`ModuleHost::load`) runs with no isolate in reach, and
//! worker isolates on other threads load the same files; the key is the
//! canonical module path, so the worst concurrent case is two threads
//! recording identical content.
//!
//! Decoding is deliberately minimal: only the `mappings` VLQ walk needed
//! for greatest-lower-bound lookup on (generated line, generated column).
//! Names, sourcesContent, and multi-source index bookkeeping beyond the
//! delta tracking are ignored -- oam's transpile maps are single-source.
//! A map that fails to decode is remembered as invalid: every lookup
//! misses, and callers keep the generated position (correct fallback).
//!
//! Column conventions (documented on `lookup`): lines are 1-based on both
//! sides, columns 0-based on both sides -- the raw source-map convention
//! plus the 1-based line every human-facing surface prints.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

enum Entry {
    /// Recorded, not yet needed by any lookup.
    Raw(String),
    /// Decoded on first lookup.
    Decoded(DecodedMap),
    /// Failed to decode; every lookup misses (callers keep the generated
    /// position). Remembered so a corrupt map is not re-parsed per frame.
    Invalid,
}

/// One generated-position segment: everything `lookup` needs, pre-sorted
/// by `gen_col` within its line.
struct Seg {
    gen_col: u32,
    src_line: u32,
    src_col: u32,
}

struct DecodedMap {
    /// Indexed by 0-based generated line; each line's segments sorted by
    /// generated column (the VLQ format guarantees ascending order, kept
    /// as-is).
    lines: Vec<Vec<Seg>>,
}

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the source map for `file` (the canonical module path string, as
/// the engine names the script to V8). Overwrites any prior entry -- the
/// latest prepared content wins. `map_json` is the standard v3 JSON; it is
/// not validated here (decode is lazy, and an invalid map just never
/// remaps).
pub fn record(file: &str, map_json: String) {
    let mut table = registry().lock().unwrap_or_else(|e| e.into_inner());
    table.insert(file.to_string(), Entry::Raw(map_json));
}

/// True when a map has been recorded for `file` (regardless of validity).
pub fn has_map(file: &str) -> bool {
    let table = registry().lock().unwrap_or_else(|e| e.into_inner());
    table.contains_key(file)
}

/// Map a generated position to the original source position for `file`.
///
/// `line` is 1-based, `column` 0-based (V8's `Message::get_start_column`
/// convention; JS `CallSite` columns are 1-based -- the op layer converts).
/// Returns `(line, column)` with the same conventions, or `None` when no
/// map is recorded, the map is invalid, the line is beyond the map, or no
/// segment starts at-or-before `column` on that generated line
/// (greatest-lower-bound bias, like every source-map consumer's default).
pub fn lookup(file: &str, line: u32, column: u32) -> Option<(u32, u32)> {
    if line == 0 {
        return None;
    }
    let mut table = registry().lock().unwrap_or_else(|e| e.into_inner());
    let entry = table.get_mut(file)?;
    if let Entry::Raw(json) = entry {
        *entry = match decode(json) {
            Some(map) => Entry::Decoded(map),
            None => Entry::Invalid,
        };
    }
    let Entry::Decoded(map) = entry else {
        return None;
    };
    let segs = map.lines.get((line - 1) as usize)?;
    // Greatest segment with gen_col <= column. partition_point returns the
    // first index where gen_col > column; the segment before it is the GLB.
    let idx = segs.partition_point(|s| s.gen_col <= column);
    let seg = segs.get(idx.checked_sub(1)?)?;
    Some((seg.src_line + 1, seg.src_col))
}

/// Decode the `mappings` field out of a v3 source-map JSON string into the
/// lookup table. Only the fields lookup needs; `None` on any malformed
/// input (bad JSON, missing/непарseable mappings, VLQ garbage).
fn decode(map_json: &str) -> Option<DecodedMap> {
    let value: serde_json::Value = serde_json::from_str(map_json).ok()?;
    let mappings = value.get("mappings")?.as_str()?;
    decode_mappings(mappings)
}

fn decode_mappings(mappings: &str) -> Option<DecodedMap> {
    let mut lines = Vec::new();
    // Source-index / source-line / source-column deltas carry across the
    // whole mappings string; generated column resets per generated line.
    let mut src_index: i64 = 0;
    let mut src_line: i64 = 0;
    let mut src_col: i64 = 0;
    for group in mappings.split(';') {
        let mut segs = Vec::new();
        let mut gen_col: i64 = 0;
        if !group.is_empty() {
            for seg in group.split(',') {
                let fields = decode_vlq(seg)?;
                match fields.len() {
                    1 => {
                        // Generated-only segment: no source info, advances
                        // the generated column but produces no mapping.
                        gen_col += fields[0];
                    }
                    4 | 5 => {
                        gen_col += fields[0];
                        src_index += fields[1];
                        src_line += fields[2];
                        src_col += fields[3];
                        // (fields[4] is a names index -- unused.)
                        if gen_col < 0 || src_line < 0 || src_col < 0 || src_index < 0 {
                            return None;
                        }
                        segs.push(Seg {
                            gen_col: u32::try_from(gen_col).ok()?,
                            src_line: u32::try_from(src_line).ok()?,
                            src_col: u32::try_from(src_col).ok()?,
                        });
                    }
                    _ => return None,
                }
            }
        }
        lines.push(segs);
    }
    Some(DecodedMap { lines })
}

/// Decode one comma-separated VLQ segment into its signed fields.
fn decode_vlq(seg: &str) -> Option<Vec<i64>> {
    const CONTINUATION: u32 = 32;
    let mut fields = Vec::new();
    let mut value: i64 = 0;
    let mut shift: u32 = 0;
    let mut in_field = false;
    for byte in seg.bytes() {
        let digit = base64_value(byte)? as u32;
        in_field = true;
        // Cap the shift: 7 base64 digits already exceed any real position,
        // and an unbounded shift is UB-adjacent overflow territory.
        if shift > 62 {
            return None;
        }
        value += i64::from(digit & (CONTINUATION - 1)) << shift;
        if digit & CONTINUATION == 0 {
            let negative = value & 1 == 1;
            let magnitude = value >> 1;
            fields.push(if negative { -magnitude } else { magnitude });
            value = 0;
            shift = 0;
            in_field = false;
        } else {
            shift += 5;
        }
    }
    // A dangling continuation bit is malformed.
    if in_field || fields.is_empty() {
        return None;
    }
    Some(fields)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique file keys per test: the registry is process-global and cargo
    /// runs tests in parallel.
    fn key(label: &str) -> String {
        format!("sourcemap-test://{label}-{}", std::process::id())
    }

    #[test]
    fn vlq_decodes_known_vectors() {
        // "AAAA" = [0,0,0,0]; "IACK" = [4,0,1,5]... spot-check the
        // canonical encodings.
        assert_eq!(decode_vlq("AAAA").unwrap(), vec![0, 0, 0, 0]);
        assert_eq!(decode_vlq("A").unwrap(), vec![0]);
        assert_eq!(decode_vlq("C").unwrap(), vec![1]);
        assert_eq!(decode_vlq("D").unwrap(), vec![-1]);
        assert_eq!(decode_vlq("gB").unwrap(), vec![16]);
        // Dangling continuation bit is malformed, not a panic.
        assert_eq!(decode_vlq("g"), None);
        // Non-base64 bytes are malformed.
        assert_eq!(decode_vlq("A!"), None);
    }

    #[test]
    fn lookup_maps_generated_to_source_with_glb_bias() {
        // Hand-built map: generated line 1 col 0 -> source line 3 col 0
        // ("AGAA": [0,+3 src lines? no -- fields are [gen_col, src_idx,
        // src_line, src_col]): "AAGA" = [0,0,3,0]. Generated line 2 has two
        // segments: col 0 -> src 5:0 and col 10 -> src 6:2.
        let file = key("glb");
        let map = r#"{"version":3,"sources":["x.ts"],"names":[],"mappings":"AAGA;AAEA,UACE"}"#;
        record(&file, map.to_string());
        // Gen 1:0 -> src line 4 (0-based 3), col 0.
        assert_eq!(lookup(&file, 1, 0), Some((4, 0)));
        // GLB bias: any column on gen line 1 maps to the same segment.
        assert_eq!(lookup(&file, 1, 99), Some((4, 0)));
        // Gen 2:0 -> src 6:0 (deltas: +2 lines from line 4).
        assert_eq!(lookup(&file, 2, 0), Some((6, 0)));
        // Gen 2 col 9 is before the second segment (col 10) -> GLB is col 0.
        assert_eq!(lookup(&file, 2, 9), Some((6, 0)));
        // Gen 2 col 10 and beyond -> second segment, src 7:2.
        assert_eq!(lookup(&file, 2, 10), Some((7, 2)));
        assert_eq!(lookup(&file, 2, 40), Some((7, 2)));
        // Beyond the map -> None.
        assert_eq!(lookup(&file, 3, 0), None);
        assert_eq!(lookup(&file, 0, 0), None, "line 0 never maps");
    }

    #[test]
    fn unrecorded_and_invalid_maps_miss() {
        assert_eq!(lookup(&key("never-recorded"), 1, 0), None);
        let bad = key("invalid");
        record(&bad, "not json at all".to_string());
        assert_eq!(lookup(&bad, 1, 0), None);
        // Still remembered (as invalid) -- has_map is about registration.
        assert!(has_map(&bad));
        // Valid JSON, garbage mappings -> invalid too.
        let garbage = key("garbage-mappings");
        record(
            &garbage,
            r#"{"version":3,"sources":[],"names":[],"mappings":"!!"}"#.to_string(),
        );
        assert_eq!(lookup(&garbage, 1, 0), None);
    }

    #[test]
    fn record_overwrites_prior_entry() {
        let file = key("overwrite");
        record(
            &file,
            r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"AAAA"}"#.to_string(),
        );
        assert_eq!(lookup(&file, 1, 0), Some((1, 0)));
        // New content for the same path (file edited between prepares).
        record(
            &file,
            r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"AAGA"}"#.to_string(),
        );
        assert_eq!(lookup(&file, 1, 0), Some((4, 0)));
    }

    /// The acceptance core, engine-free: transpile the brief's shape (an
    /// erased `import type`, a multi-line interface, a `throw` on source
    /// line 7 that codegen hoists to line ~2) and prove the emitted map
    /// routes the generated throw position back to line 7.
    #[test]
    fn transpiled_map_routes_generated_throw_back_to_source_line_7() {
        let source = "import type { Missing } from './x.js';\n\
                      interface Wide {\n  a: number;\n  b: string;\n}\n\
                      const n: number = 6;\n\
                      throw new Error('boom' + n);\n";
        let path = std::path::Path::new("fidelity.ts");
        let out = crate::transpile_typescript_mapped(path, source).unwrap();
        let map = out.source_map.expect("mapped entry point emits a map");
        let file = key("fidelity");
        record(&file, map);
        // Find `throw` in the GENERATED text; its mapped position must be
        // source line 7 (1-based), where the user wrote it.
        let (gen_line0, gen_col0) = out
            .code
            .lines()
            .enumerate()
            .find_map(|(i, l)| l.find("throw").map(|c| (i, c)))
            .expect("codegen keeps the throw");
        assert_ne!(gen_line0, 6, "codegen must reflow for this test to bite");
        let (src_line, _) =
            lookup(&file, (gen_line0 + 1) as u32, gen_col0 as u32).expect("throw position maps");
        assert_eq!(src_line, 7, "generated throw maps to source line 7");
        // The plain (unmapped) entry point emits no map -- its callers pay
        // nothing for this feature.
        assert!(
            crate::transpile_typescript(path, source).is_ok(),
            "plain wrapper still works"
        );
    }

    #[test]
    fn generated_only_segments_produce_no_mapping() {
        let file = key("gen-only");
        // Line 1: a 1-field segment (no source info) at col 0 -- lookups on
        // that line miss rather than inventing a position.
        record(
            &file,
            r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"E"}"#.to_string(),
        );
        assert_eq!(lookup(&file, 1, 0), None);
        assert_eq!(lookup(&file, 1, 10), None);
    }
}
