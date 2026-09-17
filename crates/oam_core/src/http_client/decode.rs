//! Response body decoding for the `fetch` op: undici 6.24.1's content-coding
//! rules (fetch/index.js:2131-2172) over a streaming decoder whose output is
//! bounded per call.
//!
//! Two halves:
//!
//! - [`plan`] decides at headers time: which codings, none, or too many.
//! - [`Decoder`] turns compressed frames into decoded chunks of at most
//!   [`OUT_CAP`] bytes, pull-style: [`Decoder::push`] consumes only as much
//!   input as it needs to fill one chunk and leaves the rest in the caller's
//!   `Bytes`.
//!
//! Why not the node:zlib backends in lib.rs (measured for #143, review
//! "behaviour-preservation"):
//!
//! - they return a write's whole output at once: a 106-byte brotli body came
//!   out as ONE 64 MiB chunk, and a hyper frame can hold ~408 KB of gzip
//!   (h1 DEFAULT_MAX_BUFFER_SIZE). A remote peer could exhaust memory. Node
//!   emits 16 KiB chunks (zlib `chunkSize`), and so does this.
//! - flate2's `write::*` decoders hand back each write's output one write late
//!   (zio.rs:220-224 dumps before it runs), so a sync-flushed gzip SSE event
//!   would reach JS only when the next one arrives. Here the bytes a frame
//!   makes decodable come out before the next frame is read.
//! - they error on an empty or truncated body. undici decodes with
//!   `finishFlush: Z_SYNC_FLUSH` / `BROTLI_OPERATION_FLUSH`, so neither is an
//!   error: an empty gzip 200 is "" and a truncated body yields what decoded.
//!
//! gzip member framing is done here by hand because flate2 1.1.9 on its pure
//! Rust backend (miniz_oxide) has no gzip-wrapped `Decompress` (`new_gzip` is
//! `cfg(any_zlib)`): header, raw inflate, CRC32 + ISIZE trailer, following
//! zlib's inflate.c and node_zlib.cc for which bytes are an error and when.

use brotli_decompressor::{BrotliDecompressStream, BrotliResult, BrotliState, StandardAlloc};
use bytes::{Buf, Bytes};
use flate2::{Crc, Decompress, FlushDecompress, Status};

/// undici fetch/index.js:2140: more content-codings than this fails the fetch
/// ("too many content-encodings in response: N, maximum allowed is 5"), a
/// resource-exhaustion control (urllib3 GHSA-gm62-xv2j-4w53, curl
/// CVE-2022-32206).
pub const MAX_CODINGS: usize = 5;

/// Largest chunk one [`Decoder::push`] / [`Decoder::finish`] returns: Node's
/// zlib `chunkSize` default (16 KiB), the largest chunk node's fetch delivers
/// for a decoded body (measured).
pub const OUT_CAP: usize = 16 * 1024;

/// The headers-time decision for a response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Deliver the body as received.
    Identity,
    /// Decode with these codings, in header order (the last one listed was
    /// applied last by the server, so it is undone first).
    Decode(Vec<Coding>),
    /// More than [`MAX_CODINGS`] codings: fail the fetch, naming the count.
    TooMany(usize),
}

/// A content-coding undici decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    /// `gzip` or `x-gzip`.
    Gzip,
    /// `deflate`: zlib-wrapped or raw, told apart by the first byte.
    Deflate,
    /// `br`.
    Brotli,
}

/// Decide how to decode a body. `encodings` is every `content-encoding` field
/// value in wire order.
///
/// undici, fetch/index.js:2129-2172:
/// - no decoding for HEAD and CONNECT, or statuses 101/204/205/304 (the
///   caller also skips it for a 3xx it is about to follow);
/// - the field lines are joined with ", " (`headersList.get(name, true)`),
///   lower-cased and split on ',' with empty tokens KEPT, so `gzip,` is two
///   codings;
/// - more than five fails, and that is checked before any token is looked at;
/// - each token is trimmed; `gzip`/`x-gzip`, `deflate` and `br` decode, and
///   ANY other token -- `identity` and the empty token included -- turns
///   decoding off altogether (node returns `gzip, identity` still encoded).
pub fn plan(method: &http::Method, status: u16, encodings: &[&[u8]]) -> Plan {
    if *method == http::Method::HEAD
        || *method == http::Method::CONNECT
        || matches!(status, 101 | 204 | 205 | 304)
    {
        return Plan::Identity;
    }
    let joined = encodings.join(&b", "[..]);
    // `contentEncoding ? ... : []`: an absent header and an empty one both
    // mean no codings.
    if joined.is_empty() {
        return Plan::Identity;
    }
    let tokens: Vec<&[u8]> = joined.split(|&b| b == b',').collect();
    if tokens.len() > MAX_CODINGS {
        return Plan::TooMany(tokens.len());
    }
    let mut codings = Vec::with_capacity(tokens.len());
    for token in tokens {
        let token = trim_js_whitespace(token);
        let coding = if token.eq_ignore_ascii_case(b"gzip") || token.eq_ignore_ascii_case(b"x-gzip")
        {
            Coding::Gzip
        } else if token.eq_ignore_ascii_case(b"deflate") {
            Coding::Deflate
        } else if token.eq_ignore_ascii_case(b"br") {
            Coding::Brotli
        } else {
            return Plan::Identity;
        };
        codings.push(coding);
    }
    Plan::Decode(codings)
}

/// `String.prototype.trim` over a header value undici holds as a latin1
/// string: the JS whitespace and line terminators that are single latin1
/// characters -- TAB, LF, VT, FF, CR, SP and NBSP (0xA0).
fn trim_js_whitespace(mut s: &[u8]) -> &[u8] {
    let ws = |b: &u8| matches!(b, 0x09..=0x0d | 0x20 | 0xa0);
    while let [first, rest @ ..] = s
        && ws(first)
    {
        s = rest;
    }
    while let [rest @ .., last] = s
        && ws(last)
    {
        s = rest;
    }
    s
}

/// A corrupt body. The message mirrors zlib's (or brotli's) for the same
/// input where node reports one; the transport maps every decode error to one
/// body-read failure, so the text is for tests and debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(&'static str);

