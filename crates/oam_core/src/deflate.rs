//! node:zlib's deflate and deflateRaw with the `dictionary` option.
//!
//! zlib's `deflateSetDictionary` fills the window with the dictionary before
//! the first input byte, so the data can copy from it, and a zlib stream
//! then sets FDICT and carries the dictionary's Adler-32 (DICTID). The
//! deflater oam uses everywhere else is flate2's over miniz_oxide, which has
//! no such call. [`DictDeflate`] gets the same window another way: it runs
//! the dictionary's last 32 KiB through the raw deflater under a sync flush
//! and drops that output. A sync flush ends on a byte boundary with no final
//! block, so the data's blocks that follow are a raw deflate stream of their
//! own, whose copies reach into the dictionary as zlib's do. The zlib header
//! and Adler-32 trailer are written here.
//!
//! The bytes are miniz's, not zlib's: they differ from node's as the
//! deflaters' bytes without a dictionary do (docs/node-divergences.md).
//! Without a dictionary nothing here runs.

use flate2::{Compress, Compression, FlushCompress, Status};

/// The deflate window: the farthest back a copy can reach, and so the most
/// of a dictionary that can matter (deflate.c `deflateSetDictionary` uses
/// the last `w_size` bytes of a longer one).
const WINDOW: usize = 32 * 1024;

/// A raw or zlib deflate stream primed with a dictionary.
pub struct DictDeflate {
    raw: Compress,
    /// For the zlib wrapper: the Adler-32 of the data so far.
    adler: Option<u32>,
    /// Header or trailer bytes not yet handed out, from `pending_at` on.
    pending: Vec<u8>,
    pending_at: usize,
    /// The raw stream has ended (and the trailer is in `pending`).
    ended: bool,
}

impl DictDeflate {
    /// `level` is node's (-1 is zlib's default, 6); `zlib` adds the zlib
    /// wrapper. `dictionary` must not be empty: an empty one is none, and
    /// the plain encoders serve that.
    pub fn new(level: i32, zlib: bool, dictionary: &[u8]) -> DictDeflate {
        debug_assert!(!dictionary.is_empty());
        let compression = if (0..=9).contains(&level) {
            Compression::new(level as u32)
        } else {
            Compression::default()
        };
        let mut raw = Compress::new(compression, false);
        let tail = &dictionary[dictionary.len().saturating_sub(WINDOW)..];
        // Stored blocks are the worst case: 5 bytes per 64 KiB block, plus
        // the sync flush's empty stored block.
        let mut sink = vec![0u8; tail.len() + 64];
        let mut fed = 0;
        loop {
            let before = (raw.total_in(), raw.total_out());
            raw.compress(&tail[fed..], &mut sink, FlushCompress::Sync)
                .expect("miniz raw deflate does not fail on valid parameters");
            fed += (raw.total_in() - before.0) as usize;
            let made = (raw.total_out() - before.1) as usize;
            if fed == tail.len() && made < sink.len() {
                break;
            }
        }
        let mut pending = Vec::new();
        let adler = zlib.then(|| {
            pending.extend_from_slice(&zlib_header(level));
            pending.extend_from_slice(&miniz_oxide::mz_adler32_oxide(1, dictionary).to_be_bytes());
            1
        });
        DictDeflate {
            raw,
            adler,
            pending,
            pending_at: 0,
            ended: false,
        }
    }

