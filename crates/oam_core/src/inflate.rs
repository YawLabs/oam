//! Inflate as zlib does it, over miniz_oxide's core decoder.
//!
//! Two layers:
//!
//! - [`Inflater`]: raw (or miniz-parsed zlib) deflate with the history held
//!   here, so a copy from before the first output byte fails as zlib's does.
//!   Shared by fetch's body decoder (`http_client::decode`) and node:zlib.
//! - [`NodeInflate`]: node:zlib's inflate streams -- the gzip, zlib and raw
//!   wrappers and unzip's auto-detection, framed by hand, with node_zlib.cc's
//!   rule for a gzip member followed by more input, and every failure as the
//!   zlib return code and message node reports.
//!
//! Why not flate2's decoders (which node:zlib used until #166): they keep
//! miniz's wrapping 32 KiB dictionary, where a back-reference before the
//! start of the output reads zeros instead of failing; their gzip decoders
//! stop after the first member (`read::GzDecoder` drops the rest, and
//! `write::GzDecoder` refuses it); and their errors carry no zlib code.

use flate2::Crc;
use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_COMPUTE_ADLER32, TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_IGNORE_ADLER32,
    TINFL_FLAG_PARSE_ZLIB_HEADER, TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use miniz_oxide::inflate::core::{DecompressorOxide, decompress};

/// The deflate window: the farthest a copy can reach back (RFC 1951), and the
/// window node's zlib inflates with (windowBits 15 for gunzip and raw; a
/// zlib header's smaller CINFO does not shrink it, inflate.c keeps `wbits`).
pub(crate) const WINDOW: usize = 32 * 1024;

/// The most one [`Inflater::step`] writes: node's zlib `chunkSize` default
/// (16 KiB).
pub(crate) const STEP_OUT: usize = 16 * 1024;

/// Why an [`Inflater`] step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InflateFailure {
    /// A miniz-parsed zlib stream's Adler-32 did not match.
    DataCheck,
    /// A copy reached back before the first byte of the output: zlib's
    /// "invalid distance too far back".
    TooFarBack,
    /// Any other malformed deflate data. miniz reports one failure for every
    /// kind (bad block type, code lengths, codes, stored length), so which
    /// one zlib would have named is not known.
    Invalid,
}

/// Raw or zlib inflate over miniz_oxide's core decoder, in its non-wrapping
/// mode: the output buffer is the history, and a copy reaching past its start
/// fails -- zlib's "invalid distance too far back" check (inflate.c
/// `state->offset > state->whave + out - left`).
///
/// `hist[..pos]` holds the last `min(output so far, WINDOW)` bytes, and each
/// step decodes into the [`STEP_OUT`] after them, so a step's output is at
/// most one chunk -- zlib's granularity with node's 16 KiB `chunkSize`, where
/// flate2 inflated up to its 32 KiB dictionary per call. When the next step
/// would not fit, the last `WINDOW` bytes slide to the front; `pos` only
/// exceeds `WINDOW` once the stream has, so the check stays exact.
pub(crate) struct Inflater {
    core: Box<DecompressorOxide>,
    hist: Box<[u8]>,
    pos: usize,
    zlib: bool,
}

impl Inflater {
    /// `zlib`: miniz parses the zlib header and checks the Adler-32 trailer.
    /// Otherwise the input is raw deflate.
    pub(crate) fn new(zlib: bool) -> Inflater {
        Inflater {
            core: Box::default(),
            hist: vec![0u8; WINDOW + STEP_OUT].into_boxed_slice(),
            pos: 0,
            zlib,
        }
    }

    /// Start a new stream: zlib's `inflateReset` empties the window too
    /// (`whave = 0`), so the next gzip member cannot copy from this one.
    pub(crate) fn reset(&mut self) {
        self.core.init();
        self.pos = 0;
    }

    /// Prime the window with a preset dictionary before the stream's first
    /// byte, as zlib's `inflateSetDictionary` does: its last [`WINDOW`]
    /// bytes become history a copy may reach into, and they count toward
    /// the "too far back" check. The output is unchanged.
    pub(crate) fn prime(&mut self, dictionary: &[u8]) {
        debug_assert_eq!(self.pos, 0, "a dictionary primes a fresh stream");
        let tail = &dictionary[dictionary.len().saturating_sub(WINDOW)..];
        self.hist[..tail.len()].copy_from_slice(tail);
        self.pos = tail.len();
    }

