//! node:zlib's deflaters: gzip, zlib and raw deflate, with the `dictionary`
//! option and a level that can change mid-stream (`params()`).
//!
//! [`NodeDeflate`] drives miniz_oxide's compressor directly. flate2's
//! encoders (which served node:zlib until `params()` landed) wrap the same
//! compressor but keep it private, and their miniz backend has no
//! `set_level`; miniz itself supports a level change after compression has
//! started (`CompressorOxide::set_compression_level_raw`). The deflate data
//! is the one flate2 wrote, and the gzip header and trailer are written here
//! as flate2's `GzEncoder` writes them (a unit test holds the two
//! byte-for-byte).
//!
//! The zlib wrapper is written here too, as zlib writes it, not miniz's.
//! miniz's header follows its own idea of the level: at level 0 it declares
//! a 256-byte window (`08 1d`, CINFO 0), which held for a stream of stored
//! blocks; once `params()` raises the level mid-stream the data copies from
//! up to 32 KiB back, and an inflater that sizes its window from the header
//! (node's, given any options object) fails it with "invalid distance too
//! far back". zlib always declares the 32 KiB window, with FLEVEL from the
//! level the stream starts at: `78 01`, `78 5e`, `78 9c` or `78 da`.
//!
//! The `dictionary` option: zlib's `deflateSetDictionary` fills the window
//! with the dictionary before the first input byte, so the data can copy
//! from it, and a zlib stream then sets FDICT and carries the dictionary's
//! Adler-32 (DICTID). miniz has no such call. [`NodeDeflate`] gets the same
//! window another way: it runs the dictionary's last 32 KiB through the raw
//! deflater under a sync flush and drops that output. A sync flush ends on a
//! byte boundary with no final block, so the data's blocks that follow are a
//! raw deflate stream of their own, whose copies reach into the dictionary
//! as zlib's do. The zlib header and Adler-32 trailer are then written here.
//!
//! The bytes are miniz's, not zlib's: they differ from node's
//! (docs/node-divergences.md).

use crate::zlib::Format;
use flate2::Crc;
use miniz_oxide::deflate::core::CompressorOxide;
use miniz_oxide::deflate::stream::deflate as miniz_deflate;
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

/// The deflate window: the farthest back a copy can reach, and so the most
/// of a dictionary that can matter (deflate.c `deflateSetDictionary` uses
/// the last `w_size` bytes of a longer one).
const WINDOW: usize = 32 * 1024;

/// zlib's `Z_SYNC_FLUSH` and `Z_FINISH`.
const Z_SYNC_FLUSH: i32 = 2;
const Z_FINISH: i32 = 4;

/// What follows the raw deflate data, computed here as the data goes by.
enum Trailer {
    /// Raw deflate.
    None,
    /// A zlib stream: the Adler-32 of the data so far.
    Adler(u32),
    /// A gzip member: the CRC-32 and length of the data so far.
    Gzip(Crc),
}

/// A gzip, zlib or raw deflate stream, as node's `Gzip`, `Deflate` and
/// `DeflateRaw` (and their one-shot forms and handles) write it.
pub struct NodeDeflate {
    core: Box<CompressorOxide>,
    trailer: Trailer,
    /// Header or trailer bytes not yet handed out, from `pending_at` on.
    pending: Vec<u8>,
    pending_at: usize,
    /// The deflate data has ended (and the trailer is in `pending`).
    ended: bool,
    /// [`Self::deflate_vec`]'s output step, kept for the stream's life so a
    /// streamed write does not allocate one per chunk.
    scratch: Vec<u8>,
}

/// miniz's level for node's: -1 (Z_DEFAULT_COMPRESSION) is zlib's default,
/// 6, as is anything outside 0..=9 (the JS layer rejects those first).
fn miniz_level(level: i32) -> u8 {
    u8::try_from(level)
        .ok()
        .filter(|level| *level <= 9)
        .unwrap_or(6)
}

