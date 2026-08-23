//! `cargo run -p xtask -- unsafe-budget`: the CEILING ratchet for the `unsafe`
//! surface (AI-POLICY.md gate 5 -- the "`unsafe` budget").
//!
//! It scans EVERY workspace member -- the crates under `crates/` plus `xtask`
//! itself -- and within each one every `.rs` file under `src/` and `tests/`,
//! plus a top-level `build.rs`. `tests/` and `build.rs` are compiled Rust that
//! CI builds and runs, and `xtask` is the gate's own tooling (no `[lints]`
//! section, no `forbid(unsafe_code)`), so leaving any of the three out of the
//! walk would make unsafe landing there invisible to every gate in the repo.
//! `benches/` and `examples/` are NOT walked -- no crate in the tree has
//! either; adding one means widening this walk again.
//!
//! In each file it counts the REAL unsafe constructs per crate (`unsafe fn` /
//! `unsafe impl` / `unsafe extern` block / `unsafe extern "C" fn <name>` FFI
//! definition / `unsafe {` block), then diffs the per-crate total against the
//! committed baseline in `conformance/unsafe-budget.json`.
//!
//! The counter is deliberately NOT the old `grep -c unsafe` (scripts/ci-local.sh
//! step 9, which it replaces): that grep over-counted (885 matched lines vs 704
//! real constructs on the tree at introduction) by including
//! `#[unsafe(no_mangle)]` attributes, `// SAFETY:` comments, and
//! `unsafe extern "C" fn(...)` used as function-POINTER TYPES (declarations
//! with no body). Those are lexed out here so the number tracks actual unsafe
//! code, not mentions of the word.
//!
//! # This gate COUNTS; clippy COVERS
//!
//! There used to be a second metric here: a `documented_count` FLOOR over
//! `// SAFETY:` line comments, a lexical proxy for per-site justification. It
//! is RETIRED, and must not be restored. The lexical floor and clippy's
//! `unnecessary_safety_comment` lint were in direct CONFLICT: roughly 155 of
//! the comments the floor was counting are ones the lint FORBIDS. A
//! `// SAFETY:` sitting on an unsafe FN DEFINITION is misplaced -- the contract
//! a caller must uphold belongs in a `/// # Safety` doc section, not in a line
//! comment above the signature -- and an `unsafe extern` FOREIGN MODULE needs
//! no justification at all. So raising the floor meant writing comments clippy
//! rejects, and satisfying clippy meant falling below the floor. Those ~155
//! misplaced comments have now been fixed across the tree, and the floor is
//! retired with them.
//!
//! Per-site coverage is henceforth owned by clippy, applied to EVERY crate:
//! `undocumented_unsafe_blocks = deny` (every unsafe block/impl carries a
//! justification), `unnecessary_safety_comment = deny` (nothing that must NOT
//! carry one does), and `missing_safety_doc = deny` (every public unsafe fn
//! carries a `# Safety` section). Clippy checks each SITE, which counting lines
//! cannot do; a count can be ratcheted DOWNWARD over time, which clippy cannot
//! do. This file keeps only the half clippy provably cannot: the per-crate
//! `unsafe_count` CEILING.
//!
//! Known residue, named honestly, because "clippy owns coverage" is not the
//! same as "everything is covered": `missing_safety_doc` fires only on PUBLIC
//! items, so a PRIVATE `unsafe fn` is structurally uncovered, and an
//! `unsafe extern` foreign module is not a site any of the three lints demands
//! a justification on. Those two shapes rely on REVIEW, not on a lint. They are
//! still inside this ceiling, though, so one cannot be ADDED without a reviewed
//! baseline bump -- which is exactly where a human notices them.
//!
//! The gate is a one-way ratchet, like `conformance.rs::compare_exports` and
//! `node_suite.rs::ratchet_violation`: `unsafe_count` is a CEILING (may only go
//! DOWN). ABOVE it = new unsafe added -> FAIL. Strictly BELOW it = you removed
//! unsafe but did not tighten the ceiling -> FAIL "stale, run --regen". That
//! second, "strictly better than recorded" direction is what forces a
//! ratchet-tightening commit, so the baseline can never drift behind the tree.
//!
//! Regenerate with `cargo run -p xtask -- unsafe-budget --regen` (alias
//! `--bless`): it rewrites the baseline from the current tree.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::conformance::repo_root;

/// One crate's slice of the budget. Ceiling-only by design: per-site
/// `// SAFETY` coverage is clippy's job now, not a count's (see module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CrateBudget {
    /// Ceiling: real unsafe constructs. May only go DOWN.
    unsafe_count: usize,
}

/// The whole budget: crate name -> its slice.
type Budget = BTreeMap<String, CrateBudget>;

pub(crate) fn run(regen: bool) -> Result<()> {
    run_at(&repo_root()?, regen)
}