    /// One step: consume from `src`, write at most `dst.len().min(STEP_OUT)`
    /// bytes to `dst`. Returns (consumed, produced, ended): `ended` is true
    /// once the deflate stream (and for zlib its Adler-32) is complete.
    /// Running out of input is never an error here; the caller decides what
    /// an unfinished stream means.
    pub(crate) fn step(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
    ) -> Result<(usize, usize, bool), InflateFailure> {
        let room = dst.len().min(STEP_OUT);
        if self.pos + room > self.hist.len() {
            self.hist.copy_within(self.pos - WINDOW..self.pos, 0);
            self.pos = WINDOW;
        }
        let mut flags = TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_HAS_MORE_INPUT;
        flags |= if self.zlib {
            TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_COMPUTE_ADLER32
        } else {
            TINFL_FLAG_IGNORE_ADLER32
        };
        // A copy can reach before the start of the output only while the
        // stream has produced less than a window (`pos` is the whole output
        // until the first slide, and WINDOW or more after it). Only then is
        // the decoder state kept, to tell that failure from the others.
        let before = (self.pos < WINDOW).then(|| self.core.clone());
        let (status, consumed, produced) = decompress(
            &mut self.core,
            src,
            &mut self.hist[..self.pos + room],
            self.pos,
            flags,
        );
        match status {
            TINFLStatus::Done | TINFLStatus::NeedsMoreInput | TINFLStatus::HasMoreOutput => {
                let out = self.pos..self.pos + produced;
                dst[..produced].copy_from_slice(&self.hist[out]);
                self.pos += produced;
                Ok((consumed, produced, status == TINFLStatus::Done))
            }
            TINFLStatus::Adler32Mismatch => Err(InflateFailure::DataCheck),
            // Failed (a bad block, code, length or distance), and the two
            // statuses these flags rule out (BadParam, FailedCannotMakeProgress).
            _ => {
                let too_far = match before {
                    Some(mut core) => {
                        self.reaches_before_start(&mut core, src, room, flags, produced)
                    }
                    None => false,
                };
                Err(if too_far {
                    InflateFailure::TooFarBack
                } else {
                    InflateFailure::Invalid
                })
            }
        }
    }

    /// Whether a failed step failed on a copy from before the start of the
    /// output: run it again from the state `core` held before it, over the
    /// same history with a window of zeros in front. Such a copy then
    /// succeeds and the rerun gets past the point the step stopped at; any
    /// other failure stops the rerun at the same byte, as the zeros are only
    /// ever read by such a copy.
    fn reaches_before_start(
        &self,
        core: &mut DecompressorOxide,
        src: &[u8],
        room: usize,
        flags: u32,
        produced: usize,
    ) -> bool {
        let mut padded = vec![0u8; WINDOW + self.pos + room];
        padded[WINDOW..WINDOW + self.pos].copy_from_slice(&self.hist[..self.pos]);
        let (status, _, rerun) = decompress(core, src, &mut padded, WINDOW + self.pos, flags);
        status != TINFLStatus::Failed || rerun > produced
    }
}

// ---------------------------------------------------------------------------
// node:zlib
// ---------------------------------------------------------------------------

/// A node:zlib inflate failure: zlib's return code as node names it, its
/// number (`errno`) and the message node reports. Node raises these as a
/// plain `Error` with own `errno` and `code` (lib/zlib.js `zlibOnError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZlibError {
    pub code: &'static str,
    pub errno: i32,
    pub message: &'static str,
}

impl ZlibError {
    const fn data(message: &'static str) -> ZlibError {
        ZlibError {
            code: "Z_DATA_ERROR",
            errno: -3,
            message,
        }
    }

    /// The input ended inside the stream with a finishing flush: node_zlib.cc
    /// `CheckError`, Z_BUF_ERROR with output room left under Z_FINISH.
    pub const UNEXPECTED_EOF: ZlibError = ZlibError {
        code: "Z_BUF_ERROR",
        errno: -5,
        message: "unexpected end of file",
    };

    /// A zlib header asking for a preset dictionary that was not supplied
    /// (node_zlib.cc's message; zlib's inflate returns Z_NEED_DICT).
    pub const NEED_DICT: ZlibError = ZlibError {
        code: "Z_NEED_DICT",
        errno: 2,
        message: "Missing dictionary",
    };

