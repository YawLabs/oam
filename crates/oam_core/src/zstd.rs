//! node:zlib's zstd engine: the native half of node v22.22.2's
//! `ZstdCompress` / `ZstdDecompress` binding (src/node_zlib.cc `ZstdStream`,
//! `ZstdCompressContext`, `ZstdDecompressContext`), over libzstd 1.5.7 -- the
//! library node links -- through its safe binding `zstd-safe`.
//!
//! The shape is node's binding, not a convenience codec: one [`ZstdHandle::write`]
//! is exactly one `ZSTD_compressStream2` or `ZSTD_decompressStream` call into
//! an output window of the size the caller gives, reporting what it consumed
//! and produced. The input is the one place the shape differs: node's binding
//! reads the caller's chunk in place on every call, while this one holds its
//! own copy ([`ZstdHandle::set_input`]), taken once per chunk, and each call
//! reads on from where the last one stopped. Copying the unconsumed rest on
//! every call instead would be quadratic in the chunk's size, because node's
//! loop calls once per output window (16 KiB by default). node's JavaScript (lib/zlib.js `processChunkSync` and
//! `processCallback`) decides from those two numbers whether to call again, so
//! node's observable quirks fall out of running the same loop over the same
//! library rather than being special-cased here:
//!
//! - a one-shot decompress returns only the FIRST frame, and ignores what
//!   follows it, because `ZSTD_decompressStream` returns at a frame's end and
//!   the loop only calls again when the output window filled;
//! - input that stops inside a frame is not an error (nothing checks for an
//!   unfinished frame), so a truncated frame decodes to what it held;
//! - every byte of compressed output is libzstd's own, at the same level and
//!   parameters, so it is byte-identical to node's.
//!
//! Errors are node's: a codec failure carries `ZSTD_getErrorCode` as `errno`,
//! node's `ZSTD_error_*` name for it as `code`, and libzstd's message; an
//! initialisation failure is the message node throws as
//! `ERR_ZLIB_INITIALIZATION_FAILED`.

use zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd_safe::{CCtx, CParameter, DCtx, DParameter, FrameFormat, InBuffer, OutBuffer, Strategy};

/// A codec failure, as node reports it (lib/zlib.js `zlibOnError` gives the
/// `Error` this message, then own `errno` and `code`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZstdError {
    /// `ZSTD_getErrorCode` of the failure: zstd_errors.h's `ZSTD_ErrorCode`.
    pub errno: i32,
    /// node's name for it (node_zlib.cc `ZstdStrerror`).
    pub code: &'static str,
    /// libzstd's message for it (`ZSTD_getErrorString`).
    pub message: &'static str,
}

impl ZstdError {
    /// The error a libzstd return code that `ZSTD_isError` holds stands for.
    /// zstd_errors.h defines `ZSTD_getErrorCode(code)` as `0 - code`, so the
    /// enum value is computed here with the same wrapping arithmetic instead
    /// of through FFI.
    fn from_code(code: usize) -> Self {
        let errno = 0usize.wrapping_sub(code) as i32;
        Self {
            errno,
            code: error_name(errno),
            message: zstd_safe::get_error_name(code),
        }
    }

    /// The error for a `ZSTD_ErrorCode` value (zstd_errors.h), as libzstd
    /// would return it.
    fn from_errno(errno: i32) -> Self {
        Self::from_code(0usize.wrapping_sub(errno as usize))
    }
}

impl std::fmt::Display for ZstdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ZstdError {}

/// node_zlib.cc's `ZstdStrerror`: the `ZSTD_ERROR_CODES` list node names (the
/// same list `zlib.constants` publishes), "ZSTD_error_GENERIC" for any other.
fn error_name(errno: i32) -> &'static str {
    match errno {
        0 => "ZSTD_error_no_error",
        10 => "ZSTD_error_prefix_unknown",
        12 => "ZSTD_error_version_unsupported",
        14 => "ZSTD_error_frameParameter_unsupported",
        16 => "ZSTD_error_frameParameter_windowTooLarge",
        20 => "ZSTD_error_corruption_detected",
        22 => "ZSTD_error_checksum_wrong",
        24 => "ZSTD_error_literals_headerWrong",
        30 => "ZSTD_error_dictionary_corrupted",
        32 => "ZSTD_error_dictionary_wrong",
        34 => "ZSTD_error_dictionaryCreation_failed",
        40 => "ZSTD_error_parameter_unsupported",
        41 => "ZSTD_error_parameter_combination_unsupported",
        42 => "ZSTD_error_parameter_outOfBound",
        44 => "ZSTD_error_tableLog_tooLarge",
        46 => "ZSTD_error_maxSymbolValue_tooLarge",
        48 => "ZSTD_error_maxSymbolValue_tooSmall",
        50 => "ZSTD_error_stabilityCondition_notRespected",
        60 => "ZSTD_error_stage_wrong",
        62 => "ZSTD_error_init_missing",
        64 => "ZSTD_error_memory_allocation",
        66 => "ZSTD_error_workSpace_tooSmall",
        70 => "ZSTD_error_dstSize_tooSmall",
        72 => "ZSTD_error_srcSize_wrong",
        74 => "ZSTD_error_dstBuffer_null",
        80 => "ZSTD_error_noForwardProgress_destFull",
        82 => "ZSTD_error_noForwardProgress_inputEmpty",
        _ => "ZSTD_error_GENERIC",
    }
}

