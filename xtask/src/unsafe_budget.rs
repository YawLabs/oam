//! `cargo run -p xtask -- unsafe-budget`: the bidirectional-ratchet gate for
//! the `unsafe` surface (AI-POLICY.md gate 5 -- the "`unsafe` budget").
//!
//! It scans `crates/*/src/**/*.rs`, counts the REAL unsafe constructs per
//! crate (`unsafe fn` / `unsafe impl` / `unsafe extern` block / `unsafe extern
//! "C" fn <name>` FFI definition / `unsafe {` block), and the `// SAFETY:`
//! justification comments, then diffs both against the committed baseline in
//! `conformance/unsafe-budget.json`.
//!
//! The counter is deliberately NOT the old `grep -c unsafe` (scripts/ci-local.sh
//! step 9, which it replaces): that grep over-counted (885 matched lines vs 704
//! real constructs on the tree at introduction) by including
//! `#[unsafe(no_mangle)]` attributes, `// SAFETY:` comments, and
//! `unsafe extern "C" fn(...)` used as function-POINTER TYPES (declarations
//! with no body). Those are lexed out here so the number tracks actual unsafe
//! code, not mentions of the word.
//!
//! The gate is BIDIRECTIONAL, like `conformance.rs::compare_exports` and
//! `node_suite.rs::ratchet_violation`:
//!   * `unsafe_count` is a CEILING (may only go DOWN). ABOVE it = new unsafe
//!     added -> FAIL. Strictly BELOW it = you removed unsafe but did not
//!     tighten the ceiling -> FAIL "stale, run --regen".
//!   * `documented_count` (SAFETY comments) is a FLOOR (may only go UP). BELOW
//!     it = SAFETY coverage regressed -> FAIL. Strictly ABOVE it = you
//!     documented more but did not raise the floor -> FAIL "stale, run --regen".
//!
//! Either "strictly better than recorded" direction forces a ratchet-tightening
//! commit, so the baseline can never drift behind the tree.
//!
//! Scope: oam_core / oam_cli / oam_ts are EXCLUDED from the documented-count
//! (SAFETY-coverage) check -- a sibling branch puts them under clippy
//! `undocumented_unsafe_blocks = deny`, which owns their coverage. Their
//! `unsafe_count` CEILING is still tracked here. The documented FLOOR is tracked
//! for oam_engine (with an additional PER-FILE floor on the FFI-boundary files,
//! the ratchet toward 100%) and oam_napi_test_addon. oam_loader / oam_mcp /
//! oam_diagnostics carry no unsafe at all (ceiling pinned at 0).
//!
//! Regenerate with `cargo run -p xtask -- unsafe-budget --regen` (alias
//! `--bless`): it rewrites the baseline from the current tree.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::conformance::repo_root;

/// Crates whose SAFETY-coverage FLOOR is tracked here. The rest (oam_core,
/// oam_cli, oam_ts) get their coverage from clippy `undocumented_unsafe_blocks`
/// on a sibling branch, so only their `unsafe_count` ceiling is tracked.
const DOCUMENTED_SCOPE: &[&str] = &["oam_engine", "oam_napi_test_addon"];

/// oam_engine's FFI-boundary files carrying a PER-FILE documented_count floor
/// -- the "toward 100%" ratchet, file by file. Matched by file name at the
/// crate's `src/` root.
const ENGINE_FILES: &[&str] = &[
    "napi.rs",
    "node_ops.rs",
    "modules.rs",
    "lib.rs",
    "crash.rs",
    "inspector.rs",
    "vm_module.rs",
];

/// One crate's slice of the budget. `documented_count` is `Some` only for
/// crates in `DOCUMENTED_SCOPE`; `files` is non-empty only for oam_engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CrateBudget {
    /// Ceiling: real unsafe constructs. May only go DOWN.
    unsafe_count: usize,
    /// Floor: `// SAFETY:` justification comments. May only go UP. `None` when
    /// the crate is out of the documented-coverage scope.
    documented_count: Option<usize>,
    /// Per-file `documented_count` floors (oam_engine only). file name -> floor.
    files: BTreeMap<String, usize>,
}