impl NodeDeflate {
    /// A deflater for `format` at node's `level`. `dictionary` is node's
    /// option: used by a zlib or raw stream when not empty, ignored by gzip
    /// (as node's zlib ignores it there).
    pub fn new(format: Format, level: i32, dictionary: Option<&[u8]>) -> NodeDeflate {
        let level = miniz_level(level);
        let dictionary = dictionary.filter(|d| !d.is_empty() && format != Format::Gzip);
        let mut pending = Vec::new();
        // The core always writes raw deflate; the wrappers are written here.
        let trailer = match format {
            Format::Deflate => {
                pending.extend_from_slice(&zlib_header(level, dictionary.is_some()));
                if let Some(dictionary) = dictionary {
                    pending.extend_from_slice(
                        &miniz_oxide::mz_adler32_oxide(1, dictionary).to_be_bytes(),
                    );
                }
                Trailer::Adler(1)
            }
            Format::DeflateRaw => Trailer::None,
            Format::Gzip => {
                pending.extend_from_slice(&gzip_header(level));
                Trailer::Gzip(Crc::new())
            }
        };
        let mut core = Box::<CompressorOxide>::default();
        core.set_format_and_level(DataFormat::Raw, level);
        if let Some(dictionary) = dictionary {
            prime(
                &mut core,
                &dictionary[dictionary.len().saturating_sub(WINDOW)..],
            );
        }
        NodeDeflate {
            core,
            trailer,
            pending,
            pending_at: 0,
            ended: false,
            scratch: Vec::new(),
        }
    }