/// zstd_errors.h `ZSTD_error_parameter_outOfBound`: what libzstd answers an
/// end directive outside `ZSTD_e_continue..=ZSTD_e_end` with.
const PARAMETER_OUT_OF_BOUND: i32 = 42;

/// node_zlib.cc's messages for a handle that cannot be set up. node throws
/// each as `ERR_ZLIB_INITIALIZATION_FAILED` with the message as it is.
pub const PARAM_SET_FAILED: &str = "Setting parameter failed";
pub const DICTIONARY_LOAD_FAILED: &str = "Failed to load zstd dictionary";
pub const PLEDGED_SIZE_FAILED: &str = "Could not set pledged src size";

/// The codec context: libzstd's compression or decompression context.
enum Context {
    Compress(CCtx<'static>),
    Decompress(DCtx<'static>),
}

/// One node zstd handle (node_zlib.cc `ZstdStream`): a context, set up as
/// node's `init()` sets it up, and the pledged source size its `reset()`
/// sets up again.
pub struct ZstdHandle {
    context: Context,
    pledged_src_size: Option<u64>,
    /// The chunk being worked through ([`Self::set_input`]) and how much of
    /// it libzstd has consumed so far.
    input: Vec<u8>,
    input_pos: usize,
}

impl ZstdHandle {
    /// node's `init(params, pledgedSrcSize, ..., dictionary)`: a fresh context,
    /// the dictionary (if any), the pledged source size (compression only),
    /// then each parameter in key order. `params` holds the set entries of
    /// node's `initParamsArray` -- (key, value) with the value as the int32
    /// node's binding passes -- in ascending key order. The `Err` is node's
    /// message for the step that failed.
    pub fn new(
        compress: bool,
        pledged_src_size: Option<u64>,
        dictionary: Option<&[u8]>,
        params: &[(u32, i32)],
    ) -> Result<Self, &'static str> {
        let mut handle = Self {
            context: init_context(compress, pledged_src_size, dictionary)?,
            pledged_src_size,
            input: Vec::new(),
            input_pos: 0,
        };
        for &(key, value) in params {
            handle.set_parameter(key, value)?;
        }
        Ok(handle)
    }

    /// node's `reset()` (`ZstdCompressContext::ResetStream` /
    /// `ZstdDecompressContext::ResetStream`): a brand-new context, given only
    /// the pledged source size. As in node, the parameters and the dictionary
    /// `init()` set do not survive it.
    pub fn reset(&mut self) -> Result<(), &'static str> {
        let compress = matches!(self.context, Context::Compress(_));
        self.context = init_context(compress, self.pledged_src_size, None)?;
        self.set_input(Vec::new());
        Ok(())
    }

    /// Start on a new input chunk: the following [`Self::write`] calls read
    /// it from its start, each going on where the one before stopped (what
    /// node's loop does when it passes the same chunk back at `inOff +
    /// consumed`). The handle owns the bytes so a chunk is copied out of
    /// JavaScript once, not once per call.
    pub fn set_input(&mut self, input: Vec<u8>) {
        self.input = input;
        self.input_pos = 0;
    }