    /// Deflate from `input` into `output` under zlib's `flush` (0 none, 1
    /// partial, 2 sync, 3 full, 4 finish). Returns (consumed, produced).
    /// Under Z_FINISH the stream is complete once a call leaves room in
    /// `output`.
    pub fn deflate(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> (usize, usize) {
        let mut produced = self.drain(output);
        if self.ended || produced == output.len() {
            return (0, produced);
        }
        let flush = match flush {
            1 => FlushCompress::Partial,
            2 => FlushCompress::Sync,
            3 => FlushCompress::Full,
            4 => FlushCompress::Finish,
            _ => FlushCompress::None,
        };
        let before = (self.raw.total_in(), self.raw.total_out());
        let status = self
            .raw
            .compress(input, &mut output[produced..], flush)
            .expect("miniz raw deflate does not fail on valid parameters");
        let consumed = (self.raw.total_in() - before.0) as usize;
        produced += (self.raw.total_out() - before.1) as usize;
        if let Some(adler) = &mut self.adler {
            *adler = miniz_oxide::mz_adler32_oxide(*adler, &input[..consumed]);
        }
        if status == Status::StreamEnd {
            self.ended = true;
            if let Some(adler) = self.adler {
                self.pending.extend_from_slice(&adler.to_be_bytes());
            }
            produced += self.drain(&mut output[produced..]);
        }
        (consumed, produced)
    }

    /// All of `input`, deflated under `flush`, into a new buffer.
    pub fn deflate_vec(&mut self, mut input: &[u8], flush: i32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; crate::inflate::STEP_OUT];
        loop {
            let (consumed, produced) = self.deflate(input, &mut buf, flush);
            input = &input[consumed..];
            out.extend_from_slice(&buf[..produced]);
            if input.is_empty() && produced < buf.len() {
                return out;
            }
        }
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

/// zlib's header for a stream with a preset dictionary, as deflate.c writes
/// it for node: CMF for a 32 KiB window, FLEVEL from the level (zlib's
/// default strategy), FDICT, then FCHECK.
fn zlib_header(level: i32) -> [u8; 2] {
    let flevel: u16 = match level {
        0 | 1 => 0,
        2..=5 => 1,
        7..=9 => 3,
        _ => 2,
    };
    let mut header: u16 = 0x7800 | flevel << 6 | 0x20;
    header += 31 - header % 31;
    header.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::{DictDeflate, zlib_header};
    use crate::inflate::{NodeInflate, Wrap};

    fn inflate(wrap: Wrap, dictionary: &[u8], input: &[u8]) -> Vec<u8> {
        let mut dec = NodeInflate::with_dictionary(wrap, Some(dictionary));
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
            assert_eq!(zlib_header(level), [0x78, flg], "level {level}");
        }
    }

    #[test]
    fn small_input_matches_nodes_bytes() {
        // node v22.22.2: deflateSync / deflateRawSync('hello world hello',
        // { dictionary: 'hello world dictionary' }).
        let dict = b"hello world dictionary";
        let data = b"hello world hello";
        let zlib = DictDeflate::new(-1, true, dict).deflate_vec(data, 4);
        assert_eq!(
            zlib,
            [
                0x78, 0xbb, 0x62, 0x20, 0x08, 0xb3, 0xcb, 0x40, 0x12, 0x05, 0xb3, 0x01, 0x3b, 0x20,
                0x06, 0x91
            ]
        );
        let raw = DictDeflate::new(-1, false, dict).deflate_vec(data, 4);
        assert_eq!(raw, [0xcb, 0x40, 0x12, 0x05, 0xb3, 0x01]);
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
        let plain = crate::zlib::compress(&data, crate::zlib::Format::DeflateRaw, 6).unwrap();
        for level in [-1, 0, 1, 6, 9] {
            let raw = DictDeflate::new(level, false, &dict).deflate_vec(&data, 4);
            assert_eq!(inflate(Wrap::Raw, &dict, &raw), data, "raw {level}");
            let zlib = DictDeflate::new(level, true, &dict).deflate_vec(&data, 4);
            assert_eq!(zlib[1] & 0x20, 0x20);
            assert_eq!(inflate(Wrap::Zlib, &dict, &zlib), data, "zlib {level}");
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
        let mut enc = DictDeflate::new(6, true, dict);
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        let mut input = &data[..];
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
            input = &input[chunk.len()..];
        }
        assert!(input.is_empty());
        loop {
            let (_, made) = enc.deflate(&[], &mut buf, 4);
            out.extend_from_slice(&buf[..made]);
            if made < buf.len() {
                break;
            }
        }
        assert_eq!(inflate(Wrap::Zlib, dict, &out), data);
    }
}