/// The whole budget: crate name -> its slice.
type Budget = BTreeMap<String, CrateBudget>;

pub(crate) fn run(regen: bool) -> Result<()> {
    let repo = repo_root()?;
    let measured = measure_budget(&repo)?;
    let path = repo.join("conformance/unsafe-budget.json");

    if regen {
        write_budget(&path, &measured)?;
        print_summary(&measured);
        println!("\nwrote {} (regenerated from the tree)", rel(&repo, &path));
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
            "unsafe-budget ratchet:\n{}\n\nThe unsafe_count ceiling may only go DOWN and the \
             documented_count floor may only go UP. A NEW unsafe needs a `// SAFETY:` and a \
             reviewed baseline bump; any count that is strictly BETTER than recorded must be \
             blessed with `cargo run -p xtask -- unsafe-budget --regen` so the ratchet tightens.",
            violations
                .iter()
                .map(|v| format!("  {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    println!("\nunsafe-budget: within ceilings and at/above floors (ratchet holds).");
    Ok(())
}

fn print_summary(b: &Budget) {
    println!("unsafe budget (real constructs, // SAFETY: coverage):");
    for (name, c) in b {
        match c.documented_count {
            Some(d) => println!("  {name}: {} unsafe, {d} documented", c.unsafe_count),
            None => println!(
                "  {name}: {} unsafe (coverage via clippy, not tracked here)",
                c.unsafe_count
            ),
        }
        for (file, floor) in &c.files {
            println!("      {file}: {floor} documented");
        }
    }
}

// ------------------------------------------------------------- gating (pure)

/// A ceiling that may only go DOWN. Fails ABOVE it (a regression -- new unsafe)
/// and STRICTLY BELOW it (stale -- the tree improved but the baseline did not
/// follow, so the ratchet must be re-blessed). Pure; unit-tested both ways.
fn ceiling_check(label: &str, measured: usize, recorded: usize) -> Option<String> {
    if measured > recorded {
        Some(format!(
            "{label}: {measured} > ceiling {recorded} -- new unsafe added. Justify each with a \
             `// SAFETY:` and (with review) raise the ceiling."
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

/// A floor that may only go UP. Fails BELOW it (coverage regressed) and
/// STRICTLY ABOVE it (stale -- more was documented than the floor records, so
/// re-bless to lock the gain in). Pure; unit-tested both ways.
fn floor_check(label: &str, measured: usize, recorded: usize) -> Option<String> {
    if measured < recorded {
        Some(format!(
            "{label}: {measured} < floor {recorded} -- SAFETY coverage regressed. Restore the \
             `// SAFETY:` justification(s)."
        ))
    } else if measured > recorded {
        Some(format!(
            "{label}: {measured} > floor {recorded} -- stale floor (more is documented than the \
             baseline records). Run --regen to ratchet it up."
        ))
    } else {
        None
    }
}

/// Diff the measured tree against the committed baseline. Returns every
/// violation (empty = ratchet holds). Pure by design -- no IO -- so the gating
/// decisions are unit-testable the way `ratchet_violation` /
/// `compare_exports` are. Both budgets are already scope-shaped by
/// `measure_budget` / `load_budget`, so this is a straight symmetric diff.
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

        match (m.documented_count, base.documented_count) {
            (Some(mc), Some(bc)) => {
                if let Some(msg) = floor_check(&format!("crate `{name}` documented_count"), mc, bc)
                {
                    out.push(msg);
                }
            }
            (None, Some(_)) => out.push(format!(
                "crate `{name}` is recorded as documented-scoped but the tree no longer tracks \
                 it -- run --regen."
            )),
            (Some(_), None) => out.push(format!(
                "crate `{name}` is now documented-scoped but the baseline does not record a \
                 documented_count -- run --regen."
            )),
            (None, None) => {}
        }

        for (file, bc) in &base.files {
            match m.files.get(file) {
                Some(mc) => {
                    if let Some(msg) =
                        floor_check(&format!("`{name}`/{file} documented_count"), *mc, *bc)
                    {
                        out.push(msg);
                    }
                }
                None => out.push(format!(
                    "`{name}`/{file} has a per-file floor in the baseline but was not found in \
                     the tree -- run --regen."
                )),
            }
        }
        for file in m.files.keys() {
            if !base.files.contains_key(file) {
                out.push(format!(
                    "`{name}`/{file} is tracked in the tree but has no per-file floor in the \
                     baseline -- run --regen."
                ));
            }
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
        let mut documented_count = 0usize;
        let mut files = BTreeMap::new();
        for file in rust_files(&src)? {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let (u, d) = scan_source(&text);
            unsafe_count += u;
            documented_count += d;
            // Per-file floor: only oam_engine's named FFI files, and only when
            // they sit at the crate's src root (not a same-named file nested in
            // a subdir).
            if name == "oam_engine"
                && file.parent() == Some(src.as_path())
                && let Some(fname) = file.file_name().and_then(|n| n.to_str())
                && ENGINE_FILES.contains(&fname)
            {
                files.insert(fname.to_string(), d);
            }
        }
        budget.insert(
            name.clone(),
            CrateBudget {
                unsafe_count,
                documented_count: DOCUMENTED_SCOPE
                    .contains(&name.as_str())
                    .then_some(documented_count),
                files,
            },
        );
    }
    Ok(budget)
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

/// Count `(real unsafe constructs, // SAFETY: comments)` in one source file.
fn scan_source(src: &str) -> (usize, usize) {
    let (code, safety) = strip_comments_and_strings(src);
    (count_unsafe(&code), safety)
}

/// Blank every comment, string, char, byte and raw literal to spaces (newlines
/// preserved) so keyword scanning never trips on the word `unsafe` inside a
/// comment or string, or on a `"` / `//` living inside a raw string. Returns
/// the blanked code plus the count of `// SAFETY:` line comments.
fn strip_comments_and_strings(src: &str) -> (String, usize) {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut safety = 0usize;
    let mut i = 0usize;

    while i < n {
        let c = chars[i];

        // Line comment `// ...` (covers `///` and `//!`).
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let mut j = i + 2;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            let text: String = chars[i + 2..j].iter().collect();
            if text.trim_start().starts_with("SAFETY:") {
                safety += 1;
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

    (out, safety)
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

const COMMENT: &str = "Real `unsafe` constructs per crate (unsafe_count, a CEILING that may only go \
DOWN) and `// SAFETY:` justification comments (documented_count, a FLOOR that may only go UP), \
measured by `cargo run -p xtask -- unsafe-budget`. This is the AI-POLICY.md gate-5 ratchet; it \
GATES: a crate above its unsafe ceiling, or below its documented floor, fails the run -- and so \
does any count strictly BETTER than recorded (regenerate with --regen so the ratchet tightens). \
documented_count is tracked only for oam_engine (with per-file floors on the FFI-boundary files) \
and oam_napi_test_addon; oam_core/oam_cli/oam_ts get coverage from clippy undocumented_unsafe_blocks \
on a sibling branch, so only their unsafe_count ceiling lives here. Do not edit by hand.";

fn budget_to_json(b: &Budget) -> Value {
    let mut crates = serde_json::Map::new();
    for (name, c) in b {
        let mut obj = serde_json::Map::new();
        obj.insert("unsafe_count".into(), json!(c.unsafe_count));
        if let Some(d) = c.documented_count {
            obj.insert("documented_count".into(), json!(d));
        }
        if !c.files.is_empty() {
            let files: serde_json::Map<String, Value> =
                c.files.iter().map(|(f, n)| (f.clone(), json!(n))).collect();
            obj.insert("files".into(), Value::Object(files));
        }
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
        let documented_count = entry
            .get("documented_count")
            .and_then(Value::as_u64)
            .map(|x| x as usize);
        let mut files = BTreeMap::new();
        if let Some(map) = entry.get("files").and_then(Value::as_object) {
            for (f, n) in map {
                if let Some(n) = n.as_u64() {
                    files.insert(f.clone(), n as usize);
                }
            }
        }
        out.insert(
            name.clone(),
            CrateBudget {
                unsafe_count,
                documented_count,
                files,
            },
        );
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
        let (u, _) = scan_source(src);
        assert_eq!(u, 4);
    }

    #[test]
    fn counts_named_extern_fn_definition_but_not_a_fn_pointer_type() {
        // The definition has a body and is real unsafe; the pointer type is a
        // declaration only. The old grep counted both.
        let src = r#"
            pub unsafe extern "C" fn napi_get_undefined(env: Env) -> i32 { 0 }
            struct S { cb: unsafe extern "C" fn(Env, *mut u8) -> i32 }
        "#;
        let t = tally(&strip_comments_and_strings(src).0);
        assert_eq!(t.unsafe_extern_fn, 1, "one named definition");
        assert_eq!(t.total(), 1, "the fn-pointer type must not count");
    }

    #[test]
    fn does_not_count_unsafe_attributes() {
        let src = "#[unsafe(no_mangle)]\npub extern \"C\" fn f() {}\n";
        let (u, _) = scan_source(src);
        assert_eq!(u, 0, "#[unsafe(...)] is an attribute, not a construct");
    }

    #[test]
    fn does_not_count_unsafe_in_comments_or_strings() {
        let src = r##"
            // this unsafe word is in a line comment
            /* and this unsafe one is in a block comment */
            let s = "an unsafe string literal";
            let r = r#"a raw "unsafe" string with quotes"#;
        "##;
        let (u, _) = scan_source(src);
        assert_eq!(u, 0);
    }

    #[test]
    fn counts_safety_comments_and_only_those() {
        let src = [
            "// SAFETY: this one counts",
            "//SAFETY: no space also counts",
            "//   SAFETY: indented counts",
            "// not a safety comment",
            r#"let x = "// SAFETY: inside a string does not";"#,
        ]
        .join("\n");
        let (_, d) = scan_source(&src);
        assert_eq!(d, 3);
    }

    #[test]
    fn char_literal_with_quote_does_not_start_a_string() {
        // If `'"'` were mishandled the trailing `unsafe {` would be swallowed
        // into a phantom string and miscounted as 0.
        let src = "let q = '\"'; fn f() { unsafe { g(); } }";
        let (u, _) = scan_source(src);
        assert_eq!(u, 1);
    }

    #[test]
    fn lifetimes_are_not_char_literals() {
        let src = "fn f<'a>(x: &'a str) -> &'static str { unsafe { h() } }";
        let (u, _) = scan_source(src);
        assert_eq!(u, 1);
    }

    #[test]
    fn backslash_char_literal_does_not_run_away() {
        // `'\\'` is an escaped backslash. If char_literal_end scans from past the
        // backslash it jumps the real closing quote and runs to the next stray
        // quote (in the string), blanking the `unsafe` -- undercounting the
        // ceiling this gate protects. Both plain and byte forms.
        for lit in ["'\\\\'", "b'\\\\'"] {
            let src = format!("let c = {lit}; let x = \"'\"; unsafe {{ f(); }}");
            let (u, _) = scan_source(&src);
            assert_eq!(u, 1, "backslash char literal {lit} miscounted");
        }
    }

    #[test]
    fn escaped_quote_char_literal_is_counted() {
        // `'\''` (escaped single-quote) must not desync the quote scan.
        let src = "let q = '\\''; unsafe { g(); }";
        let (u, _) = scan_source(src);
        assert_eq!(u, 1);
    }

    // ---- pure gate: both directions on both metrics ----

    // (name, unsafe_count, documented_count, per-file floors).
    type Row<'a> = (&'a str, usize, Option<usize>, &'a [(&'a str, usize)]);

    fn budget(entries: &[Row]) -> Budget {
        entries
            .iter()
            .map(|(name, u, d, files)| {
                (
                    (*name).to_string(),
                    CrateBudget {
                        unsafe_count: *u,
                        documented_count: *d,
                        files: files.iter().map(|(f, n)| ((*f).to_string(), *n)).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn exact_match_is_clean() {
        let b = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 600)])]);
        assert!(compute_violations(&b, &b).is_empty());
    }

    #[test]
    fn unsafe_above_ceiling_fails() {
        let measured = budget(&[("oam_engine", 701, Some(690), &[("napi.rs", 600)])]);
        let baseline = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 600)])]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("701 > ceiling 700"), "{}", v[0]);
    }

    #[test]
    fn unsafe_below_ceiling_is_stale() {
        let measured = budget(&[("oam_engine", 699, Some(690), &[("napi.rs", 600)])]);
        let baseline = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 600)])]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("stale ceiling"), "{}", v[0]);
        assert!(v[0].contains("699 < recorded 700"), "{}", v[0]);
    }

    #[test]
    fn documented_below_floor_fails() {
        let measured = budget(&[("oam_engine", 700, Some(689), &[("napi.rs", 600)])]);
        let baseline = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 600)])]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("689 < floor 690"), "{}", v[0]);
        assert!(v[0].contains("coverage regressed"), "{}", v[0]);
    }

    #[test]
    fn documented_above_floor_is_stale() {
        let measured = budget(&[("oam_engine", 700, Some(691), &[("napi.rs", 600)])]);
        let baseline = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 600)])]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("stale floor"), "{}", v[0]);
        assert!(v[0].contains("691 > floor 690"), "{}", v[0]);
    }

    #[test]
    fn per_file_floor_bites_both_directions() {
        let baseline = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 600)])]);
        let below = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 599)])]);
        let above = budget(&[("oam_engine", 700, Some(690), &[("napi.rs", 601)])]);
        let vb = compute_violations(&below, &baseline);
        assert!(
            vb.iter()
                .any(|m| m.contains("napi.rs") && m.contains("< floor 600")),
            "{vb:?}"
        );
        let va = compute_violations(&above, &baseline);
        assert!(
            va.iter()
                .any(|m| m.contains("napi.rs") && m.contains("> floor 600")),
            "{va:?}"
        );
    }

    #[test]
    fn a_new_crate_must_be_blessed() {
        let measured = budget(&[
            ("oam_engine", 700, Some(690), &[]),
            ("oam_new", 2, None, &[]),
        ]);
        let baseline = budget(&[("oam_engine", 700, Some(690), &[])]);
        let v = compute_violations(&measured, &baseline);
        assert!(
            v.iter()
                .any(|m| m.contains("oam_new") && m.contains("not in the baseline")),
            "{v:?}"
        );
    }

    #[test]
    fn out_of_scope_crate_tracks_ceiling_only() {
        // oam_core carries unsafe but documented_count is None (clippy owns
        // coverage): the floor check is skipped, the ceiling still bites.
        let measured = budget(&[("oam_core", 98, None, &[])]);
        let baseline = budget(&[("oam_core", 97, None, &[])]);
        let v = compute_violations(&measured, &baseline);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("97"), "{}", v[0]);
    }

    // ---- JSON round-trip ----

    #[test]
    fn json_round_trips() {
        let b = budget(&[
            ("oam_core", 97, None, &[]),
            (
                "oam_engine",
                700,
                Some(690),
                &[("napi.rs", 600), ("lib.rs", 5)],
            ),
        ]);
        let back = budget_from_json(&budget_to_json(&b)).unwrap();
        assert_eq!(b, back);
    }
}