    /// A zlib header asking for a preset dictionary whose Adler-32 is not
    /// the supplied one's (node_zlib.cc: inflateSetDictionary's Z_DATA_ERROR
    /// reported as Z_NEED_DICT with this message).
    pub const BAD_DICT: ZlibError = ZlibError {
        code: "Z_NEED_DICT",
        errno: 2,
        message: "Bad dictionary",
    };
}

impl std::fmt::Display for ZlibError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ZlibError {}

impl From<InflateFailure> for ZlibError {
    fn from(failure: InflateFailure) -> ZlibError {
        match failure {
            InflateFailure::DataCheck => ZlibError::data("incorrect data check"),
            InflateFailure::TooFarBack => ZlibError::data("invalid distance too far back"),
            // Not a zlib text: zlib names the defect (for example "invalid
            // block type"), which miniz does not report. The code is exact.
            InflateFailure::Invalid => ZlibError::data("invalid deflate data"),
        }
    }
}

/// Which wrapper a node:zlib inflate stream reads (node_zlib.cc's modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrap {
    /// GUNZIP: gzip members only.
    Gzip,
    /// INFLATE: the zlib wrapper.
    Zlib,
    /// INFLATERAW: no wrapper.
    Raw,
    /// UNZIP: gzip or zlib, by the first two bytes (zlib's windowBits + 32).
    Auto,
}

const GZ_FHCRC: u8 = 0x02;
const GZ_FEXTRA: u8 = 0x04;
const GZ_FNAME: u8 = 0x08;
const GZ_FCOMMENT: u8 = 0x10;
const GZ_RESERVED: u8 = 0xe0;
const ZLIB_FDICT: u8 = 0x20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// The first header byte (zlib CMF or gzip ID1). Held: zlib looks at the
    /// first two together (NEEDBITS(16)), so a stream that ends after one
    /// byte is truncated, not malformed.
    Head0,
    Head1,
    /// zlib DICTID: `n` of its 4 bytes seen.
    DictId(u8),
    /// gzip CM, checked together with FLG (zlib's NEEDBITS(16) again).
    GzCm,
    GzFlg,
    /// gzip MTIME (4), XFL, OS: `n` bytes still to skip.
    GzFixed(u8),
    GzXlenLo,
    GzXlenHi,
    GzExtra(u16),
    GzName,
    GzComment,
    GzHcrcLo,
    GzHcrcHi,
    Body,
    /// zlib's Adler-32, big-endian: `n` of 4 bytes seen.
    ZlibTrailer(u8),
    /// gzip CRC-32 then ISIZE, little-endian: `n` of 8 bytes seen.
    GzTrailer(u8),
    /// A gzip member ended. node_zlib.cc: while input remains and its next
    /// byte is not zero, reset and inflate it as another member (so trailing
    /// junk is a header error); a zero byte ends decoding there.
    Between,
    /// The stream is over; any further input is ignored, as zlib leaves it
    /// unconsumed and node drops it.
    Done,
}

/// The trailer check a stream's body feeds.
enum Check {
    None,
    Adler(u32),
    Crc(Crc),
}

/// One node:zlib inflate stream: `zlib.inflate*`, `gunzip*`, `unzip*`, their
/// stream classes and the low-level handle.
pub struct NodeInflate {
    wrap: Wrap,
    at: At,
    /// node_zlib.cc reads further gzip members (its `mode_ == GUNZIP`): set
    /// for Gzip, and for Auto once it has seen a gzip header.
    members: bool,
    inflate: Inflater,
    check: Check,
    /// CRC-32 of the gzip header, for FHCRC.
    header_crc: Crc,
    held: u8,
    flags: u8,
    xlen: u16,
    trailer: [u8; 8],
    /// node's `dictionary` option, kept for a zlib header that asks for one
    /// (FDICT). A raw stream is primed with it up front instead, and a gzip
    /// member never uses one, so neither keeps it.
    dictionary: Option<Box<[u8]>>,
}

impl NodeInflate {
    pub fn new(wrap: Wrap) -> NodeInflate {
        NodeInflate::with_dictionary(wrap, None)
    }