impl DecodeError {
    pub fn message(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for DecodeError {}

/// A streaming decoder for one response body.
///
/// Memory is bounded independently of the body: one [`OUT_CAP`] window per
/// stage, the inflate state (a 32 KiB dictionary) or the brotli state (its
/// ring buffer, at most the stream's declared window), and nothing that grows
/// with the input or output size.
pub struct Decoder {
    /// Stages in application order: `stages[0]` reads the wire bytes.
    stages: Vec<Stage>,
    /// `windows[i]` holds stage i's decoded output not yet taken downstream
    /// (by stage i+1, or by the caller for the last stage).
    windows: Vec<Window>,
    /// The first error, returned again by every later call.
    error: Option<DecodeError>,
}

impl Decoder {
    /// A decoder for `codings` in header order; they are undone in reverse.
    /// No codings is a pass-through that still caps chunks at [`OUT_CAP`].
    pub fn new(codings: &[Coding]) -> Decoder {
        let stages: Vec<Stage> = codings.iter().rev().map(|&c| Stage::new(c)).collect();
        let windows = stages.iter().map(|_| Window::new()).collect();
        Decoder {
            stages,
            windows,
            error: None,
        }
    }

    /// Feed compressed input (may be empty) and take the next decoded chunk.
    ///
    /// Returns `Some(chunk)` with `1..=OUT_CAP` bytes, consuming only as much
    /// of `input` as that took (the rest stays in `input` for the next call),
    /// or `None` once `input` is fully consumed and nothing more is decodable
    /// from it -- read the next frame then. Everything a frame makes
    /// decodable (a sync-flushed unit) is returned before `None`. Never
    /// allocates more than [`OUT_CAP`] for output and never blocks.
    pub fn push(&mut self, input: &mut Bytes) -> Result<Option<Bytes>, DecodeError> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        let Some(last) = self.stages.len().checked_sub(1) else {
            if input.is_empty() {
                return Ok(None);
            }
            return Ok(Some(input.split_to(input.len().min(OUT_CAP))));
        };
        if self.windows[last].is_empty() {
            match self.pull(last, input) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) => {
                    self.error = Some(e.clone());
                    return Err(e);
                }
            }
        }
        Ok(Some(self.windows[last].take()))
    }

    /// End of body: return what is still decodable, one chunk (`<= OUT_CAP`)
    /// per call, until `None`. Call it after every frame was pushed to
    /// `None`. A truncated stream is NOT an error (undici's `finishFlush`),
    /// and neither is an empty body.
    pub fn finish(&mut self) -> Result<Option<Bytes>, DecodeError> {
        // Every stage decodes with sync-flush semantics, so nothing is held
        // back for end of input: finishing is draining the stage windows.
        self.push(&mut Bytes::new())
    }

    /// Fill `windows[k]` (empty on entry). Returns false when the stages
    /// upstream of it are dry and `input` is fully consumed.
    ///
    /// Walks upstream while a stage cannot progress for lack of input and back
    /// downstream as soon as one produced something, so every window below
    /// `k` holds at most one stage's single step of output: a 400 KB gzip
    /// frame is pulled through 16 KiB at a time, never inflated in one go.
    fn pull(&mut self, k: usize, input: &mut Bytes) -> Result<bool, DecodeError> {
        let mut j = k;
        loop {
            let (upstream, rest) = self.windows.split_at_mut(j);
            let dst = &mut rest[0];
            debug_assert!(dst.is_empty());
            dst.start = 0;
            dst.end = 0;
            let src: &[u8] = match j {
                0 => input,
                _ => upstream[j - 1].bytes(),
            };
            let src_len = src.len();
            let step = self.stages[j].run(src, &mut dst.data)?;
            debug_assert!(step.consumed <= src_len && step.produced <= dst.data.len());
            match j {
                0 => input.advance(step.consumed),
                _ => upstream[j - 1].start += step.consumed,
            }
            dst.end = step.produced;
            if step.produced > 0 {
                if j == k {
                    return Ok(true);
                }
                j += 1;
            } else if step.consumed == 0 {
                if src_len != 0 {
                    // A stage with input and an empty window always consumes
                    // or produces; stopping here beats looping forever or
                    // clearing a window that still holds data.
                    return Err(DecodeError("decoder made no progress"));
                }
                if j == 0 {
                    return Ok(false);
                }
                j -= 1;
            }
        }
    }
}

/// One stage's output buffer.
struct Window {
    data: Box<[u8]>,
    start: usize,
    end: usize,
}

impl Window {
    fn new() -> Window {
        Window {
            data: vec![0u8; OUT_CAP].into_boxed_slice(),
            start: 0,
            end: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.start == self.end
    }

    fn bytes(&self) -> &[u8] {
        &self.data[self.start..self.end]
    }

    fn take(&mut self) -> Bytes {
        let chunk = Bytes::copy_from_slice(self.bytes());
        self.start = 0;
        self.end = 0;
        chunk
    }
}

/// What one stage step did.
struct Step {
    consumed: usize,
    produced: usize,
}

enum Stage {
    Gzip(Box<GzipStage>),
    Deflate(DeflateStage),
    // 2.6 KB of state: boxed so the enum stays small (clippy
    // large_enum_variant).
    Brotli(Box<BrotliStage>),
}

impl Stage {
    fn new(coding: Coding) -> Stage {
        match coding {
            Coding::Gzip => Stage::Gzip(Box::new(GzipStage::new())),
            Coding::Deflate => Stage::Deflate(DeflateStage::Detect),
            Coding::Brotli => Stage::Brotli(Box::new(BrotliStage::new())),
        }
    }