    /// Deflate from `input` into `output` under zlib's `flush` (0 none, 1
    /// partial, 2 sync, 3 full, 4 finish; 5, Z_BLOCK, is read as none).
    /// Returns (consumed, produced). Under Z_FINISH the stream is complete
    /// once a call leaves room in `output`.
    pub fn deflate(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> (usize, usize) {
        let mut produced = self.drain(output);
        if self.ended || produced == output.len() {
            return (0, produced);
        }
        let flush = match flush {
            1 => MZFlush::Partial,
            2 => MZFlush::Sync,
            3 => MZFlush::Full,
            4 => MZFlush::Finish,
            _ => MZFlush::None,
        };
        let result = miniz_deflate(&mut self.core, input, &mut output[produced..], flush);
        let consumed = result.bytes_consumed;
        produced += result.bytes_written;
        let data = &input[..consumed];
        match &mut self.trailer {
            Trailer::None => {}
            Trailer::Adler(adler) => *adler = miniz_oxide::mz_adler32_oxide(*adler, data),
            Trailer::Gzip(crc) => crc.update(data),
        }
        // A buffer error is "no progress possible" (nothing to do, or no
        // room), which the caller sees as nothing consumed or produced;
        // miniz reports nothing else for a raw or zlib stream on valid
        // parameters.
        if result.status == Ok(MZStatus::StreamEnd) {
            self.ended = true;
            match &self.trailer {
                Trailer::None => {}
                Trailer::Adler(adler) => self.pending.extend_from_slice(&adler.to_be_bytes()),
                Trailer::Gzip(crc) => {
                    self.pending.extend_from_slice(&crc.sum().to_le_bytes());
                    self.pending.extend_from_slice(&crc.amount().to_le_bytes());
                }
            }
            produced += self.drain(&mut output[produced..]);
        }
        (consumed, produced)
    }

    /// All of `input`, deflated under `flush`, into a new buffer.
    pub fn deflate_vec(&mut self, mut input: &[u8], flush: i32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = std::mem::take(&mut self.scratch);
        if buf.is_empty() {
            buf = vec![0u8; crate::inflate::STEP_OUT];
        }
        loop {
            let (consumed, produced) = self.deflate(input, &mut buf, flush);
            input = &input[consumed..];
            out.extend_from_slice(&buf[..produced]);
            if input.is_empty() && produced < buf.len() {
                break;
            }
        }
        self.scratch = buf;
        out
    }

    /// Node's `params(level, ...)` on a deflate stream: what was written so
    /// far, compressed at the old level under a sync flush (node's
    /// `flush(Z_SYNC_FLUSH)` before `handle.params`), then the new level, if
    /// there is one, for what follows (node's binding changes it on a
    /// deflate or deflateRaw stream only, so a gzip stream just flushes).
    /// Returns the flushed bytes.
    pub fn params(&mut self, level: Option<i32>) -> Vec<u8> {
        let flushed = self.deflate_vec(&[], Z_SYNC_FLUSH);
        if let Some(level) = level {
            self.core.set_compression_level_raw(miniz_level(level));
        }
        flushed
    }

    /// The whole stream for `input`: deflated and finished.
    pub fn finish_vec(&mut self, input: &[u8]) -> Vec<u8> {
        self.deflate_vec(input, Z_FINISH)
    }

    /// Copy out what is left of the header or trailer.
    fn drain(&mut self, output: &mut [u8]) -> usize {
        let left = &self.pending[self.pending_at..];
        let n = left.len().min(output.len());
        output[..n].copy_from_slice(&left[..n]);
        self.pending_at += n;
        n
    }
}

/// Run `tail` (at most a window of the dictionary) through the raw
/// deflater under a sync flush and drop the output: the window then holds
/// it, and the data's copies can reach into it.
fn prime(core: &mut CompressorOxide, tail: &[u8]) {
    // Stored blocks are the worst case: 5 bytes per 64 KiB block, plus the
    // sync flush's empty stored block.
    let mut sink = vec![0u8; tail.len() + 64];
    let mut fed = 0;
    loop {
        let result = miniz_deflate(core, &tail[fed..], &mut sink, MZFlush::Sync);
        fed += result.bytes_consumed;
        if fed == tail.len() && result.bytes_written < sink.len() {
            break;
        }
    }
}

/// The gzip member header flate2's `GzEncoder` writes: no name, comment,
/// extra field or mtime, XFL from the level (2 for the best, 4 for the
/// fastest), and OS 255 (unknown).
fn gzip_header(level: u8) -> [u8; 10] {
    let xfl = match level {
        9.. => 2,
        0 | 1 => 4,
        _ => 0,
    };
    [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 255]
}

/// zlib's stream header, as deflate.c writes it for node: CMF for a 32 KiB
/// window, FLEVEL from the level the stream starts at (zlib's default
/// strategy), FDICT when there is a preset dictionary, then FCHECK.
fn zlib_header(level: u8, fdict: bool) -> [u8; 2] {
    let flevel: u16 = match level {
        0 | 1 => 0,
        2..=5 => 1,
        7..=9 => 3,
        _ => 2,
    };
    let mut header: u16 = 0x7800 | flevel << 6 | if fdict { 0x20 } else { 0 };
    header += 31 - header % 31;
    header.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::{NodeDeflate, miniz_level, zlib_header};
    use crate::inflate::{NodeInflate, Wrap};
    use crate::zlib::Format;
    use std::io::Write;

    fn inflate(wrap: Wrap, dictionary: Option<&[u8]>, input: &[u8]) -> Vec<u8> {
        let mut dec = NodeInflate::with_dictionary(wrap, dictionary);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        let mut input = input;
        loop {
            let (used, produced) = dec.step(input, &mut buf).unwrap();
            input = &input[used..];
            out.extend_from_slice(&buf[..produced]);
            if used == 0 && produced == 0 {
                break;
            }
        }
        dec.finish().unwrap();
        out
    }

    fn dict_deflate(level: i32, zlib: bool, dictionary: &[u8], data: &[u8]) -> Vec<u8> {
        let format = if zlib {
            Format::Deflate
        } else {
            Format::DeflateRaw
        };
        NodeDeflate::new(format, level, Some(dictionary)).finish_vec(data)
    }

    /// Text-like bytes that compress, and noise that does not.
    fn samples() -> Vec<Vec<u8>> {
        let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog; "
            .iter()
            .cycle()
            .take(200_000)
            .copied()
            .collect();
        let mut seed = 0x2545_f491u32;
        let noise: Vec<u8> = (0..70_000)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
            .collect();
        vec![Vec::new(), b"x".to_vec(), text, noise]
    }

    /// flate2's encoder for `format` at node's `level`, the deflater
    /// node:zlib used before [`NodeDeflate`].
    fn flate2_bytes(format: Format, level: i32, data: &[u8], chunk: usize) -> Vec<u8> {
        let level = flate2::Compression::new(u32::from(miniz_level(level)));
        fn run<W: Write>(mut enc: W, data: &[u8], chunk: usize) -> W {
            for piece in data.chunks(chunk.max(1)) {
                enc.write_all(piece).unwrap();
            }
            enc
        }
        match format {
            Format::Gzip => run(
                flate2::write::GzEncoder::new(Vec::new(), level),
                data,
                chunk,
            )
            .finish()
            .unwrap(),
            Format::Deflate => run(
                flate2::write::ZlibEncoder::new(Vec::new(), level),
                data,
                chunk,
            )
            .finish()
            .unwrap(),
            Format::DeflateRaw => run(
                flate2::write::DeflateEncoder::new(Vec::new(), level),
                data,
                chunk,
            )
            .finish()
            .unwrap(),
        }
    }

    #[test]
    fn the_bytes_are_flate2s_for_every_format_level_and_chunking() {
        for data in samples() {
            for format in [Format::Gzip, Format::Deflate, Format::DeflateRaw] {
                for level in -1..=9 {
                    let mut want = flate2_bytes(format, level, &data, data.len());
                    // The zlib header is zlib's, not miniz's (which flate2
                    // writes); the data and the Adler-32 are the same.
                    if format == Format::Deflate {
                        want[..2].copy_from_slice(&zlib_header(miniz_level(level), false));
                    }
                    let whole = NodeDeflate::new(format, level, None).finish_vec(&data);
                    assert_eq!(whole, want, "{format:?} level {level} len {}", data.len());
                    // Written in pieces, as a stream is.
                    for chunk in [1usize, 7, 4096, 70_000] {
                        if chunk > data.len() {
                            continue;
                        }
                        let mut enc = NodeDeflate::new(format, level, None);
                        let mut out = Vec::new();
                        for piece in data.chunks(chunk) {
                            out.extend(enc.deflate_vec(piece, 0));
                        }
                        out.extend(enc.finish_vec(&[]));
                        assert_eq!(out, want, "{format:?} level {level} chunk {chunk}");
                    }
                }
            }
        }
    }

    #[test]
    fn params_flushes_then_compresses_at_the_new_level() {
        let text: Vec<u8> = b"hello hello hello hello world "
            .iter()
            .cycle()
            .take(6000)
            .copied()
            .collect();
        for format in [Format::Gzip, Format::Deflate, Format::DeflateRaw] {
            let wrap = match format {
                Format::Gzip => Wrap::Gzip,
                Format::Deflate => Wrap::Zlib,
                Format::DeflateRaw => Wrap::Raw,
            };
            for (from, to) in [(9, 0), (0, 9), (1, 9), (-1, -1), (6, 1)] {
                let mut enc = NodeDeflate::new(format, from, None);
                let mut out = enc.deflate_vec(&text, 0);
                let flushed = enc.params(Some(to));
                // A sync flush ends with an empty stored block.
                assert!(
                    flushed.ends_with(&[0, 0, 0xff, 0xff]),
                    "{format:?} {from}->{to}"
                );
                out.extend(flushed);
                // Everything written so far inflates from the flushed bytes.
                assert_eq!(inflate_partial(wrap, &out), text, "{format:?} {from}->{to}");
                let before = out.len();
                out.extend(enc.finish_vec(&text));
                let second = out.len() - before;
                // Level 0 stores: the second half is no smaller than the text.
                assert_eq!(
                    second >= text.len(),
                    to == 0,
                    "{format:?} {from}->{to}: {second}"
                );
                let mut both = text.clone();
                both.extend_from_slice(&text);
                assert_eq!(inflate(wrap, None, &out), both, "{format:?} {from}->{to}");
            }
        }
        // With a dictionary, too.
        let dict = b"hello world dictionary";
        let mut enc = NodeDeflate::new(Format::Deflate, 1, Some(dict));
        let mut out = enc.deflate_vec(&text, 0);
        out.extend(enc.params(Some(9)));
        out.extend(enc.finish_vec(&text));
        let mut both = text.clone();
        both.extend_from_slice(&text);
        assert_eq!(inflate(Wrap::Zlib, Some(dict), &out), both);
        // No level (node's gzip stream): the flush, and the level stays.
        let mut enc = NodeDeflate::new(Format::Gzip, 0, None);
        let mut out = enc.deflate_vec(&text, 0);
        let flushed = enc.params(None);
        assert!(flushed.ends_with(&[0, 0, 0xff, 0xff]));
        out.extend(flushed);
        let before = out.len();
        out.extend(enc.finish_vec(&text));
        assert!(out.len() - before >= text.len(), "still stored");
        assert_eq!(inflate(Wrap::Gzip, None, &out), both);
    }

    /// What a stream that has not ended inflates to so far.
    fn inflate_partial(wrap: Wrap, input: &[u8]) -> Vec<u8> {
        let mut dec = NodeInflate::with_dictionary(wrap, None);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        let mut input = input;
        loop {
            let (used, produced) = dec.step(input, &mut buf).unwrap();
            input = &input[used..];
            out.extend_from_slice(&buf[..produced]);
            if used == 0 && produced == 0 {
                return out;
            }
        }
    }

    #[test]
    fn the_header_is_zlibs_for_each_level() {
        // node v22.22.2's deflateSync(data, { dictionary, level }).
        for (level, flg) in [
            (-1, 0xbb),
            (0, 0x3f),
            (1, 0x3f),
            (2, 0x7d),
            (5, 0x7d),
            (6, 0xbb),
            (7, 0xf9),
            (9, 0xf9),
        ] {
            assert_eq!(
                zlib_header(miniz_level(level), true),
                [0x78, flg],
                "level {level}"
            );
        }
        // node v22.22.2's deflateSync('abc', { level }), and the stream's
        // first two bytes.
        for (level, flg) in [
            (-1, 0x9c),
            (0, 0x01),
            (1, 0x01),
            (2, 0x5e),
            (3, 0x5e),
            (4, 0x5e),
            (5, 0x5e),
            (6, 0x9c),
            (7, 0xda),
            (8, 0xda),
            (9, 0xda),
        ] {
            assert_eq!(
                zlib_header(miniz_level(level), false),
                [0x78, flg],
                "level {level}"
            );
            let out = NodeDeflate::new(Format::Deflate, level, None).finish_vec(b"abc");
            assert_eq!(out[..2], [0x78, flg], "stream at level {level}");
        }
    }

    #[test]
    fn a_stream_raised_from_level_0_declares_the_32_kib_window_it_uses() {
        // node v22.22.2: createDeflate({ level: 0 }), params(6), then 100 KB
        // that repeats every 1000 bytes; the header is the level-0 one, 78 01
        // (CINFO 7, a 32 KiB window). miniz's own level-0 header, 08 1d
        // (CINFO 0, 256 bytes), made node's inflateSync(out, {}) fail
        // "invalid distance too far back" on the copies params() allows.
        let unit: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let data: Vec<u8> = unit.iter().cycle().take(100_000).copied().collect();
        let mut enc = NodeDeflate::new(Format::Deflate, 0, None);
        let mut out = enc.params(Some(6));
        out.extend(enc.finish_vec(&data));
        assert_eq!(out[..2], [0x78, 0x01]);
        // CINFO is the window's log2 less 8: every copy fits in it.
        assert_eq!(out[0] >> 4, 7);
        assert!(out.len() < data.len() / 10, "compressed: {}", out.len());
        assert_eq!(inflate(Wrap::Zlib, None, &out), data);
    }

    #[test]
    fn small_input_matches_nodes_bytes() {
        // node v22.22.2: deflateSync / deflateRawSync('hello world hello',
        // { dictionary: 'hello world dictionary' }).
        let dict = b"hello world dictionary";
        let data = b"hello world hello";
        let zlib = dict_deflate(-1, true, dict, data);
        assert_eq!(
            zlib,
            [
                0x78, 0xbb, 0x62, 0x20, 0x08, 0xb3, 0xcb, 0x40, 0x12, 0x05, 0xb3, 0x01, 0x3b, 0x20,
                0x06, 0x91
            ]
        );
        let raw = dict_deflate(-1, false, dict, data);
        assert_eq!(raw, [0xcb, 0x40, 0x12, 0x05, 0xb3, 0x01]);
        // gzip takes the option and ignores it, as node's zlib does.
        assert_eq!(
            NodeDeflate::new(Format::Gzip, -1, Some(dict)).finish_vec(data),
            NodeDeflate::new(Format::Gzip, -1, None).finish_vec(data)
        );
    }

    #[test]
    fn output_copies_from_the_dictionary_and_round_trips() {
        // Bytes that do not compress on their own, so only copies from the
        // dictionary can shrink the data.
        let mut seed = 0x2545_f491u32;
        let dict: Vec<u8> = (0..70_000)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            })
            .collect();
        // Copies from 30000 bytes back (neither zlib nor miniz reaches the
        // full 32 KiB: zlib stops 262 bytes short of it), then from near.
        let data: Vec<u8> = dict[dict.len() - 30_000..dict.len() - 20_000]
            .iter()
            .chain(&dict[dict.len() - 4000..])
            .copied()
            .collect();
        let plain = crate::zlib::compress(&data, Format::DeflateRaw, 6).unwrap();
        for level in [-1, 0, 1, 6, 9] {
            let raw = dict_deflate(level, false, &dict, &data);
            assert_eq!(inflate(Wrap::Raw, Some(&dict), &raw), data, "raw {level}");
            let zlib = dict_deflate(level, true, &dict, &data);
            assert_eq!(zlib[1] & 0x20, 0x20);
            assert_eq!(
                inflate(Wrap::Zlib, Some(&dict), &zlib),
                data,
                "zlib {level}"
            );
            // Level 0 stores; miniz's level 1 keeps one position per hash
            // slot, which 30000 bytes of noise have overwritten.
            if matches!(level, -1 | 6 | 9) {
                assert!(
                    raw.len() * 10 < plain.len(),
                    "level {level}: {} vs {}",
                    raw.len(),
                    plain.len()
                );
            }
        }
    }

    #[test]
    fn small_outputs_and_flushes_lose_nothing() {
        let dict = b"the quick brown fox jumps over the lazy dog";
        let data: Vec<u8> = dict.iter().cycle().take(5000).copied().collect();
        for (format, wrap) in [(Format::Deflate, Wrap::Zlib), (Format::Gzip, Wrap::Gzip)] {
            let dictionary = (format == Format::Deflate).then_some(&dict[..]);
            let mut enc = NodeDeflate::new(format, 6, dictionary);
            let mut out = Vec::new();
            let mut buf = [0u8; 3];
            // Write in chunks with a sync flush between, through a 3-byte
            // output, then finish.
            for chunk in data.chunks(777) {
                let mut left = chunk;
                loop {
                    let (used, made) = enc.deflate(left, &mut buf, 2);
                    left = &left[used..];
                    out.extend_from_slice(&buf[..made]);
                    if left.is_empty() && made < buf.len() {
                        break;
                    }
                }
            }
            loop {
                let (_, made) = enc.deflate(&[], &mut buf, 4);
                out.extend_from_slice(&buf[..made]);
                if made < buf.len() {
                    break;
                }
            }
            assert_eq!(inflate(wrap, dictionary, &out), data, "{format:?}");
        }
    }
}