    /// An inflate stream with node's `dictionary` option, as node_zlib.cc
    /// uses it: a raw stream's window starts primed with it
    /// (`inflateSetDictionary` at init); a zlib stream uses it only when its
    /// header asks for a preset dictionary, and fails `Bad dictionary` when
    /// the header's Adler-32 is not this one's. An empty dictionary is none
    /// (node checks `dictionary_.empty()`).
    pub fn with_dictionary(wrap: Wrap, dictionary: Option<&[u8]>) -> NodeInflate {
        let dictionary = dictionary.filter(|d| !d.is_empty());
        let mut inflate = Inflater::new(false);
        let mut kept = None;
        match (wrap, dictionary) {
            (Wrap::Raw, Some(dictionary)) => inflate.prime(dictionary),
            (Wrap::Zlib | Wrap::Auto, Some(dictionary)) => kept = Some(dictionary.into()),
            _ => {}
        }
        NodeInflate {
            wrap,
            at: if wrap == Wrap::Raw {
                At::Body
            } else {
                At::Head0
            },
            members: wrap == Wrap::Gzip,
            inflate,
            check: Check::None,
            header_crc: Crc::new(),
            held: 0,
            flags: 0,
            xlen: 0,
            trailer: [0; 8],
            dictionary: kept,
        }
    }

    /// True once the stream is complete: what a finishing flush requires.
    pub fn is_complete(&self) -> bool {
        matches!(self.at, At::Between | At::Done)
    }

    /// `Err(UNEXPECTED_EOF)` unless the stream is complete: the check node
    /// makes when the input ends under Z_FINISH (one-shot calls, `end()`).
    pub fn finish(&self) -> Result<(), ZlibError> {
        if self.is_complete() {
            Ok(())
        } else {
            Err(ZlibError::UNEXPECTED_EOF)
        }
    }

    /// Decode from `src` into `dst`. Returns (consumed, produced); (0, 0)
    /// means nothing more is decodable until more input arrives. Header and
    /// trailer bytes are consumed in the same call as the body after them.
    pub fn step(&mut self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), ZlibError> {
        let mut consumed = 0;
        loop {
            match self.at {
                At::Done => return Ok((src.len(), 0)),
                At::Body => {
                    let (used, produced, ended) = self.inflate.step(&src[consumed..], dst)?;
                    consumed += used;
                    match &mut self.check {
                        Check::None => {}
                        Check::Adler(adler) => {
                            *adler = miniz_oxide::mz_adler32_oxide(*adler, &dst[..produced]);
                        }
                        Check::Crc(crc) => crc.update(&dst[..produced]),
                    }
                    if ended {
                        self.at = match self.check {
                            Check::None => At::Done,
                            Check::Adler(_) => At::ZlibTrailer(0),
                            Check::Crc(_) => At::GzTrailer(0),
                        };
                    }
                    if produced > 0 || !ended {
                        return Ok((consumed, produced));
                    }
                }
                At::Between => {
                    let Some(&b) = src.get(consumed) else { break };
                    if b == 0 {
                        self.at = At::Done;
                    } else {
                        self.at = At::Head0;
                        self.inflate.reset();
                        self.check = Check::None;
                        self.header_crc = Crc::new();
                    }
                }
                _ => {
                    let Some(&b) = src.get(consumed) else { break };
                    consumed += 1;
                    self.header_byte(b)?;
                }
            }
        }
        Ok((consumed, 0))
    }