    /// Decode from `src` into `dst` (empty, `OUT_CAP` long). Contract: with
    /// non-empty `src`, a step consumes or produces at least one byte, or
    /// fails.
    fn run(&mut self, src: &[u8], dst: &mut [u8]) -> Result<Step, DecodeError> {
        match self {
            Stage::Gzip(s) => s.run(src, dst),
            Stage::Deflate(s) => s.run(src, dst),
            Stage::Brotli(s) => s.run(src, dst),
        }
    }
}

/// One step of raw or zlib inflate with sync-flush semantics.
fn inflate_step(
    inflate: &mut Decompress,
    src: &[u8],
    dst: &mut [u8],
) -> Result<(Step, bool), DecodeError> {
    let (in_before, out_before) = (inflate.total_in(), inflate.total_out());
    let status = inflate
        .decompress(src, dst, FlushDecompress::Sync)
        .map_err(|_| DecodeError("invalid deflate data"))?;
    let step = Step {
        consumed: (inflate.total_in() - in_before) as usize,
        produced: (inflate.total_out() - out_before) as usize,
    };
    Ok((step, status == Status::StreamEnd))
}

// ---------------------------------------------------------------------------
// gzip
// ---------------------------------------------------------------------------

const FTEXT_RESERVED: u8 = 0xe0;
const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gz {
    /// Magic byte 1 (checked together with byte 2, as zlib's NEEDBITS(16)
    /// does: a lone byte at end of body is truncation, not an error).
    Id1,
    Id2,
    /// CM, checked with FLG.
    Cm,
    Flg,
    /// MTIME (4), XFL (1), OS (1): `n` bytes still to skip.
    Fixed(u8),
    XlenLo,
    XlenHi,
    Extra(u16),
    Name,
    Comment,
    HcrcLo,
    HcrcHi,
    Body,
    /// `n` of the 8 trailer bytes seen.
    Trailer(u8),
    /// A member ended. node_zlib.cc (GUNZIP): more input starting with a
    /// non-zero byte is read as the next member (so junk is a header error);
    /// a zero byte ends decoding and everything after it is dropped -- zero
    /// padding is common. Measured: gzip+00+gzip decodes the first member
    /// only; gzip+"JUNK" fails; gzip+1f at end of body does not.
    Between,
    Ignore,
}

struct GzipStage {
    at: Gz,
    inflate: Decompress,
    /// CRC32 and size of the member's decoded bytes.
    crc: Crc,
    /// CRC32 of the header bytes, for FHCRC.
    header_crc: Crc,
    /// ID1 or CM, held for the check on the byte after it.
    held: u8,
    flags: u8,
    xlen: u16,
    trailer: [u8; 8],
}

impl GzipStage {
    fn new() -> GzipStage {
        GzipStage {
            at: Gz::Id1,
            inflate: Decompress::new(false),
            crc: Crc::new(),
            header_crc: Crc::new(),
            held: 0,
            flags: 0,
            xlen: 0,
            trailer: [0; 8],
        }
    }

    fn next_member(&mut self) {
        self.at = Gz::Id1;
        self.inflate.reset(false);
        self.crc = Crc::new();
        self.header_crc = Crc::new();
    }

    fn run(&mut self, src: &[u8], dst: &mut [u8]) -> Result<Step, DecodeError> {
        let mut consumed = 0;
        loop {
            match self.at {
                Gz::Ignore => {
                    return Ok(Step {
                        consumed: src.len(),
                        produced: 0,
                    });
                }
                Gz::Body => {
                    let (step, end) = inflate_step(&mut self.inflate, &src[consumed..], dst)?;
                    consumed += step.consumed;
                    self.crc.update(&dst[..step.produced]);
                    if end {
                        self.at = Gz::Trailer(0);
                    }
                    if step.produced > 0 || !end {
                        return Ok(Step {
                            consumed,
                            produced: step.produced,
                        });
                    }
                }
                Gz::Between => {
                    let Some(&b) = src.get(consumed) else { break };
                    if b == 0 {
                        self.at = Gz::Ignore;
                    } else {
                        self.next_member();
                    }
                }
                _ => {
                    let Some(&b) = src.get(consumed) else { break };
                    consumed += 1;
                    self.byte(b)?;
                }
            }
        }
        Ok(Step {
            consumed,
            produced: 0,
        })
    }

    /// One header or trailer byte.
    fn byte(&mut self, b: u8) -> Result<(), DecodeError> {
        if !matches!(self.at, Gz::HcrcLo | Gz::HcrcHi | Gz::Trailer(_)) {
            self.header_crc.update(&[b]);
        }
        self.at = match self.at {
            Gz::Id1 => {
                self.held = b;
                Gz::Id2
            }
            Gz::Id2 => {
                if self.held != 0x1f || b != 0x8b {
                    return Err(DecodeError("incorrect header check"));
                }
                Gz::Cm
            }
            Gz::Cm => {
                self.held = b;
                Gz::Flg
            }
            Gz::Flg => {
                if self.held != 8 {
                    return Err(DecodeError("unknown compression method"));
                }
                if b & FTEXT_RESERVED != 0 {
                    return Err(DecodeError("unknown header flags set"));
                }
                self.flags = b;
                Gz::Fixed(6)
            }
            Gz::Fixed(n) if n > 1 => Gz::Fixed(n - 1),
            Gz::Fixed(_) if self.flags & FEXTRA != 0 => Gz::XlenLo,
            Gz::Fixed(_) => self.after_extra(),
            Gz::XlenLo => {
                self.xlen = u16::from(b);
                Gz::XlenHi
            }
            Gz::XlenHi => {
                self.xlen |= u16::from(b) << 8;
                match self.xlen {
                    0 => self.after_extra(),
                    n => Gz::Extra(n),
                }
            }
            Gz::Extra(n) if n > 1 => Gz::Extra(n - 1),
            Gz::Extra(_) => self.after_extra(),
            Gz::Name if b != 0 => Gz::Name,
            Gz::Name => self.after_name(),
            Gz::Comment if b != 0 => Gz::Comment,
            Gz::Comment => self.after_comment(),
            Gz::HcrcLo => {
                self.held = b;
                Gz::HcrcHi
            }
            Gz::HcrcHi => {
                let stored = u16::from_le_bytes([self.held, b]);
                if u32::from(stored) != self.header_crc.sum() & 0xffff {
                    return Err(DecodeError("header crc mismatch"));
                }
                Gz::Body
            }
            Gz::Trailer(n) => {
                self.trailer[usize::from(n)] = b;
                match n + 1 {
                    // zlib checks CRC32 as soon as its 4 bytes are in, before
                    // ISIZE has arrived (inflate.c CHECK then LENGTH).
                    4 => {
                        let stored = u32::from_le_bytes([
                            self.trailer[0],
                            self.trailer[1],
                            self.trailer[2],
                            self.trailer[3],
                        ]);
                        if stored != self.crc.sum() {
                            return Err(DecodeError("incorrect data check"));
                        }
                        Gz::Trailer(4)
                    }
                    8 => {
                        let stored = u32::from_le_bytes([
                            self.trailer[4],
                            self.trailer[5],
                            self.trailer[6],
                            self.trailer[7],
                        ]);
                        if stored != self.crc.amount() {
                            return Err(DecodeError("incorrect length check"));
                        }
                        Gz::Between
                    }
                    n => Gz::Trailer(n),
                }
            }
            Gz::Body | Gz::Between | Gz::Ignore => unreachable!("not a header byte state"),
        };
        Ok(())
    }

    fn after_extra(&self) -> Gz {
        if self.flags & FNAME != 0 {
            Gz::Name
        } else {
            self.after_name()
        }
    }

    fn after_name(&self) -> Gz {
        if self.flags & FCOMMENT != 0 {
            Gz::Comment
        } else {
            self.after_comment()
        }
    }

