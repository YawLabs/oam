//! node:zlib's zstd engine: the native half of node v26.13.1's
//! `ZstdCompress` / `ZstdDecompress` binding (src/node_zlib.cc `ZstdStream`,
//! `ZstdCompressContext`, `ZstdDecompressContext`), over libzstd 1.5.7 -- the
//! library node links -- through its safe binding `zstd-safe`.
//!
//! The shape is node's binding, not a convenience codec: one [`ZstdHandle::write`]
//! is node's `DoThreadPoolWork` followed by its `GetErrorInfo`, into an output
//! window of the size the caller gives, reporting what it consumed and
//! produced. The input is the one place the shape differs: node's binding
//! reads the caller's chunk in place on every call, while this one holds its
//! own copy ([`ZstdHandle::set_input`]), taken once per chunk, and each call
//! reads on from where the last one stopped. Copying the unconsumed rest on
//! every call instead would be quadratic in the chunk's size, because node's
//! loop calls once per output window (16 KiB by default). node's JavaScript
//! (lib/zlib.js `processChunkSync` and `processCallback`) decides from those
//! two numbers whether to call again, and the binding's own bookkeeping --
//! ported here as node 26 keeps it -- decides the rest:
//!
//! - a compressor counts the input of a frame whose size was pledged and
//!   fails the frame that ends at another size, as libzstd alone does not
//!   when the first call already ends the frame;
//! - a decompressor goes on into the next frame (or skippable frame) after
//!   one ends, ignores -- or, asked to, refuses -- input that starts neither,
//!   and fails a stream that ends inside a frame with zlib's "unexpected end
//!   of file" (v22's binding stopped after the first frame and decoded a
//!   truncated one to what it held);
//! - `reset()` starts a new session on the same context, keeping its
//!   parameters and dictionary, and refuses a frame that is half written;
//! - every byte of compressed output is libzstd's own, at the same level and
//!   parameters, so it is byte-identical to node's.
//!
//! Errors are node's: a codec failure carries `ZSTD_getErrorCode` as `errno`,
//! node's `ZSTD_error_*` name for it as `code`, and libzstd's message; an
//! initialisation failure is the message node throws as
//! `ERR_ZLIB_INITIALIZATION_FAILED`.

use zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd_safe::{
    CCtx, CParameter, DCtx, DParameter, FrameFormat, InBuffer, OutBuffer, ResetDirective, Strategy,
};

/// A codec failure, as node reports it (lib/zlib.js `zlibOnError` gives the
/// `Error` this message, then own `errno` and `code`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZstdError {
    /// `ZSTD_getErrorCode` of the failure: zstd_errors.h's `ZSTD_ErrorCode`,
    /// or the zlib/node number node's binding reports for its own failures.
    pub errno: i32,
    /// node's name for it (node_zlib.cc `ZstdStrerror`, or the binding's
    /// own code).
    pub code: &'static str,
    /// libzstd's message for it (`ZSTD_getErrorString`), or the binding's.
    pub message: &'static str,
}