/// `run` against an explicit repo root. Split out so the fail-closed missing-
/// baseline path and the corrupt-baseline regen recovery are unit-testable
/// against a throwaway tree; `repo_root()` is fixed at compile time and cannot
/// be pointed anywhere else.
fn run_at(repo: &Path, regen: bool) -> Result<()> {
    let measured = measure_budget(repo)?;
    let path = repo.join("conformance/unsafe-budget.json");

    if regen {
        // Show what re-blessing changes BEFORE overwriting, so a reviewer can
        // see (e.g.) a raised ceiling rather than blessing a new unsafe blind.
        // A missing or unreadable prior baseline is not fatal here -- regen is
        // the recovery path for exactly that, so fall back to "no diff".
        match path.exists().then(|| load_budget(&path)) {
            Some(Ok(old)) => {
                let changes = diff_budgets(&old, &measured);
                if changes.is_empty() {
                    println!("regen: no change vs the committed baseline.");
                } else {
                    println!("regen: changes vs the committed baseline:");
                    for c in &changes {
                        println!("  {c}");
                    }
                }
            }
            Some(Err(e)) => println!("regen: prior baseline unreadable ({e}); overwriting."),
            None => println!("regen: no prior baseline (first run)."),
        }
        write_budget(&path, &measured)?;
        print_summary(&measured);
        println!("\nwrote {} (regenerated from the tree)", rel(repo, &path));
        return Ok(());
    }

    // A MISSING FILE is a deleted committed artifact, not "nothing recorded
    // yet" -- treating it as the latter would silently turn the gate off. Same
    // stance as load_surface_gaps in conformance.rs.
    if !path.exists() {
        bail!(
            "{} is missing. It is a committed artifact and the unsafe-budget gate cannot \
             run without it; restore it from git, or regenerate with \
             `cargo run -p xtask -- unsafe-budget --regen`.",
            path.display()
        );
    }
    let baseline = load_budget(&path)?;
    print_summary(&measured);
    let violations = compute_violations(&measured, &baseline);
    if !violations.is_empty() {
        bail!(
            "unsafe-budget ratchet:\n{}\n\nThe unsafe_count ceiling may only go DOWN. A NEW unsafe \
             needs a reviewed baseline bump (and must satisfy the clippy safety lints at the site); \
             any count strictly BELOW the ceiling must be blessed with \
             `cargo run -p xtask -- unsafe-budget --regen` so the ratchet tightens.",
            violations
                .iter()
                .map(|v| format!("  {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    println!("\nunsafe-budget: within ceilings (ratchet holds).");
    Ok(())
}

fn print_summary(b: &Budget) {
    println!("unsafe budget (real constructs per crate):");
    for (name, c) in b {
        println!("  {name}: {} unsafe", c.unsafe_count);
    }
    println!("  (per-site safety justification is enforced by clippy, not counted here.)");
}

// ------------------------------------------------------------- gating (pure)

/// A ceiling that may only go DOWN. Fails ABOVE it (a regression -- new unsafe)
/// and STRICTLY BELOW it (stale -- the tree improved but the baseline did not
/// follow, so the ratchet must be re-blessed). Pure; unit-tested both ways.
fn ceiling_check(label: &str, measured: usize, recorded: usize) -> Option<String> {
    if measured > recorded {
        Some(format!(
            "{label}: {measured} > ceiling {recorded} -- new unsafe added. Justify each at \
             its SITE: a `// SAFETY:` comment on an unsafe block or `unsafe impl`, a \
             `/// # Safety` doc section on a public unsafe fn, and nothing at all on an \
             `unsafe extern` foreign module -- clippy rejects a `// SAFETY:` comment on \
             those last two. Then, with review, raise the ceiling."
        ))
    } else if measured < recorded {
        Some(format!(
            "{label}: {measured} < recorded {recorded} -- stale ceiling (unsafe was removed but \
             the baseline was not tightened). Run --regen to ratchet it down."
        ))
    } else {
        None
    }
}

/// Diff the measured tree against the committed baseline. Returns every
/// violation (empty = ratchet holds). Pure by design -- no IO -- so the gating
/// decisions are unit-testable the way `ratchet_violation` /
/// `compare_exports` are. One metric per crate, plus the two
/// baseline/tree-asymmetry diagnostics, so this is a straight symmetric diff.
fn compute_violations(measured: &Budget, baseline: &Budget) -> Vec<String> {
    let mut out = Vec::new();

    for (name, base) in baseline {
        let Some(m) = measured.get(name) else {
            out.push(format!(
                "crate `{name}` is in the baseline but no such crate is in the tree -- run --regen."
            ));
            continue;
        };

        if let Some(msg) = ceiling_check(
            &format!("crate `{name}` unsafe_count"),
            m.unsafe_count,
            base.unsafe_count,
        ) {
            out.push(msg);
        }
    }

    for name in measured.keys() {
        if !baseline.contains_key(name) {
            out.push(format!(
                "crate `{name}` is in the tree but not in the baseline -- run --regen."
            ));
        }
    }

    out
}

/// Human-readable "what changed" between a prior baseline and the freshly
/// measured tree, for `--regen` to print before it overwrites. Neutral (not
/// gate-framed): every per-crate count delta, plus added/removed crates. Empty
/// = the baseline already matches the tree. Pure.
fn diff_budgets(old: &Budget, new: &Budget) -> Vec<String> {
    let mut out = Vec::new();
    for (name, n) in new {
        let Some(o) = old.get(name) else {
            out.push(format!("+ {name} (new crate): unsafe {}", n.unsafe_count));
            continue;
        };
        if o.unsafe_count != n.unsafe_count {
            out.push(format!(
                "{name} unsafe_count {} -> {}",
                o.unsafe_count, n.unsafe_count
            ));
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            out.push(format!("- {name} (crate removed)"));
        }
    }
    out
}

// ---------------------------------------------------------------- measuring

fn measure_budget(repo: &Path) -> Result<Budget> {
    let crates_dir = repo.join("crates");
    let mut budget = Budget::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&crates_dir)
        .with_context(|| format!("reading {}", crates_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // `xtask` is a workspace member that does NOT live under `crates/`, and it
    // is the gate's own tooling: no `[lints]` section, no `forbid(unsafe_code)`,
    // so unsafe landing there is caught by nothing else. Its `name` derives from
    // `file_name()` -> "xtask" and it flows through the same loop; not being
    // under `crates/` there is no double-count. A missing `xtask/src` is skipped
    // by the `src.is_dir()` guard below rather than recorded as an empty crate.
    entries.push(repo.join("xtask"));
    entries.sort();

    for crate_dir in entries {
        let name = crate_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let src = crate_dir.join("src");
        if !src.is_dir() {
            continue;
        }
        let mut unsafe_count = 0usize;
        for file in crate_files(&crate_dir, &src)? {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            unsafe_count += scan_source(&text);
        }
        // Keyed by DIRECTORY NAME, so two members sharing a final path segment
        // (a `crates/xtask` alongside the real `xtask`) would collide and the
        // second insert would drop the first's unsafe from the ceiling. Hiding
        // unsafe is the one thing this gate exists to prevent: collide loudly.
        if let Some(prev) = budget.insert(name.clone(), CrateBudget { unsafe_count }) {
            bail!(
                "two workspace members map to the crate key `{name}` (one counted {} \
                 unsafe, the other {unsafe_count}). The budget is keyed by directory \
                 name, so one silently overwrites the other and drops its unsafe from \
                 the ceiling. Rename one.",
                prev.unsafe_count
            );
        }
    }
    Ok(budget)
}

/// Every `.rs` file that counts toward one crate's budget, sorted for a stable
/// walk: all of `src/` (the base -- the caller has already checked it exists),
/// all of `tests/`, and a top-level `build.rs`. The latter two are included only
/// when present. Both are compiled Rust -- integration tests CI builds and runs,
/// and a build script that runs on the build host -- so unsafe there belongs
/// under the same ceiling as `src/`. `benches/` and `examples/` are deliberately
/// out (no crate in the tree has either; see the module doc).
fn crate_files(crate_dir: &Path, src: &Path) -> Result<Vec<PathBuf>> {
    let mut out = rust_files(src)?;
    let tests = crate_dir.join("tests");
    if tests.is_dir() {
        out.extend(rust_files(&tests)?);
    }
    let build_rs = crate_dir.join("build.rs");
    if build_rs.is_file() {
        out.push(build_rs);
    }
    out.sort();
    Ok(out)
}

/// Every `.rs` file under `dir`, recursively, sorted for a stable walk.
fn rust_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .with_context(|| format!("reading {}", d.display()))?
            .flatten()
        {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

// --------------------------------------------------------- lexical scanner

/// Count the real unsafe constructs in one source file.
fn scan_source(src: &str) -> usize {
    count_unsafe(&strip_comments_and_strings(src))
}

/// Blank every comment, string, char, byte and raw literal to spaces (newlines
/// preserved) so keyword scanning never trips on the word `unsafe` inside a
/// comment or string, or on a `"` / `//` living inside a raw string.
fn strip_comments_and_strings(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;

    while i < n {
        let c = chars[i];

        // Line comment `// ...` (covers `///` and `//!`).
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let mut j = i + 2;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            for _ in i..j {
                out.push(' ');
            }
            i = j;
            continue;
        }

        // Block comment `/* ... */`, Rust-style NESTED.
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let mut depth = 1usize;
            out.push(' ');
            out.push(' ');
            let mut j = i + 2;
            while j < n && depth > 0 {
                if chars[j] == '/' && j + 1 < n && chars[j + 1] == '*' {
                    depth += 1;
                    out.push(' ');
                    out.push(' ');
                    j += 2;
                } else if chars[j] == '*' && j + 1 < n && chars[j + 1] == '/' {
                    depth -= 1;
                    out.push(' ');
                    out.push(' ');
                    j += 2;
                } else {
                    out.push(if chars[j] == '\n' { '\n' } else { ' ' });
                    j += 1;
                }
            }
            i = j;
            continue;
        }

        // String / char / byte / raw literal.
        let prev_is_word = i > 0 && is_word_char(chars[i - 1]);
        if let Some(end) = literal_end(&chars, i, prev_is_word) {
            for &ch in &chars[i..end] {
                out.push(if ch == '\n' { '\n' } else { ' ' });
            }
            i = end;
            continue;
        }

        out.push(c);
        i += 1;
    }

    out
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// If a literal starts at `i`, return the index just past its end. Handles
/// plain / raw / byte / raw-byte strings and char (byte) literals; returns
/// `None` for a lifetime/label (`'a`, `'static`) or a raw identifier (`r#foo`).
fn literal_end(chars: &[char], i: usize, prev_is_word: bool) -> Option<usize> {
    let n = chars.len();
    let c = chars[i];

    // r/b/br prefixes are literal starts only at a token boundary.
    if !prev_is_word {
        if c == 'b' {
            match chars.get(i + 1) {
                Some('"') => return plain_string_end(chars, i + 1),
                Some('\'') => return char_literal_end(chars, i + 1),
                Some('r') => return raw_string_end(chars, i + 1),
                _ => {}
            }
        }
        if c == 'r'
            && let Some(end) = raw_string_end(chars, i)
        {
            return Some(end);
        }
    }
    if c == '"' {
        return plain_string_end(chars, i);
    }
    if c == '\'' {
        return char_literal_end(chars, i);
    }
    let _ = n;
    None
}

/// `chars[q] == '"'`. Return the index past the closing quote.
fn plain_string_end(chars: &[char], q: usize) -> Option<usize> {
    let n = chars.len();
    let mut j = q + 1;
    while j < n {
        match chars[j] {
            '\\' => j += 2,
            '"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    Some(n)
}

/// `chars[r] == 'r'`. Return the index past a raw string's closing `"#..#`, or
/// `None` when this is a raw identifier (`r#foo`) rather than a raw string.
fn raw_string_end(chars: &[char], r: usize) -> Option<usize> {
    let n = chars.len();
    let mut j = r + 1;
    let mut hashes = 0usize;
    while j < n && chars[j] == '#' {
        hashes += 1;
        j += 1;
    }
    if j >= n || chars[j] != '"' {
        return None; // r#ident or a bare `r`
    }
    j += 1;
    while j < n {
        if chars[j] == '"' {
            let mut k = j + 1;
            let mut h = 0usize;
            while k < n && h < hashes && chars[k] == '#' {
                h += 1;
                k += 1;
            }
            if h == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    Some(n)
}

/// `chars[q] == '\''`. Return the index past a char literal's closing quote, or
/// `None` when it is a lifetime/label (`'a`) that carries no closing quote.
fn char_literal_end(chars: &[char], q: usize) -> Option<usize> {
    let n = chars.len();
    if q + 1 >= n {
        return None;
    }
    if chars[q + 1] == '\\' {
        // Escaped char literal: scan to the closing quote on the same line.
        // Start AT the backslash (q+1), not past it, so the `'\\' => j += 2`
        // arm consumes the whole escape pair uniformly. Starting at q+2 mis-scans
        // `'\\'`: the escaped backslash at q+2 gets read as a fresh escape, jumps
        // the real closing quote at q+3, and the scan runs away to a later stray
        // quote -- blanking (and undercounting) any `unsafe` in between.
        let mut j = q + 1;
        while j < n {
            match chars[j] {
                '\\' => j += 2,
                '\'' => return Some(j + 1),
                '\n' => return None,
                _ => j += 1,
            }
        }
        return None;
    }
    // `'x'`: exactly one char then the closing quote.
    if chars.get(q + 2) == Some(&'\'') {
        return Some(q + 3);
    }
    None // lifetime / label
}

/// Count real unsafe constructs in already-stripped code.
fn count_unsafe(code: &str) -> usize {
    tally(code).total()
}

/// Per-category tally, kept separate from the total so the categories can be
/// asserted individually in tests.
#[derive(Default, Debug, PartialEq, Eq)]
struct Tally {
    unsafe_fn: usize,
    unsafe_impl: usize,
    unsafe_extern_block: usize,
    unsafe_extern_fn: usize,
    unsafe_block: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.unsafe_fn
            + self.unsafe_impl
            + self.unsafe_extern_block
            + self.unsafe_extern_fn
            + self.unsafe_block
    }
}

fn tally(code: &str) -> Tally {
    let b = code.as_bytes();
    let n = b.len();
    let mut t = Tally::default();
    let mut i = 0usize;
    while i + 6 <= n {
        if &b[i..i + 6] == b"unsafe" {
            let prev_ok = i == 0 || !is_word_byte(b[i - 1]);
            let next_ok = i + 6 == n || !is_word_byte(b[i + 6]);
            if prev_ok && next_ok {
                classify(b, i + 6, &mut t);
                i += 6;
                continue;
            }
        }
        i += 1;
    }
    t
}

fn is_word_byte(x: u8) -> bool {
    x.is_ascii_alphanumeric() || x == b'_'
}

fn skip_ws(b: &[u8], mut j: usize) -> usize {
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t' || b[j] == b'\n' || b[j] == b'\r') {
        j += 1;
    }
    j
}

/// `b[j..]` starts with keyword `kw` at a word boundary.
fn word_at(b: &[u8], j: usize, kw: &[u8]) -> bool {
    j + kw.len() <= b.len()
        && &b[j..j + kw.len()] == kw
        && (j + kw.len() == b.len() || !is_word_byte(b[j + kw.len()]))
}

/// Classify what follows an `unsafe` keyword at index `j` and, if it is a real
/// construct, bump the matching category. Skips `#[unsafe(...)]` attributes and
/// `unsafe [extern] fn(...)` function-POINTER TYPES (no body).
fn classify(b: &[u8], j: usize, t: &mut Tally) {
    let j = skip_ws(b, j);
    if j >= b.len() {
        return;
    }
    match b[j] {
        b'{' => t.unsafe_block += 1,
        b'(' => {} // `#[unsafe(no_mangle)]` and friends: an attribute, not code.
        _ => {
            if word_at(b, j, b"fn") {
                if !is_fn_pointer(b, j + 2) {
                    t.unsafe_fn += 1;
                }
            } else if word_at(b, j, b"impl") {
                t.unsafe_impl += 1;
            } else if word_at(b, j, b"trait") {
                // Not present in the tree today, but a real unsafe construct if
                // it appears -- count it rather than silently miss a regression.
                t.unsafe_impl += 1;
            } else if word_at(b, j, b"extern") {
                classify_extern(b, j + 6, t);
            }
        }
    }
}

/// After `unsafe extern`: an ABI string (already blanked to spaces) then either
/// an extern BLOCK (`{`) or an extern FN. A named `fn foo(` is a definition
/// (counts); a bare `fn(` is a function-pointer type (does not).
fn classify_extern(b: &[u8], j: usize, t: &mut Tally) {
    let j = skip_ws(b, j);
    if j >= b.len() {
        return;
    }
    if b[j] == b'{' {
        t.unsafe_extern_block += 1;
    } else if word_at(b, j, b"fn") && !is_fn_pointer(b, j + 2) {
        t.unsafe_extern_fn += 1;
    }
}

/// `b[j..]` (just past `fn`) begins a function-pointer type: after optional
/// whitespace the next token is `(`, meaning there is no function NAME and so
/// no body -- a type, not a definition.
fn is_fn_pointer(b: &[u8], j: usize) -> bool {
    let k = skip_ws(b, j);
    k < b.len() && b[k] == b'('
}

// ------------------------------------------------------------ (de)serialize

const COMMENT: &str = "Real `unsafe` constructs per crate (unsafe_count), a CEILING that may only \
go DOWN, measured by `cargo run -p xtask -- unsafe-budget`. This is the AI-POLICY.md gate-5 \
ratchet; it GATES: a crate ABOVE its ceiling fails the run -- and so does a count strictly BELOW \
it (regenerate with --regen so the ratchet tightens). Per-SITE `// SAFETY:` coverage is NOT \
counted here: it is enforced on EVERY crate by clippy undocumented_unsafe_blocks = deny, \
unnecessary_safety_comment = deny and missing_safety_doc = deny. The old documented_count FLOOR \
was retired because it and unnecessary_safety_comment were in direct conflict -- the floor counted \
~155 comments that the lint forbids. Scanned per workspace member: every .rs file under src/ and \
tests/, plus a top-level build.rs. Do not edit by hand.";

fn budget_to_json(b: &Budget) -> Value {
    let mut crates = serde_json::Map::new();
    for (name, c) in b {
        let mut obj = serde_json::Map::new();
        obj.insert("unsafe_count".into(), json!(c.unsafe_count));
        crates.insert(name.clone(), Value::Object(obj));
    }
    json!({ "_comment": COMMENT, "crates": Value::Object(crates) })
}

fn budget_from_json(v: &Value) -> Result<Budget> {
    let crates = v
        .get("crates")
        .and_then(|c| c.as_object())
        .context("unsafe-budget.json has no `crates` object")?;
    let mut out = Budget::new();
    for (name, entry) in crates {
        let unsafe_count = entry
            .get("unsafe_count")
            .and_then(Value::as_u64)
            .with_context(|| format!("crate `{name}` has no numeric unsafe_count"))?
            as usize;
        out.insert(name.clone(), CrateBudget { unsafe_count });
    }
    Ok(out)
}

fn load_budget(path: &Path) -> Result<Budget> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    budget_from_json(&v)
}

fn write_budget(path: &Path, b: &Budget) -> Result<()> {
    let json = serde_json::to_string_pretty(&budget_to_json(b))?;
    std::fs::write(path, format!("{json}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn rel(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- lexical scanner: what counts and what deliberately does not ----

    #[test]
    fn counts_the_four_real_constructs() {
        let src = r#"
            unsafe fn a() {}
            unsafe impl Send for T {}
            unsafe extern "C" { fn c(); }
            fn d() { unsafe { g(); } }
        "#;
        assert_eq!(scan_source(src), 4);
    }

    #[test]
    fn counts_named_extern_fn_definition_but_not_a_fn_pointer_type() {
        // The definition has a body and is real unsafe; the pointer type is a
        // declaration only. The old grep counted both.
        let src = r#"
            pub unsafe extern "C" fn napi_get_undefined(env: Env) -> i32 { 0 }
            struct S { cb: unsafe extern "C" fn(Env, *mut u8) -> i32 }
        "#;
        let t = tally(&strip_comments_and_strings(src));
        assert_eq!(t.unsafe_extern_fn, 1, "one named definition");
        assert_eq!(t.total(), 1, "the fn-pointer type must not count");
    }

    #[test]
    fn does_not_count_unsafe_attributes() {
        let src = "#[unsafe(no_mangle)]\npub extern \"C\" fn f() {}\n";
        assert_eq!(
            scan_source(src),
            0,
            "#[unsafe(...)] is an attribute, not a construct"
        );
    }

    #[test]
    fn does_not_count_unsafe_in_comments_or_strings() {
        let src = r##"
            // this unsafe word is in a line comment
            /* and this unsafe one is in a block comment */
            let s = "an unsafe string literal";
            let r = r#"a raw "unsafe" string with quotes"#;
        "##;
        assert_eq!(scan_source(src), 0);
    }

    #[test]
    fn char_literal_with_quote_does_not_start_a_string() {
        // If `'"'` were mishandled the trailing `unsafe {` would be swallowed
        // into a phantom string and miscounted as 0.
        let src = "let q = '\"'; fn f() { unsafe { g(); } }";
        assert_eq!(scan_source(src), 1);
    }

    #[test]
    fn lifetimes_are_not_char_literals() {
        let src = "fn f<'a>(x: &'a str) -> &'static str { unsafe { h() } }";
        assert_eq!(scan_source(src), 1);
    }

    #[test]
    fn backslash_char_literal_does_not_run_away() {
        // `'\\'` is an escaped backslash. If char_literal_end scans from past the
        // backslash it jumps the real closing quote and runs to the next stray
        // quote (in the string), blanking the `unsafe` -- undercounting the
        // ceiling this gate protects. Both plain and byte forms.
        for lit in ["'\\\\'", "b'\\\\'"] {
            let src = format!("let c = {lit}; let x = \"'\"; unsafe {{ f(); }}");
            assert_eq!(
                scan_source(&src),
                1,
                "backslash char literal {lit} miscounted"
            );
        }
    }

    #[test]
    fn escaped_quote_char_literal_is_counted() {
        // `'\''` (escaped single-quote) must not desync the quote scan.
        let src = "let q = '\\''; unsafe { g(); }";
        assert_eq!(scan_source(src), 1);
    }

    // ---- lexical scanner: boundary / literal edge cases ----

    #[test]
    fn unsafe_as_a_substring_of_an_identifier_is_not_counted() {
        // The whole ceiling is per-crate `unsafe` counts; a broken word boundary
        // would over-count. `wrap_unsafe { .. }` is a struct literal whose name
        // ENDS in `unsafe` and is followed by `{` -- the case a dropped prev-word
        // guard would miscount as an unsafe block. Plus leading/interior forms.
        let src = "let unsafe_x = is_unsafe(); let a = wrap_unsafe { x: 1 }; unsafe { real(); }";
        assert_eq!(
            scan_source(src),
            1,
            "only the real `unsafe {{` block counts"
        );
    }

    #[test]
    fn byte_and_raw_byte_strings_are_blanked() {
        // FFI code carries C byte-strings; `unsafe` inside one must not count.
        let src = r####"
            let a = b"an unsafe byte string";
            let c = br"a raw unsafe byte string";
            unsafe { real(); }
        "####;
        assert_eq!(scan_source(src), 1);
    }

    #[test]
    fn nested_block_comment_hides_unsafe() {
        // Rust block comments nest; only correct depth tracking keeps the
        // `unsafe { hidden }` -- which sits AFTER the inner `*/` but still inside
        // the outer comment -- blanked. A naive first-`*/` scan would end the
        // comment at the inner close and count it, giving 2 instead of 1.
        let src = "/* a /* b */ unsafe { hidden(); } */ unsafe { real(); }";
        assert_eq!(scan_source(src), 1);
    }

    #[test]
    fn line_comment_inside_a_block_comment_does_not_end_it() {
        // A `//` inside a block comment is inert text: it must neither terminate
        // the block comment early nor swallow its closing `*/`, or the real
        // trailing `unsafe` after it would be blanked and lost from the ceiling.
        let src =
            "/* // hidden unsafe { g(); } */\n// also hidden: unsafe { g(); }\nunsafe { g(); }";
        assert_eq!(scan_source(src), 1, "only the bare trailing block counts");
    }

    #[test]
    fn multi_hash_raw_string_interior_quote_hash_does_not_close_early() {
        // `r##"..."##` may contain `"#` (one hash) without closing; a wrong hash
        // count would end the string early and misread the trailing code.
        let src = r####"
            let a = r##"contains "# and the word unsafe inside"##;
            unsafe { real(); }
        "####;
        assert_eq!(
            scan_source(src),
            1,
            "interior \"# must not close the raw string"
        );
    }

    // ---- pure gate: the ceiling, both directions ----

    // (crate name, unsafe_count) -- the entire model, now that the gate is
    // ceiling-only. Per-site `// SAFETY` coverage lives in clippy, not here.
    type Row<'a> = (&'a str, usize);

    fn budget(entries: &[Row]) -> Budget {
        entries
            .iter()
            .map(|(name, u)| ((*name).to_string(), CrateBudget { unsafe_count: *u }))
            .collect()
    }

    #[test]
    fn exact_match_is_clean() {
        let b = budget(&[("oam_engine", 700), ("oam_core", 90)]);
        assert!(compute_violations(&b, &b).is_empty());
    }

    #[test]
    fn unsafe_above_ceiling_fails() {
        let measured = budget(&[("oam_engine", 701)]);
        let baseline = budget(&[("oam_engine", 700)]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("701 > ceiling 700"), "{}", v[0]);
    }

    #[test]
    fn unsafe_below_ceiling_is_stale() {
        let measured = budget(&[("oam_engine", 699)]);
        let baseline = budget(&[("oam_engine", 700)]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("stale ceiling"), "{}", v[0]);
        assert!(v[0].contains("699 < recorded 700"), "{}", v[0]);
    }

    #[test]
    fn a_new_crate_must_be_blessed() {
        let measured = budget(&[("oam_engine", 700), ("oam_new", 2)]);
        let baseline = budget(&[("oam_engine", 700)]);
        let v = compute_violations(&measured, &baseline);
        assert!(
            v.iter()
                .any(|m| m.contains("oam_new") && m.contains("not in the baseline")),
            "{v:?}"
        );
    }

    #[test]
    fn crate_in_baseline_but_absent_from_tree_is_flagged() {
        // A deleted/renamed crate must force a --regen, not silently drop its
        // ceiling from the gate.
        let measured = budget(&[("oam_engine", 700)]);
        let baseline = budget(&[("oam_engine", 700), ("oam_gone", 3)]);
        let v = compute_violations(&measured, &baseline);
        assert!(
            v.iter()
                .any(|m| m.contains("oam_gone") && m.contains("no such crate is in the tree")),
            "{v:?}"
        );
    }

    // ---- regen diff (what --regen prints before overwriting) ----

    #[test]
    fn diff_reports_count_and_crate_changes() {
        let old = budget(&[("oam_engine", 700), ("oam_gone", 3)]);
        let new = budget(&[
            // ceiling ratcheted down.
            ("oam_engine", 699),
            // brand-new crate.
            ("oam_new", 2),
            // oam_gone dropped.
        ]);
        let d = diff_budgets(&old, &new);
        assert!(
            d.iter().any(|m| m == "oam_engine unsafe_count 700 -> 699"),
            "{d:?}"
        );
        assert!(
            d.iter().any(|m| m == "+ oam_new (new crate): unsafe 2"),
            "{d:?}"
        );
        assert!(d.iter().any(|m| m == "- oam_gone (crate removed)"), "{d:?}");
        assert_eq!(d.len(), 3, "and nothing else: {d:?}");
    }

    #[test]
    fn diff_is_empty_when_baseline_matches_tree() {
        let b = budget(&[("oam_engine", 700), ("oam_core", 90)]);
        assert!(diff_budgets(&b, &b).is_empty());
    }

    // ---- JSON round-trip ----

    #[test]
    fn json_round_trips() {
        let b = budget(&[("oam_core", 97), ("oam_engine", 700), ("oam_loader", 0)]);
        let back = budget_from_json(&budget_to_json(&b)).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn retired_keys_are_neither_written_nor_read() {
        // documented_count / files are gone from the model. A freshly written
        // baseline must not carry them, and a STALE baseline that still does
        // must load fine on its unsafe_count alone rather than erroring -- the
        // gate has to keep running while the committed artifact catches up.
        let v = budget_to_json(&budget(&[("oam_engine", 700)]));
        let entry = &v["crates"]["oam_engine"];
        assert!(entry.get("unsafe_count").is_some(), "{entry}");
        assert!(entry.get("documented_count").is_none(), "{entry}");
        assert!(entry.get("files").is_none(), "{entry}");

        let stale = budget_from_json(&json!({
            "crates": {
                "oam_engine": {
                    "unsafe_count": 700,
                    "documented_count": 690,
                    "files": { "napi.rs": 600 }
                }
            }
        }))
        .unwrap();
        assert_eq!(stale, budget(&[("oam_engine", 700)]));
    }

    // ---- deserialize: a corrupt committed baseline must fail loudly ----

    #[test]
    fn baseline_without_crates_object_errors() {
        let err = budget_from_json(&json!({ "_comment": "x" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("crates"), "{err}");
    }

    #[test]
    fn baseline_crate_without_unsafe_count_errors() {
        let err = budget_from_json(&json!({ "crates": { "oam_x": {} } }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no numeric unsafe_count"), "{err}");
    }

    #[test]
    fn baseline_crate_with_non_numeric_unsafe_count_errors() {
        // The one metric left is load-bearing: a corrupt value must fail the
        // run, never silently read as 0 (which would pass every crate).
        let err = budget_from_json(&json!({ "crates": { "oam_x": { "unsafe_count": "700" } } }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no numeric unsafe_count"), "{err}");
    }

    // ---- file walk: which surfaces the ceiling can actually see ----

    /// A throwaway repo tree under the OS temp dir, removed on Drop so a failing
    /// assertion cannot leak it. std-only: xtask has no `tempfile` dependency.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            // pid + a process-local counter: the test harness runs these in
            // parallel threads, so a clock-based name could collide.
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "oam-unsafe-budget-{tag}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Write `text` at repo-relative `rel` (forward slashes), creating any
        /// missing parent directories.
        fn file(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sum `scan_source` over everything `crate_files` walks for ONE crate --
    /// the same arithmetic `measure_budget` does, minus the `crates/` layout, so
    /// the walk's contribution to the ceiling can be asserted on its own.
    fn scan_crate(crate_dir: &Path) -> usize {
        crate_files(crate_dir, &crate_dir.join("src"))
            .unwrap()
            .iter()
            .map(|p| scan_source(&std::fs::read_to_string(p).unwrap()))
            .sum()
    }

    #[test]
    fn crate_files_walks_src_tests_and_build_rs_recursively() {
        // The walk's own contract: `src/` and `tests/` recursively plus a
        // top-level `build.rs`, .rs only, sorted. `benches/` is the decoy.
        let t = TempRepo::new("files");
        t.file("oam_x/src/lib.rs", "");
        t.file("oam_x/src/sub/deep.rs", "");
        t.file("oam_x/src/notes.txt", "");
        t.file("oam_x/tests/it.rs", "");
        t.file("oam_x/tests/sub/nested.rs", "");
        t.file("oam_x/build.rs", "");
        t.file("oam_x/benches/b.rs", "");
        let dir = t.path().join("oam_x");
        let found: Vec<String> = crate_files(&dir, &dir.join("src"))
            .unwrap()
            .iter()
            .map(|p| rel(&dir, p))
            .collect();
        assert_eq!(
            found,
            [
                "build.rs",
                "src/lib.rs",
                "src/sub/deep.rs",
                "tests/it.rs",
                "tests/sub/nested.rs",
            ]
        );
    }

    #[test]
    fn crate_files_counts_unsafe_in_a_tests_file() {
        // `tests/` is compiled Rust that CI builds and runs. Before the walk
        // widened, an `unsafe` block there was structurally invisible to the
        // ceiling. The `src/` file is deliberately clean, so the whole count
        // has to come from `tests/`.
        let t = TempRepo::new("walk-tests");
        t.file("oam_x/src/lib.rs", "pub fn f() {}\n");
        t.file("oam_x/tests/it.rs", "#[test]\nfn t() { unsafe { g(); } }\n");
        t.file("oam_x/tests/sub/nested.rs", "fn n() { unsafe { g(); } }\n");
        assert_eq!(
            scan_crate(&t.path().join("oam_x")),
            2,
            "tests/ is inside the ceiling, recursively"
        );
    }

    #[test]
    fn crate_files_counts_unsafe_in_a_top_level_build_rs() {
        // A build script is compiled and RUN on the build host; unsafe there is
        // real. Only the top-level `build.rs` is picked up -- it is a file, not
        // a walked directory -- so the clean `src/` leaves the count to it.
        let t = TempRepo::new("walk-build");
        t.file("oam_x/src/lib.rs", "pub fn f() {}\n");
        t.file("oam_x/build.rs", "fn main() { unsafe { g(); } }\n");
        assert_eq!(
            scan_crate(&t.path().join("oam_x")),
            1,
            "build.rs is inside the ceiling"
        );
    }

    #[test]
    fn crate_files_walks_src_subdirectories_recursively() {
        // Widening the walk to tests/ + build.rs must not have cost the original
        // property: `src/` is walked to arbitrary depth, not just its root. Two
        // levels down, with a non-.rs decoy alongside it.
        let t = TempRepo::new("walk-deep");
        t.file("oam_x/src/lib.rs", "pub fn f() { unsafe { g(); } }\n");
        t.file("oam_x/src/a/b/deep.rs", "fn d() { unsafe { g(); } }\n");
        t.file("oam_x/src/a/b/notes.txt", "unsafe { not_rust(); }\n");
        assert_eq!(
            scan_crate(&t.path().join("oam_x")),
            2,
            "src/ is walked to arbitrary depth, .rs only"
        );
    }

    #[test]
    fn tests_dir_and_build_rs_count_toward_the_crate_ceiling() {
        // The same widening at the `measure_budget` level: all three surfaces
        // must land in ONE per-crate total, not just in `crate_files`' list.
        let t = TempRepo::new("walk");
        t.file(
            "crates/oam_engine/src/engine.rs",
            "pub fn a() { unsafe { g(); } }\n",
        );
        t.file(
            "crates/oam_engine/tests/it.rs",
            "#[test]\nfn t() { unsafe { g(); } }\n",
        );
        t.file(
            "crates/oam_engine/build.rs",
            "fn main() { unsafe { g(); } }\n",
        );
        let b = measure_budget(t.path()).unwrap();
        assert_eq!(
            b["oam_engine"].unsafe_count, 3,
            "src + tests/ + build.rs all counted"
        );
    }

    #[test]
    fn xtask_is_scanned_like_any_other_workspace_member() {
        // xtask lives outside `crates/` and carries no [lints] section, so
        // without this it is the one workspace member where unsafe is caught by
        // nothing at all.
        let t = TempRepo::new("xtask");
        t.file("crates/oam_x/src/lib.rs", "pub fn f() {}\n");
        t.file("xtask/src/main.rs", "fn main() { unsafe { g(); } }\n");
        t.file("xtask/src/sub/helper.rs", "fn h() { unsafe { g(); } }\n");
        let b = measure_budget(t.path()).unwrap();
        assert_eq!(
            b["xtask"].unsafe_count, 2,
            "walked recursively, like every other member"
        );
        let names: Vec<&str> = b.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["oam_x", "xtask"],
            "xtask is not under crates/, so it must appear exactly once"
        );
    }

    #[test]
    fn two_members_with_the_same_directory_name_fail_loudly() {
        // The budget is keyed by directory name. A `crates/xtask` alongside the
        // real `xtask` would map to one key, and the loser's unsafe would vanish
        // from the ceiling -- silently hiding unsafe, which is the single thing
        // this gate exists to prevent. It must ERROR, not overwrite.
        let t = TempRepo::new("dupkey");
        t.file(
            "crates/xtask/src/lib.rs",
            "pub fn f() { unsafe { g(); } }\n",
        );
        t.file("xtask/src/main.rs", "fn main() { unsafe { g(); } }\n");
        let err = measure_budget(t.path()).unwrap_err().to_string();
        assert!(err.contains("`xtask`"), "{err}");
        assert!(err.contains("two workspace members"), "{err}");
    }

    #[test]
    fn crlf_source_counts_the_same_as_lf() {
        // The committed baseline is ONE set of numbers shared by all four CI
        // legs, and this is a Windows box where autocrlf can hand the scanner
        // `\r\n`. If `\r` ever perturbed the line-comment scan or `skip_ws` the
        // gate would fail on exactly one platform, reported as a "stale
        // ceiling" that points nowhere near the real cause.
        // `fn c` deliberately puts the brace on the NEXT line: that is the one
        // place `\r` must be skipped as whitespace for the block to be seen.
        let lf = "// a line comment x\nunsafe fn a() {}\nfn b() { unsafe { g(); } }\nfn c() {\n    unsafe\n    { g(); }\n}\n";
        let crlf = lf.replace('\n', "\r\n");
        assert_eq!(scan_source(lf), 3);
        assert_eq!(
            scan_source(&crlf),
            scan_source(lf),
            "CRLF must not shift the count"
        );
    }

    #[test]
    fn unsafe_trait_counts_toward_the_ceiling() {
        // `classify` carries a dedicated branch for this precisely because the
        // tree has none today -- an untested future-proofing branch is one
        // refactor away from being a silent hole.
        let t = tally(&strip_comments_and_strings("unsafe trait Send2 {}\n"));
        assert_eq!(t.total(), 1, "an unsafe trait is a real unsafe construct");
    }

    #[test]
    fn a_raw_identifier_is_not_read_as_a_raw_string() {
        // `r#type` must not open a raw string: if it did, the scan would blank
        // the rest of the file to spaces and undercount that crate's ceiling --
        // the same silent-undercount class as the `'\'` runaway above.
        let src = "let r#type = 1; let r#match = 2; unsafe { g(); }";
        assert_eq!(scan_source(src), 1);
    }

    #[test]
    fn every_violation_is_reported_not_just_the_first() {
        // A reviewer acting on a truncated list fixes one problem, re-runs, and
        // hits the next. All of them must surface in one pass.
        let measured = budget(&[("oam_engine", 701), ("oam_new", 2)]);
        let baseline = budget(&[("oam_engine", 700)]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v.iter().any(|m| m.contains("701 > ceiling 700")), "{v:?}");
        assert!(v.iter().any(|m| m.contains("oam_new")), "{v:?}");
    }

    #[test]
    fn budget_round_trips_through_a_real_file() {
        // `json_round_trips` covers the in-memory Value only. The on-disk path
        // adds the _comment, pretty-printing and a trailing newline -- a
        // formatting change there would make every regen diff noisily.
        let t = TempRepo::new("io");
        let path = t.path().join("unsafe-budget.json");
        let b = budget(&[("oam_core", 90), ("oam_engine", 573)]);
        write_budget(&path, &b).unwrap();
        assert_eq!(load_budget(&path).unwrap(), b);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("_comment"), "the baseline explains itself");
        assert!(text.ends_with("}\n"), "exactly one trailing newline");
    }

    #[test]
    fn a_missing_baseline_fails_closed() {
        // The file is a COMMITTED artifact. Treating its absence as "nothing
        // recorded yet" would silently switch the whole gate off, which is the
        // one failure mode a ratchet must never have.
        let t = TempRepo::new("nobaseline");
        t.file(
            "crates/oam_x/src/lib.rs",
            "pub fn f() { unsafe { g(); } }\n",
        );
        let err = run_at(t.path(), false).unwrap_err().to_string();
        assert!(err.contains("is missing"), "{err}");
        assert!(
            err.contains("--regen"),
            "the message names the recovery: {err}"
        );
    }

    #[test]
    fn regen_recovers_from_a_corrupt_baseline() {
        // --regen IS the documented recovery path for an unreadable baseline,
        // so it must not abort on one; if this regressed to a `?` the recovery
        // would be dead exactly when it is needed.
        let t = TempRepo::new("corrupt");
        t.file(
            "crates/oam_x/src/lib.rs",
            "pub fn f() { unsafe { g(); } }\n",
        );
        t.file("conformance/unsafe-budget.json", "{ not valid json");
        run_at(t.path(), true).expect("regen must survive a corrupt baseline");
        let back = load_budget(&t.path().join("conformance/unsafe-budget.json")).unwrap();
        assert_eq!(back["oam_x"].unsafe_count, 1, "rewritten from the tree");
    }

    #[test]
    fn a_member_without_a_src_dir_is_skipped() {
        // Both directions of the `src.is_dir()` continue: a docs-only directory
        // under `crates/` is not a crate, and an absent `xtask/` must not be
        // recorded as an empty crate just because its path is pushed.
        let t = TempRepo::new("nosrc");
        t.file("crates/oam_x/src/lib.rs", "pub fn f() {}\n");
        t.file("crates/oam_docs/README.md", "not a crate\n");
        let b = measure_budget(t.path()).unwrap();
        let names: Vec<&str> = b.keys().map(String::as_str).collect();
        assert_eq!(names, ["oam_x"]);
    }

    #[test]
    fn benches_and_examples_are_not_walked() {
        // Pins the CURRENT scope rather than a wish: no crate in the tree has
        // either directory today. If one appears, this failing test is the
        // reminder to widen `crate_files` instead of silently missing it.
        let t = TempRepo::new("benches");
        t.file("crates/oam_x/src/lib.rs", "pub fn f() {}\n");
        t.file("crates/oam_x/benches/b.rs", "fn b() { unsafe { g(); } }\n");
        t.file(
            "crates/oam_x/examples/e.rs",
            "fn main() { unsafe { g(); } }\n",
        );
        let b = measure_budget(t.path()).unwrap();
        assert_eq!(
            b["oam_x"].unsafe_count, 0,
            "benches/ and examples/ are out of the walk"
        );
    }

    // ---- the retired floor: coverage is clippy's job, not a count's ----

    #[test]
    fn a_crate_with_no_safety_comments_still_passes_the_gate() {
        // The deliberate handoff, pinned so a future reader does not "restore"
        // the floor: per-site justification is enforced by clippy
        // (undocumented_unsafe_blocks / unnecessary_safety_comment /
        // missing_safety_doc, denied on every crate), NOT by counting
        // `// SAFETY` lines here. Stripping every one of them from a crate must
        // therefore change nothing this gate measures, and must not trip it.
        //
        // If this test starts failing, a lexical floor has been re-introduced.
        // Read the module doc before "fixing" it -- the floor and
        // `unnecessary_safety_comment` are in DIRECT conflict: ~155 of the
        // comments the old floor counted are ones the lint forbids.
        let documented = concat!(
            "// SAFETY: the pointer is non-null and uniquely owned here.\n",
            "fn f() { unsafe { g(); } }\n",
            "/// # Safety\n",
            "/// The caller must uphold X.\n",
            "pub unsafe fn h() {}\n",
        );
        let bare = concat!("fn f() { unsafe { g(); } }\n", "pub unsafe fn h() {}\n");

        let with = TempRepo::new("documented");
        with.file("crates/oam_x/src/lib.rs", documented);
        let without = TempRepo::new("bare");
        without.file("crates/oam_x/src/lib.rs", bare);

        let a = measure_budget(with.path()).unwrap();
        let b = measure_budget(without.path()).unwrap();
        assert_eq!(a, b, "SAFETY comments are invisible to the ceiling");
        assert_eq!(a["oam_x"].unsafe_count, 2, "one block + one unsafe fn");

        // ...and the stripped tree still passes against the same baseline.
        assert!(
            compute_violations(&b, &budget(&[("oam_x", 2)])).is_empty(),
            "removing every SAFETY comment must not trip the gate"
        );
    }
}