    fn after_comment(&self) -> Gz {
        if self.flags & FHCRC != 0 {
            Gz::HcrcLo
        } else {
            Gz::Body
        }
    }
}

// ---------------------------------------------------------------------------
// deflate
// ---------------------------------------------------------------------------

enum DeflateStage {
    /// No byte seen yet. undici's InflateStream (fetch/util.js:1351-1367)
    /// skips empty chunks and picks on the first byte of the first non-empty
    /// one: `(b & 0x0f) === 0x08` is a zlib header (CM 8), anything else raw
    /// deflate.
    Detect,
    Inflate(Decompress),
    /// The deflate stream ended. node's zlib ignores whatever follows
    /// (Inflate/InflateRaw only look past the end for gzip members); measured:
    /// zlib+"JUNK" and raw+"JUNK" both decode without error.
    Done,
}

impl DeflateStage {
    fn run(&mut self, src: &[u8], dst: &mut [u8]) -> Result<Step, DecodeError> {
        if let DeflateStage::Detect = self {
            let Some(&first) = src.first() else {
                return Ok(Step {
                    consumed: 0,
                    produced: 0,
                });
            };
            *self = DeflateStage::Inflate(Decompress::new(first & 0x0f == 0x08));
        }
        match self {
            DeflateStage::Inflate(inflate) => {
                let (step, end) = inflate_step(inflate, src, dst)?;
                if end {
                    *self = DeflateStage::Done;
                }
                Ok(step)
            }
            _ => Ok(Step {
                consumed: src.len(),
                produced: 0,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// brotli
// ---------------------------------------------------------------------------

struct BrotliStage {
    state: BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>,
    /// The stream is complete; like zlib, node's brotli decoder drops the
    /// bytes after it (measured: br+"JUNK" decodes without error).
    done: bool,
}

impl BrotliStage {
    fn new() -> BrotliStage {
        BrotliStage {
            state: BrotliState::new(
                StandardAlloc::default(),
                StandardAlloc::default(),
                StandardAlloc::default(),
            ),
            done: false,
        }
    }

    fn run(&mut self, src: &[u8], dst: &mut [u8]) -> Result<Step, DecodeError> {
        if self.done {
            return Ok(Step {
                consumed: src.len(),
                produced: 0,
            });
        }
        let mut available_in = src.len();
        let mut input_offset = 0;
        let mut available_out = dst.len();
        let mut output_offset = 0;
        let mut total_out = 0;
        // With room in `dst` the decoder writes out what it has decoded before
        // it reports NeedsMoreInput (brotli-decompressor decode.rs:2717-2727,
        // C brotli's "pro-actively push output"), which is what makes a
        // BROTLI_OPERATION_FLUSH'd unit arrive with its frame.
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            src,
            &mut available_out,
            &mut output_offset,
            dst,
            &mut total_out,
            &mut self.state,
        );
        match result {
            BrotliResult::ResultFailure => Err(DecodeError("brotli decompression failed")),
            BrotliResult::ResultSuccess => {
                self.done = true;
                Ok(Step {
                    consumed: src.len(),
                    produced: output_offset,
                })
            }
            BrotliResult::NeedsMoreInput | BrotliResult::NeedsMoreOutput => Ok(Step {
                consumed: input_offset,
                produced: output_offset,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};
    use std::io::Write;

    // -- plan ---------------------------------------------------------------

    fn plan_get(values: &[&str]) -> Plan {
        let raw: Vec<&[u8]> = values.iter().map(|v| v.as_bytes()).collect();
        plan(&http::Method::GET, 200, &raw)
    }

    #[test]
    fn plan_vectors_measured_against_node() {
        use Coding::*;
        assert_eq!(plan_get(&[]), Plan::Identity);
        assert_eq!(plan_get(&[""]), Plan::Identity);
        assert_eq!(plan_get(&["gzip"]), Plan::Decode(vec![Gzip]));
        assert_eq!(plan_get(&["GZIP"]), Plan::Decode(vec![Gzip]));
        assert_eq!(plan_get(&["x-gzip"]), Plan::Decode(vec![Gzip]));
        assert_eq!(plan_get(&["X-Gzip"]), Plan::Decode(vec![Gzip]));
        assert_eq!(plan_get(&["deflate"]), Plan::Decode(vec![Deflate]));
        assert_eq!(plan_get(&["br"]), Plan::Decode(vec![Brotli]));
        assert_eq!(plan_get(&[" \tgzip\t "]), Plan::Decode(vec![Gzip]));
        // Trailing comma: an empty token, which is unknown.
        assert_eq!(plan_get(&["gzip,"]), Plan::Identity);
        assert_eq!(plan_get(&["gzip, identity"]), Plan::Identity);
        assert_eq!(plan_get(&["identity"]), Plan::Identity);
        assert_eq!(plan_get(&["compress"]), Plan::Identity);
        assert_eq!(plan_get(&["zstd"]), Plan::Identity);
        // Two header lines are joined: both layers decode.
        assert_eq!(plan_get(&["gzip", "gzip"]), Plan::Decode(vec![Gzip, Gzip]));
        assert_eq!(plan_get(&["gzip", "br"]), Plan::Decode(vec![Gzip, Brotli]));
        assert_eq!(
            plan_get(&["deflate, gzip,br"]),
            Plan::Decode(vec![Deflate, Gzip, Brotli])
        );
        // Five is allowed, six is not -- counted before tokens are looked at.
        assert_eq!(
            plan_get(&["gzip,gzip,gzip,gzip,gzip"]),
            Plan::Decode(vec![Gzip; 5])
        );
        assert_eq!(
            plan_get(&["gzip,gzip,gzip,gzip,gzip,gzip"]),
            Plan::TooMany(6)
        );
        assert_eq!(plan_get(&["a,b,c", "d,e,f,g"]), Plan::TooMany(7));
        assert_eq!(plan_get(&[",,,,,"]), Plan::TooMany(6));
        assert_eq!(plan_get(&["", "", "", "", "", ""]), Plan::TooMany(6));
    }

    #[test]
    fn plan_trims_like_string_trim_over_latin1() {
        assert_eq!(
            plan(&http::Method::GET, 200, &[b"\xa0br\x0b"]),
            Plan::Decode(vec![Coding::Brotli])
        );
        // U+0085 (NEL) is not JS whitespace.
        assert_eq!(plan(&http::Method::GET, 200, &[b"br\x85"]), Plan::Identity);
    }

    #[test]
    fn plan_exempt_methods_and_statuses() {
        let gz: &[&[u8]] = &[b"gzip"];
        for m in [http::Method::HEAD, http::Method::CONNECT] {
            assert_eq!(plan(&m, 200, gz), Plan::Identity);
        }
        for s in [101, 204, 205, 304] {
            assert_eq!(plan(&http::Method::GET, s, gz), Plan::Identity, "{s}");
        }
        for s in [200, 201, 202, 206, 301, 404, 500] {
            assert_eq!(
                plan(&http::Method::GET, s, gz),
                Plan::Decode(vec![Coding::Gzip])
            );
        }
        // The count check does not run for exempt responses either.
        let many: &[&[u8]] = &[b"gzip,gzip,gzip,gzip,gzip,gzip"];
        assert_eq!(plan(&http::Method::HEAD, 200, many), Plan::Identity);
    }

    // -- vector builders ----------------------------------------------------

    /// Deterministic, moderately compressible text spanning several OUT_CAP
    /// chunks.
    fn text(len: usize, seed: u64) -> Vec<u8> {
        const WORDS: &[&str] = &[
            "data: ",
            "token",
            " event",
            "\n\n",
            "{\"id\":",
            "42",
            "}",
            "stream",
            " the ",
            "oam",
            "chunk",
            ", ",
            "\u{e9}t\u{e9}",
            "zlib",
            "\r\n",
        ];
        let mut rng = Rng(seed);
        let mut out = Vec::with_capacity(len + 16);
        while out.len() < len {
            let w = WORDS[(rng.next() % WORDS.len() as u64) as usize];
            out.extend_from_slice(w.as_bytes());
            if rng.next().is_multiple_of(7) {
                out.push((rng.next() % 256) as u8);
            }
        }
        out.truncate(len);
        out
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
    }

    #[derive(Default)]
    struct GzOpts {
        extra: Option<Vec<u8>>,
        name: Option<&'static [u8]>,
        comment: Option<&'static [u8]>,
        hcrc: bool,
    }

    /// A gzip member built by hand: header per `opts`, raw deflate of `data`,
    /// CRC32 + ISIZE trailer.
    fn gzip_member(data: &[u8], opts: &GzOpts) -> Vec<u8> {
        let mut flags = 0u8;
        if opts.extra.is_some() {
            flags |= FEXTRA;
        }
        if opts.name.is_some() {
            flags |= FNAME;
        }
        if opts.comment.is_some() {
            flags |= FCOMMENT;
        }
        if opts.hcrc {
            flags |= FHCRC;
        }
        let mut out = vec![0x1f, 0x8b, 8, flags, 1, 2, 3, 4, 0, 255];
        if let Some(extra) = &opts.extra {
            out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            out.extend_from_slice(extra);
        }
        if let Some(name) = opts.name {
            out.extend_from_slice(name);
            out.push(0);
        }
        if let Some(comment) = opts.comment {
            out.extend_from_slice(comment);
            out.push(0);
        }
        if opts.hcrc {
            let mut crc = Crc::new();
            crc.update(&out);
            out.extend_from_slice(&((crc.sum() & 0xffff) as u16).to_le_bytes());
        }
        let mut enc = flate2::write::DeflateEncoder::new(out, Compression::default());
        enc.write_all(data).unwrap();
        let mut out = enc.finish().unwrap();
        let mut crc = Crc::new();
        crc.update(data);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&crc.amount().to_le_bytes());
        out
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn br(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
            enc.write_all(data).unwrap();
        }
        out
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    // -- drivers ------------------------------------------------------------

    /// Feed `wire` in frames whose sizes come from `sizes`, draining each with
    /// `push` and the end with `finish`, checking the chunk contract as it
    /// goes.
    fn decode_split(
        codings: &[Coding],
        wire: &[u8],
        mut sizes: impl FnMut() -> usize,
    ) -> Result<Vec<u8>, DecodeError> {
        let mut d = Decoder::new(codings);
        let mut out = Vec::new();
        let mut rest = wire;
        let drain = |d: &mut Decoder, frame: &mut Bytes, out: &mut Vec<u8>| loop {
            match d.push(frame)? {
                Some(chunk) => {
                    assert!(
                        !chunk.is_empty() && chunk.len() <= OUT_CAP,
                        "{}",
                        chunk.len()
                    );
                    out.extend_from_slice(&chunk);
                }
                None => {
                    assert!(
                        frame.is_empty(),
                        "None with {} bytes unconsumed",
                        frame.len()
                    );
                    return Ok::<(), DecodeError>(());
                }
            }
        };
        while !rest.is_empty() {
            let n = sizes().clamp(1, rest.len());
            let mut frame = Bytes::copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            drain(&mut d, &mut frame, &mut out)?;
            // An empty frame between real ones changes nothing.
            drain(&mut d, &mut Bytes::new(), &mut out)?;
        }
        while let Some(chunk) = d.finish()? {
            assert!(!chunk.is_empty() && chunk.len() <= OUT_CAP);
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    fn decode_all(codings: &[Coding], wire: &[u8]) -> Result<Vec<u8>, DecodeError> {
        decode_split(codings, wire, || usize::MAX)
    }

    fn err(msg: &'static str) -> Result<Vec<u8>, DecodeError> {
        Err(DecodeError(msg))
    }

    // -- behaviour ----------------------------------------------------------

    #[test]
    fn single_and_multi_member_gzip() {
        let a = text(40_000, 1);
        let b = text(3_000, 2);
        assert_eq!(decode_all(&[Coding::Gzip], &gzip(&a)), Ok(a.clone()));
        let two = cat(&[&gzip(&a), &gzip(&b)]);
        assert_eq!(decode_all(&[Coding::Gzip], &two), Ok(cat(&[&a, &b])));
        // An empty member in the middle is still a member.
        let three = cat(&[&gzip(&a), &gzip(b""), &gzip(&b)]);
        assert_eq!(decode_all(&[Coding::Gzip], &three), Ok(cat(&[&a, &b])));
    }

    #[test]
    fn gzip_header_fields_are_parsed() {
        let a = text(5_000, 3);
        let opts = GzOpts {
            extra: Some(vec![b'A', b'B', 2, 0, 7, 9]),
            name: Some(b"name.txt"),
            comment: Some(b"a comment"),
            hcrc: true,
        };
        assert_eq!(
            decode_all(&[Coding::Gzip], &gzip_member(&a, &opts)),
            Ok(a.clone())
        );
        let empty_extra = GzOpts {
            extra: Some(Vec::new()),
            ..GzOpts::default()
        };
        assert_eq!(
            decode_all(&[Coding::Gzip], &gzip_member(&a, &empty_extra)),
            Ok(a.clone())
        );
    }

    #[test]
    fn gzip_errors_match_zlib() {
        let a = text(2_000, 4);
        let good = gzip(&a);
        let n = good.len();
        let mut bad_crc = good.clone();
        bad_crc[n - 8] ^= 1;
        assert_eq!(
            decode_all(&[Coding::Gzip], &bad_crc),
            err("incorrect data check")
        );
        let mut bad_len = good.clone();
        bad_len[n - 1] ^= 1;
        assert_eq!(
            decode_all(&[Coding::Gzip], &bad_len),
            err("incorrect length check")
        );
        // CRC is checked once its four bytes are in, ISIZE or not.
        assert_eq!(
            decode_all(&[Coding::Gzip], &bad_crc[..n - 2]),
            err("incorrect data check")
        );
        assert_eq!(
            decode_all(&[Coding::Gzip], b"not gzip at all"),
            err("incorrect header check")
        );
        let mut cm7 = good.clone();
        cm7[2] = 7;
        assert_eq!(
            decode_all(&[Coding::Gzip], &cm7),
            err("unknown compression method")
        );
        let mut reserved = good.clone();
        reserved[3] |= 0x20;
        assert_eq!(
            decode_all(&[Coding::Gzip], &reserved),
            err("unknown header flags set")
        );
        let mut hcrc = gzip_member(
            &a,
            &GzOpts {
                hcrc: true,
                ..GzOpts::default()
            },
        );
        hcrc[10] ^= 0xff;
        assert_eq!(
            decode_all(&[Coding::Gzip], &hcrc),
            err("header crc mismatch")
        );
        let mut corrupt = good.clone();
        corrupt[10] = 0xff; // BTYPE 11: reserved block type
        assert_eq!(
            decode_all(&[Coding::Gzip], &corrupt),
            err("invalid deflate data")
        );
    }

    #[test]
    fn gzip_bytes_after_a_member() {
        let a = text(3_000, 5);
        let b = text(100, 6);
        let g = gzip(&a);
        // Junk: read as a next member, fails its header check.
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, b"JUNK"])),
            err("incorrect header check")
        );
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, &[0x1f, 0x00]])),
            err("incorrect header check")
        );
        // A lone byte at the end is a truncated next header.
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, &[0x1f]])),
            Ok(a.clone())
        );
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, b"J"])),
            Ok(a.clone())
        );
        // Zero padding ends decoding; even a valid member after it is dropped.
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, &[0, 0, 0]])),
            Ok(a.clone())
        );
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, &[0], &gzip(&b)])),
            Ok(a.clone())
        );
        assert_eq!(
            decode_all(&[Coding::Gzip], &cat(&[&g, &[0], b"JUNK"])),
            Ok(a.clone())
        );
    }

    #[test]
    fn truncation_is_not_an_error_at_any_length() {
        let a = text(20_000, 7);
        let vectors: [(Coding, Vec<u8>); 4] = [
            (
                Coding::Gzip,
                gzip_member(
                    &a,
                    &GzOpts {
                        extra: Some(vec![1, 2, 3]),
                        name: Some(b"n"),
                        comment: Some(b"c"),
                        hcrc: true,
                    },
                ),
            ),
            (Coding::Deflate, zlib(&a)),
            (Coding::Deflate, raw_deflate(&a)),
            (Coding::Brotli, br(&a)),
        ];
        for (coding, wire) in &vectors {
            // Every prefix up to a few hundred bytes, then a stride through
            // the rest: each decodes to a prefix of the text without error.
            let lens = (0..wire.len().min(300)).chain((300..wire.len()).step_by(97));
            for len in lens {
                let got = decode_all(&[*coding], &wire[..len])
                    .unwrap_or_else(|e| panic!("{coding:?} prefix {len}: {e}"));
                assert!(a.starts_with(&got), "{coding:?} prefix {len}");
            }
        }
    }

    #[test]
    fn a_body_cut_before_the_trailer_delivers_all_data() {
        let a = text(10_000, 8);
        let g = gzip(&a);
        for cut in 1..=8 {
            assert_eq!(
                decode_all(&[Coding::Gzip], &g[..g.len() - cut]),
                Ok(a.clone())
            );
        }
    }

    #[test]
    fn empty_body_is_empty_for_every_coding() {
        for c in [Coding::Gzip, Coding::Deflate, Coding::Brotli] {
            assert_eq!(decode_all(&[c], b""), Ok(Vec::new()), "{c:?}");
            let mut d = Decoder::new(&[c]);
            assert_eq!(d.push(&mut Bytes::new()), Ok(None));
            assert_eq!(d.finish(), Ok(None));
        }
        assert_eq!(
            decode_all(&[Coding::Gzip, Coding::Brotli, Coding::Deflate], b""),
            Ok(Vec::new())
        );
    }

    #[test]
    fn deflate_zlib_and_raw_autodetect() {
        let a = text(30_000, 9);
        assert_eq!(decode_all(&[Coding::Deflate], &zlib(&a)), Ok(a.clone()));
        assert_eq!(
            decode_all(&[Coding::Deflate], &raw_deflate(&a)),
            Ok(a.clone())
        );
        // Bytes after the end of the stream are dropped, not an error.
        assert_eq!(
            decode_all(&[Coding::Deflate], &cat(&[&zlib(&a), b"JUNK"])),
            Ok(a.clone())
        );
        assert_eq!(
            decode_all(&[Coding::Deflate], &cat(&[&raw_deflate(&a), b"JUNK"])),
            Ok(a.clone())
        );
        let z = zlib(&a);
        let mut bad_adler = z.clone();
        let n = bad_adler.len();
        bad_adler[n - 1] ^= 1;
        assert!(decode_all(&[Coding::Deflate], &bad_adler).is_err());
        assert!(decode_all(&[Coding::Deflate], b"garbage garbage garbage").is_err());
        // A gzip body labelled deflate: 0x1f & 0x0f != 8 -> raw -> corrupt.
        assert!(decode_all(&[Coding::Deflate], &gzip(&a)).is_err());
    }

    #[test]
    fn brotli_decodes_and_drops_trailing_bytes() {
        let a = text(50_000, 10);
        assert_eq!(decode_all(&[Coding::Brotli], &br(&a)), Ok(a.clone()));
        assert_eq!(
            decode_all(&[Coding::Brotli], &cat(&[&br(&a), b"JUNK"])),
            Ok(a.clone())
        );
        assert_eq!(
            decode_all(&[Coding::Brotli], b"garbage garbage garbage"),
            err("brotli decompression failed")
        );
    }

    #[test]
    fn stacked_codings_undo_in_reverse() {
        let a = text(25_000, 11);
        // `content-encoding: gzip, br`: gzip first, then br over it.
        let wire = br(&gzip(&a));
        assert_eq!(
            decode_all(&[Coding::Gzip, Coding::Brotli], &wire),
            Ok(a.clone())
        );
        // The wrong order fails.
        assert!(decode_all(&[Coding::Brotli, Coding::Gzip], &wire).is_err());
        // Five layers.
        let wire = gzip(&br(&zlib(&gzip(&raw_deflate(&a)))));
        let codings = [
            Coding::Deflate,
            Coding::Gzip,
            Coding::Deflate,
            Coding::Brotli,
            Coding::Gzip,
        ];
        assert_eq!(decode_all(&codings, &wire), Ok(a.clone()));
    }

    #[test]
    fn no_codings_is_a_capped_pass_through() {
        let a = text(40_000, 12);
        assert_eq!(decode_split(&[], &a, || 40_000), Ok(a.clone()));
    }

    #[test]
    fn errors_are_sticky() {
        let mut d = Decoder::new(&[Coding::Gzip]);
        let mut junk = Bytes::from_static(b"JUNK");
        assert!(d.push(&mut junk).is_err());
        assert!(d.push(&mut Bytes::from(gzip(b"x"))).is_err());
        assert!(d.finish().is_err());
    }

    #[test]
    fn decoder_is_send() {
        fn assert_send<T: Send + 'static>() {}
        assert_send::<Decoder>();
    }

    // -- every split point ----------------------------------------------------

    enum Want {
        Data(Vec<u8>),
        Fails,
    }

    fn split_vectors() -> Vec<(&'static str, Vec<Coding>, Vec<u8>, Want)> {
        let a = text(20_000, 13);
        let b = text(700, 14);
        let full = GzOpts {
            extra: Some(vec![9; 40]),
            name: Some(b"file.json"),
            comment: Some(b"comment"),
            hcrc: true,
        };
        let members = cat(&[&gzip_member(&a, &full), &gzip(&b), &gzip(b"")]);
        vec![
            (
                "gzip members with every header field",
                vec![Coding::Gzip],
                members,
                Want::Data(cat(&[&a, &b])),
            ),
            (
                "zlib",
                vec![Coding::Deflate],
                zlib(&a),
                Want::Data(a.clone()),
            ),
            (
                "raw deflate",
                vec![Coding::Deflate],
                raw_deflate(&a),
                Want::Data(a.clone()),
            ),
            (
                "brotli",
                vec![Coding::Brotli],
                br(&a),
                Want::Data(a.clone()),
            ),
            (
                "gzip, br",
                vec![Coding::Gzip, Coding::Brotli],
                br(&gzip(&a)),
                Want::Data(a.clone()),
            ),
            (
                "deflate, gzip",
                vec![Coding::Deflate, Coding::Gzip],
                gzip(&zlib(&b)),
                Want::Data(b.clone()),
            ),
            (
                "gzip then zero padding then a member",
                vec![Coding::Gzip],
                cat(&[&gzip(&b), &[0, 0], &gzip(&b)]),
                Want::Data(b.clone()),
            ),
            (
                "truncated gzip",
                vec![Coding::Gzip],
                gzip(&a)[..9_000.min(gzip(&a).len() - 1)].to_vec(),
                Want::Data(Vec::new()), // checked as a prefix below
            ),
            (
                "gzip then junk",
                vec![Coding::Gzip],
                cat(&[&gzip(&b), b"JUNK"]),
                Want::Fails,
            ),
            (
                "bad gzip crc",
                vec![Coding::Gzip],
                {
                    let mut g = gzip(&b);
                    let n = g.len();
                    g[n - 6] ^= 0x40;
                    g
                },
                Want::Fails,
            ),
        ]
    }

    /// Every vector fed in frames of every size 1..=17 and along
    /// pseudo-random splits decodes to the same bytes (or fails), so no
    /// header, trailer, magic or window boundary depends on where the network
    /// cut the body.
    #[test]
    fn every_split_point_gives_the_same_result() {
        for (name, codings, wire, want) in split_vectors() {
            let mut runs: Vec<Box<dyn FnMut() -> usize>> = Vec::new();
            for size in 1..=17usize {
                runs.push(Box::new(move || size));
            }
            for seed in 1..=6u64 {
                let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
                runs.push(Box::new(move || match rng.next() % 4 {
                    0 => 1,
                    1 => (rng.next() % 8) as usize + 1,
                    2 => (rng.next() % 300) as usize + 1,
                    _ => (rng.next() % 20_000) as usize + 1,
                }));
            }
            for (i, sizes) in runs.into_iter().enumerate() {
                let got = decode_split(&codings, &wire, sizes);
                match &want {
                    Want::Fails => assert!(got.is_err(), "{name}: run {i} did not fail"),
                    Want::Data(data) if name == "truncated gzip" => {
                        let got = got.unwrap_or_else(|e| panic!("{name}: run {i}: {e}"));
                        assert!(data.is_empty() && got.len() > 1000, "{name}: run {i}");
                        assert_eq!(got, decode_all(&codings, &wire).unwrap(), "{name}: run {i}");
                    }
                    Want::Data(data) => {
                        assert_eq!(
                            got.as_ref().map(Vec::len),
                            Ok(data.len()),
                            "{name}: run {i}"
                        );
                        assert!(got.unwrap() == *data, "{name}: run {i}");
                    }
                }
            }
        }
    }

    // -- sync flush: no one-frame lag -----------------------------------------

    const UNITS: [&[u8]; 3] = [
        b"data: {\"tok\":\"one\"}\n\n",
        b"data: {\"tok\":\"two\"}\n\n",
        b"data: {\"tok\":\"three\"}\n\n",
    ];

    /// Drain everything `frame` makes decodable.
    fn drain(d: &mut Decoder, frame: &[u8]) -> Vec<u8> {
        let mut frame = Bytes::copy_from_slice(frame);
        let mut out = Vec::new();
        while let Some(chunk) = d.push(&mut frame).unwrap() {
            out.extend_from_slice(&chunk);
        }
        assert!(frame.is_empty());
        out
    }

    /// Deflate-family frames: each unit compressed and sync-flushed on its
    /// own, as a gzip/zlib/raw SSE server writes them.
    fn flushed_frames(zlib_header: bool, gzip_wrap: bool) -> Vec<Vec<u8>> {
        let mut c = Compress::new(Compression::default(), zlib_header);
        let mut frames = Vec::new();
        let mut crc = Crc::new();
        for (i, unit) in UNITS.iter().enumerate() {
            let mut frame = if gzip_wrap && i == 0 {
                vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]
            } else {
                Vec::new()
            };
            crc.update(unit);
            let mut out = Vec::with_capacity(unit.len() + 64);
            c.compress_vec(unit, &mut out, FlushCompress::Sync).unwrap();
            assert_eq!(
                c.total_in() as usize,
                UNITS[..=i].iter().map(|u| u.len()).sum::<usize>()
            );
            frame.extend_from_slice(&out);
            frames.push(frame);
        }
        let mut tail = Vec::with_capacity(64);
        c.compress_vec(&[], &mut tail, FlushCompress::Finish)
            .unwrap();
        if gzip_wrap {
            tail.extend_from_slice(&crc.sum().to_le_bytes());
            tail.extend_from_slice(&crc.amount().to_le_bytes());
        }
        frames.push(tail);
        frames
    }

    #[test]
    fn sync_flushed_units_come_out_with_their_own_frame() {
        let cases = [
            ("gzip", Coding::Gzip, flushed_frames(false, true)),
            ("zlib", Coding::Deflate, flushed_frames(true, false)),
            ("raw", Coding::Deflate, flushed_frames(false, false)),
        ];
        for (name, coding, frames) in cases {
            let mut d = Decoder::new(&[coding]);
            for (i, unit) in UNITS.iter().enumerate() {
                assert_eq!(drain(&mut d, &frames[i]), *unit, "{name} unit {i}");
            }
            assert_eq!(drain(&mut d, &frames[3]), b"", "{name} tail");
            assert_eq!(d.finish(), Ok(None));
        }
    }

    #[test]
    fn brotli_flushed_units_come_out_with_their_own_frame() {
        let mut enc = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        let mut frames = Vec::new();
        let mut seen = 0;
        for unit in UNITS {
            enc.write_all(unit).unwrap();
            enc.flush().unwrap();
            let all = enc.get_ref();
            frames.push(all[seen..].to_vec());
            seen = all.len();
        }
        let all = enc.into_inner();
        frames.push(all[seen..].to_vec());
        let mut d = Decoder::new(&[Coding::Brotli]);
        for (i, unit) in UNITS.iter().enumerate() {
            assert_eq!(drain(&mut d, &frames[i]), *unit, "br unit {i}");
        }
        assert_eq!(drain(&mut d, &frames[3]), b"");
        assert_eq!(d.finish(), Ok(None));
    }

    // -- bombs: output per call stays bounded ---------------------------------

    /// `zlib.brotliCompressSync(Buffer.alloc(64 * 1024 * 1024))` on node
    /// v22.22.2: 106 bytes that decode to 64 MiB of zeros. Node's fetch emits
    /// it in 16 KiB chunks; the node:zlib backend emitted one 64 MiB chunk.
    const BR_BOMB_64M: &str = "cbffff3ff82700e2b14020f7fe8fffff7ff04f00c4611180eefd1fffffffe09f0088c30200ddfb3ffeffffc13f0110870500baf77ffcffff837f02200e0b0074effff8ffff07ff04401c1600e8defff1ffff0ffe0980382c00d0bdffcbffff3ffc1300715800a07bff07";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn brotli_bomb_is_delivered_in_bounded_chunks() {
        let wire = unhex(BR_BOMB_64M);
        assert_eq!(wire.len(), 106);
        let mut d = Decoder::new(&[Coding::Brotli]);
        let mut frame = Bytes::from(wire);
        let (mut total, mut chunks, mut max) = (0usize, 0usize, 0usize);
        while let Some(chunk) = d.push(&mut frame).unwrap() {
            assert!(chunk.iter().all(|&b| b == 0));
            max = max.max(chunk.len());
            total += chunk.len();
            chunks += 1;
        }
        while let Some(chunk) = d.finish().unwrap() {
            max = max.max(chunk.len());
            total += chunk.len();
            chunks += 1;
        }
        assert_eq!(total, 64 * 1024 * 1024);
        assert_eq!(max, OUT_CAP);
        assert!(chunks >= 4096, "{chunks}");
    }

    /// A ~400 KB frame of gzip (the size of hyper's largest h1 read) is
    /// pulled through 16 KiB at a time: the first push consumes a few dozen
    /// bytes, not the frame, and no chunk exceeds OUT_CAP.
    #[test]
    fn gzip_bomb_frame_is_pulled_in_bounded_chunks() {
        let mib = vec![0u8; 1024 * 1024];
        let member = gzip(&mib);
        let copies = 400_000 / member.len() + 1;
        let frame_bytes: Vec<u8> = member.repeat(copies);
        assert!(frame_bytes.len() >= 400_000);
        let mut d = Decoder::new(&[Coding::Gzip]);
        let mut frame = Bytes::from(frame_bytes);
        let before = frame.len();
        let first = d.push(&mut frame).unwrap().unwrap();
        assert_eq!(first.len(), OUT_CAP);
        assert!(
            before - frame.len() < 1024,
            "consumed {}",
            before - frame.len()
        );
        // Decode the first 64 MiB of it and stop, as a reader that gave up
        // would: every chunk bounded, input consumed only in step with output.
        let mut total = first.len();
        while total < 64 * 1024 * 1024 {
            let chunk = d.push(&mut frame).unwrap().unwrap();
            assert!(chunk.len() <= OUT_CAP && chunk.iter().all(|&b| b == 0));
            total += chunk.len();
        }
        let per_mib = member.len();
        let consumed = before - frame.len();
        assert!(consumed <= 65 * per_mib, "consumed {consumed} for 64 MiB");
    }

    #[test]
    fn gzip_bomb_decodes_fully_with_bounded_chunks() {
        let zeros = vec![0u8; 32 * 1024 * 1024];
        let wire = gzip(&zeros);
        let mut d = Decoder::new(&[Coding::Gzip]);
        let mut frame = Bytes::from(wire);
        let (mut total, mut max) = (0usize, 0usize);
        while let Some(chunk) = d.push(&mut frame).unwrap() {
            max = max.max(chunk.len());
            total += chunk.len();
        }
        while let Some(chunk) = d.finish().unwrap() {
            max = max.max(chunk.len());
            total += chunk.len();
        }
        assert_eq!(total, zeros.len());
        assert_eq!(max, OUT_CAP);
    }
}