    /// `ZSTD_CCtx_setParameter` / `ZSTD_DCtx_setParameter(key, value)`, failing
    /// with node's message. The safe binding takes typed parameters, so each
    /// key node can pass is mapped to its variant with `value` carried through
    /// unchanged where the binding passes it on as the same `int`
    /// (`as u32` round-trips through its `as c_int`). Two kinds of key need
    /// more than that:
    ///
    /// - the flags (`contentSizeFlag`, `checksumFlag`, `dictIDFlag`) are
    ///   `bool` in the binding; libzstd stores `value != 0` for them, so that
    ///   is what is passed;
    /// - `strategy` is an enum: libzstd takes 0 as "the level's own" (what an
    ///   unset parameter already is) and refuses anything outside 1..=9.
    ///
    /// `ZSTD_c_format` (10) is the one experimental libzstd compression
    /// parameter inside node's key range (`kMaxZstdCParam`, 402): 0 is the
    /// standard frame, 1 the magicless one, and libzstd's bound check refuses
    /// anything else. (`ZSTD_d_format` is 1000, above node's decompression
    /// range, so a decompressor never sees it.)
    ///
    /// `enableLongDistanceMatching` is libzstd's three-way switch (0 auto,
    /// 1 enable, 2 disable) but a `bool` in the binding; 2 is accepted, as
    /// libzstd accepts it, and set as 0 (docs/node-divergences.md).
    /// A key libzstd has no parameter for fails, as libzstd fails it.
    fn set_parameter(&mut self, key: u32, value: i32) -> Result<(), &'static str> {
        let result = match &mut self.context {
            Context::Compress(cctx) => {
                let param = match (key, value) {
                    (10, 0) => CParameter::Format(FrameFormat::One),
                    (10, 1) => CParameter::Format(FrameFormat::Magicless),
                    (10, _) => return Err(PARAM_SET_FAILED),
                    (100, v) => CParameter::CompressionLevel(v),
                    (101, v) => CParameter::WindowLog(v as u32),
                    (102, v) => CParameter::HashLog(v as u32),
                    (103, v) => CParameter::ChainLog(v as u32),
                    (104, v) => CParameter::SearchLog(v as u32),
                    (105, v) => CParameter::MinMatch(v as u32),
                    (106, v) => CParameter::TargetLength(v as u32),
                    (107, 0) => return Ok(()),
                    (107, v) => CParameter::Strategy(strategy(v).ok_or(PARAM_SET_FAILED)?),
                    (130, v) => CParameter::TargetCBlockSize(v as u32),
                    (160, 0 | 2) => CParameter::EnableLongDistanceMatching(false),
                    (160, 1) => CParameter::EnableLongDistanceMatching(true),
                    (160, _) => return Err(PARAM_SET_FAILED),
                    (161, v) => CParameter::LdmHashLog(v as u32),
                    (162, v) => CParameter::LdmMinMatch(v as u32),
                    (163, v) => CParameter::LdmBucketSizeLog(v as u32),
                    (164, v) => CParameter::LdmHashRateLog(v as u32),
                    (200, v) => CParameter::ContentSizeFlag(v != 0),
                    (201, v) => CParameter::ChecksumFlag(v != 0),
                    (202, v) => CParameter::DictIdFlag(v != 0),
                    (400, v) => CParameter::NbWorkers(v as u32),
                    (401, v) => CParameter::JobSize(v as u32),
                    (402, v) => CParameter::OverlapSizeLog(v as u32),
                    _ => return Err(PARAM_SET_FAILED),
                };
                cctx.set_parameter(param)
            }
            Context::Decompress(dctx) => match key {
                100 => dctx.set_parameter(DParameter::WindowLogMax(value as u32)),
                _ => return Err(PARAM_SET_FAILED),
            },
        };
        result.map(drop).map_err(|_| PARAM_SET_FAILED)
    }

    /// One `ZSTD_compressStream2(flush)` or `ZSTD_decompressStream` call
    /// (node_zlib.cc `DoThreadPoolWork`) over what is left of the input
    /// [`Self::set_input`] gave, into an output window of `out_len` bytes:
    /// `(bytes of input consumed, the bytes produced)`. `flush` is the end
    /// directive (`ZSTD_e_*`) for a compressor and ignored by a decompressor,
    /// as node's binding ignores it.
    pub fn write(&mut self, flush: i32, out_len: usize) -> Result<(usize, Vec<u8>), ZstdError> {
        let mut out = vec![0u8; out_len];
        let mut src = InBuffer::around(&self.input[self.input_pos..]);
        let (result, produced) = {
            let mut dst = OutBuffer::around(out.as_mut_slice());
            let result = match &mut self.context {
                Context::Compress(cctx) => match end_directive(flush) {
                    Some(directive) => cctx.compress_stream2(&mut dst, &mut src, directive),
                    None => return Err(ZstdError::from_errno(PARAMETER_OUT_OF_BOUND)),
                },
                Context::Decompress(dctx) => dctx.decompress_stream(&mut dst, &mut src),
            };
            (result, dst.pos())
        };
        result.map_err(ZstdError::from_code)?;
        out.truncate(produced);
        let consumed = src.pos();
        self.input_pos += consumed;
        // A chunk libzstd has taken all of is not read again (libzstd keeps
        // what it still needs in its own window), so it is let go now rather
        // than held until the next chunk replaces it.
        if self.input_pos == self.input.len() {
            self.set_input(Vec::new());
        }
        Ok((consumed, out))
    }
}