    /// One header or trailer byte, checked in inflate.c's order.
    fn header_byte(&mut self, b: u8) -> Result<(), ZlibError> {
        if matches!(
            self.at,
            At::Head0
                | At::Head1
                | At::GzCm
                | At::GzFlg
                | At::GzFixed(_)
                | At::GzXlenLo
                | At::GzXlenHi
                | At::GzExtra(_)
                | At::GzName
                | At::GzComment
        ) {
            self.header_crc.update(&[b]);
        }
        self.at = match self.at {
            At::Head0 => {
                self.held = b;
                At::Head1
            }
            At::Head1 => {
                let gzip_ok = matches!(self.wrap, Wrap::Gzip | Wrap::Auto);
                if gzip_ok && self.held == 0x1f && b == 0x8b {
                    self.members = true;
                    self.check = Check::Crc(Crc::new());
                    At::GzCm
                } else if self.wrap == Wrap::Gzip {
                    return Err(ZlibError::data("incorrect header check"));
                } else {
                    self.zlib_header(self.held, b)?
                }
            }
            At::DictId(n) => {
                self.trailer[usize::from(n)] = b;
                if n < 3 {
                    At::DictId(n + 1)
                } else {
                    self.preset_dictionary()?
                }
            }
            At::GzCm => {
                self.held = b;
                At::GzFlg
            }
            At::GzFlg => {
                if self.held != 8 {
                    return Err(ZlibError::data("unknown compression method"));
                }
                if b & GZ_RESERVED != 0 {
                    return Err(ZlibError::data("unknown header flags set"));
                }
                self.flags = b;
                At::GzFixed(6)
            }
            At::GzFixed(n) if n > 1 => At::GzFixed(n - 1),
            At::GzFixed(_) if self.flags & GZ_FEXTRA != 0 => At::GzXlenLo,
            At::GzFixed(_) => self.after_extra(),
            At::GzXlenLo => {
                self.xlen = u16::from(b);
                At::GzXlenHi
            }
            At::GzXlenHi => {
                self.xlen |= u16::from(b) << 8;
                match self.xlen {
                    0 => self.after_extra(),
                    n => At::GzExtra(n),
                }
            }
            At::GzExtra(n) if n > 1 => At::GzExtra(n - 1),
            At::GzExtra(_) => self.after_extra(),
            At::GzName if b != 0 => At::GzName,
            At::GzName => self.after_name(),
            At::GzComment if b != 0 => At::GzComment,
            At::GzComment => self.after_comment(),
            At::GzHcrcLo => {
                self.held = b;
                At::GzHcrcHi
            }
            At::GzHcrcHi => {
                let stored = u16::from_le_bytes([self.held, b]);
                if u32::from(stored) != self.header_crc.sum() & 0xffff {
                    return Err(ZlibError::data("header crc mismatch"));
                }
                At::Body
            }
            At::ZlibTrailer(n) => {
                self.trailer[usize::from(n)] = b;
                if n < 3 {
                    At::ZlibTrailer(n + 1)
                } else {
                    let stored = u32::from_be_bytes([
                        self.trailer[0],
                        self.trailer[1],
                        self.trailer[2],
                        self.trailer[3],
                    ]);
                    if !matches!(self.check, Check::Adler(sum) if sum == stored) {
                        return Err(ZlibError::data("incorrect data check"));
                    }
                    self.after_stream()
                }
            }
            At::GzTrailer(n) => {
                self.trailer[usize::from(n)] = b;
                let Check::Crc(crc) = &self.check else {
                    unreachable!("a gzip member's body feeds a CRC")
                };
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
                        if stored != crc.sum() {
                            return Err(ZlibError::data("incorrect data check"));
                        }
                        At::GzTrailer(4)
                    }
                    8 => {
                        let stored = u32::from_le_bytes([
                            self.trailer[4],
                            self.trailer[5],
                            self.trailer[6],
                            self.trailer[7],
                        ]);
                        if stored != crc.amount() {
                            return Err(ZlibError::data("incorrect length check"));
                        }
                        self.after_stream()
                    }
                    n => At::GzTrailer(n),
                }
            }
            At::Body | At::Between | At::Done => unreachable!("not a header byte state"),
        };
        Ok(())
    }

    /// inflate.c HEAD for the zlib wrapper: the FCHECK remainder, then the
    /// method, then the window size, then FDICT.
    fn zlib_header(&mut self, cmf: u8, flg: u8) -> Result<At, ZlibError> {
        if (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
            return Err(ZlibError::data("incorrect header check"));
        }
        if cmf & 0x0f != 8 {
            return Err(ZlibError::data("unknown compression method"));
        }
        if (cmf >> 4) + 8 > 15 {
            return Err(ZlibError::data("invalid window size"));
        }
        if flg & ZLIB_FDICT != 0 {
            return Ok(At::DictId(0));
        }
        self.check = Check::Adler(1);
        Ok(At::Body)
    }

    /// inflate.c DICT, with node_zlib.cc's answer to its Z_NEED_DICT: the
    /// header's DICTID (in `trailer[..4]`) names the dictionary by its
    /// Adler-32. Without a dictionary that is "Missing dictionary"; with
    /// one whose Adler-32 differs, "Bad dictionary"; with the right one the
    /// window is primed with it and the body inflates. The data's own
    /// Adler-32 does not cover the dictionary.
    fn preset_dictionary(&mut self) -> Result<At, ZlibError> {
        let Some(dictionary) = self.dictionary.take() else {
            return Err(ZlibError::NEED_DICT);
        };
        let id = u32::from_be_bytes([
            self.trailer[0],
            self.trailer[1],
            self.trailer[2],
            self.trailer[3],
        ]);
        if miniz_oxide::mz_adler32_oxide(1, &dictionary) != id {
            return Err(ZlibError::BAD_DICT);
        }
        self.inflate.prime(&dictionary);
        self.check = Check::Adler(1);
        Ok(At::Body)
    }

    fn after_stream(&self) -> At {
        if self.members { At::Between } else { At::Done }
    }

    fn after_extra(&self) -> At {
        if self.flags & GZ_FNAME != 0 {
            At::GzName
        } else {
            self.after_name()
        }
    }

    fn after_name(&self) -> At {
        if self.flags & GZ_FCOMMENT != 0 {
            At::GzComment
        } else {
            self.after_comment()
        }
    }

    fn after_comment(&self) -> At {
        if self.flags & GZ_FHCRC != 0 {
            At::GzHcrcLo
        } else {
            At::Body
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeInflate, Wrap, ZlibError};
    use flate2::Compression;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Feed `input` in `size`-byte writes, then finish.
    fn run(wrap: Wrap, input: &[u8], size: usize) -> Result<Vec<u8>, ZlibError> {
        let mut dec = NodeInflate::new(wrap);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1000];
        for chunk in input.chunks(size.max(1)) {
            let mut off = 0;
            loop {
                let (used, produced) = dec.step(&chunk[off..], &mut buf)?;
                off += used;
                out.extend_from_slice(&buf[..produced]);
                if used == 0 && produced == 0 {
                    break;
                }
            }
        }
        dec.finish()?;
        Ok(out)
    }

    fn data_error(message: &'static str) -> Result<Vec<u8>, ZlibError> {
        Err(ZlibError {
            code: "Z_DATA_ERROR",
            errno: -3,
            message,
        })
    }

    /// Raw deflate for "a" then a 3-byte copy from distance 5: before the
    /// start of the output.
    const TOO_FAR: [u8; 4] = [0x4b, 0x04, 0x12, 0x00];

    #[test]
    fn a_copy_from_before_the_output_fails_in_every_wrapper() {
        let out = [0x61u8, 0, 0, 0];
        let mut z = vec![0x78, 0x9c];
        z.extend_from_slice(&TOO_FAR);
        z.extend_from_slice(&miniz_oxide::mz_adler32_oxide(1, &out).to_be_bytes());
        let mut g = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
        g.extend_from_slice(&TOO_FAR);
        let mut crc = flate2::Crc::new();
        crc.update(&out);
        g.extend_from_slice(&crc.sum().to_le_bytes());
        g.extend_from_slice(&4u32.to_le_bytes());
        let too_far = data_error("invalid distance too far back");
        assert_eq!(run(Wrap::Raw, &TOO_FAR, 4), too_far);
        assert_eq!(run(Wrap::Zlib, &z, z.len()), too_far);
        assert_eq!(run(Wrap::Gzip, &g, g.len()), too_far);
        assert_eq!(run(Wrap::Auto, &g, 1), too_far);
        // A second member cannot copy from the first.
        let mut two = gzip(b"hello");
        two.extend_from_slice(&g);
        assert_eq!(run(Wrap::Gzip, &two, two.len()), too_far);
    }

    #[test]
    fn a_long_stream_is_not_mistaken_for_one_reaching_too_far() {
        // Copies at the full window distance, over several slides.
        let plain: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        for size in [1, 777, 70_000] {
            assert_eq!(run(Wrap::Zlib, &zlib(&plain), size).unwrap(), plain);
            assert_eq!(run(Wrap::Gzip, &gzip(&plain), size).unwrap(), plain);
        }
    }

    #[test]
    fn gzip_members_decode_as_one_stream() {
        let mut two = gzip(b"hello");
        two.extend(gzip(b"world"));
        for size in [1, 2, 5, two.len()] {
            assert_eq!(run(Wrap::Gzip, &two, size).unwrap(), b"helloworld");
            assert_eq!(run(Wrap::Auto, &two, size).unwrap(), b"helloworld");
        }
        // A zero byte ends decoding; anything else must be a member.
        let mut padded = gzip(b"hello");
        padded.push(0);
        padded.extend(gzip(b"world"));
        assert_eq!(run(Wrap::Gzip, &padded, 3).unwrap(), b"hello");
        let mut junk = gzip(b"hello");
        junk.extend_from_slice(b"JUNK");
        assert_eq!(
            run(Wrap::Gzip, &junk, 3),
            data_error("incorrect header check")
        );
        let mut lone = gzip(b"hello");
        lone.push(0x1f);
        assert_eq!(run(Wrap::Gzip, &lone, 3), Err(ZlibError::UNEXPECTED_EOF));
    }

    #[test]
    fn zlib_and_raw_ignore_what_follows_the_stream() {
        let mut z = zlib(b"hello");
        z.extend_from_slice(b"JUNK");
        assert_eq!(run(Wrap::Zlib, &z, 2).unwrap(), b"hello");
        assert_eq!(run(Wrap::Auto, &z, 2).unwrap(), b"hello");
    }

    #[test]
    fn headers_fail_with_zlibs_messages() {
        let bad = |wrap, input: &[u8], message| {
            assert_eq!(
                run(wrap, input, input.len()),
                data_error(message),
                "{input:02x?}"
            );
        };
        bad(Wrap::Gzip, b"not gzip", "incorrect header check");
        bad(Wrap::Zlib, b"not zlib", "incorrect header check");
        // Valid FCHECK, so the next check is the one that fails.
        bad(Wrap::Zlib, &[0x77, 0x09], "unknown compression method");
        bad(Wrap::Zlib, &[0x88, 0x1c], "invalid window size");
        bad(
            Wrap::Gzip,
            &[0x1f, 0x8b, 7, 0],
            "unknown compression method",
        );
        bad(
            Wrap::Gzip,
            &[0x1f, 0x8b, 8, 0x20],
            "unknown header flags set",
        );
        assert_eq!(
            run(Wrap::Zlib, &[0x78, 0xbb, 0, 0, 0, 1], 6),
            Err(ZlibError::NEED_DICT)
        );
    }

    /// Feed `input` in `size`-byte writes to a stream made with `dictionary`.
    fn run_dict(
        wrap: Wrap,
        dictionary: &[u8],
        input: &[u8],
        size: usize,
    ) -> Result<Vec<u8>, ZlibError> {
        let mut dec = NodeInflate::with_dictionary(wrap, Some(dictionary));
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1000];
        for chunk in input.chunks(size.max(1)) {
            let mut off = 0;
            loop {
                let (used, produced) = dec.step(&chunk[off..], &mut buf)?;
                off += used;
                out.extend_from_slice(&buf[..produced]);
                if used == 0 && produced == 0 {
                    break;
                }
            }
        }
        dec.finish()?;
        Ok(out)
    }

    /// node v22.22.2's `deflateSync('hello world hello', { dictionary })`
    /// and `deflateRawSync` with the same dictionary: both copy from it.
    const DICT: &[u8] = b"hello world dictionary";
    const NODE_FDICT: [u8; 16] = [
        0x78, 0xbb, 0x62, 0x20, 0x08, 0xb3, 0xcb, 0x40, 0x12, 0x05, 0xb3, 0x01, 0x3b, 0x20, 0x06,
        0x91,
    ];
    const NODE_RAW_DICT: [u8; 6] = [0xcb, 0x40, 0x12, 0x05, 0xb3, 0x01];

    #[test]
    fn a_preset_dictionary_primes_the_window() {
        for size in [1, 3, NODE_FDICT.len()] {
            for wrap in [Wrap::Zlib, Wrap::Auto] {
                assert_eq!(
                    run_dict(wrap, DICT, &NODE_FDICT, size).unwrap(),
                    b"hello world hello"
                );
            }
            assert_eq!(
                run_dict(Wrap::Raw, DICT, &NODE_RAW_DICT, size).unwrap(),
                b"hello world hello"
            );
        }
        // A dictionary longer than the window: only its last 32 KiB is
        // history.
        let long: Vec<u8> = (0..70_000u32).map(|i| (i * 131 % 251) as u8).collect();
        let dec = NodeInflate::with_dictionary(Wrap::Raw, Some(&long));
        assert_eq!(dec.inflate.pos, super::WINDOW);
        assert_eq!(
            dec.inflate.hist[..super::WINDOW],
            long[long.len() - super::WINDOW..]
        );
    }

    #[test]
    fn a_preset_dictionary_fails_as_node_reports_it() {
        let bad = |wrap, dictionary: &[u8], input: &[u8], want: ZlibError| {
            assert_eq!(run_dict(wrap, dictionary, input, 2), Err(want), "{wrap:?}");
        };
        // No dictionary (an empty one is none), or one with another Adler-32.
        assert_eq!(run(Wrap::Zlib, &NODE_FDICT, 2), Err(ZlibError::NEED_DICT));
        bad(Wrap::Zlib, b"", &NODE_FDICT, ZlibError::NEED_DICT);
        bad(Wrap::Auto, b"", &NODE_FDICT, ZlibError::NEED_DICT);
        bad(Wrap::Zlib, b"nope", &NODE_FDICT, ZlibError::BAD_DICT);
        bad(Wrap::Auto, b"nope", &NODE_FDICT, ZlibError::BAD_DICT);
        // Checked as soon as DICTID is in, before the body arrives.
        bad(Wrap::Zlib, b"nope", &NODE_FDICT[..6], ZlibError::BAD_DICT);
        bad(
            Wrap::Zlib,
            DICT,
            &NODE_FDICT[..6],
            ZlibError::UNEXPECTED_EOF,
        );
        bad(
            Wrap::Zlib,
            DICT,
            &NODE_FDICT[..4],
            ZlibError::UNEXPECTED_EOF,
        );
        // A raw stream cannot tell: a short wrong dictionary leaves a copy
        // reaching before it, a long one is copied from.
        bad(
            Wrap::Raw,
            b"x",
            &NODE_RAW_DICT,
            data_error("invalid distance too far back").unwrap_err(),
        );
        assert_eq!(
            run_dict(Wrap::Raw, &[b'x'; 41], &NODE_RAW_DICT, 6).unwrap(),
            b"hxxxxxxxxxxxhxxxx"
        );
        assert_eq!(
            run(Wrap::Raw, &NODE_RAW_DICT, 6),
            data_error("invalid distance too far back")
        );
    }

    #[test]
    fn a_dictionary_nothing_asks_for_is_unused() {
        let plain = b"hello world hello";
        assert_eq!(run_dict(Wrap::Zlib, DICT, &zlib(plain), 3).unwrap(), plain);
        assert_eq!(run_dict(Wrap::Gzip, DICT, &gzip(plain), 3).unwrap(), plain);
        assert_eq!(run_dict(Wrap::Auto, DICT, &gzip(plain), 3).unwrap(), plain);
        // Not primed for a stream without FDICT: a copy before its start
        // still fails.
        let mut z = vec![0x78, 0x9c];
        z.extend_from_slice(&TOO_FAR);
        assert_eq!(
            run_dict(Wrap::Zlib, DICT, &z, z.len()),
            data_error("invalid distance too far back")
        );
    }

    #[test]
    fn trailers_are_checked() {
        let mut z = zlib(b"hello world");
        *z.last_mut().unwrap() ^= 1;
        assert_eq!(run(Wrap::Zlib, &z, 3), data_error("incorrect data check"));
        let g = gzip(b"hello world");
        let mut crc = g.clone();
        let at = crc.len() - 8;
        crc[at] ^= 1;
        assert_eq!(run(Wrap::Gzip, &crc, 3), data_error("incorrect data check"));
        let mut len = g.clone();
        let at = len.len() - 4;
        len[at] ^= 1;
        assert_eq!(
            run(Wrap::Gzip, &len, 3),
            data_error("incorrect length check")
        );
    }

    #[test]
    fn an_unfinished_stream_is_unexpected_end_of_file() {
        let z = zlib(b"hello world hello world");
        let g = gzip(b"hello world");
        for (wrap, input) in [
            (Wrap::Zlib, &z[..8]),
            (Wrap::Zlib, &z[..z.len() - 1]),
            (Wrap::Gzip, &g[..g.len() - 3]),
            (Wrap::Auto, &[0x1f][..]),
            (Wrap::Auto, &[0x78, 0x01][..]),
            (Wrap::Raw, &[][..]),
            (Wrap::Gzip, &[][..]),
            (Wrap::Zlib, &[0x78, 0xbb][..]),
        ] {
            assert_eq!(
                run(wrap, input, 1),
                Err(ZlibError::UNEXPECTED_EOF),
                "{wrap:?} {input:02x?}"
            );
        }
    }
}