impl ZstdError {
    /// The error a libzstd return code that `ZSTD_isError` holds stands for.
    /// zstd_errors.h defines `ZSTD_getErrorCode(code)` as `0 - code`, so the
    /// enum value is computed here with the same wrapping arithmetic instead
    /// of through FFI.
    pub(crate) fn from_code(code: usize) -> Self {
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
/// zstd_errors.h `ZSTD_error_srcSize_wrong`.
const SRC_SIZE_WRONG: i32 = 72;
/// zstd_errors.h `ZSTD_error_GENERIC`.
const GENERIC: i32 = 1;
/// zstd_errors.h `ZSTD_error_stage_wrong`.
const STAGE_WRONG: i32 = 60;
/// zlib.h `Z_BUF_ERROR`.
const Z_BUF_ERROR: i32 = -5;
/// `ZSTD_e_end`'s value.
const E_END: i32 = 2;

/// node_zlib.cc's messages for a handle that cannot be set up. node throws
/// each as `ERR_ZLIB_INITIALIZATION_FAILED` with the message as it is.
pub const PARAM_SET_FAILED: &str = "Setting parameter failed";
pub const DICTIONARY_LOAD_FAILED: &str = "Failed to load zstd dictionary";
pub const PLEDGED_SIZE_FAILED: &str = "Could not set pledged src size";

/// The decompressor's refusal of input after a frame that starts no other
/// frame (`rejectGarbageAfterEnd`): DoThreadPoolWork's, with libzstd's
/// generic error number.
const TRAILING_JUNK_IN_WORK: ZstdError = ZstdError {
    errno: GENERIC,
    code: "ERR_TRAILING_JUNK_AFTER_STREAM_END",
    message: "Trailing junk found after the end of the compressed stream",
};
/// The same refusal when the stream ends inside such a prefix
/// (GetErrorInfo's, with -1).
const TRAILING_JUNK_AT_END: ZstdError = ZstdError {
    errno: -1,
    code: "ERR_TRAILING_JUNK_AFTER_STREAM_END",
    message: "Trailing junk found after the end of the compressed stream",
};
/// A stream that ends inside a frame (GetErrorInfo's zlib error).
const UNEXPECTED_END: ZstdError = ZstdError {
    errno: Z_BUF_ERROR,
    code: "Z_BUF_ERROR",
    message: "unexpected end of file",
};
/// ZstdCompressContext::ResetStream's refusal of a half-written frame.
const INCOMPLETE_FRAME: ZstdError = ZstdError {
    errno: STAGE_WRONG,
    code: "ERR_ZLIB_INCOMPLETE_FRAME",
    message: "Cannot reset a zstd stream with an incomplete frame; end the frame \
              or discard the output produced so far",
};

/// The zstd frame magic, and a skippable frame's (its first byte's low four
/// bits vary).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const SKIPPABLE_MAGIC: [u8; 4] = [0x50, 0x2a, 0x4d, 0x18];

/// ZstdCompressContext's frame bookkeeping.
struct CompressState {
    /// Input taken into the current frame, counted only while a size is
    /// pledged for it (`consumed_src_size_`).
    consumed_src_size: Option<u64>,
    /// The last frame ended (`ZSTD_e_end` returned 0), or none was begun.
    frame_complete: bool,
    /// The current, unfinished frame has written output.
    frame_output_emitted: bool,
}

/// ZstdDecompressContext's frame bookkeeping.
struct DecompressState {
    frame_complete: bool,
    decoding_frame_after_complete: bool,
    reject_garbage_after_end: bool,
    ignoring_trailing_input: bool,
    frame_prefix_size: usize,
    /// Bit 0: the prefix can still be a zstd frame; bit 1: a skippable one.
    possible_frame_types: u8,
}

impl DecompressState {
    fn new(reject_garbage_after_end: bool) -> Self {
        Self {
            frame_complete: false,
            decoding_frame_after_complete: false,
            reject_garbage_after_end,
            ignoring_trailing_input: false,
            frame_prefix_size: 0,
            possible_frame_types: 0,
        }
    }
}

/// The codec context: libzstd's compression or decompression context, with
/// node's bookkeeping for it.
enum Context {
    Compress(CCtx<'static>, CompressState),
    Decompress(DCtx<'static>, DecompressState),
}

/// One node zstd handle (node_zlib.cc `ZstdStream`): a context, set up as
/// node's `init()` sets it up, and the pledged source size its `reset()`
/// pledges again.
pub struct ZstdHandle {
    context: Context,
    pledged_src_size: Option<u64>,
    /// The chunk being worked through ([`Self::set_input`]) and how much of
    /// it libzstd has consumed so far.
    input: Vec<u8>,
    input_pos: usize,
}

impl ZstdHandle {
    /// node's `init(params, pledgedSrcSize, ..., dictionary,
    /// rejectGarbageAfterEnd)`: a fresh context, the dictionary (if any), the
    /// pledged source size (compression only), then each parameter in key
    /// order. `params` holds the set entries of node's `initParamsArray` --
    /// (key, value) with the value as the int32 node's binding passes -- in
    /// ascending key order. `reject_garbage_after_end` is a decompressor's.
    /// The `Err` is node's message for the step that failed.
    pub fn new(
        compress: bool,
        pledged_src_size: Option<u64>,
        dictionary: Option<&[u8]>,
        params: &[(u32, i32)],
        reject_garbage_after_end: bool,
    ) -> Result<Self, &'static str> {
        let mut handle = Self {
            context: init_context(
                compress,
                pledged_src_size,
                dictionary,
                reject_garbage_after_end,
            )?,
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
    /// `ZstdDecompressContext::ResetStream`): a new session on the same
    /// context (`ZSTD_reset_session_only`), so the parameters and the
    /// dictionary `init()` set survive it, and a compressor pledges the same
    /// source size again. A compressor whose current frame has already
    /// written output refuses (those bytes cannot be taken back).
    pub fn reset(&mut self) -> Result<(), ZstdError> {
        let pledged = self.pledged_src_size;
        match &mut self.context {
            Context::Compress(cctx, state) => {
                if !state.frame_complete && state.frame_output_emitted {
                    return Err(INCOMPLETE_FRAME);
                }
                cctx.reset(ResetDirective::SessionOnly)
                    .map_err(ZstdError::from_code)?;
                cctx.set_pledged_src_size(pledged)
                    .map_err(ZstdError::from_code)?;
                state.consumed_src_size = pledged.map(|_| 0);
                state.frame_complete = true;
                state.frame_output_emitted = false;
            }
            Context::Decompress(dctx, state) => {
                dctx.reset(ResetDirective::SessionOnly)
                    .map_err(ZstdError::from_code)?;
                *state = DecompressState::new(state.reject_garbage_after_end);
            }
        }
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
            Context::Compress(cctx, _) => {
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
            Context::Decompress(dctx, _) => match key {
                100 => dctx.set_parameter(DParameter::WindowLogMax(value as u32)),
                _ => return Err(PARAM_SET_FAILED),
            },
        };
        result.map(drop).map_err(|_| PARAM_SET_FAILED)
    }

    /// node's `DoThreadPoolWork` then `GetErrorInfo`, over what is left of
    /// the input [`Self::set_input`] gave, into an output window of `out_len`
    /// bytes: `(bytes of input consumed, the bytes produced)`, or the error
    /// node's binding would emit (and then the output is not delivered, as
    /// node delivers none). `flush` is the end directive (`ZSTD_e_*`): a
    /// compressor's for `ZSTD_compressStream2`, and for a decompressor the
    /// sign that the stream ends with this input (`ZSTD_e_end`).
    pub fn write(&mut self, flush: i32, out_len: usize) -> Result<(usize, Vec<u8>), ZstdError> {
        let mut out = vec![0u8; out_len];
        let pledged = self.pledged_src_size;
        let remaining = &self.input[self.input_pos..];
        let in_size = remaining.len();
        let mut src = InBuffer::around(remaining);
        let mut dst = OutBuffer::around(out.as_mut_slice());
        let outcome = match &mut self.context {
            Context::Compress(cctx, state) => {
                compress_work(cctx, state, pledged, &mut src, &mut dst, flush)
            }
            Context::Decompress(dctx, state) => {
                let worked = decompress_work(dctx, state, &mut src, &mut dst);
                worked.and_then(|()| {
                    decompress_error_info(state, flush, src.pos() == in_size, dst.pos() < out_len)
                })
            }
        };
        let (consumed, produced) = (src.pos(), dst.pos());
        outcome?;
        out.truncate(produced);
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

/// ZstdCompressContext::DoThreadPoolWork: one `ZSTD_compressStream2`, and the
/// frame bookkeeping. libzstd overrides a pledged size when the call that
/// begins a frame already ends it, so the frame's input is counted here and a
/// frame that ends at another size than the pledge fails.
fn compress_work(
    cctx: &mut CCtx<'static>,
    state: &mut CompressState,
    pledged: Option<u64>,
    src: &mut InBuffer<'_>,
    dst: &mut OutBuffer<'_, [u8]>,
    flush: i32,
) -> Result<(), ZstdError> {
    let remaining = match end_directive(flush) {
        Some(directive) => cctx.compress_stream2(dst, src, directive),
        None => Err(0usize.wrapping_sub(PARAMETER_OUT_OF_BOUND as usize)),
    };
    if let Some(consumed) = state.consumed_src_size.as_mut() {
        *consumed += src.pos() as u64;
    }
    if dst.pos() > 0 {
        state.frame_output_emitted = true;
    }
    match remaining {
        Err(code) => {
            state.frame_complete = false;
            Err(ZstdError::from_code(code))
        }
        Ok(0) if flush == E_END => {
            state.frame_complete = true;
            state.frame_output_emitted = false;
            match state.consumed_src_size.take() {
                Some(consumed) if Some(consumed) != pledged => {
                    Err(ZstdError::from_errno(SRC_SIZE_WRONG))
                }
                _ => Ok(()),
            }
        }
        Ok(_) => {
            state.frame_complete = false;
            Ok(())
        }
    }
}

/// ZstdDecompressContext::DoThreadPoolWork: `ZSTD_decompressStream` until
/// the output window fills or the input runs out, going on into the next
/// frame after one ends. Input after a finished frame that begins neither a
/// zstd nor a skippable frame is ignored from there on -- or refused, with
/// `rejectGarbageAfterEnd`.
fn decompress_work(
    dctx: &mut DCtx<'static>,
    state: &mut DecompressState,
    src: &mut InBuffer<'_>,
    dst: &mut OutBuffer<'_, [u8]>,
) -> Result<(), ZstdError> {
    if state.ignoring_trailing_input {
        return Ok(());
    }
    // node's loop calls again with no input when the last call filled the
    // output window; that is not the start of a new, unfinished frame.
    if state.frame_complete && src.src.is_empty() {
        return Ok(());
    }
    loop {
        if state.frame_complete {
            state.decoding_frame_after_complete = true;
            state.frame_prefix_size = 0;
            state.possible_frame_types = 0b11;
        }
        if state.decoding_frame_after_complete && state.frame_prefix_size < 4 {
            let mut offset = 0;
            while state.frame_prefix_size < 4 && src.pos() + offset < src.src.len() {
                let index = state.frame_prefix_size;
                let byte = src.src[src.pos() + offset];
                if byte != ZSTD_MAGIC[index] {
                    state.possible_frame_types &= !0b01;
                }
                if (index == 0 && (byte & 0xf0) != SKIPPABLE_MAGIC[0])
                    || (index != 0 && byte != SKIPPABLE_MAGIC[index])
                {
                    state.possible_frame_types &= !0b10;
                }
                state.frame_prefix_size += 1;
                offset += 1;
            }
            if state.possible_frame_types == 0 {
                state.frame_complete = true;
                state.decoding_frame_after_complete = false;
                if state.reject_garbage_after_end {
                    return Err(TRAILING_JUNK_IN_WORK);
                }
                state.ignoring_trailing_input = true;
                return Ok(());
            }
        }
        match dctx.decompress_stream(dst, src) {
            Err(code) => {
                state.frame_complete = false;
                return Err(ZstdError::from_code(code));
            }
            Ok(hint) => {
                state.frame_complete = hint == 0;
                if state.frame_complete {
                    state.decoding_frame_after_complete = false;
                }
            }
        }
        if !(state.frame_complete && src.pos() < src.src.len() && dst.pos() < dst.capacity()) {
            return Ok(());
        }
    }
}

/// ZstdDecompressContext::GetErrorInfo past a codec error: a stream told it
/// ends here (`ZSTD_e_end`) that took all its input, with room to spare, and
/// is inside a frame, ended early -- unless all it is inside is the first
/// bytes after a finished frame, which are only refused when asked to.
fn decompress_error_info(
    state: &DecompressState,
    flush: i32,
    input_used: bool,
    output_room: bool,
) -> Result<(), ZstdError> {
    if flush == E_END && !state.frame_complete && input_used && output_room {
        if state.decoding_frame_after_complete && state.frame_prefix_size < 4 {
            if state.reject_garbage_after_end {
                return Err(TRAILING_JUNK_AT_END);
            }
            return Ok(());
        }
        return Err(UNEXPECTED_END);
    }
    Ok(())
}

/// node's `Init(pledged_src_size, dictionary, reject_garbage_after_end)` for
/// either context.
fn init_context(
    compress: bool,
    pledged_src_size: Option<u64>,
    dictionary: Option<&[u8]>,
    reject_garbage_after_end: bool,
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
        Ok(Context::Compress(
            cctx,
            CompressState {
                consumed_src_size: pledged_src_size.map(|_| 0),
                frame_complete: true,
                frame_output_emitted: false,
            },
        ))
    } else {
        let mut dctx = DCtx::create();
        if let Some(dictionary) = dictionary {
            dctx.load_dictionary(dictionary)
                .map_err(|_| DICTIONARY_LOAD_FAILED)?;
        }
        Ok(Context::Decompress(
            dctx,
            DecompressState::new(reject_garbage_after_end),
        ))
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
        // node v26.13.1 (as v22.22.2): zlib.zstdCompressSync('hello hello hello') and ('').
        let mut c = ZstdHandle::new(true, None, None, &[], false).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"hello hello hello", 2).unwrap()),
            "28b52ffd20116500003068656c6c6f200100314a11"
        );
        let mut c = ZstdHandle::new(true, None, None, &[], false).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"", 2).unwrap()),
            "28b52ffd2000010000"
        );
        // With ZSTD_c_checksumFlag (201) set to 5: libzstd stores `!= 0`.
        let mut c = ZstdHandle::new(true, None, None, &[(201, 5)], false).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"x", 2).unwrap()),
            "28b52ffd24010900007823110483"
        );
    }

    #[test]
    fn decodes_every_frame_and_refuses_a_truncation() {
        // node v26.13.1: zlib.zstdCompressSync('a') and ('b'), decoded
        // together, give "ab"; one cut short, or no input at all, is
        // Z_BUF_ERROR "unexpected end of file".
        let frame_a = unhex("28b52ffd200109000061");
        let frame_b = unhex("28b52ffd200109000062");
        let both = [frame_a.clone(), frame_b].concat();
        let mut d = ZstdHandle::new(false, None, None, &[], false).unwrap();
        assert_eq!(one_shot(&mut d, &both, 2).unwrap(), b"ab");
        for cut in [&frame_a[..frame_a.len() - 1], &[][..]] {
            let mut d = ZstdHandle::new(false, None, None, &[], false).unwrap();
            let err = one_shot(&mut d, cut, 2).unwrap_err();
            assert_eq!(
                (err.errno, err.code, err.message),
                (-5, "Z_BUF_ERROR", "unexpected end of file")
            );
        }
    }

    #[test]
    fn trailing_junk_is_ignored_or_refused() {
        // node v26.13.1: zstdDecompressSync(frame 'a' + 'xyz') gives "a"; with
        // rejectGarbageAfterEnd the binding refuses the junk.
        let input = [unhex("28b52ffd200109000061"), b"xyz".to_vec()].concat();
        let mut d = ZstdHandle::new(false, None, None, &[], false).unwrap();
        assert_eq!(one_shot(&mut d, &input, 2).unwrap(), b"a");
        let mut d = ZstdHandle::new(false, None, None, &[], true).unwrap();
        let err = one_shot(&mut d, &input, 2).unwrap_err();
        assert_eq!(err.code, "ERR_TRAILING_JUNK_AFTER_STREAM_END");
    }

    #[test]
    fn a_pledged_size_holds_when_one_call_ends_the_frame() {
        // node v26.13.1: zstdCompressSync('hello hello hello',
        // { pledgedSrcSize: 5 }) is "Src size is incorrect"; pledging 17 makes
        // the bytes the unpledged call makes.
        let mut c = ZstdHandle::new(true, Some(5), None, &[], false).unwrap();
        let err = one_shot(&mut c, b"hello hello hello", 2).unwrap_err();
        assert_eq!(err.code, "ZSTD_error_srcSize_wrong");
        let mut c = ZstdHandle::new(true, Some(17), None, &[], false).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"hello hello hello", 2).unwrap()),
            "28b52ffd20116500003068656c6c6f200100314a11"
        );
    }

    #[test]
    fn fails_as_node_reports() {
        let mut d = ZstdHandle::new(false, None, None, &[], false).unwrap();
        let err = one_shot(&mut d, b"garbage!", 2).unwrap_err();
        assert_eq!(
            (err.errno, err.code, err.message),
            (10, "ZSTD_error_prefix_unknown", "Unknown frame descriptor")
        );
        let mut c = ZstdHandle::new(true, None, None, &[], false).unwrap();
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
                ZstdHandle::new(compress, None, None, params, false).err(),
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
                ZstdHandle::new(true, None, None, params, false).is_ok(),
                "{params:?}"
            );
        }
    }

    #[test]
    fn writes_a_magicless_frame_as_node_does() {
        // node v22.22.2: zlib.zstdCompressSync('hi', { params: { 10: v } })
        // for v = 0 and v = 1 (ZSTD_c_format: standard, magicless).
        let mut c = ZstdHandle::new(true, None, None, &[(10, 0)], false).unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"hi", 2).unwrap()),
            "28b52ffd20021100006869"
        );
        let mut c = ZstdHandle::new(true, None, None, &[(10, 1)], false).unwrap();
        assert_eq!(hex(&one_shot(&mut c, b"hi", 2).unwrap()), "20021100006869");
    }

    #[test]
    fn reads_a_chunk_on_across_calls() {
        // One chunk worked through in small output windows: each call goes on
        // where the last stopped, and the round trip gives the input back.
        let input: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let mut c = ZstdHandle::new(true, None, None, &[], false).unwrap();
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
        let mut d = ZstdHandle::new(false, None, None, &[], false).unwrap();
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
        let mut c = ZstdHandle::new(true, Some(3), None, &[], false).unwrap();
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
    fn reset_keeps_the_parameters_and_refuses_a_half_written_frame() {
        // node v26.13.1: a checksummed stream reset() still checksums its
        // next frame (a session-only reset); one that has flushed part of a
        // frame refuses, ERR_ZLIB_INCOMPLETE_FRAME.
        let mut c = ZstdHandle::new(true, None, None, &[(201, 1)], false).unwrap();
        c.reset().unwrap();
        assert_eq!(
            hex(&one_shot(&mut c, b"def", 2).unwrap()),
            "28b52ffd2403190000646566a8d553db"
        );
        let mut c = ZstdHandle::new(true, None, None, &[], false).unwrap();
        c.set_input(b"abc".to_vec());
        c.write(1, 16384).unwrap();
        let err = c.reset().unwrap_err();
        assert_eq!((err.errno, err.code), (60, "ERR_ZLIB_INCOMPLETE_FRAME"));
    }
}