/// node's `Init(pledged_src_size, dictionary)` for either context.
fn init_context(
    compress: bool,
    pledged_src_size: Option<u64>,
    dictionary: Option<&[u8]>,
) -> Result<Context, &'static str> {
    // node loads a dictionary only when it is not empty.
    let dictionary = dictionary.filter(|d| !d.is_empty());
    if compress {
        let mut cctx = CCtx::create();
        if let Some(dictionary) = dictionary {
            cctx.load_dictionary(dictionary)
                .map_err(|_| DICTIONARY_LOAD_FAILED)?;
        }
        cctx.set_pledged_src_size(pledged_src_size)
            .map_err(|_| PLEDGED_SIZE_FAILED)?;
        Ok(Context::Compress(cctx))
    } else {
        let mut dctx = DCtx::create();
        if let Some(dictionary) = dictionary {
            dctx.load_dictionary(dictionary)
                .map_err(|_| DICTIONARY_LOAD_FAILED)?;
        }
        Ok(Context::Decompress(dctx))
    }
}

/// `ZSTD_strategy` for its value (zstd.h: ZSTD_fast = 1 .. ZSTD_btultra2 = 9).
fn strategy(value: i32) -> Option<Strategy> {
    Some(match value {
        1 => Strategy::ZSTD_fast,
        2 => Strategy::ZSTD_dfast,
        3 => Strategy::ZSTD_greedy,
        4 => Strategy::ZSTD_lazy,
        5 => Strategy::ZSTD_lazy2,
        6 => Strategy::ZSTD_btlazy2,
        7 => Strategy::ZSTD_btopt,
        8 => Strategy::ZSTD_btultra,
        9 => Strategy::ZSTD_btultra2,
        _ => return None,
    })
}

/// `ZSTD_EndDirective` for its value (zstd.h: continue 0, flush 1, end 2).
fn end_directive(value: i32) -> Option<ZSTD_EndDirective> {
    Some(match value {
        0 => ZSTD_EndDirective::ZSTD_e_continue,
        1 => ZSTD_EndDirective::ZSTD_e_flush,
        2 => ZSTD_EndDirective::ZSTD_e_end,
        _ => return None,
    })
}

// `ZstdHandle` crosses to the blocking pool for the async writes; zstd-safe
// declares its contexts `Send` (each is used by one thread at a time, which
// the registry's lock guarantees). Proven here rather than asserted.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<ZstdHandle>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// node's `processChunkSync` loop over one input under one flush, at
    /// node's default chunk size: what a one-shot call returns.
    fn one_shot(handle: &mut ZstdHandle, input: &[u8], flush: i32) -> Result<Vec<u8>, ZstdError> {
        const CHUNK: usize = 16 * 1024;
        let mut out = Vec::new();
        handle.set_input(input.to_vec());
        let mut offset = 0;
        loop {
            let avail = CHUNK - offset;
            let (_, made) = handle.write(flush, avail)?;
            offset += made.len();
            let full = made.len() == avail;
            out.extend(made);
            if full || offset >= CHUNK {
                offset = 0;
            }
            if !full {
                return Ok(out);
            }
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn compresses_to_nodes_bytes() {
        // node v22.22.2: zlib.zstdCompressSync('hello hello hello') and ('').
        let mut c = ZstdHandle::new(true, None, None, &[]).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"hello hello hello", 2).unwrap()),
            "28b52ffd20116500003068656c6c6f200100314a11"
        );
        let mut c = ZstdHandle::new(true, None, None, &[]).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"", 2).unwrap()),
            "28b52ffd2000010000"
        );
        // With ZSTD_c_checksumFlag (201) set to 5: libzstd stores `!= 0`.
        let mut c = ZstdHandle::new(true, None, None, &[(201, 5)]).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"x", 2).unwrap()),
            "28b52ffd24010900007823110483"
        );
    }

    #[test]
    fn decodes_the_first_frame_and_a_truncation_without_error() {
        // node v22.22.2: zlib.zstdCompressSync('a') and ('b').
        let frame_a = unhex("28b52ffd200109000061");
        let frame_b = unhex("28b52ffd200109000062");
        let both = [frame_a.clone(), frame_b].concat();
        let mut d = ZstdHandle::new(false, None, None, &[]).unwrap();
        assert_eq!(one_shot(&mut d, &both, 2).unwrap(), b"a");
        let mut d = ZstdHandle::new(false, None, None, &[]).unwrap();
        assert_eq!(
            one_shot(&mut d, &frame_a[..frame_a.len() - 1], 2).unwrap(),
            b""
        );
    }

    #[test]
    fn fails_as_node_reports() {
        let mut d = ZstdHandle::new(false, None, None, &[]).unwrap();
        let err = one_shot(&mut d, b"garbage!", 2).unwrap_err();
        assert_eq!(
            (err.errno, err.code, err.message),
            (10, "ZSTD_error_prefix_unknown", "Unknown frame descriptor")
        );
        let mut c = ZstdHandle::new(true, None, None, &[]).unwrap();
        c.set_input(b"x".to_vec());
        let err = c.write(4, 64).unwrap_err();
        assert_eq!(
            (err.errno, err.code, err.message),
            (
                42,
                "ZSTD_error_parameter_outOfBound",
                "Parameter is out of bound"
            )
        );
        for (compress, params) in [
            (true, &[(5u32, 1i32)][..]),
            (true, &[(107, 10)]),
            (true, &[(160, 3)]),
            (true, &[(10, 2)]),
            (false, &[(5, 1)]),
            (false, &[(100, 9)]),
        ] {
            assert_eq!(
                ZstdHandle::new(compress, None, None, params).err(),
                Some(PARAM_SET_FAILED),
                "{compress} {params:?}"
            );
        }
        // Accepted as node (multithreaded libzstd) accepts them.
        for params in [
            &[(400u32, 2i32)][..],
            &[(401, 1)],
            &[(402, 5)],
            &[(160, 2)],
            &[(107, 0)],
        ] {
            assert!(
                ZstdHandle::new(true, None, None, params).is_ok(),
                "{params:?}"
            );
        }
    }

    #[test]
    fn writes_a_magicless_frame_as_node_does() {
        // node v22.22.2: zlib.zstdCompressSync('hi', { params: { 10: v } })
        // for v = 0 and v = 1 (ZSTD_c_format: standard, magicless).
        let mut c = ZstdHandle::new(true, None, None, &[(10, 0)]).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"hi", 2).unwrap()),
            "28b52ffd20021100006869"
        );
        let mut c = ZstdHandle::new(true, None, None, &[(10, 1)]).unwrap();
        assert_eq!(hex(&one_shot(&mut c, b"hi", 2).unwrap()), "20021100006869");
    }

    #[test]
    fn reads_a_chunk_on_across_calls() {
        // One chunk worked through in small output windows: each call goes on
        // where the last stopped, and the round trip gives the input back.
        let input: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let mut c = ZstdHandle::new(true, None, None, &[]).unwrap();
        c.set_input(input.clone());
        let (mut frame, mut taken) = (Vec::new(), 0);
        loop {
            let (used, made) = c.write(2, 64).unwrap();
            taken += used;
            let full = made.len() == 64;
            frame.extend(made);
            if !full {
                break;
            }
        }
        assert_eq!(taken, input.len());
        let mut d = ZstdHandle::new(false, None, None, &[]).unwrap();
        d.set_input(frame);
        let mut back = Vec::new();
        loop {
            let (_, made) = d.write(0, 1000).unwrap();
            let full = made.len() == 1000;
            back.extend(made);
            if !full {
                break;
            }
        }
        assert_eq!(back, input);
    }

    #[test]
    fn a_wrong_pledged_size_fails_a_streamed_frame() {
        // node: createZstdCompress({ pledgedSrcSize: 3 }) written 'hel' then
        // 'lo' errors ZSTD_error_srcSize_wrong (72) "Src size is incorrect".
        let mut c = ZstdHandle::new(true, Some(3), None, &[]).unwrap();
        c.set_input(b"hel".to_vec());
        c.write(0, 16384).unwrap();
        c.set_input(b"lo".to_vec());
        let err = c.write(2, 16384).unwrap_err();
        assert_eq!(
            (err.errno, err.code, err.message),
            (72, "ZSTD_error_srcSize_wrong", "Src size is incorrect")
        );
    }

    #[test]
    fn reset_drops_the_parameters() {
        // node: a checksummed stream reset() writes its next frame without one.
        let mut c = ZstdHandle::new(true, None, None, &[(201, 1)]).unwrap();
        c.reset().unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"def", 2).unwrap()),
            "28b52ffd2003190000646566"
        );
    }
}
