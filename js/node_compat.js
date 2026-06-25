// oam node: compat wave 1 â€” the JS half of the builtin modules.
//
// Architecture: every builtin is a FACTORY registered on __oamNode and
// instantiated lazily at first import/require, receiving the natives
// object (globalThis.__oam.node, installed post-restore). Factories may
// cross-reference each other via __oamNode.get(). Pure-JS pieces (Buffer,
// TextEncoder/TextDecoder, atob/btoa, setImmediate) install as globals at
// snapshot evaluation; runtime-data globals (process, performance, the
// upgraded console) install post-restore via installRuntimeGlobals().
//
// SNAPSHOT CONSTRAINT (same as bootstrap.js): this file is evaluated at
// BUILD time into the V8 startup snapshot, where no native bindings exist.
// Anything from __oam must be looked up at CALL time, never captured at
// eval time. Nothing at top level may touch __oam, console, or timers.
//
// Wave-1 documented divergences (all carry a clear error or doc note):
// - process.env is a copy taken at first access of the process module;
//   writes mutate the JS object only.
// - process.exit() exits immediately (no 'exit' event flush), like Bun.
// - process.nextTick is microtask-based: it interleaves with promise
//   jobs instead of running strictly before them.
// - fs streams (createReadStream/...) throw with a wave-2 pointer.
// - TextDecoder supports utf-8 only (fatal + ignoreBOM honored).
"use strict";
(() => {
  // ---------------------------------------------------------------- utf-8
  class TextEncoder {
    get encoding() {
      return "utf-8";
    }
    encode(input = "") {
      const str = String(input);
      const out = new Uint8Array(str.length * 3);
      let o = 0;
      for (let i = 0; i < str.length; i++) {
        let cp = str.codePointAt(i);
        if (cp > 0xffff) i++;
        // Lone surrogates encode as U+FFFD, per WHATWG.
        if (cp >= 0xd800 && cp <= 0xdfff) cp = 0xfffd;
        if (cp < 0x80) {
          out[o++] = cp;
        } else if (cp < 0x800) {
          out[o++] = 0xc0 | (cp >> 6);
          out[o++] = 0x80 | (cp & 63);
        } else if (cp < 0x10000) {
          out[o++] = 0xe0 | (cp >> 12);
          out[o++] = 0x80 | ((cp >> 6) & 63);
          out[o++] = 0x80 | (cp & 63);
        } else {
          out[o++] = 0xf0 | (cp >> 18);
          out[o++] = 0x80 | ((cp >> 12) & 63);
          out[o++] = 0x80 | ((cp >> 6) & 63);
          out[o++] = 0x80 | (cp & 63);
        }
      }
      return out.slice(0, o);
    }
    encodeInto(source, destination) {
      const bytes = this.encode(source);
      const written = Math.min(bytes.length, destination.length);
      destination.set(bytes.subarray(0, written));
      // `read` is approximate when truncation splits a code point; exact
      // accounting arrives with the native encoder.
      let read = 0;
      let count = 0;
      const str = String(source);
      while (read < str.length) {
        const cp = str.codePointAt(read);
        const size = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
        if (count + size > written) break;
        count += size;
        read += cp > 0xffff ? 2 : 1;
      }
      return { read, written: count };
    }
  }

  class TextDecoder {
    constructor(label = "utf-8", options = {}) {
      const canonical = String(label).toLowerCase();
      if (canonical !== "utf-8" && canonical !== "utf8" && canonical !== "unicode-1-1-utf-8") {
        throw new RangeError(
          `TextDecoder: only utf-8 is supported in oam today (got '${label}')`,
        );
      }
      this.encoding = "utf-8";
      this.fatal = options.fatal === true;
      this.ignoreBOM = options.ignoreBOM === true;
      this._pending = null; // carry-over bytes between stream:true chunks
      this._atStart = true; // BOM stripping applies to the stream head only
    }
    decode(input, options) {
      const stream = options != null && options.stream === true;
      let bytes;
      if (input === undefined) bytes = new Uint8Array(0);
      else if (input instanceof Uint8Array) bytes = input;
      else if (ArrayBuffer.isView(input))
        bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
      else bytes = new Uint8Array(input);

      if (this._pending !== null && this._pending.length > 0) {
        const joined = new Uint8Array(this._pending.length + bytes.length);
        joined.set(this._pending);
        joined.set(bytes, this._pending.length);
        bytes = joined;
      }
      this._pending = null;

      if (stream) {
        // Hold back a trailing INCOMPLETE multi-byte sequence for the next
        // chunk (the standard SSE/streaming decode pattern). Complete or
        // invalid trailers decode now.
        let hold = bytes.length;
        let back = bytes.length - 1;
        let cont = 0;
        while (back >= 0 && cont < 3 && (bytes[back] & 0xc0) === 0x80) {
          back--;
          cont++;
        }
        if (back >= 0) {
          const lead = bytes[back];
          const need = lead >= 0xf0 ? 4 : lead >= 0xe0 ? 3 : lead >= 0xc0 ? 2 : 1;
          if (lead >= 0xc0 && cont + 1 < need) hold = back;
        }
        if (hold < bytes.length) {
          this._pending = bytes.slice(hold); // copy: caller may reuse input
          bytes = bytes.subarray(0, hold);
        }
      }

      let i = 0;
      const atStart = this._atStart;
      this._atStart = !stream; // a final (non-stream) decode resets the stream
      if (
        atStart &&
        !this.ignoreBOM &&
        bytes.length >= 3 &&
        bytes[0] === 0xef &&
        bytes[1] === 0xbb &&
        bytes[2] === 0xbf
      ) {
        i = 3;
      }
      let out = "";
      const fail = (at) => {
        if (this.fatal) throw new TypeError(`TextDecoder: invalid UTF-8 at byte ${at}`);
        out += "�";
      };
      while (i < bytes.length) {
        const b = bytes[i];
        if (b < 0x80) {
          out += String.fromCharCode(b);
          i++;
        } else if (b >= 0xc2 && b <= 0xdf) {
          if (i + 1 < bytes.length && (bytes[i + 1] & 0xc0) === 0x80) {
            out += String.fromCharCode(((b & 31) << 6) | (bytes[i + 1] & 63));
            i += 2;
          } else {
            fail(i);
            i++;
          }
        } else if (b >= 0xe0 && b <= 0xef) {
          const b1 = bytes[i + 1];
          const b2 = bytes[i + 2];
          const lower = b === 0xe0 ? 0xa0 : 0x80;
          const upper = b === 0xed ? 0x9f : 0xbf;
          if (
            i + 2 < bytes.length &&
            b1 >= lower &&
            b1 <= upper &&
            (b2 & 0xc0) === 0x80
          ) {
            out += String.fromCharCode(((b & 15) << 12) | ((b1 & 63) << 6) | (b2 & 63));
            i += 3;
          } else {
            fail(i);
            i++;
          }
        } else if (b >= 0xf0 && b <= 0xf4) {
          const b1 = bytes[i + 1];
          const b2 = bytes[i + 2];
          const b3 = bytes[i + 3];
          const lower = b === 0xf0 ? 0x90 : 0x80;
          const upper = b === 0xf4 ? 0x8f : 0xbf;
          if (
            i + 3 < bytes.length &&
            b1 >= lower &&
            b1 <= upper &&
            (b2 & 0xc0) === 0x80 &&
            (b3 & 0xc0) === 0x80
          ) {
            const cp =
              ((b & 7) << 18) | ((b1 & 63) << 12) | ((b2 & 63) << 6) | (b3 & 63);
            out += String.fromCodePoint(cp);
            i += 4;
          } else {
            fail(i);
            i++;
          }
        } else {
          fail(i);
          i++;
        }
      }
      return out;
    }
  }

  globalThis.TextEncoder = TextEncoder;
  globalThis.TextDecoder = TextDecoder;
  const utf8Encoder = new TextEncoder();
  // Buffer#toString never strips a BOM (Node parity) â€” only the WHATWG
  // TextDecoder default does.
  const utf8Decoder = new TextDecoder("utf-8", { ignoreBOM: true });

  // ---------------------------------------------------------------- Buffer
  function normalizeEncoding(enc) {
    const e = String(enc === undefined ? "utf8" : enc).toLowerCase();
    switch (e) {
      case "utf8":
      case "utf-8":
        return "utf8";
      case "hex":
        return "hex";
      case "base64":
        return "base64";
      case "base64url":
        return "base64url";
      case "latin1":
      case "binary":
        return "latin1";
      case "ascii":
        return "ascii";
      case "utf16le":
      case "utf-16le":
      case "ucs2":
      case "ucs-2":
        return "utf16le";
      default:
        return undefined;
    }
  }

  const B64_LOOKUP = (() => {
    const table = new Int8Array(256).fill(-1);
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    for (let i = 0; i < alphabet.length; i++) table[alphabet.charCodeAt(i)] = i;
    table[0x2b] = 62; // '+'
    table[0x2f] = 63; // '/'
    table[0x2d] = 62; // '-'  (base64url)
    table[0x5f] = 63; // '_'  (base64url)
    return table;
  })();

  function decodeBase64Lenient(str) {
    const out = new Uint8Array(((str.length * 3) >> 2) + 3);
    let o = 0;
    let acc = 0;
    let bits = 0;
    for (let i = 0; i < str.length; i++) {
      const c = str.charCodeAt(i);
      if (c === 0x3d) break; // '=' ends the data
      if (c === 0x20 || (c >= 0x09 && c <= 0x0d)) continue; // whitespace
      const v = c < 256 ? B64_LOOKUP[c] : -1;
      if (v === -1) break; // junk terminates, per Node
      acc = (acc << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out[o++] = (acc >> bits) & 0xff;
      }
    }
    return out.subarray(0, o);
  }

  // Node-shaped error codes: each entry creates a typed error with .code
  function makeNodeError(code, message) {
    var err = new Error(message);
    err.code = code;
    return err;
  }

  // Apply Node's coded-error shape to an Error instance: `.code` is set, `.name`
  // stays the plain base name (RangeError/TypeError -- assert.throws({name})
  // compares it strictly), and `.toString()`/`.stack` show "BaseName [CODE]: msg"
  // exactly as Node does (the code is injected into the rendered form, not name).
  function applyNodeErrorShape(inst, code) {
    const baseName = inst.name; // plain "TypeError" / "RangeError" / "Error"
    inst.code = code;
    Object.defineProperty(inst, "toString", {
      value: function () {
        const m = this.message;
        return baseName + " [" + code + "]" + (m ? ": " + m : "");
      },
      writable: true,
      configurable: true,
      enumerable: false,
    });
    // Node renders the code into the stack header too.
    if (typeof inst.stack === "string") {
      inst.stack = inst.stack.replace(
        baseName + ":",
        baseName + " [" + code + "]:",
      );
    }
    return inst;
  }

  function E(code, Base, msgFn) {
    function NodeError() {
      var args = Array.prototype.slice.call(arguments);
      var msg = typeof msgFn === "function" ? msgFn.apply(null, args) : msgFn;
      var inst = new Base(msg);
      return applyNodeErrorShape(inst, code);
    }
    return NodeError;
  }

  var codes = {};
  // ---- TypeError family ----
  codes.ERR_INVALID_ARG_TYPE = E("ERR_INVALID_ARG_TYPE", TypeError, function(name, expected, actual) {
    return 'The "' + name + '" argument must be of type ' + expected + '. Received ' + typeof actual;
  });
  codes.ERR_INVALID_ARG_VALUE = E("ERR_INVALID_ARG_VALUE", TypeError, function(name, value, reason) {
    return 'The argument "' + name + '" is invalid. Received ' + String(value) + (reason ? ". " + reason : "");
  });
  codes.ERR_INVALID_CALLBACK = E("ERR_INVALID_CALLBACK", TypeError, function(name) {
    return 'Callback must be a function. Received ' + String(name);
  });
  codes.ERR_INVALID_THIS = E("ERR_INVALID_THIS", TypeError, function(expected) {
    return 'Value of "this" must be of type ' + expected;
  });
  codes.ERR_INVALID_RETURN_VALUE = E("ERR_INVALID_RETURN_VALUE", TypeError, function(input, name, value) {
    return 'Expected ' + input + ' to be returned from the "' + name + '" function but got ' + typeof value + ".";
  });
  codes.ERR_MISSING_ARGS = E("ERR_MISSING_ARGS", TypeError, function() {
    var args = Array.prototype.slice.call(arguments);
    return 'The ' + args.map(function(a) { return '"' + a + '"'; }).join(", ") + ' argument' + (args.length > 1 ? 's' : '') + ' must be specified';
  });
  codes.ERR_UNKNOWN_ENCODING = E("ERR_UNKNOWN_ENCODING", TypeError, function(enc) {
    return 'Unknown encoding: ' + enc;
  });
  codes.ERR_INVALID_URL = E("ERR_INVALID_URL", TypeError, function(input) {
    return 'Invalid URL: ' + input;
  });
  codes.ERR_INVALID_URL_SCHEME = E("ERR_INVALID_URL_SCHEME", TypeError, function(expected) {
    return 'The URL must be of scheme ' + expected;
  });
  codes.ERR_INVALID_PROTOCOL = E("ERR_INVALID_PROTOCOL", TypeError, function(protocol, expected) {
    return 'Protocol "' + protocol + '" not supported. Expected "' + expected + '"';
  });
  codes.ERR_METHOD_NOT_IMPLEMENTED = E("ERR_METHOD_NOT_IMPLEMENTED", TypeError, function(name) {
    return 'The ' + name + ' method is not implemented';
  });
  codes.ERR_SOCKET_BAD_TYPE = E("ERR_SOCKET_BAD_TYPE", TypeError, function() {
    return 'Bad socket type specified. Valid types are: udp4, udp6';
  });
  codes.ERR_UNKNOWN_SIGNAL = E("ERR_UNKNOWN_SIGNAL", TypeError, function(signal) {
    return 'Unknown signal: ' + signal;
  });
  codes.ERR_UNESCAPED_CHARACTERS = E("ERR_UNESCAPED_CHARACTERS", TypeError, function(name) {
    return name + ' contains unescaped characters';
  });
  // ---- RangeError family ----
  codes.ERR_OUT_OF_RANGE = E("ERR_OUT_OF_RANGE", RangeError, function(name, range, received) {
    return 'The value of "' + name + '" is out of range. It must be ' + range + '. Received ' + received;
  });
  codes.ERR_BUFFER_OUT_OF_BOUNDS = E("ERR_BUFFER_OUT_OF_BOUNDS", RangeError, function(name) {
    return name ? '"' + name + '" is outside the bounds of the buffer' : 'Attempt to access memory outside buffer bounds';
  });
  codes.ERR_CHILD_CLOSED_BEFORE_REPLY = E("ERR_CHILD_CLOSED_BEFORE_REPLY", RangeError, function() {
    return 'Child closed before reply';
  });
  codes.ERR_SOCKET_BAD_PORT = E("ERR_SOCKET_BAD_PORT", RangeError, function(name, port, allowZero) {
    return '"' + name + '" option should be >= ' + (allowZero ? '0' : '1') + ' and < 65536. Received ' + port;
  });
  // ---- Error family ----
  codes.ERR_STREAM_DESTROYED = E("ERR_STREAM_DESTROYED", Error, function(name) {
    return 'Cannot call ' + (name || 'write') + ' after a stream was destroyed';
  });
  codes.ERR_STREAM_PREMATURE_CLOSE = E("ERR_STREAM_PREMATURE_CLOSE", Error, function() {
    return 'Premature close';
  });
  codes.ERR_STREAM_NULL_VALUES = E("ERR_STREAM_NULL_VALUES", TypeError, function() {
    return 'May not write null values to stream';
  });
  codes.ERR_STREAM_WRITE_AFTER_END = E("ERR_STREAM_WRITE_AFTER_END", Error, function() {
    return 'write after end';
  });
  codes.ERR_STREAM_ALREADY_FINISHED = E("ERR_STREAM_ALREADY_FINISHED", Error, function(name) {
    return 'Cannot call ' + (name || 'write') + ' after a stream was finished';
  });
  codes.ERR_STREAM_PUSH_AFTER_EOF = E("ERR_STREAM_PUSH_AFTER_EOF", Error, function() {
    return 'stream.push() after EOF';
  });
  codes.ERR_STREAM_UNSHIFT_AFTER_END_EVENT = E("ERR_STREAM_UNSHIFT_AFTER_END_EVENT", Error, function() {
    return 'stream.unshift() after end event';
  });
  codes.ERR_MULTIPLE_CALLBACK = E("ERR_MULTIPLE_CALLBACK", Error, function() {
    return 'Callback called multiple times';
  });
  codes.ERR_INVALID_FILE_URL_PATH = E("ERR_INVALID_FILE_URL_PATH", Error, function(msg) {
    return 'File URL path ' + msg;
  });
  codes.ERR_INVALID_FILE_URL_HOST = E("ERR_INVALID_FILE_URL_HOST", Error, function(host) {
    return 'File URL host must be "localhost" or empty on ' + host;
  });
  codes.ERR_FS_CP_DIR_TO_NON_DIR = E("ERR_FS_CP_DIR_TO_NON_DIR", Error, function(msg) {
    return msg;
  });
  codes.ERR_FS_EISDIR = E("ERR_FS_EISDIR", Error, function(msg) {
    return msg || 'Path is a directory';
  });
  codes.ERR_MODULE_NOT_FOUND = E("ERR_MODULE_NOT_FOUND", Error, function(path, base) {
    return 'Cannot find module "' + path + '"' + (base ? ' imported from ' + base : '');
  });
  codes.ERR_PACKAGE_PATH_NOT_EXPORTED = E("ERR_PACKAGE_PATH_NOT_EXPORTED", Error, function(pkgPath, subpath) {
    return 'Package subpath "' + subpath + '" is not defined by "exports" in ' + pkgPath;
  });
  codes.ERR_PACKAGE_IMPORT_NOT_DEFINED = E("ERR_PACKAGE_IMPORT_NOT_DEFINED", TypeError, function(specifier, pkgPath) {
    return 'Package import specifier "' + specifier + '" is not defined in ' + pkgPath;
  });
  codes.ERR_UNSUPPORTED_DIR_IMPORT = E("ERR_UNSUPPORTED_DIR_IMPORT", Error, function(path) {
    return 'Directory import "' + path + '" is not supported';
  });
  codes.ERR_UNSUPPORTED_ESM_URL_SCHEME = E("ERR_UNSUPPORTED_ESM_URL_SCHEME", Error, function(url) {
    return 'Only URLs with a scheme in: file and data are supported by the default ESM loader. Received protocol "' + url + '"';
  });
  codes.ERR_ASSERTION = E("ERR_ASSERTION", Error, function(msg) {
    return msg || 'assertion error';
  });
  codes.ERR_CRYPTO_FIPS_FORCED = E("ERR_CRYPTO_FIPS_FORCED", Error, function() {
    return 'Cannot set FIPS mode, it was forced with --force-fips at startup.';
  });
  codes.ERR_WORKER_NOT_SUPPORTED = E("ERR_WORKER_NOT_SUPPORTED", Error, function() {
    return 'Worker threads are not supported in this environment';
  });
  codes.ERR_ENV_FILE_NOT_FOUND = E("ERR_ENV_FILE_NOT_FOUND", Error, function(path) {
    return 'Cannot find env file: ' + path;
  });
  codes.ERR_INVALID_BUFFER_SIZE = E("ERR_INVALID_BUFFER_SIZE", RangeError, function() {
    return 'Buffer size must be a multiple of 8';
  });


  function bytesFromString(str, enc) {
    const encoding = normalizeEncoding(enc);
    switch (encoding) {
      case "utf8":
        return utf8Encoder.encode(str);
      case "hex": {
        // Node is lenient: parse stops at the first invalid pair.
        const len = str.length >>> 1;
        const out = new Uint8Array(len);
        let o = 0;
        for (let i = 0; i + 1 < str.length; i += 2) {
          const byte = parseInt(str.slice(i, i + 2), 16);
          if (Number.isNaN(byte)) break;
          out[o++] = byte;
        }
        return out.subarray(0, o);
      }
      case "base64":
      case "base64url":
        // Node's lenient decoder for BOTH labels: either alphabet accepted
        // ('-'/'_' alongside '+'/'/'), whitespace skipped, '=' or junk
        // terminates, trailing partial groups decode greedily. JWT-era code
        // decodes base64url payloads via 'base64' constantly â€” the strict
        // Uint8Array.fromBase64 path returned EMPTY for those.
        return decodeBase64Lenient(str);
      case "latin1": {
        const out = new Uint8Array(str.length);
        for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0xff;
        return out;
      }
      case "ascii": {
        const out = new Uint8Array(str.length);
        for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0x7f;
        return out;
      }
      case "utf16le": {
        const out = new Uint8Array(str.length * 2);
        for (let i = 0; i < str.length; i++) {
          const c = str.charCodeAt(i);
          out[i * 2] = c & 0xff;
          out[i * 2 + 1] = c >> 8;
        }
        return out;
      }
      default:
        throw codes.ERR_UNKNOWN_ENCODING(enc);
    }
  }

  // Mutable buffer-module state shared between the Buffer class and the
  // require('buffer') export. INSPECT_MAX_BYTES is settable via the module
  // (buffer.INSPECT_MAX_BYTES = N) and read by Buffer.prototype.inspect().
  const bufferState = { INSPECT_MAX_BYTES: 50 };

  // -------------------------------------------------------- Buffer validation
  // Shared argument-validation layer matching Node's lib/internal/buffer.js +
  // lib/internal/validators.js. The vendored conformance tests assert on the
  // exact { code, name, message } of the thrown error, so these must produce
  // Node-identical errors (codes.ERR_* are defined above via E()).

  // Render a number the way Node's ERR_OUT_OF_RANGE does: underscore thousands
  // separators only for magnitudes strictly greater than 2**32 (Node parity --
  // 2**32 itself prints plain, 2**40 gets separators); otherwise String(n).
  function fmtRange(n) {
    if (typeof n === "bigint") {
      const neg = n < 0n;
      const s = (neg ? -n : n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, "_");
      return (neg ? "-" : "") + s + "n";
    }
    if (typeof n === "number" && Number.isInteger(n) && Math.abs(n) > 2 ** 32) {
      const neg = n < 0;
      const s = Math.abs(n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, "_");
      return (neg ? "-" : "") + s;
    }
    return String(n);
  }

  // Node's boundsError: distinguishes non-integer offset, out-of-range offset,
  // and buffer-too-short. `length` is buf.length - byteSize (the max offset).
  function boundsError(value, length, type) {
    if (Math.floor(value) !== value) {
      // Non-integer offset (NaN, 1.01, ...).
      throw codes.ERR_OUT_OF_RANGE(type || "offset", "an integer", value);
    }
    if (length < 0) {
      // Buffer is shorter than the requested read/write width.
      throw codes.ERR_BUFFER_OUT_OF_BOUNDS();
    }
    throw codes.ERR_OUT_OF_RANGE(
      type || "offset",
      ">= 0 and <= " + length,
      value,
    );
  }

  // Validate a read/write offset for an access of `byteSize` bytes into `buf`.
  // Mirrors Node's checkBounds(buf, offset, byteSize).
  function checkBounds(buf, offset, byteSize) {
    if (typeof offset !== "number") {
      throw argTypeOfError("offset", "number", offset);
    }
    const max = buf.length - byteSize;
    if (offset < 0 || offset > max || Math.floor(offset) !== offset) {
      boundsError(offset, max);
    }
    return offset;
  }

  // Validate a write value against [min, max]. Matches Node's checkInt: NaN /
  // non-numeric coerce to a value that fails neither comparison and so passes
  // (Node then masks/truncates on store). `rangeStr` is the human range text.
  function checkValue(value, min, max, name, rangeStr) {
    if (value > max || value < min) {
      throw codes.ERR_OUT_OF_RANGE(name || "value", rangeStr, fmtRange(value));
    }
  }

  // Validate the byteLength arg for the variable-width int family (1..6).
  function checkVarLenArg(byteLength) {
    if (typeof byteLength !== "number") {
      throw argTypeOfError("byteLength", "number", byteLength);
    }
    if (Math.floor(byteLength) !== byteLength) {
      throw codes.ERR_OUT_OF_RANGE("byteLength", "an integer", byteLength);
    }
    if (byteLength < 1 || byteLength > 6) {
      throw codes.ERR_OUT_OF_RANGE("byteLength", ">= 1 and <= 6", byteLength);
    }
  }

  // Node's common.invalidArgTypeHelper(): builds the " Received ..." suffix of
  // an ERR_INVALID_ARG_TYPE message. Must match byte-for-byte (tests assert it).
  function receivedSuffix(input) {
    if (input == null) return " Received " + input;
    if (typeof input === "function") return " Received function " + input.name;
    if (typeof input === "object") {
      const cn = input.constructor && input.constructor.name;
      if (cn) return " Received an instance of " + cn;
      return " Received [Object: null prototype] {}";
    }
    let inspected;
    if (typeof input === "string") inspected = "'" + input + "'";
    else if (typeof input === "bigint") inspected = input.toString() + "n";
    else if (typeof input === "symbol") inspected = input.toString();
    else inspected = String(input);
    if (inspected.length > 28) inspected = inspected.slice(0, 25) + "...";
    return " Received type " + typeof input + " (" + inspected + ")";
  }

  // Build an ERR_INVALID_ARG_TYPE TypeError whose message follows Node's
  // "argument must be an instance of <expected>." + receivedSuffix form.
  function argTypeError(argName, expected, value) {
    return applyNodeErrorShape(
      new TypeError(
        'The "' + argName + '" argument must be an instance of ' +
          expected + "." + receivedSuffix(value),
      ),
      "ERR_INVALID_ARG_TYPE",
    );
  }

  // The "must be of type <expected>" variant (used for scalar args like
  // offset/byteLength where Node expects a primitive type, not an instance).
  function argTypeOfError(argName, expected, value) {
    return applyNodeErrorShape(
      new TypeError(
        'The "' + argName + '" argument must be of type ' +
          expected + "." + receivedSuffix(value),
      ),
      "ERR_INVALID_ARG_TYPE",
    );
  }

  // The exact ERR_INVALID_ARG_TYPE thrown by Buffer.from() for an unusable
  // first argument (Node's "first argument must be of type ..." phrasing).
  function argTypeFromError(value) {
    return applyNodeErrorShape(
      new TypeError(
        "The first argument must be of type string or an instance of " +
          "Buffer, ArrayBuffer, or Array or an Array-like Object." +
          receivedSuffix(value),
      ),
      "ERR_INVALID_ARG_TYPE",
    );
  }

  // Validate that a value is a Buffer/Uint8Array (compare/equals targets).
  function validateUint8Array(value, name) {
    if (!(value instanceof Uint8Array)) {
      throw argTypeError(name, "Buffer or Uint8Array", value);
    }
  }

  // Guard the `this` receiver of indexOf/lastIndexOf against misuse (e.g.
  // `new Buffer.prototype.lastIndexOf(...)`), matching Node's "buffer" arg error.
  function validateBufferThis(self) {
    if (!(self instanceof Uint8Array) && !ArrayBuffer.isView(self)) {
      throw argTypeError(
        "buffer",
        "Buffer, TypedArray, or DataView",
        self,
      );
    }
  }

  // Validate a start/end offset arg (compare): number-typed integer in
  // [0, max]. Node uses "of type number" for non-numbers and the "&&" range
  // form for ERR_OUT_OF_RANGE.
  function validateOffsetRange(value, name, max) {
    if (typeof value !== "number") {
      throw argTypeOfError(name, "number", value);
    }
    if (Math.floor(value) !== value) {
      throw codes.ERR_OUT_OF_RANGE(name, "an integer", value);
    }
    if (value < 0 || value > max) {
      throw codes.ERR_OUT_OF_RANGE(name, ">= 0 && <= " + max, fmtRange(value));
    }
  }

  // Node's toInteger coercion for copy() offsets: Number() the value, NaN -> 0,
  // truncate toward zero. A throwing Symbol.toPrimitive/valueOf propagates.
  function toIntegerOrZero(value) {
    const n = Number(value);
    if (Number.isNaN(n)) return 0;
    if (n === Infinity) return Infinity;
    if (n === -Infinity) return -Infinity;
    return Math.trunc(n);
  }

  // Validate an allocation size (alloc / allocUnsafe / SlowBuffer). Node throws
  // ERR_INVALID_ARG_TYPE for non-numbers and ERR_OUT_OF_RANGE for negative,
  // NaN, or > Number.MAX_SAFE_INTEGER. Sizes between MAX_SAFE_INTEGER and the
  // host allocator limit fail later with a V8 allocation error (Node parity --
  // its kMaxLength is MAX_SAFE_INTEGER on 64-bit, and the OS rejects the alloc).
  function validateSize(size) {
    if (typeof size !== "number") {
      throw argTypeOfError("size", "number", size);
    }
    if (Number.isNaN(size) || size < 0 || size > Number.MAX_SAFE_INTEGER) {
      throw codes.ERR_OUT_OF_RANGE(
        "size",
        ">= 0 && <= 9007199254740991",
        fmtRange(size),
      );
    }
  }

  class Buffer extends Uint8Array {
    static get [Symbol.species]() {
      return Buffer;
    }

    static isBuffer(value) {
      return value instanceof Buffer;
    }

    static isEncoding(enc) {
      return normalizeEncoding(enc) !== undefined;
    }

    static alloc(size, fill, encoding) {
      validateSize(size);
      const buf = new Buffer(size);
      if (fill !== undefined && fill !== 0) buf.fill(fill, 0, buf.length, encoding);
      return buf;
    }

    static allocUnsafe(size) {
      validateSize(size);
      return new Buffer(size);
    }

    static allocUnsafeSlow(size) {
      validateSize(size);
      return new Buffer(size);
    }

    static from(value, encodingOrOffset, length) {
      if (typeof value === "string") {
        const bytes = bytesFromString(value, encodingOrOffset);
        return new Buffer(bytes.buffer, bytes.byteOffset, bytes.length);
      }
      if (value instanceof ArrayBuffer || value instanceof SharedArrayBuffer) {
        // Views the SAME memory, per Node.
        return new Buffer(value, encodingOrOffset, length);
      }
      if (ArrayBuffer.isView(value)) {
        // COPIES, per Node (from(typedArray) copies; from(arrayBuffer) views).
        const src = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        const buf = new Buffer(src.length);
        buf.set(src);
        return buf;
      }
      if (Array.isArray(value)) {
        const buf = new Buffer(value.length >>> 0);
        for (let i = 0; i < buf.length; i++) buf[i] = value[i] & 0xff;
        return buf;
      }
      if (value && typeof value === "object" && value.type === "Buffer" && Array.isArray(value.data)) {
        return Buffer.from(value.data);
      }
      // Node coerces an object via valueOf()/Symbol.toPrimitive BEFORE the
      // array-like path: `new String('test')` / `MyString extends String` /
      // an object with a string [Symbol.toPrimitive] all become that string.
      // This must precede array-like because String objects also have .length.
      if (value != null && typeof value === "object") {
        if (typeof value.valueOf === "function") {
          const coerced = value.valueOf();
          if (coerced != null && coerced !== value) {
            if (typeof coerced === "string") return Buffer.from(coerced, encodingOrOffset);
          }
        }
        const prim = value[Symbol.toPrimitive];
        if (typeof prim === "function") {
          const coerced = prim.call(value, "string");
          if (typeof coerced === "string") return Buffer.from(coerced, encodingOrOffset);
        }
      }
      // Array-like (objects with a numeric .length): fromArrayLike. Functions
      // have a numeric .length too but are NOT valid input -- Node rejects them.
      if (value != null && typeof value === "object" && typeof value.length === "number") {
        const len = value.length >>> 0;
        const buf = new Buffer(len);
        for (let i = 0; i < len; i++) buf[i] = value[i] & 0xff;
        return buf;
      }
      throw argTypeFromError(value);
    }

    static byteLength(value, encoding) {
      if (typeof value !== "string") {
        if (ArrayBuffer.isView(value) || value instanceof ArrayBuffer) {
          return value.byteLength;
        }
        throw applyNodeErrorShape(
          new TypeError(
            'The "string" argument must be of type string or an instance of ' +
              "Buffer or ArrayBuffer." + receivedSuffix(value),
          ),
          "ERR_INVALID_ARG_TYPE",
        );
      }
      switch (normalizeEncoding(encoding)) {
        case "latin1":
        case "ascii":
          return value.length;
        case "utf16le":
          return value.length * 2;
        case "hex":
          return value.length >>> 1;
        case "base64":
        case "base64url": {
          const trimmed = value.replace(/=+$/, "");
          return Math.floor((trimmed.length * 3) / 4);
        }
        default:
          return utf8Encoder.encode(value).length;
      }
    }

    static concat(list, totalLength) {
      if (!Array.isArray(list)) {
        throw argTypeError("list", "Array", list);
      }
      // Node returns an empty Buffer for an empty list, ignoring totalLength.
      if (list.length === 0) return new Buffer(0);
      for (let i = 0; i < list.length; i++) {
        if (!(list[i] instanceof Uint8Array)) {
          throw argTypeError("list[" + i + "]", "Buffer or Uint8Array", list[i]);
        }
      }
      let total = totalLength;
      if (total === undefined) {
        total = 0;
        for (const item of list) total += item.length;
      }
      const out = new Buffer(total);
      let offset = 0;
      for (const item of list) {
        if (offset >= total) break;
        const chunk = item.subarray(0, Math.min(item.length, total - offset));
        out.set(chunk, offset);
        offset += chunk.length;
      }
      return out;
    }

    static compare(a, b) {
      validateUint8Array(a, "buf1");
      validateUint8Array(b, "buf2");
      return a.compare(b);
    }

    toString(encoding, start, end) {
      // Node coerces start/end with Number(): NaN/non-numeric -> 0, fractional
      // truncates toward zero, negative clamps to 0, > length clamps to length;
      // an undefined end defaults to length. end <= start yields "".
      const len = this.length;
      let s = start === undefined ? 0 : Number(start);
      if (Number.isNaN(s)) s = 0;
      else s = Math.trunc(s);
      if (s < 0) s = 0;
      else if (s > len) s = len;
      let e;
      if (end === undefined) {
        e = len;
      } else {
        e = Number(end);
        if (Number.isNaN(e)) e = 0;
        else e = Math.trunc(e);
        if (e < 0) e = 0;
        else if (e > len) e = len;
      }
      const view = e <= s ? this.subarray(0, 0) : this.subarray(s, e);
      switch (normalizeEncoding(encoding)) {
        case "hex":
          return view.toHex();
        case "base64":
          return view.toBase64();
        case "base64url":
          return view.toBase64({ alphabet: "base64url", omitPadding: true });
        case "latin1": {
          let out = "";
          for (let i = 0; i < view.length; i++) out += String.fromCharCode(view[i]);
          return out;
        }
        case "ascii": {
          let out = "";
          for (let i = 0; i < view.length; i++) out += String.fromCharCode(view[i] & 0x7f);
          return out;
        }
        case "utf16le": {
          let out = "";
          for (let i = 0; i + 1 < view.length; i += 2) {
            out += String.fromCharCode(view[i] | (view[i + 1] << 8));
          }
          return out;
        }
        case "utf8":
          return utf8Decoder.decode(view);
        default:
          throw codes.ERR_UNKNOWN_ENCODING(encoding);
      }
    }

    // Node Buffer#slice is a VIEW (Uint8Array#slice copies).
    slice(start, end) {
      return this.subarray(start, end);
    }

    equals(other) {
      validateUint8Array(other, "otherBuffer");
      if (this === other) return true;
      if (this.length !== other.length) return false;
      for (let i = 0; i < this.length; i++) {
        if (this[i] !== other[i]) return false;
      }
      return true;
    }

    compare(target, targetStart, targetEnd, sourceStart, sourceEnd) {
      validateUint8Array(target, "target");
      if (targetStart === undefined) targetStart = 0;
      if (targetEnd === undefined) targetEnd = target.length;
      if (sourceStart === undefined) sourceStart = 0;
      if (sourceEnd === undefined) sourceEnd = this.length;
      // start offsets: integer in [0, MAX_SAFE_INTEGER]; end offsets: [0, len].
      validateOffsetRange(targetStart, "targetStart", Number.MAX_SAFE_INTEGER);
      validateOffsetRange(targetEnd, "targetEnd", target.length);
      validateOffsetRange(sourceStart, "sourceStart", Number.MAX_SAFE_INTEGER);
      validateOffsetRange(sourceEnd, "sourceEnd", this.length);
      const a = this.subarray(sourceStart, sourceEnd);
      const b = target.subarray(targetStart, targetEnd);
      const len = Math.min(a.length, b.length);
      for (let i = 0; i < len; i++) {
        if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
      }
      return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
    }

    copy(target, targetStart, sourceStart, sourceEnd) {
      // Node accepts any Uint8Array OR TypedArray target (it operates on the
      // target's underlying bytes); targetStart is an index into those bytes.
      if (!(target instanceof Uint8Array) && !ArrayBuffer.isView(target)) {
        throw argTypeError("target", "Buffer or Uint8Array", target);
      }
      const targetBytes =
        target instanceof Uint8Array
          ? target
          : new Uint8Array(target.buffer, target.byteOffset, target.byteLength);
      // Node's copy() coerces offsets with toInteger (NaN -> 0, truncates),
      // lower-bounds targetStart/sourceEnd at 0 (no upper-bound message) and
      // bounds sourceStart to [0, source.length].
      targetStart = toIntegerOrZero(targetStart);
      sourceStart = toIntegerOrZero(sourceStart);
      sourceEnd = sourceEnd === undefined ? this.length : toIntegerOrZero(sourceEnd);
      if (targetStart < 0) {
        throw codes.ERR_OUT_OF_RANGE("targetStart", ">= 0", fmtRange(targetStart));
      }
      if (sourceStart < 0 || sourceStart > this.length) {
        throw codes.ERR_OUT_OF_RANGE(
          "sourceStart",
          ">= 0 && <= " + this.length,
          fmtRange(sourceStart),
        );
      }
      if (sourceEnd < 0) {
        throw codes.ERR_OUT_OF_RANGE("sourceEnd", ">= 0", fmtRange(sourceEnd));
      }
      if (targetStart >= targetBytes.length || sourceStart >= sourceEnd) return 0;
      const chunk = this.subarray(sourceStart, Math.min(sourceEnd, this.length));
      const writable = Math.min(chunk.length, targetBytes.length - targetStart);
      targetBytes.set(chunk.subarray(0, writable), targetStart);
      return writable;
    }

    write(string, offset, length, encoding) {
      // write(str), write(str, enc), write(str, offset, enc),
      // write(str, offset, length, enc)
      if (typeof offset === "string") {
        encoding = offset;
        offset = 0;
        length = undefined;
      } else if (typeof length === "string") {
        encoding = length;
        length = undefined;
      }
      offset = offset ?? 0;
      // Validate offset/length bounds (Node throws ERR_OUT_OF_RANGE).
      validateOffsetRange(offset, "offset", this.length);
      if (length !== undefined) {
        validateOffsetRange(length, "length", this.length);
      }
      const bytes = bytesFromString(String(string), encoding);
      let writable = Math.min(bytes.length, length ?? this.length - offset, this.length - offset);
      // Node never writes partial characters: back off to a character
      // boundary when the encoded string does not fit.
      if (writable < bytes.length) {
        const norm = normalizeEncoding(encoding);
        if (norm === "utf8" || norm === undefined) {
          while (writable > 0 && (bytes[writable] & 0xc0) === 0x80) writable--;
        } else if (norm === "utf16le") {
          writable &= ~1;
        }
      }
      this.set(bytes.subarray(0, writable), offset);
      return writable;
    }

    fill(value, start = 0, end = this.length, encoding) {
      // Node signature: fill(value[, offset[, end]][, encoding]) â€” a
      // trailing string in the offset or end position is the encoding.
      if (typeof start === "string") {
        encoding = start;
        start = 0;
        end = this.length;
      } else if (typeof end === "string") {
        encoding = end;
        end = this.length;
      }
      if (start < 0 || end > this.length || start > end) {
        throw Object.assign(
          new RangeError(
            `The value of "offset" is out of range. It must be >= 0 and <= ${this.length}. Received ${start < 0 ? start : end}`,
          ),
          { code: "ERR_OUT_OF_RANGE" },
        );
      }
      if (typeof value === "number") {
        Uint8Array.prototype.fill.call(this, value & 0xff, start, end);
        return this;
      }
      const pattern =
        typeof value === "string" ? bytesFromString(value, encoding) : Buffer.from(value);
      if (pattern.length === 0) {
        Uint8Array.prototype.fill.call(this, 0, start, end);
        return this;
      }
      for (let i = start; i < end; i++) this[i] = pattern[(i - start) % pattern.length];
      return this;
    }

    indexOf(needle, byteOffset = 0, encoding) {
      validateBufferThis(this);
      if (typeof byteOffset === "string") {
        encoding = byteOffset;
        byteOffset = 0;
      }
      // Node coerces byteOffset numerically; NaN (and other non-numbers) -> 0.
      byteOffset = +byteOffset;
      if (Number.isNaN(byteOffset)) byteOffset = 0;
      if (byteOffset < 0) byteOffset = Math.max(0, this.length + byteOffset);
      if (typeof needle === "number") {
        return Uint8Array.prototype.indexOf.call(this, needle & 0xff, byteOffset);
      }
      // Node validates the needle type: number/string/Buffer/Uint8Array only.
      if (typeof needle !== "string" && !(needle instanceof Uint8Array)) {
        throw applyNodeErrorShape(
          new TypeError(
            'The "value" argument must be one of type number or string ' +
              "or an instance of Buffer or Uint8Array." + receivedSuffix(needle),
          ),
          "ERR_INVALID_ARG_TYPE",
        );
      }
      const pattern =
        typeof needle === "string" ? bytesFromString(needle, encoding) : needle;
      if (pattern.length === 0) return byteOffset <= this.length ? byteOffset : this.length;
      // ucs2/utf16le search is 2-byte aligned: byteOffset rounds to even and a
      // match must start on an even byte (Node parity); odd-length needle -> -1.
      const norm = normalizeEncoding(encoding);
      const step = norm === "utf16le" ? 2 : 1;
      if (step === 2) {
        if (pattern.length % 2 !== 0) return -1;
        byteOffset -= byteOffset % 2;
      }
      outer: for (let i = byteOffset; i + pattern.length <= this.length; i += step) {
        for (let j = 0; j < pattern.length; j++) {
          if (this[i + j] !== pattern[j]) continue outer;
        }
        return i;
      }
      return -1;
    }

    lastIndexOf(needle, byteOffset, encoding) {
      validateBufferThis(this);
      if (typeof byteOffset === "string") {
        encoding = byteOffset;
        byteOffset = undefined;
      }
      // Coerce byteOffset (Node semantics): undefined/NaN -> search from end;
      // Infinity -> end; -Infinity -> no match; negative -> length + offset.
      // `endOff` is the empty-needle/clamp anchor (length when at the end).
      let off;
      let endOff;
      if (byteOffset === undefined) {
        off = this.length - 1;
        endOff = this.length;
      } else {
        off = +byteOffset;
        if (Number.isNaN(off)) {
          off = this.length - 1;
          endOff = this.length;
        } else if (off === Infinity) {
          off = this.length - 1;
          endOff = this.length;
        } else if (off === -Infinity) {
          return -1;
        } else {
          off = Math.trunc(off);
          if (off < 0) off += this.length;
          if (off < 0) return -1;
          endOff = off;
          if (off > this.length - 1) off = this.length - 1;
        }
      }
      if (typeof needle === "number") {
        return Uint8Array.prototype.lastIndexOf.call(this, needle & 0xff, off);
      }
      if (typeof needle !== "string" && !(needle instanceof Uint8Array)) {
        throw applyNodeErrorShape(
          new TypeError(
            'The "value" argument must be one of type number or string ' +
              "or an instance of Buffer or Uint8Array." + receivedSuffix(needle),
          ),
          "ERR_INVALID_ARG_TYPE",
        );
      }
      const pattern =
        typeof needle === "string" ? bytesFromString(needle, encoding) : needle;
      if (pattern.length === 0) return Math.min(endOff, this.length);
      const last = Math.min(off, this.length - pattern.length);
      outer: for (let i = last; i >= 0; i--) {
        for (let j = 0; j < pattern.length; j++) {
          if (this[i + j] !== pattern[j]) continue outer;
        }
        return i;
      }
      return -1;
    }

    includes(needle, byteOffset, encoding) {
      return this.indexOf(needle, byteOffset, encoding) !== -1;
    }

    toJSON() {
      return { type: "Buffer", data: Array.from(this) };
    }

    inspect() {
      const max = bufferState.INSPECT_MAX_BYTES;
      const head = Array.from(this.subarray(0, max))
        .map((b) => b.toString(16).padStart(2, "0"))
        .join(" ");
      let extra = "";
      if (this.length > max) {
        const remaining = this.length - max;
        extra = ` ... ${remaining} more byte${remaining > 1 ? "s" : ""}`;
      }
      return `<Buffer ${head}${extra}>`;
    }

    // Node aliases buf.parent -> buf.buffer (legacy pre-Uint8Array name).
    get parent() {
      return this.buffer;
    }

    // Node exposes buf.offset === buf.byteOffset.
    get offset() {
      return this.byteOffset;
    }
  }

  // The numeric read/write family, generated over DataView. Each entry carries
  // the [min, max] write-value range and its human range string (Node parity);
  // a null range means no value check (Float/Double accept any finite/NaN). The
  // BigInt entries carry bigint min/max checked separately.
  {
    const specs = [
      ["UInt8", 1, "Uint8", false, 0, 255, ">= 0 and <= 255", false],
      ["Int8", 1, "Int8", false, -128, 127, ">= -128 and <= 127", false],
      ["UInt16", 2, "Uint16", true, 0, 65535, ">= 0 and <= 65535", false],
      ["Int16", 2, "Int16", true, -32768, 32767, ">= -32768 and <= 32767", false],
      ["UInt32", 4, "Uint32", true, 0, 4294967295, ">= 0 and <= 4294967295", false],
      ["Int32", 4, "Int32", true, -2147483648, 2147483647, ">= -2147483648 and <= 2147483647", false],
      ["Float", 4, "Float32", true, null, null, null, false],
      ["Double", 8, "Float64", true, null, null, null, false],
      ["BigUInt64", 8, "BigUint64", true, 0n, 2n ** 64n - 1n, ">= 0n and < 2n ** 64n", true],
      ["BigInt64", 8, "BigInt64", true, -(2n ** 63n), 2n ** 63n - 1n, ">= -(2n ** 63n) and < 2n ** 63n", true],
    ];
    const view = (buf) => new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
    // BigInt write value check (Node: must be a bigint within range).
    const checkBig = (value, min, max, range) => {
      if (typeof value !== "bigint") {
        throw argTypeOfError("value", "bigint", value);
      }
      if (value < min || value > max) {
        throw codes.ERR_OUT_OF_RANGE("value", range, fmtRange(value));
      }
    };
    for (const [name, _size, dv, multi, vmin, vmax, vrange, isBig] of specs) {
      if (multi) {
        for (const [suffix, little] of [["LE", true], ["BE", false]]) {
          Buffer.prototype[`read${name}${suffix}`] = function (offset = 0) {
            checkBounds(this, offset, _size);
            return view(this)[`get${dv}`](offset, little);
          };
          Buffer.prototype[`write${name}${suffix}`] = function (value, offset = 0) {
            if (isBig) checkBig(value, vmin, vmax, vrange);
            else if (vrange !== null) checkValue(value, vmin, vmax, "value", vrange);
            checkBounds(this, offset, _size);
            view(this)[`set${dv}`](offset, value, little);
            return offset + _size;
          };
        }
      } else {
        Buffer.prototype[`read${name}`] = function (offset = 0) {
          checkBounds(this, offset, _size);
          return view(this)[`get${dv}`](offset);
        };
        Buffer.prototype[`write${name}`] = function (value, offset = 0) {
          if (isBig) checkBig(value, vmin, vmax, vrange);
          else if (vrange !== null) checkValue(value, vmin, vmax, "value", vrange);
          checkBounds(this, offset, _size);
          view(this)[`set${dv}`](offset, value);
          return offset + _size;
        };
      }
    }
  }
  // Variable-byteLength integer family (24/40/48-bit wire formats).
  {
    // READ order (Node): offset must be a number (rejects undefined/string
    // before byteLength is examined), then byteLength in [1,6], then offset
    // bounds.
    function checkVarBounds(buf, offset, byteLength) {
      if (typeof offset !== "number") {
        throw argTypeOfError("offset", "number", offset);
      }
      checkVarLenArg(byteLength);
      checkVarOffset(buf, offset, byteLength);
    }
    // Offset-only bound check (used by writes, where byteLength + value were
    // already validated before the offset is examined).
    function checkVarOffset(buf, offset, byteLength) {
      if (typeof offset !== "number") {
        throw argTypeOfError("offset", "number", offset);
      }
      const max = buf.length - byteLength;
      if (offset < 0 || offset > max || Math.floor(offset) !== offset) {
        boundsError(offset, max);
      }
    }
    // Unsigned value range for a varint write: Node uses "<= 2**(8n)-1" for
    // 1..4 bytes (exact integer) and "< 2 ** N" for 5..6 bytes (loses precision).
    function checkVarUInt(value, byteLength) {
      const bits = byteLength * 8;
      if (byteLength < 5) {
        const max = 2 ** bits - 1;
        if (value > max || value < 0) {
          throw codes.ERR_OUT_OF_RANGE("value", ">= 0 and <= " + max, fmtRange(value));
        }
      } else {
        const max = 2 ** bits;
        if (value >= max || value < 0) {
          throw codes.ERR_OUT_OF_RANGE("value", ">= 0 and < 2 ** " + bits, fmtRange(value));
        }
      }
    }
    // Signed value range for a varint write.
    function checkVarInt(value, byteLength) {
      const bits = byteLength * 8;
      if (byteLength < 5) {
        const lim = 2 ** (bits - 1);
        if (value > lim - 1 || value < -lim) {
          throw codes.ERR_OUT_OF_RANGE(
            "value",
            ">= " + -lim + " and <= " + (lim - 1),
            fmtRange(value),
          );
        }
      } else {
        const lim = 2 ** (bits - 1);
        if (value >= lim || value < -lim) {
          throw codes.ERR_OUT_OF_RANGE(
            "value",
            ">= -(2 ** " + (bits - 1) + ") and < 2 ** " + (bits - 1),
            fmtRange(value),
          );
        }
      }
    }
    Buffer.prototype.readUIntLE = function (offset, byteLength) {
      checkVarBounds(this, offset, byteLength);
      let value = 0;
      for (let i = byteLength - 1; i >= 0; i--) value = value * 256 + this[offset + i];
      return value;
    };
    Buffer.prototype.readUIntBE = function (offset, byteLength) {
      checkVarBounds(this, offset, byteLength);
      let value = 0;
      for (let i = 0; i < byteLength; i++) value = value * 256 + this[offset + i];
      return value;
    };
    Buffer.prototype.readIntLE = function (offset, byteLength) {
      checkVarBounds(this, offset, byteLength);
      let unsigned = 0;
      for (let i = byteLength - 1; i >= 0; i--) unsigned = unsigned * 256 + this[offset + i];
      const limit = 2 ** (byteLength * 8 - 1);
      return unsigned >= limit ? unsigned - limit * 2 : unsigned;
    };
    Buffer.prototype.readIntBE = function (offset, byteLength) {
      checkVarBounds(this, offset, byteLength);
      let unsigned = 0;
      for (let i = 0; i < byteLength; i++) unsigned = unsigned * 256 + this[offset + i];
      const limit = 2 ** (byteLength * 8 - 1);
      return unsigned >= limit ? unsigned - limit * 2 : unsigned;
    };
    // Writes validate in Node's order: byteLength, then value range, then offset.
    Buffer.prototype.writeUIntLE = function (value, offset, byteLength) {
      checkVarLenArg(byteLength);
      checkVarUInt(value, byteLength);
      checkVarOffset(this, offset, byteLength);
      let v = value;
      for (let i = 0; i < byteLength; i++) {
        this[offset + i] = v % 256;
        v = Math.floor(v / 256);
      }
      return offset + byteLength;
    };
    Buffer.prototype.writeUIntBE = function (value, offset, byteLength) {
      checkVarLenArg(byteLength);
      checkVarUInt(value, byteLength);
      checkVarOffset(this, offset, byteLength);
      let v = value;
      for (let i = byteLength - 1; i >= 0; i--) {
        this[offset + i] = v % 256;
        v = Math.floor(v / 256);
      }
      return offset + byteLength;
    };
    Buffer.prototype.writeIntLE = function (value, offset, byteLength) {
      checkVarLenArg(byteLength);
      checkVarInt(value, byteLength);
      checkVarOffset(this, offset, byteLength);
      const limit = 2 ** (byteLength * 8);
      const v = value < 0 ? value + limit : value;
      let acc = v;
      for (let i = 0; i < byteLength; i++) {
        this[offset + i] = acc % 256;
        acc = Math.floor(acc / 256);
      }
      return offset + byteLength;
    };
    Buffer.prototype.writeIntBE = function (value, offset, byteLength) {
      checkVarLenArg(byteLength);
      checkVarInt(value, byteLength);
      checkVarOffset(this, offset, byteLength);
      const limit = 2 ** (byteLength * 8);
      const v = value < 0 ? value + limit : value;
      let acc = v;
      for (let i = byteLength - 1; i >= 0; i--) {
        this[offset + i] = acc % 256;
        acc = Math.floor(acc / 256);
      }
      return offset + byteLength;
    };

    function swapper(width) {
      return function () {
        if (this.length % width !== 0) {
          throw Object.assign(
            new RangeError(`Buffer size must be a multiple of ${width * 8}-bits`),
            { code: "ERR_INVALID_BUFFER_SIZE" },
          );
        }
        for (let i = 0; i < this.length; i += width) {
          for (let j = 0; j < width / 2; j++) {
            const tmp = this[i + j];
            this[i + j] = this[i + width - 1 - j];
            this[i + width - 1 - j] = tmp;
          }
        }
        return this;
      };
    }
    Buffer.prototype.swap16 = swapper(2);
    Buffer.prototype.swap32 = swapper(4);
    Buffer.prototype.swap64 = swapper(8);

    // Node >= 14.9 lowercase aliases: readUint8, writeBigUint64LE, ...
    for (const name of Object.getOwnPropertyNames(Buffer.prototype)) {
      if (name.includes("UInt")) {
        Buffer.prototype[name.replace("UInt", "Uint")] = Buffer.prototype[name];
      }
    }
  }
  // Node's Buffer statics are ENUMERABLE (assigned, not class-static) —
  // safer-buffer/safe-buffer clone them with for..in, and class statics
  // (non-enumerable) would clone to an empty shell (iconv-lite found it).
  for (const name of Object.getOwnPropertyNames(Buffer)) {
    if (name === "prototype" || name === "name" || name === "length") continue;
    const descriptor = Object.getOwnPropertyDescriptor(Buffer, name);
    if (descriptor && descriptor.configurable && !descriptor.enumerable) {
      Object.defineProperty(Buffer, name, { ...descriptor, enumerable: true });
    }
  }
  Buffer.poolSize = 8192;

  function btoa(input) {
    const str = String(input);
    const bytes = new Uint8Array(str.length);
    for (let i = 0; i < str.length; i++) {
      const c = str.charCodeAt(i);
      if (c > 0xff) {
        throw new (globalThis.DOMException ?? TypeError)(
          "btoa: the string contains characters outside of the Latin1 range",
        );
      }
      bytes[i] = c;
    }
    return bytes.toBase64();
  }

  function atob(input) {
    let bytes;
    try {
      bytes = Uint8Array.fromBase64(String(input).replace(/\s/g, ""), {
        lastChunkHandling: "loose",
      });
    } catch {
      throw new (globalThis.DOMException ?? TypeError)(
        "atob: the string to be decoded is not correctly encoded",
      );
    }
    let out = "";
    for (let i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i]);
    return out;
  }

  globalThis.Buffer = Buffer;
  globalThis.atob = atob;
  globalThis.btoa = btoa;
  globalThis.setImmediate = function setImmediate(fn, ...args) {
    return globalThis.setTimeout(fn, 0, ...args);
  };
  globalThis.clearImmediate = function clearImmediate(id) {
    return globalThis.clearTimeout(id);
  };

  if (typeof globalThis.structuredClone !== "function") {
    globalThis.structuredClone = function structuredClone(value) {
      if (value === undefined) return undefined;
      return JSON.parse(JSON.stringify(value));
    };
  }

  // ------------------------------------------------------------- registry
  const registry = {
    factories: { __proto__: null },
    cache: new Map(),
    get(name) {
      if (registry.cache.has(name)) return registry.cache.get(name);
      const factory = registry.factories[name];
      if (!factory) {
        throw new Error(`oam internal: no builtin factory registered for '${name}'`);
      }
      const mod = factory(globalThis.__oam.node);
      registry.cache.set(name, mod);
      return mod;
    },
  };
  Object.defineProperty(globalThis, "__oamNode", {
    value: registry,
    writable: false,
    enumerable: false,
    configurable: false,
  });

  // --------------------------------------------------------------- events
  registry.factories.events = () => {
    const kMax = Symbol("maxListeners");
    const errorMonitor = Symbol("events.errorMonitor");

    // Lazy state init (Node parity): express-style code mixes
    // EventEmitter.prototype onto plain functions WITHOUT running the
    // constructor, so every method must tolerate a missing _events.
    function eventsOf(self) {
      if (self._events === undefined) {
        self._events = { __proto__: null };
        self._eventsCount = 0;
      }
      return self._events;
    }

    // Defined as a function constructor (NOT an ES6 class) for Node parity:
    // transpiled CJS subclasses (TS `__extends`, ioredis, pg) invoke the
    // superclass as a plain function -- `EventEmitter.call(this)` -- which an
    // ES6 class rejects with "Class constructor cannot be invoked without
    // 'new'". A function constructor accepts both that call form AND native
    // `class X extends EventEmitter { super() }`.
    function EventEmitter() {
      this._events = { __proto__: null };
      this._eventsCount = 0;
    }
    EventEmitter.prototype.setMaxListeners = function (n) {
      this[kMax] = n;
      return this;
    };
    EventEmitter.prototype.getMaxListeners = function () {
      return this[kMax] ?? EventEmitter.defaultMaxListeners;
    };
    EventEmitter.prototype._add = function (type, listener, prepend, once) {
      eventsOf(this);
      if (typeof listener !== "function") {
        throw new TypeError(`The "listener" argument must be a function`);
      }
      let entry = listener;
      if (once) {
        const wrapper = (...args) => {
          this.removeListener(type, wrapper);
          listener.apply(this, args);
        };
        wrapper.listener = listener;
        entry = wrapper;
      }
      if (this._events.newListener) {
        this.emit("newListener", type, entry.listener ?? entry);
      }
      const existing = this._events[type];
      if (existing === undefined) {
        this._events[type] = entry;
        this._eventsCount++;
      } else if (typeof existing === "function") {
        this._events[type] = prepend ? [entry, existing] : [existing, entry];
      } else if (prepend) {
        existing.unshift(entry);
      } else {
        existing.push(entry);
      }
      const count = this.listenerCount(type);
      const max = this.getMaxListeners();
      if (max > 0 && count > max && !this._events[type].warned) {
        this._events[type].warned = true;
        if (globalThis.console) {
          globalThis.console.warn(
            `(oam) MaxListenersExceededWarning: ${count} ${String(type)} listeners added to EventEmitter. ` +
              `Use emitter.setMaxListeners() to increase limit`,
          );
        }
      }
      return this;
    };
    EventEmitter.prototype.addListener = function (type, listener) {
      return this._add(type, listener, false, false);
    };
    EventEmitter.prototype.on = function (type, listener) {
      return this._add(type, listener, false, false);
    };
    EventEmitter.prototype.once = function (type, listener) {
      return this._add(type, listener, false, true);
    };
    EventEmitter.prototype.prependListener = function (type, listener) {
      return this._add(type, listener, true, false);
    };
    EventEmitter.prototype.prependOnceListener = function (type, listener) {
      return this._add(type, listener, true, true);
    };
    EventEmitter.prototype.removeListener = function (type, listener) {
      const existing = eventsOf(this)[type];
      if (existing === undefined) return this;
      if (existing === listener || existing.listener === listener) {
        delete this._events[type];
        this._eventsCount--;
        if (this._events.removeListener) {
          this.emit("removeListener", type, existing.listener ?? existing);
        }
        return this;
      }
      if (typeof existing !== "function") {
        for (let i = existing.length - 1; i >= 0; i--) {
          if (existing[i] === listener || existing[i].listener === listener) {
            const removed = existing[i];
            existing.splice(i, 1);
            if (existing.length === 1) this._events[type] = existing[0];
            else if (existing.length === 0) {
              delete this._events[type];
              this._eventsCount--;
            }
            if (this._events.removeListener) {
              this.emit("removeListener", type, removed.listener ?? removed);
            }
            break;
          }
        }
      }
      return this;
    };
    EventEmitter.prototype.off = function (type, listener) {
      return this.removeListener(type, listener);
    };
    EventEmitter.prototype.removeAllListeners = function (type) {
      eventsOf(this);
      if (type === undefined) {
        this._events = { __proto__: null };
        this._eventsCount = 0;
      } else if (this._events[type] !== undefined) {
        delete this._events[type];
        this._eventsCount--;
      }
      return this;
    };
    EventEmitter.prototype.listeners = function (type) {
      return this.rawListeners(type).map((l) => l.listener ?? l);
    };
    EventEmitter.prototype.rawListeners = function (type) {
      const existing = eventsOf(this)[type];
      if (existing === undefined) return [];
      return typeof existing === "function" ? [existing] : existing.slice();
    };
    EventEmitter.prototype.listenerCount = function (type) {
      const existing = eventsOf(this)[type];
      if (existing === undefined) return 0;
      return typeof existing === "function" ? 1 : existing.length;
    };
    EventEmitter.prototype.eventNames = function () {
      return Reflect.ownKeys(eventsOf(this));
    };
    EventEmitter.prototype.emit = function (type, ...args) {
      const events = eventsOf(this);
      if (type === "error" && events[errorMonitor]) {
        for (const l of this.rawListeners(errorMonitor)) l.apply(this, args);
      }
      const existing = events[type];
      if (existing === undefined) {
        if (type === "error") {
          const err = args[0];
          throw err instanceof Error
            ? err
            : new Error(`Unhandled error. (${String(err)})`);
        }
        return false;
      }
      const list = typeof existing === "function" ? [existing] : existing.slice();
      for (const listener of list) listener.apply(this, args);
      return true;
    };
    EventEmitter.defaultMaxListeners = 10;
    EventEmitter.errorMonitor = errorMonitor;

    function once(emitter, type) {
      return new Promise((resolve, reject) => {
        const onEvent = (...args) => {
          emitter.removeListener("error", onError);
          resolve(args);
        };
        const onError = (err) => {
          emitter.removeListener(type, onEvent);
          reject(err);
        };
        emitter.once(type, onEvent);
        if (type !== "error") emitter.once("error", onError);
      });
    }

    function on(emitter, event, options) {
      var signal = options && options.signal;
      var unconsumed = [];
      var waiting = [];
      var error = null;
      var done = false;

      function eventHandler() {
        var args = [];
        for (var i = 0; i < arguments.length; i++) args.push(arguments[i]);
        if (waiting.length > 0) {
          waiting.shift().resolve({ value: args, done: false });
        } else {
          unconsumed.push(args);
        }
      }

      function errorHandler(err) {
        error = err;
        var w = waiting.slice();
        waiting.length = 0;
        for (var i = 0; i < w.length; i++) w[i].reject(err);
      }

      function abortHandler() {
        var err = new Error("The operation was aborted");
        err.code = "ABORT_ERR";
        err.name = "AbortError";
        errorHandler(err);
      }

      emitter.on(event, eventHandler);
      if (event !== "error") emitter.on("error", errorHandler);
      if (signal) {
        if (signal.aborted) { abortHandler(); }
        else { signal.addEventListener("abort", abortHandler, { once: true }); }
      }

      var iterator = {
        next: function() {
          if (unconsumed.length > 0) {
            return Promise.resolve({ value: unconsumed.shift(), done: false });
          }
          if (error) {
            var e = error;
            return Promise.reject(e);
          }
          if (done) {
            return Promise.resolve({ value: undefined, done: true });
          }
          return new Promise(function(resolve, reject) {
            waiting.push({ resolve: resolve, reject: reject });
          });
        },
        return: function() {
          done = true;
          emitter.removeListener(event, eventHandler);
          if (event !== "error") emitter.removeListener("error", errorHandler);
          var w = waiting.slice();
          waiting.length = 0;
          for (var i = 0; i < w.length; i++) w[i].resolve({ value: undefined, done: true });
          return Promise.resolve({ value: undefined, done: true });
        },
        throw: function(err) {
          error = err;
          emitter.removeListener(event, eventHandler);
          if (event !== "error") emitter.removeListener("error", errorHandler);
          return Promise.reject(err);
        },
      };
      iterator[Symbol.asyncIterator] = function() { return iterator; };
      return iterator;
    }

    function getEventListeners(emitter, name) {
      if (typeof emitter.listeners === "function") return emitter.listeners(name);
      return [];
    }

    function setMaxListeners(n) {
      if (arguments.length > 1) {
        for (var i = 1; i < arguments.length; i++) {
          if (typeof arguments[i].setMaxListeners === "function") {
            arguments[i].setMaxListeners(n);
          }
        }
      }
    }

    // require('events') === EventEmitter, with the named forms attached.
    EventEmitter.EventEmitter = EventEmitter;
    EventEmitter.once = once;
    EventEmitter.on = on;
    EventEmitter.getEventListeners = getEventListeners;
    EventEmitter.setMaxListeners = setMaxListeners;
    EventEmitter.listenerCount = (emitter, type) => emitter.listenerCount(type);
    EventEmitter.getMaxListeners = function getMaxListeners(emitterOrTarget) {
      if (typeof emitterOrTarget.getMaxListeners === "function") return emitterOrTarget.getMaxListeners();
      return EventEmitter.defaultMaxListeners;
    };
    EventEmitter.addAbortListener = function addAbortListener(signal, listener) {
      if (signal.aborted) {
        queueMicrotask(() => listener());
        return { [Symbol.dispose]() {} };
      }
      signal.addEventListener("abort", listener, { once: true });
      return { [Symbol.dispose]() { signal.removeEventListener("abort", listener); } };
    };
    EventEmitter.captureRejectionSymbol = Symbol.for("nodejs.rejection");
    class EventEmitterAsyncResource extends EventEmitter {
      constructor(options) {
        super(options);
        this.asyncResource = { type: (options && options.name) || "EventEmitterAsyncResource" };
      }
      get asyncId() { return 0; }
      get triggerAsyncId() { return 0; }
      emitDestroy() { return this; }
    }
    EventEmitter.EventEmitterAsyncResource = EventEmitterAsyncResource;
    return EventEmitter;
  };

  // ----------------------------------------------------------------- path
  function makePathModule(isWin, natives) {
    const sep = isWin ? "\\" : "/";
    const delimiter = isWin ? ";" : ":";
    const isSep = isWin ? (c) => c === "/" || c === "\\" : (c) => c === "/";
    const isDrive = (p, i = 0) =>
      isWin && /[A-Za-z]/.test(p[i] ?? "") && p[i + 1] === ":";

    function assertPath(p) {
      if (typeof p !== "string") {
        throw new TypeError(`Path must be a string. Received ${typeof p}`);
      }
    }

    /// {root, rest}: root is "" (relative), "/" or "\", "C:\", "C:"
    /// (drive-relative), or "\\host\share\".
    function splitRoot(p) {
      if (!isWin) {
        return isSep(p[0]) ? { root: "/", rest: p.slice(1) } : { root: "", rest: p };
      }
      if (p.length >= 2 && isSep(p[0]) && isSep(p[1])) {
        let i = 2;
        while (i < p.length && !isSep(p[i])) i++;
        if (i > 2 && i < p.length) {
          let j = i + 1;
          let k = j;
          while (k < p.length && !isSep(p[k])) k++;
          if (k > j) {
            return {
              root: "\\\\" + p.slice(2, i) + "\\" + p.slice(j, k) + "\\",
              rest: k < p.length ? p.slice(k + 1) : "",
            };
          }
        }
        return { root: "\\", rest: p.slice(1) };
      }
      if (isDrive(p)) {
        // Drive letter case is preserved (Node parity); comparisons that
        // need case-insensitivity fold at the comparison site.
        if (p.length > 2 && isSep(p[2])) {
          return { root: p.slice(0, 2) + "\\", rest: p.slice(3) };
        }
        return { root: p.slice(0, 2), rest: p.slice(2) };
      }
      if (isSep(p[0])) return { root: "\\", rest: p.slice(1) };
      return { root: "", rest: p };
    }

    /// Is this root drive-relative ('C:' with no separator)? Such paths are
    /// NOT absolute: '..' segments survive normalization, and resolve()
    /// keeps scanning for an absolute anchor.
    const isDriveRelativeRoot = (root) => isWin && root.length === 2 && root[1] === ":";

    function normalizeParts(rest, allowAboveRoot) {
      const out = [];
      let part = "";
      const flush = () => {
        if (part === "" || part === ".") {
          // skip
        } else if (part === "..") {
          if (out.length > 0 && out[out.length - 1] !== "..") out.pop();
          else if (allowAboveRoot) out.push("..");
        } else {
          out.push(part);
        }
        part = "";
      };
      for (let i = 0; i < rest.length; i++) {
        if (isSep(rest[i])) flush();
        else part += rest[i];
      }
      flush();
      return out;
    }

    function normalize(p) {
      assertPath(p);
      if (p.length === 0) return ".";
      const { root, rest } = splitRoot(p);
      // '..' survives when there is no ABSOLUTE anchor â€” that includes
      // drive-relative roots ('C:..' stays 'C:..', per Node).
      const parts = normalizeParts(rest, root === "" || isDriveRelativeRoot(root));
      let out = root + parts.join(sep);
      if (out.length === 0) out = ".";
      // Preserve a single trailing separator, per Node.
      if (isSep(p[p.length - 1]) && !isSep(out[out.length - 1]) && out !== ".") {
        out += sep;
      }
      return out;
    }

    function isAbsolute(p) {
      assertPath(p);
      if (!isWin) return isSep(p[0]);
      if (isSep(p[0])) return true;
      return isDrive(p) && isSep(p[2]);
    }

    function join(...args) {
      if (args.length === 0) return ".";
      let joined = "";
      let firstPart = "";
      for (const arg of args) {
        assertPath(arg);
        if (arg.length > 0) {
          if (joined.length === 0) firstPart = arg;
          joined += joined.length > 0 ? sep + arg : arg;
        }
      }
      if (joined.length === 0) return ".";
      if (isWin) {
        // Node's UNC guard: joining must never FABRICATE a network path.
        // join('\\\\', 'host') would otherwise normalize as UNC root
        // '\\\\host'. Only a first argument that itself spells >= 2 leading
        // separators (with a server name following) keeps UNC intent.
        let needsReplace = true;
        let slashCount = 0;
        if (isSep(firstPart[0])) {
          slashCount++;
          if (firstPart.length > 1 && isSep(firstPart[1])) {
            slashCount++;
            if (firstPart.length > 2) {
              if (isSep(firstPart[2])) slashCount++;
              else needsReplace = false; // genuine '\\\\host...' UNC intent
            }
          }
        }
        if (needsReplace) {
          while (slashCount < joined.length && isSep(joined[slashCount])) slashCount++;
          if (slashCount >= 2) joined = sep + joined.slice(slashCount);
        }
      }
      return normalize(joined);
    }

    function resolve(...args) {
      // Node's win32 algorithm: scan right-to-left tracking DEVICE and
      // ABSOLUTENESS separately. An arg on a different device than the one
      // already chosen is skipped; scanning continues until both a device
      // and an absolute anchor are found (so resolve('C:\\\\base','C:file')
      // is 'C:\\\\base\\\\file', and resolve('C:\\\\a','\\\\b') is 'C:\\\\b').
      let device = "";
      let tail = "";
      let absolute = false;
      for (let i = args.length - 1; i >= 0 && !(device !== "" && absolute); i--) {
        const p = args[i];
        assertPath(p);
        if (p.length === 0) continue;
        const { root, rest } = splitRoot(p);
        const argDevice = isWin
          ? isDriveRelativeRoot(root)
            ? root
            : root.length >= 3 && root[1] === ":"
              ? root.slice(0, 2)
              : root.startsWith("\\\\")
                ? root
                : ""
          : root;
        const argAbsolute = root !== "" && !isDriveRelativeRoot(root);
        if (
          device !== "" &&
          argDevice !== "" &&
          argDevice.toUpperCase() !== device.toUpperCase()
        ) {
          continue; // different drive: irrelevant to this resolution
        }
        if (device === "" && argDevice !== "") device = argDevice;
        if (!absolute) {
          tail = rest + (tail.length > 0 ? sep + tail : "");
          absolute = argAbsolute;
        }
      }
      if (!absolute || (isWin && device === "")) {
        // cwd fills whatever is missing: the tail anchor (when nothing was
        // absolute; same-device cwd anchors fully, foreign-device cwd
        // anchors at the device root â€” wave-1 simplification of Node's
        // per-drive cwd tracking) and/or the device (when an absolute
        // driveless '\\x' path needs qualification).
        const cwd = natives ? natives.cwd() : "/";
        const cwdSplit = splitRoot(cwd);
        const cwdDevice =
          isWin && cwdSplit.root.length >= 2 ? cwdSplit.root.slice(0, 2) : cwdSplit.root;
        if (!absolute && (device === "" || cwdDevice.toUpperCase() === device.toUpperCase())) {
          tail = cwdSplit.rest + (tail.length > 0 ? sep + tail : "");
        }
        if (device === "") device = isWin ? cwdDevice : "";
        absolute = true;
      }
      const root = isWin ? (device.startsWith("\\\\") ? device : device + "\\") : "/";
      const parts = normalizeParts(tail, false);
      const out = root + parts.join(sep);
      return out.length > 0 ? out : ".";
    }

    function relative(from, to) {
      assertPath(from);
      assertPath(to);
      const f = resolve(from);
      const t = resolve(to);
      const cmp = (s) => (isWin ? s.toLowerCase() : s);
      if (cmp(f) === cmp(t)) return "";
      const fSplit = splitRoot(f);
      const tSplit = splitRoot(t);
      if (cmp(fSplit.root) !== cmp(tSplit.root)) return t;
      const fParts = fSplit.rest.length > 0 ? fSplit.rest.split(sep) : [];
      const tParts = tSplit.rest.length > 0 ? tSplit.rest.split(sep) : [];
      let common = 0;
      while (
        common < fParts.length &&
        common < tParts.length &&
        cmp(fParts[common]) === cmp(tParts[common])
      ) {
        common++;
      }
      const ups = fParts.length - common;
      const segments = [];
      for (let i = 0; i < ups; i++) segments.push("..");
      segments.push(...tParts.slice(common));
      return segments.join(sep);
    }

    function basename(p, suffix) {
      assertPath(p);
      const { rest } = splitRoot(p);
      let end = rest.length;
      while (end > 0 && isSep(rest[end - 1])) end--;
      let start = end;
      while (start > 0 && !isSep(rest[start - 1])) start--;
      let base = rest.slice(start, end);
      if (suffix !== undefined && base.endsWith(suffix) && base !== suffix) {
        base = base.slice(0, base.length - suffix.length);
      }
      return base;
    }

    function extname(p) {
      const base = basename(p);
      const dot = base.lastIndexOf(".");
      return dot <= 0 ? "" : base.slice(dot);
    }

    function dirname(p) {
      assertPath(p);
      const { root, rest } = splitRoot(p);
      let end = rest.length;
      while (end > 0 && isSep(rest[end - 1])) end--;
      let cut = end;
      while (cut > 0 && !isSep(rest[cut - 1])) cut--;
      while (cut > 0 && isSep(rest[cut - 1])) cut--;
      if (cut === 0) return root.length > 0 ? root : ".";
      return root + rest.slice(0, cut);
    }

    function parse(p) {
      assertPath(p);
      const { root } = splitRoot(p);
      const base = basename(p);
      const ext = extname(p);
      const dir = dirname(p);
      return {
        root,
        dir: dir === "." && !p.startsWith(".") && root === "" && p.indexOf(sep) === -1 ? "" : dir,
        base,
        ext,
        name: base.slice(0, base.length - ext.length),
      };
    }

    function format(obj) {
      const dir = obj.dir || obj.root || "";
      const base = obj.base || (obj.name || "") + (obj.ext || "");
      if (!dir) return base;
      return dir === obj.root ? dir + base : dir + sep + base;
    }

    function toNamespacedPath(p) {
      if (!isWin || typeof p !== "string" || p.length === 0) return p;
      const resolved = resolve(p);
      if (resolved.startsWith("\\\\")) {
        if (resolved.startsWith("\\\\?\\")) return resolved;
        return "\\\\?\\UNC\\" + resolved.slice(2);
      }
      if (isDrive(resolved) && resolved[2] === "\\") return "\\\\?\\" + resolved;
      return p;
    }

    return {
      sep,
      delimiter,
      normalize,
      isAbsolute,
      join,
      resolve,
      relative,
      basename,
      extname,
      dirname,
      parse,
      format,
      toNamespacedPath,
    };
  }

  registry.factories.path = (natives) => {
    const win32 = makePathModule(true, natives);
    const posix = makePathModule(false, natives);
    const mod = natives.platform === "win32" ? { ...win32 } : { ...posix };
    win32.win32 = win32;
    win32.posix = posix;
    posix.win32 = win32;
    posix.posix = posix;
    mod.win32 = win32;
    mod.posix = posix;
    return mod;
  };
  registry.factories["path/posix"] = () => registry.get("path").posix;
  registry.factories["path/win32"] = () => registry.get("path").win32;

  // Node's fs.constants O_* are the platform's fcntl/CRT values: O_CREAT/O_EXCL/
  // O_TRUNC/O_APPEND differ between Linux, Windows (MSVCRT), and macOS (BSD).
  // (O_RDONLY/O_WRONLY/O_RDWR are 0/1/2 everywhere.) numericOpenFlags() must use
  // the same per-platform values, so this is the single source for both.
  function platformOFlags(platform) {
    if (platform === "win32") return { O_CREAT: 256, O_EXCL: 1024, O_TRUNC: 512, O_APPEND: 8 };
    if (platform === "darwin") return { O_CREAT: 512, O_EXCL: 2048, O_TRUNC: 1024, O_APPEND: 8 };
    return { O_CREAT: 64, O_EXCL: 128, O_TRUNC: 512, O_APPEND: 1024 }; // linux + other
  }

  // os.constants.errno is POSITIVE in Node (POSIX errno; ENOENT=2), unlike the
  // libuv-negative codes used elsewhere. Flip the sign of the negative table.
  function positiveErrno(table) {
    const out = {};
    for (const k of Object.keys(table)) out[k] = -table[k];
    return out;
  }

  // ------------------------------------------------------------------- os
  registry.factories.os = (natives) => {
    const isWin = natives.platform === "win32";
    return {
      EOL: isWin ? "\r\n" : "\n",
      devNull: isWin ? "\\\\.\\nul" : "/dev/null",
      platform: () => natives.platform,
      arch: () => natives.arch,
      type: () =>
        natives.platform === "win32"
          ? "Windows_NT"
          : natives.platform === "darwin"
            ? "Darwin"
            : "Linux",
      release: () => natives.osRelease(),
      version: () => natives.osRelease(),
      homedir: () => natives.homedir(),
      tmpdir: () => natives.tmpdir(),
      hostname: () => natives.hostname(),
      // oam targets x86/arm64 (both LE); hardcoded -- revisit for BE platforms
      endianness: () => "LE",
      availableParallelism: () => natives.cpuCount,
      cpus: () => {
        const model = natives.cpuModel();
        const speed = natives.cpuSpeed();
        return Array.from({ length: natives.cpuCount }, () => ({
          model,
          speed,
          times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 },
        }));
      },
      totalmem: () => natives.osTotalMem(),
      freemem: () => natives.osFreeMem(),
      uptime: () => natives.uptimeMs() / 1000,
      loadavg: () => [0, 0, 0],
      networkInterfaces: () => JSON.parse(natives.networkInterfaces()),
      machine: () => {
        var a = natives.arch;
        if (a === "arm64") return "aarch64";
        if (a === "x64") return "x86_64";
        if (a === "ia32") return "i686";
        if (a === "arm") return "armv7l";
        return a;
      },
      userInfo: () => ({
        username: natives.username(),
        homedir: natives.homedir(),
        shell: null,
        uid: -1,
        gid: -1,
      }),
      constants: {
        signals: {
          SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5,
          SIGABRT: 6, SIGBUS: 7, SIGFPE: 8, SIGKILL: 9, SIGUSR1: 10,
          SIGSEGV: 11, SIGUSR2: 12, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15,
          SIGCHLD: 17, SIGCONT: 18, SIGSTOP: 19, SIGTSTP: 20, SIGTTIN: 21,
          SIGTTOU: 22, SIGURG: 23, SIGXCPU: 24, SIGXFSZ: 25, SIGVTALRM: 26,
          SIGPROF: 27, SIGWINCH: 28, SIGIO: 29, SIGINFO: 29, SIGSYS: 31,
        },
        errno: positiveErrno({
          E2BIG: -7, EACCES: -13, EADDRINUSE: -98, EADDRNOTAVAIL: -99,
          EAFNOSUPPORT: -97, EAGAIN: -11, EALREADY: -114, EBADF: -9,
          EBADMSG: -74, EBUSY: -16, ECANCELED: -125, ECHILD: -10,
          ECONNABORTED: -103, ECONNREFUSED: -111, ECONNRESET: -104,
          EDEADLK: -35, EDESTADDRREQ: -89, EDOM: -33, EDQUOT: -122,
          EEXIST: -17, EFAULT: -14, EFBIG: -27, EHOSTUNREACH: -113,
          EIDRM: -43, EILSEQ: -84, EINPROGRESS: -115, EINTR: -4,
          EINVAL: -22, EIO: -5, EISCONN: -106, EISDIR: -21,
          ELOOP: -40, EMFILE: -24, EMLINK: -31, EMSGSIZE: -90,
          EMULTIHOP: -72, ENAMETOOLONG: -36, ENETDOWN: -100,
          ENETRESET: -102, ENETUNREACH: -101, ENFILE: -23, ENOBUFS: -105,
          ENODATA: -61, ENODEV: -19, ENOENT: -2, ENOEXEC: -8,
          ENOLCK: -37, ENOLINK: -67, ENOMEM: -12, ENOMSG: -42,
          ENOPROTOOPT: -92, ENOSPC: -28, ENOSR: -63, ENOSTR: -60,
          ENOSYS: -38, ENOTCONN: -107, ENOTDIR: -20, ENOTEMPTY: -39,
          ENOTSOCK: -88, ENOTSUP: -95, ENOTTY: -25, ENXIO: -6,
          EOPNOTSUPP: -95, EOVERFLOW: -75, EPERM: -1, EPIPE: -32,
          EPROTO: -71, EPROTONOSUPPORT: -93, EPROTOTYPE: -91,
          ERANGE: -34, EROFS: -30, ESPIPE: -29, ESRCH: -3,
          ESTALE: -116, ETIME: -62, ETIMEDOUT: -110, ETXTBSY: -26,
          EWOULDBLOCK: -11, EXDEV: -18,
        }),
        priority: {
          PRIORITY_LOW: 19, PRIORITY_BELOW_NORMAL: 10, PRIORITY_NORMAL: 0,
          PRIORITY_ABOVE_NORMAL: -7, PRIORITY_HIGH: -14, PRIORITY_HIGHEST: -20,
        },
      },
    };
  };

  // ------------------------------------------------------------ constants
  // Legacy node:constants -- the deprecated flat union of the os signal /
  // errno / priority constants, the fs flags (O_*/S_*/F_OK...), and crypto's
  // constants. Still require()'d by graceful-fs and other older fs packages.
  registry.factories.constants = () => {
    const os = registry.get("os");
    const fs = registry.get("fs");
    const merged = {
      ...os.constants.signals,
      ...os.constants.errno,
      ...os.constants.priority,
      ...fs.constants,
    };
    try {
      const cryptoConstants = registry.get("crypto").constants;
      if (cryptoConstants) Object.assign(merged, cryptoConstants);
    } catch {
      // crypto constants are optional for the legacy module
    }
    return merged;
  };

  // ----------------------------------------------------------------- util
  registry.factories.util = (natives) => {
    function inspect(value, options = {}) {
      const depth = options.depth === undefined ? 2 : options.depth;
      const seen = new Set();

      function walk(v, level) {
        if (v === null) return "null";
        const t = typeof v;
        if (t === "string") return level === 0 && options.bare ? v : `'${v}'`;
        if (t === "number" || t === "boolean" || t === "undefined") return String(v);
        if (t === "bigint") return `${v}n`;
        if (t === "symbol") return v.toString();
        if (t === "function") {
          const name = v.name ? `: ${v.name}` : " (anonymous)";
          const kind = String(v).startsWith("class") ? "class" : "Function";
          return `[${kind}${name}]`;
        }
        if (v instanceof Date) {
          return Number.isNaN(v.getTime()) ? "Invalid Date" : v.toISOString();
        }
        if (v instanceof RegExp) return v.toString();
        if (v instanceof Error) return v.stack || `${v.name}: ${v.message}`;
        if (globalThis.Buffer && v instanceof globalThis.Buffer) return v.inspect();
        if (seen.has(v)) return "[Circular *1]";
        if (depth !== null && level > depth) {
          return Array.isArray(v) ? "[Array]" : "[Object]";
        }
        seen.add(v);
        try {
          if (Array.isArray(v)) {
            const max = 100;
            const items = v.slice(0, max).map((item) => walk(item, level + 1));
            if (v.length > max) items.push(`... ${v.length - max} more items`);
            return `[ ${items.join(", ")} ]`;
          }
          if (v instanceof Map) {
            const items = [];
            for (const [k, val] of v) {
              items.push(`${walk(k, level + 1)} => ${walk(val, level + 1)}`);
              if (items.length >= 100) break;
            }
            return `Map(${v.size}) { ${items.join(", ")} }`;
          }
          if (v instanceof Set) {
            const items = [];
            for (const val of v) {
              items.push(walk(val, level + 1));
              if (items.length >= 100) break;
            }
            return `Set(${v.size}) { ${items.join(", ")} }`;
          }
          if (ArrayBuffer.isView(v)) {
            const name = v.constructor?.name ?? "TypedArray";
            const items = Array.from(v.subarray ? v.subarray(0, 50) : []).join(", ");
            return `${name}(${v.length ?? v.byteLength}) [ ${items} ]`;
          }
          const ctor = v.constructor?.name;
          const prefix = ctor && ctor !== "Object" ? `${ctor} ` : "";
          const keys = Reflect.ownKeys(v).filter(
            (k) => Object.getOwnPropertyDescriptor(v, k)?.enumerable,
          );
          if (keys.length === 0) return `${prefix}{}`;
          const items = keys.slice(0, 200).map((k) => {
            const keyText =
              typeof k === "symbol"
                ? `[${k.toString()}]`
                : /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k)
                  ? k
                  : `'${k}'`;
            const desc = Object.getOwnPropertyDescriptor(v, k);
            const valText = desc.get
              ? "[Getter]"
              : walk(v[k], level + 1);
            return `${keyText}: ${valText}`;
          });
          return `${prefix}{ ${items.join(", ")} }`;
        } finally {
          seen.delete(v);
        }
      }
      return walk(value, 0);
    }

    function formatValue(v) {
      return typeof v === "string" ? v : inspect(v);
    }

    function format(f, ...args) {
      if (typeof f !== "string") {
        return [f, ...args].map(formatValue).join(" ");
      }
      // Node fast-path: a lone format string with NO substitution args is
      // returned verbatim ('%%' stays '%%').
      if (args.length === 0) return f;
      let i = 0;
      let out = f.replace(/%[sdifjoO%]/g, (spec) => {
        if (spec === "%%") return "%";
        if (i >= args.length) return spec;
        const arg = args[i++];
        switch (spec) {
          case "%s":
            return typeof arg === "string" ? arg : inspect(arg, { bare: true });
          case "%d":
            if (typeof arg === "symbol") return "NaN"; // Number(Symbol) throws; Node prints NaN
            return typeof arg === "bigint" ? `${arg}n` : String(Number(arg));
          case "%i":
            if (typeof arg === "symbol") return "NaN";
            return typeof arg === "bigint" ? `${arg}n` : String(parseInt(arg, 10));
          case "%f":
            if (typeof arg === "symbol") return "NaN";
            return String(parseFloat(arg));
          case "%j":
            try {
              return JSON.stringify(arg);
            } catch {
              return "[Circular]";
            }
          case "%o":
          case "%O":
            return inspect(arg);
          default:
            return spec;
        }
      });
      for (; i < args.length; i++) out += " " + formatValue(args[i]);
      return out;
    }

    const customPromisify = Symbol.for("nodejs.util.promisify.custom");
    function promisify(original) {
      if (typeof original !== "function") {
        throw new TypeError('The "original" argument must be of type function');
      }
      if (original[customPromisify]) return original[customPromisify];
      function promisified(...args) {
        return new Promise((resolve, reject) => {
          original.call(this, ...args, (err, value) => {
            if (err) reject(err);
            else resolve(value);
          });
        });
      }
      Object.defineProperty(promisified, "name", { value: original.name });
      return promisified;
    }
    promisify.custom = customPromisify;

    function callbackify(original) {
      function callbackified(...args) {
        const cb = args.pop();
        original.apply(this, args).then(
          (value) => queueMicrotask(() => cb(null, value)),
          (reason) =>
            queueMicrotask(() =>
              cb(reason ?? new Error("Promise was rejected with falsy value")),
            ),
        );
      }
      Object.defineProperty(callbackified, "name", { value: original.name });
      return callbackified;
    }

    function inherits(ctor, superCtor) {
      Object.defineProperty(ctor, "super_", { value: superCtor, writable: true, configurable: true });
      Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
    }

    function deprecate(fn, msg) {
      let warned = false;
      function deprecated(...args) {
        if (!warned) {
          warned = true;
          if (globalThis.console) globalThis.console.warn(`DeprecationWarning: ${msg}`);
        }
        return fn.apply(this, args);
      }
      return deprecated;
    }

    function debuglog(section) {
      const enabled = () => {
        const debug = globalThis.process?.env?.NODE_DEBUG ?? "";
        return debug
          .split(",")
          .some((s) => s.trim().toLowerCase() === String(section).toLowerCase());
      };
      const logger = (...args) => {
        if (enabled() && globalThis.console) {
          globalThis.console.error(`${String(section).toUpperCase()}: ${format(...args)}`);
        }
      };
      logger.enabled = enabled();
      return logger;
    }

    function deepEqualImpl(a, b, strict, memo) {
      // Strict is SameValue (Object.is): NaN equals NaN, +0 does NOT
      // equal -0 â€” exactly Node's deepStrictEqual primitive rule.
      const primitiveEqual = strict
        ? Object.is
        : // eslint-disable-next-line eqeqeq
          (x, y) => x == y || (Number.isNaN(x) && Number.isNaN(y));
      if (a === null || b === null || typeof a !== "object" || typeof b !== "object") {
        return primitiveEqual(a, b);
      }
      if (a === b) return true;
      if (strict && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
      if (a instanceof Date) return b instanceof Date && a.getTime() === b.getTime();
      if (a instanceof RegExp) {
        return b instanceof RegExp && a.source === b.source && a.flags === b.flags;
      }
      if (ArrayBuffer.isView(a) || ArrayBuffer.isView(b)) {
        if (!ArrayBuffer.isView(a) || !ArrayBuffer.isView(b)) return false;
        const ua = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
        const ub = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
        if (ua.length !== ub.length) return false;
        for (let i = 0; i < ua.length; i++) if (ua[i] !== ub[i]) return false;
        return true;
      }
      memo = memo ?? new Map();
      const prior = memo.get(a);
      if (prior && prior.has(b)) return true;
      if (prior) prior.add(b);
      else memo.set(a, new Set([b]));

      if (Array.isArray(a)) {
        if (!Array.isArray(b) || a.length !== b.length) return false;
        for (let i = 0; i < a.length; i++) {
          if (!deepEqualImpl(a[i], b[i], strict, memo)) return false;
        }
        return true;
      }
      if (a instanceof Map) {
        if (!(b instanceof Map) || a.size !== b.size) return false;
        for (const [k, v] of a) {
          if (b.has(k)) {
            if (!deepEqualImpl(v, b.get(k), strict, memo)) return false;
          } else {
            let found = false;
            for (const [bk, bv] of b) {
              if (deepEqualImpl(k, bk, strict, memo) && deepEqualImpl(v, bv, strict, memo)) {
                found = true;
                break;
              }
            }
            if (!found) return false;
          }
        }
        return true;
      }
      if (a instanceof Set) {
        if (!(b instanceof Set) || a.size !== b.size) return false;
        for (const v of a) {
          if (b.has(v)) continue;
          let found = false;
          for (const bv of b) {
            if (deepEqualImpl(v, bv, strict, memo)) {
              found = true;
              break;
            }
          }
          if (!found) return false;
        }
        return true;
      }
      if (a instanceof Error) {
        if (!(b instanceof Error) || a.name !== b.name || a.message !== b.message) {
          return false;
        }
      }
      const aKeys = Object.keys(a);
      const bKeys = Object.keys(b);
      if (aKeys.length !== bKeys.length) return false;
      for (const key of aKeys) {
        if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
        if (!deepEqualImpl(a[key], b[key], strict, memo)) return false;
      }
      return true;
    }

    function parseArgs(config) {
      config = config || {};
      var argv = config.args || globalThis.process.argv.slice(2);
      var options = config.options || {};
      var strict = config.strict !== false;
      var allowPositionals = config.allowPositionals !== false;
      var tokens = [];
      var values = {};
      var positionals = [];

      for (var name in options) {
        if (options[name].default !== undefined) {
          values[name] = options[name].default;
        }
      }

      var i = 0;
      while (i < argv.length) {
        var arg = argv[i];

        if (arg === "--") {
          tokens.push({ kind: "option-terminator", index: i });
          i++;
          while (i < argv.length) {
            positionals.push(argv[i]);
            tokens.push({ kind: "positional", index: i, value: argv[i] });
            i++;
          }
          break;
        }

        if (arg.startsWith("--")) {
          var eqIdx = arg.indexOf("=");
          var longName, longValue;
          if (eqIdx !== -1) {
            longName = arg.slice(2, eqIdx);
            longValue = arg.slice(eqIdx + 1);
          } else {
            longName = arg.slice(2);
            longValue = undefined;
          }
          var optDef = options[longName];
          if (strict && !optDef) {
            throw new TypeError("Unknown option '--" + longName + "'");
          }
          var optType = optDef ? optDef.type : "boolean";
          if (optType === "string") {
            if (longValue === undefined) {
              i++;
              if (i >= argv.length) {
                throw new TypeError("Option '--" + longName + "' requires a value");
              }
              longValue = argv[i];
            }
            if (optDef && optDef.multiple) {
              if (!Array.isArray(values[longName])) values[longName] = [];
              values[longName].push(longValue);
            } else {
              values[longName] = longValue;
            }
          } else {
            if (longValue !== undefined && strict) {
              throw new TypeError("Option '--" + longName + "' does not take a value");
            }
            if (optDef && optDef.multiple) {
              if (!Array.isArray(values[longName])) values[longName] = [];
              values[longName].push(true);
            } else {
              values[longName] = true;
            }
          }
          tokens.push({ kind: "option", name: longName, value: values[longName], index: i - (longValue !== undefined && eqIdx === -1 ? 1 : 0) });
          i++;
          continue;
        }

        if (arg.startsWith("-") && arg.length > 1 && arg[1] !== "-") {
          var shortChars = arg.slice(1);
          for (var ci = 0; ci < shortChars.length; ci++) {
            var ch = shortChars[ci];
            var shortName = null;
            for (var oName in options) {
              if (options[oName].short === ch) {
                shortName = oName;
                break;
              }
            }
            if (strict && !shortName) {
              throw new TypeError("Unknown option '-" + ch + "'");
            }
            if (!shortName) shortName = ch;
            var sDef = options[shortName];
            var sType = sDef ? sDef.type : "boolean";
            if (sType === "string") {
              var sVal;
              if (ci + 1 < shortChars.length) {
                sVal = shortChars.slice(ci + 1);
                ci = shortChars.length;
              } else {
                i++;
                if (i >= argv.length) {
                  throw new TypeError("Option '-" + ch + "' requires a value");
                }
                sVal = argv[i];
              }
              if (sDef && sDef.multiple) {
                if (!Array.isArray(values[shortName])) values[shortName] = [];
                values[shortName].push(sVal);
              } else {
                values[shortName] = sVal;
              }
              tokens.push({ kind: "option", name: shortName, value: values[shortName], index: i });
            } else {
              if (sDef && sDef.multiple) {
                if (!Array.isArray(values[shortName])) values[shortName] = [];
                values[shortName].push(true);
              } else {
                values[shortName] = true;
              }
              tokens.push({ kind: "option", name: shortName, value: true, index: i });
            }
          }
          i++;
          continue;
        }

        if (!allowPositionals && strict) {
          throw new TypeError("Unexpected argument '" + arg + "'");
        }
        positionals.push(arg);
        tokens.push({ kind: "positional", index: i, value: arg });
        i++;
      }

      var result = { values: values, positionals: positionals };
      if (config.tokens) result.tokens = tokens;
      return result;
    }

    // util.getCallSites([frameCount[, options]]) (Node >=22.9): structured
    // current call stack. Built on V8's CallSite API (Error.prepareStackTrace +
    // captureStackTrace, both supported by oam). Named so captureStackTrace can
    // exclude getCallSites' own frame -> [0] is the immediate caller (Node parity).
    function getCallSites(frameCountOrOptions, options) {
      let frameCount = 10;
      if (typeof frameCountOrOptions === "number") frameCount = frameCountOrOptions;
      else if (frameCountOrOptions && typeof frameCountOrOptions === "object") {
        options = frameCountOrOptions;
      }
      void options; // the sourceMap option is not yet applied
      const target = {};
      const originalPrepare = Error.prepareStackTrace;
      const originalLimit = Error.stackTraceLimit;
      Error.prepareStackTrace = (_e, frames) => frames;
      Error.stackTraceLimit = Math.max(frameCount + 1, 10);
      try {
        Error.captureStackTrace(target, getCallSites);
        const frames = Array.isArray(target.stack) ? target.stack : [];
        return frames.slice(0, frameCount).map((f) => {
          const sourceURL =
            typeof f.getScriptNameOrSourceURL === "function" ? f.getScriptNameOrSourceURL() : null;
          const scriptName = sourceURL || f.getFileName() || "";
          const columnNumber = f.getColumnNumber() || 0;
          let scriptId = "";
          if (typeof f.getScriptId === "function") {
            const id = f.getScriptId();
            if (id !== null && id !== undefined) scriptId = String(id);
          }
          return {
            functionName: f.getFunctionName() || "",
            scriptId,
            scriptName,
            lineNumber: f.getLineNumber() || 0,
            columnNumber,
            column: columnNumber,
          };
        });
      } finally {
        Error.prepareStackTrace = originalPrepare;
        Error.stackTraceLimit = originalLimit;
      }
    }

    return {
      format,
      formatWithOptions: (_opts, ...args) => format(...args),
      inspect,
      getCallSites,
      parseArgs,
      aborted: (signal, resource) => {
        return new Promise((resolve) => {
          if (signal.aborted) { resolve(signal.reason); return; }
          signal.addEventListener("abort", () => resolve(signal.reason), { once: true });
        });
      },
      parseEnv: (content) => {
        var result = Object.create(null);
        var LF = String.fromCharCode(10);
        var rawLines = String(content).split(LF);
        for (var i = 0; i < rawLines.length; i++) {
          var line = rawLines[i];
          if (line.length > 0 && line.charCodeAt(line.length - 1) === 13) line = line.slice(0, -1);
          line = line.trim();
          if (!line || line.charAt(0) === "#") continue;
          var eq = line.indexOf("=");
          if (eq === -1) continue;
          var key = line.slice(0, eq).trim();
          var val = line.slice(eq + 1).trim();
          var DQ = String.fromCharCode(34); var SQ = String.fromCharCode(39);
          if (val.length >= 2 && ((val.charAt(0) === DQ && val.charAt(val.length - 1) === DQ) || (val.charAt(0) === SQ && val.charAt(val.length - 1) === SQ))) val = val.slice(1, -1);
          result[key] = val;
        }
        return result;
      },
      MIMEType: function MIMETypeClass(input) {
        if (!(this instanceof MIMETypeClass)) throw new TypeError("MIMEType is a constructor, call with new");
        var str = String(input).trim();
        var semi = str.indexOf(";");
        var essence = semi === -1 ? str : str.slice(0, semi).trim();
        var slash = essence.indexOf("/");
        if (slash === -1) throw new TypeError("Invalid MIME type: " + input);
        this.type = essence.slice(0, slash).toLowerCase();
        this.subtype = essence.slice(slash + 1).toLowerCase();
        this.essence = this.type + "/" + this.subtype;
        var params = new Map();
        if (semi !== -1) {
          var rest = str.slice(semi + 1);
          var parts = rest.split(";");
          for (var i = 0; i < parts.length; i++) {
            var part = parts[i].trim();
            if (!part) continue;
            var eq = part.indexOf("=");
            if (eq === -1) continue;
            var k = part.slice(0, eq).trim().toLowerCase();
            var v = part.slice(eq + 1).trim();
            if (v.length >= 2 && v.charAt(0) === '"' && v.charAt(v.length - 1) === '"') v = v.slice(1, -1);
            params.set(k, v);
          }
        }
        this.params = { get: function(k) { return params.get(k.toLowerCase()); }, set: function(k, v) { params.set(k.toLowerCase(), v); }, has: function(k) { return params.has(k.toLowerCase()); }, delete: function(k) { return params.delete(k.toLowerCase()); }, entries: function() { return params.entries(); }, keys: function() { return params.keys(); }, values: function() { return params.values(); }, forEach: function(fn) { params.forEach(fn); } };
        this.toString = function() {
          var s = this.type + "/" + this.subtype;
          params.forEach(function(v, k) { s += ";" + k + "=" + v; });
          return s;
        };
      },
      promisify,
      callbackify,
      inherits,
      deprecate,
      debuglog,
      debug: debuglog,
      isArray: Array.isArray,
      isDeepStrictEqual: (a, b) => deepEqualImpl(a, b, true),
      toUSVString: (val) => {
        var s = String(val);
        if (typeof s.toWellFormed === "function") return s.toWellFormed();
        return s.replace(/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g, "\uFFFD");
      },
      _deepEqual: deepEqualImpl,
      stripVTControlCharacters: (str) =>
        // eslint-disable-next-line no-control-regex
        String(str).replace(/\[[0-9;]*[A-Za-z]/g, ""),
      TextEncoder: globalThis.TextEncoder,
      TextDecoder: globalThis.TextDecoder,
      getSystemErrorName: function getSystemErrorName(err) {
        var map = {
          [-1]: "EOF", [-2]: "ENOENT", [-3]: "EACCES", [-4]: "EEXIST",
          [-5]: "ENOTDIR", [-6]: "EISDIR", [-7]: "ENOTEMPTY", [-8]: "EPERM",
          [-9]: "EBADF", [-10]: "EINVAL", [-11]: "ENOMEM", [-12]: "EBUSY",
          [-13]: "EAGAIN", [-14]: "ENOSYS", [-15]: "EMFILE", [-16]: "ENFILE",
          [-17]: "EADDRINUSE", [-18]: "EADDRNOTAVAIL", [-19]: "ECONNREFUSED",
          [-20]: "ECONNRESET", [-21]: "ECONNABORTED", [-22]: "EPIPE",
          [-23]: "ETIMEDOUT", [-24]: "ENETUNREACH", [-25]: "EHOSTUNREACH",
          [-26]: "ELOOP", [-27]: "ENAMETOOLONG", [-28]: "ERANGE",
        };
        return map[err] || ("Unknown system error " + err);
      },
      getSystemErrorMap: function getSystemErrorMap() {
        return new Map([
          [-1, ["EOF", "end of file"]],
          [-2, ["ENOENT", "no such file or directory"]],
          [-3, ["EACCES", "permission denied"]],
          [-4, ["EEXIST", "file already exists"]],
          [-5, ["ENOTDIR", "not a directory"]],
          [-6, ["EISDIR", "illegal operation on a directory"]],
          [-7, ["ENOTEMPTY", "directory not empty"]],
          [-8, ["EPERM", "operation not permitted"]],
          [-9, ["EBADF", "bad file descriptor"]],
          [-10, ["EINVAL", "invalid argument"]],
          [-11, ["ENOMEM", "not enough memory"]],
          [-12, ["EBUSY", "resource busy or locked"]],
          [-13, ["EAGAIN", "resource temporarily unavailable"]],
          [-17, ["EADDRINUSE", "address already in use"]],
          [-19, ["ECONNREFUSED", "connection refused"]],
          [-20, ["ECONNRESET", "connection reset by peer"]],
          [-22, ["EPIPE", "broken pipe"]],
          [-23, ["ETIMEDOUT", "connection timed out"]],
        ]);
      },
      styleText: function styleText(format, text) {
        var ESC = String.fromCharCode(27);
        var codes = {
          reset: [0, 0],
          bold: [1, 22],
          dim: [2, 22],
          italic: [3, 23],
          underline: [4, 24],
          inverse: [7, 27],
          hidden: [8, 28],
          strikethrough: [9, 29],
          black: [30, 39],
          red: [31, 39],
          green: [32, 39],
          yellow: [33, 39],
          blue: [34, 39],
          magenta: [35, 39],
          cyan: [36, 39],
          white: [37, 39],
          gray: [90, 39],
          grey: [90, 39],
          redBright: [91, 39],
          greenBright: [92, 39],
          yellowBright: [93, 39],
          blueBright: [94, 39],
          magentaBright: [95, 39],
          cyanBright: [96, 39],
          whiteBright: [97, 39],
          bgBlack: [40, 49],
          bgRed: [41, 49],
          bgGreen: [42, 49],
          bgYellow: [43, 49],
          bgBlue: [44, 49],
          bgMagenta: [45, 49],
          bgCyan: [46, 49],
          bgWhite: [47, 49],
        };
        if (Array.isArray(format)) {
          var result = String(text);
          for (var fi = 0; fi < format.length; fi++) {
            var c = codes[format[fi]];
            if (c) result = ESC + "[" + c[0] + "m" + result + ESC + "[" + c[1] + "m";
          }
          return result;
        }
        var pair = codes[format];
        if (!pair) return String(text);
        return ESC + "[" + pair[0] + "m" + String(text) + ESC + "[" + pair[1] + "m";
      },
      log: function utilLog() {
        var d = new Date();
        var ts = d.getUTCDate() + " " + ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"][d.getUTCMonth()] + " " + ("0" + d.getUTCHours()).slice(-2) + ":" + ("0" + d.getUTCMinutes()).slice(-2) + ":" + ("0" + d.getUTCSeconds()).slice(-2);
        console.log(ts + " - " + Array.prototype.join.call(arguments, " "));
      },
      isRegExp: (v) => v instanceof RegExp,
      isDate: (v) => v instanceof Date,
      isError: (v) => v instanceof Error,
      isPrimitive: (v) => v === null || (typeof v !== "object" && typeof v !== "function"),
      isBuffer: (v) => globalThis.Buffer.isBuffer(v),
      isFunction: (v) => typeof v === "function",
      isObject: (v) => typeof v === "object" && v !== null,
      isNullOrUndefined: (v) => v === null || v === undefined,
      isString: (v) => typeof v === "string",
      isNumber: (v) => typeof v === "number",
      isBoolean: (v) => typeof v === "boolean",
      isNull: (v) => v === null,
      isUndefined: (v) => v === undefined,
      isSymbol: (v) => typeof v === "symbol",
      types: {
        isDate: (v) => v instanceof Date,
        isRegExp: (v) => v instanceof RegExp,
        isNativeError: (v) => v instanceof Error,
        isPromise: (v) => v instanceof Promise,
        isMap: (v) => v instanceof Map,
        isSet: (v) => v instanceof Set,
        isWeakMap: (v) => v instanceof WeakMap,
        isWeakSet: (v) => v instanceof WeakSet,
        isArrayBuffer: (v) => v instanceof ArrayBuffer,
        isSharedArrayBuffer: (v) =>
          typeof SharedArrayBuffer !== "undefined" && v instanceof SharedArrayBuffer,
        isAnyArrayBuffer: (v) =>
          v instanceof ArrayBuffer ||
          (typeof SharedArrayBuffer !== "undefined" && v instanceof SharedArrayBuffer),
        isTypedArray: (v) => ArrayBuffer.isView(v) && !(v instanceof DataView),
        isUint8Array: (v) => v instanceof Uint8Array,
        isDataView: (v) => v instanceof DataView,
        isAsyncFunction: (v) =>
          typeof v === "function" && v.constructor?.name === "AsyncFunction",
        isGeneratorFunction: (v) =>
          typeof v === "function" && v.constructor?.name === "GeneratorFunction",
        isProxy: () => false,
        isArrayBufferView: (v) => ArrayBuffer.isView(v),
        isUint8ClampedArray: (v) => v instanceof Uint8ClampedArray,
        isUint16Array: (v) => v instanceof Uint16Array,
        isUint32Array: (v) => v instanceof Uint32Array,
        isInt8Array: (v) => v instanceof Int8Array,
        isInt16Array: (v) => v instanceof Int16Array,
        isInt32Array: (v) => v instanceof Int32Array,
        isFloat32Array: (v) => v instanceof Float32Array,
        isFloat64Array: (v) => v instanceof Float64Array,
        isBigInt64Array: (v) => typeof BigInt64Array !== "undefined" && v instanceof BigInt64Array,
        isBigUint64Array: (v) => typeof BigUint64Array !== "undefined" && v instanceof BigUint64Array,
        isMapIterator: (v) => { try { Map.prototype.has.call(v); return false; } catch { return String(v) === "[object Map Iterator]"; } },
        isSetIterator: (v) => { try { Set.prototype.has.call(v); return false; } catch { return String(v) === "[object Set Iterator]"; } },
        isGeneratorObject: (v) => v != null && typeof v.next === "function" && typeof v.throw === "function" && typeof v[Symbol.iterator] === "function",
        isWeakRef: (v) => v instanceof WeakRef,
        isModuleNamespaceObject: () => false,
        isExternal: () => false,
        isArgumentsObject: (v) => Object.prototype.toString.call(v) === "[object Arguments]",
        isBooleanObject: (v) => v instanceof Boolean,
        isNumberObject: (v) => v instanceof Number,
        isStringObject: (v) => v instanceof String,
        isSymbolObject: (v) => Object.prototype.toString.call(v) === "[object Symbol]" && typeof v === "object",
        isCryptoKey: (v) => typeof CryptoKey !== "undefined" && v instanceof CryptoKey,
        isKeyObject: () => false,
        isBoxedPrimitive: (v) =>
          v instanceof Number || v instanceof String || v instanceof Boolean,
      },
    };
  };

  // --------------------------------------------------------------- assert
  registry.factories["util/types"] = () => registry.get("util").types;

  registry.factories["assert/strict"] = () => registry.get("assert").strict;

    registry.factories.assert = () => {
    const util = registry.get("util");
    const deepEqual = util._deepEqual;

    class AssertionError extends Error {
      constructor(options) {
        super(
          options.message ||
            `${util.inspect(options.actual)} ${options.operator} ${util.inspect(options.expected)}`,
        );
        this.name = "AssertionError";
        this.code = "ERR_ASSERTION";
        this.actual = options.actual;
        this.expected = options.expected;
        this.operator = options.operator;
        this.generatedMessage = !options.message;
      }
    }

    function innerFail(actual, expected, message, operator) {
      throw new AssertionError({
        actual,
        expected,
        message: message instanceof Error ? undefined : message,
        operator,
      });
    }

    function ok(value, message) {
      if (!value) {
        if (message instanceof Error) throw message;
        throw new AssertionError({
          actual: value,
          expected: true,
          message:
            message ?? "The expression evaluated to a falsy value",
          operator: "==",
        });
      }
    }

    function checkExpected(err, expected) {
      if (expected instanceof RegExp) {
        return expected.test(err instanceof Error ? err.message : String(err));
      }
      if (typeof expected === "function") {
        if (expected.prototype !== undefined && err instanceof expected) return true;
        if (expected === Error || Object.getPrototypeOf(expected) === Error) {
          return err instanceof expected;
        }
        return expected(err) === true;
      }
      if (expected && typeof expected === "object") {
        for (const key of Object.keys(expected)) {
          const want = expected[key];
          const got = err?.[key];
          if (want instanceof RegExp) {
            if (!want.test(String(got))) return false;
          } else if (!deepEqual(got, want, true)) {
            return false;
          }
        }
        return true;
      }
      return true;
    }

    function throws(fn, expected, message) {
      if (typeof expected === "string") {
        message = expected;
        expected = undefined;
      }
      let threw = false;
      let thrown;
      try {
        fn();
      } catch (e) {
        threw = true;
        thrown = e;
      }
      if (!threw) {
        throw new AssertionError({
          actual: undefined,
          expected,
          message: message ?? "Missing expected exception",
          operator: "throws",
        });
      }
      if (expected !== undefined && !checkExpected(thrown, expected)) {
        throw thrown instanceof AssertionError
          ? thrown
          : new AssertionError({
              actual: thrown,
              expected,
              message: message ?? `The error does not match the expected pattern`,
              operator: "throws",
            });
      }
      return thrown;
    }

    async function rejects(fnOrPromise, expected, message) {
      if (typeof expected === "string") {
        message = expected;
        expected = undefined;
      }
      let rejected = false;
      let reason;
      try {
        await (typeof fnOrPromise === "function" ? fnOrPromise() : fnOrPromise);
      } catch (e) {
        rejected = true;
        reason = e;
      }
      if (!rejected) {
        throw new AssertionError({
          actual: undefined,
          expected,
          message: message ?? "Missing expected rejection",
          operator: "rejects",
        });
      }
      if (expected !== undefined && !checkExpected(reason, expected)) {
        throw new AssertionError({
          actual: reason,
          expected,
          message: message ?? "The rejection reason does not match the expected pattern",
          operator: "rejects",
        });
      }
      return reason;
    }

    const assert = Object.assign(ok, {
      AssertionError,
      ok,
      fail: (message) => {
        if (message instanceof Error) throw message;
        throw new AssertionError({ message: message ?? "Failed", operator: "fail" });
      },
      equal: (actual, expected, message) => {
        // eslint-disable-next-line eqeqeq
        if (!(actual == expected || (Number.isNaN(actual) && Number.isNaN(expected)))) {
          innerFail(actual, expected, message, "==");
        }
      },
      notEqual: (actual, expected, message) => {
        // eslint-disable-next-line eqeqeq
        if (actual == expected) innerFail(actual, expected, message, "!=");
      },
      strictEqual: (actual, expected, message) => {
        if (!Object.is(actual, expected)) {
          innerFail(actual, expected, message, "strictEqual");
        }
      },
      notStrictEqual: (actual, expected, message) => {
        if (Object.is(actual, expected)) {
          innerFail(actual, expected, message, "notStrictEqual");
        }
      },
      deepEqual: (actual, expected, message) => {
        if (!deepEqual(actual, expected, false)) {
          innerFail(actual, expected, message, "deepEqual");
        }
      },
      notDeepEqual: (actual, expected, message) => {
        if (deepEqual(actual, expected, false)) {
          innerFail(actual, expected, message, "notDeepEqual");
        }
      },
      deepStrictEqual: (actual, expected, message) => {
        if (!deepEqual(actual, expected, true)) {
          innerFail(actual, expected, message, "deepStrictEqual");
        }
      },
      notDeepStrictEqual: (actual, expected, message) => {
        if (deepEqual(actual, expected, true)) {
          innerFail(actual, expected, message, "notDeepStrictEqual");
        }
      },
      throws,
      doesNotThrow: (fn, message) => {
        try {
          fn();
        } catch (e) {
          throw new AssertionError({
            actual: e,
            expected: undefined,
            message: message ?? `Got unwanted exception: ${e?.message ?? e}`,
            operator: "doesNotThrow",
          });
        }
      },
      rejects,
      doesNotReject: async (fnOrPromise, message) => {
        try {
          await (typeof fnOrPromise === "function" ? fnOrPromise() : fnOrPromise);
        } catch (e) {
          throw new AssertionError({
            actual: e,
            expected: undefined,
            message: message ?? `Got unwanted rejection: ${e?.message ?? e}`,
            operator: "doesNotReject",
          });
        }
      },
      match: (string, regexp, message) => {
        if (!regexp.test(string)) {
          innerFail(string, regexp, message ?? `The input did not match the regular expression`, "match");
        }
      },
      ifError: (value) => {
        if (value !== null && value !== undefined) {
          if (value instanceof Error) throw value;
          var e = new Error("ifError got unwanted exception: " + value);
          e.actual = value;
          e.expected = null;
          e.operator = "ifError";
          throw e;
        }
      },
      doesNotMatch: (string, regexp, message) => {
        if (regexp.test(string)) {
          innerFail(string, regexp, message ?? `The input was expected to not match`, "doesNotMatch");
        }
      },
    });
    // assert.strict: equal family promoted to strict semantics.
    // assert.CallTracker (deprecated DEP0173) -- faithful port of Node's
    // internal/assert/calltracker.js. Proxy-wraps the tracked fn so length /
    // own properties survive; tracks {thisArg, arguments} per call; report()
    // entries carry .operator (fn name) + .stack; verify() throws AssertionError.
    const codedError = (Ctor, code, message) => {
      const e = new Ctor(message);
      e.code = code;
      return e;
    };
    const validateExpectedUint32 = (value) => {
      if (typeof value !== "number") {
        throw codedError(
          TypeError,
          "ERR_INVALID_ARG_TYPE",
          'The "expected" argument must be of type number. Received ' + typeof value,
        );
      }
      if (!Number.isInteger(value)) {
        throw codedError(
          RangeError,
          "ERR_OUT_OF_RANGE",
          'The value of "expected" is out of range. It must be an integer. Received ' + value,
        );
      }
      if (value < 1 || value > 4294967295) {
        throw codedError(
          RangeError,
          "ERR_OUT_OF_RANGE",
          'The value of "expected" is out of range. It must be >= 1 && <= 4294967295. Received ' + value,
        );
      }
    };
    assert.CallTracker = class CallTracker {
      constructor() {
        this._callChecks = new Set();
        this._trackedFunctions = new WeakMap();
      }
      _getTracked(tracked) {
        if (!this._trackedFunctions.has(tracked)) {
          throw codedError(
            TypeError,
            "ERR_INVALID_ARG_VALUE",
            'The argument "tracked" is invalid. Received ' + String(tracked) +
              ". is not a tracked function",
          );
        }
        return this._trackedFunctions.get(tracked);
      }
      calls(fn, expected = 1) {
        if (process._exiting) {
          throw codedError(
            Error,
            "ERR_UNAVAILABLE_DURING_EXIT",
            "Cannot call a CallTracker function during process exit",
          );
        }
        if (typeof fn === "number") {
          expected = fn;
          fn = function () {};
        } else if (fn === undefined) {
          fn = function () {};
        }
        validateExpectedUint32(expected);

        const calls = [];
        const name = fn.name || "calls";
        const stackTrace = new Error();
        const context = {
          track(thisArg, args) {
            calls.push(
              Object.freeze({ thisArg, arguments: Object.freeze(Array.prototype.slice.call(args)) }),
            );
          },
          reset() {
            calls.length = 0;
          },
          getCalls() {
            return Object.freeze(calls.slice());
          },
          report() {
            if (calls.length - expected !== 0) {
              return {
                message:
                  "Expected the " + name + " function to be executed " + expected +
                  " time(s) but was executed " + calls.length + " time(s).",
                actual: calls.length,
                expected,
                operator: name,
                stack: stackTrace,
              };
            }
            return undefined;
          },
        };
        const tracked = new Proxy(fn, {
          __proto__: null,
          apply(target, thisArg, argList) {
            context.track(thisArg, argList);
            return Reflect.apply(target, thisArg, argList);
          },
        });
        this._callChecks.add(context);
        this._trackedFunctions.set(tracked, context);
        return tracked;
      }
      getCalls(tracked) {
        return this._getTracked(tracked).getCalls();
      }
      reset(tracked) {
        if (tracked === undefined) {
          this._callChecks.forEach((check) => check.reset());
          return;
        }
        this._getTracked(tracked).reset();
      }
      report() {
        const errors = [];
        this._callChecks.forEach((context) => {
          const message = context.report();
          if (message !== undefined) errors.push(message);
        });
        return errors;
      }
      verify() {
        const errors = this.report();
        if (errors.length === 0) return;
        const message =
          errors.length === 1
            ? errors[0].message
            : "Functions were not called the expected number of times";
        throw new AssertionError({ message, details: errors });
      }
    };
    assert.strict = Object.assign(
      (value, message) => ok(value, message),
      assert,
      {
        equal: assert.strictEqual,
        notEqual: assert.notStrictEqual,
        deepEqual: assert.deepStrictEqual,
        notDeepEqual: assert.notDeepStrictEqual,
      },
    );
    assert.strict.strict = assert.strict;
    return assert;
  };

  // ------------------------------------------------------------------- fs
  function wrapStat(raw) {
    return {
      ...raw,
      isFile: () => raw.kind === "file",
      isDirectory: () => raw.kind === "dir",
      isSymbolicLink: () => raw.kind === "symlink",
      isBlockDevice: () => false,
      isCharacterDevice: () => false,
      isFIFO: () => false,
      isSocket: () => false,
      mtime: new Date(raw.mtimeMs),
      atime: new Date(raw.atimeMs),
      ctime: new Date(raw.ctimeMs),
      birthtime: new Date(raw.birthtimeMs),
    };
  }

  class Dirent {
    constructor(name, parentPath, kind) {
      this.name = name;
      this.parentPath = parentPath;
      this.path = parentPath;
      this._kind = kind;
    }
    isFile() { return this._kind === "file"; }
    isDirectory() { return this._kind === "dir"; }
    isSymbolicLink() { return this._kind === "symlink"; }
    isBlockDevice() { return false; }
    isCharacterDevice() { return false; }
    isFIFO() { return false; }
    isSocket() { return false; }
  }

  function wrapDirents(parent, entries, withFileTypes) {
    if (!withFileTypes) return entries.map((e) => e.name);
    return entries.map((e) => new Dirent(e.name, parent, e.kind));
  }

  function makeDirent(parentPath, entry) {
    var name = typeof entry === "string" ? entry : entry.name;
    var kind = typeof entry === "object" && entry.kind ? entry.kind : "file";
    return new Dirent(name, parentPath, kind);
  }

  function readOptions(options) {
    if (typeof options === "string") return { encoding: options };
    return options ?? {};
  }

  /// Natives always hand back raw bytes; encodings decode HERE via
  /// Buffer#toString so 'base64'/'hex'/'latin1'/... all behave (the
  /// Rust-side decode was utf8-only and silently wrong for the rest).
  function decodeRead(bytes, encoding) {
    const BufferCtor = globalThis.Buffer;
    const buf = new BufferCtor(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    return encoding ? buf.toString(encoding) : buf;
  }

  /// Encode a write payload: strings honor the encoding option (Node
  /// decodes 'base64'/'hex'/... before writing); views pass through.
  function encodeWrite(data, options) {
    if (typeof data === "string") {
      return globalThis.Buffer.from(data, readOptions(options).encoding ?? "utf8");
    }
    return data;
  }

  registry.factories["fs/promises"] = (natives) => {
    const isWin = natives.platform === "win32";
    return {
      readFile: async (path, options) => {
        const bytes = await natives.fsReadFile(String(path));
        return decodeRead(bytes, readOptions(options).encoding ?? null);
      },
      writeFile: (path, data, options) =>
        natives.fsWriteFile(String(path), encodeWrite(data, options), false),
      appendFile: (path, data, options) =>
        natives.fsWriteFile(String(path), encodeWrite(data, options), true),
      stat: async (path) => wrapStat(await natives.fsStat(String(path), false)),
      lstat: async (path) => wrapStat(await natives.fsStat(String(path), true)),
      readdir: async (path, options) => {
        const { withFileTypes } = readOptions(options);
        const entries = await natives.fsReaddir(String(path));
        return wrapDirents(String(path), entries, withFileTypes === true);
      },
      mkdir: async (path, options) => {
        await natives.fsMkdir(String(path), readOptions(options).recursive === true);
      },
      rm: async (path, options = {}) => {
        await natives.fsRm(String(path), options.recursive === true, options.force === true);
      },
      rmdir: async (path) => {
        // Node never deletes a FILE through rmdir (code-probing callers
        // depend on the throw); kind-check first.
        const raw = await natives.fsStat(String(path), true);
        if (raw.kind !== "dir") {
          throw makeNodeError(
            isWin ? "ENOENT" : "ENOTDIR",
            `${isWin ? "ENOENT" : "ENOTDIR"}: not a directory, rmdir '${path}'`,
          );
        }
        await natives.fsRm(String(path), false, false);
      },
      unlink: (path) => natives.fsUnlink(String(path)),
      rename: (from, to) => natives.fsRename(String(from), String(to)),
      copyFile: (from, to) => natives.fsCopyFile(String(from), String(to)),
      access: (path, mode) => natives.fsAccess(String(path), mode ?? 0),
      realpath: (path) => natives.fsRealpath(String(path)),
      mkdtemp: (prefix) => natives.fsMkdtemp(String(prefix)),
      symlink: (target, path) => natives.fsSymlink(String(target), String(path)),
      readlink: (path) => natives.fsReadlink(String(path)),
      link: (existing, newPath) => natives.fsLink(String(existing), String(newPath)),
      chmod: (path, mode) => natives.fsChmod(String(path), mode),
      truncate: (path, len) => natives.fsTruncate(String(path), len ?? 0),
      opendir: async function (path) {
        var dirPath = String(path);
        var entries = await natives.fsReaddir(dirPath);
        var idx = 0;
        var dir = {
          path: dirPath,
          read: async function () {
            if (idx >= entries.length) return null;
            var entry = entries[idx++];
            return makeDirent(dirPath, entry);
          },
          close: async function () { idx = entries.length; },
          [Symbol.asyncIterator]: function () {
            return {
              next: async function () {
                var d = await dir.read();
                if (d === null) return { done: true, value: undefined };
                return { done: false, value: d };
              }
            };
          }
        };
        return dir;
      },
      cp: async function cpRecursive(src, dest, options) {
        var srcStr = String(src);
        var destStr = String(dest);
        var opts = options || {};
        var raw;
        try { raw = await natives.fsStat(srcStr, false); } catch (e) { throw e; }
        if (raw.kind === "dir") {
          if (!opts.recursive) throw makeNodeError("ERR_FS_CP_DIR_TO_NON_DIR", "cp: -r not specified; omitting directory '" + srcStr + "'");
          try { await natives.fsMkdir(destStr, true); } catch (e) {}
          var entries = await natives.fsReaddir(srcStr);
          for (var i = 0; i < entries.length; i++) {
            var sep = srcStr.endsWith("/") || srcStr.endsWith("\\") ? "" : "/";
            await cpRecursive(srcStr + sep + entries[i].name, destStr + sep + entries[i].name, opts);
          }
        } else {
          await natives.fsCopyFile(srcStr, destStr);
        }
      },
      open: async function (path, flags, mode) {
        flags = flags || "r";
        var info = await natives.fsOpen(String(path), String(flags));
        var h = info.handle;
        var closed = false;
        var fh = {
          fd: h,
          readFile: async function (options) {
            var enc = (options && typeof options === "object") ? options.encoding : (typeof options === "string" ? options : null);
            var chunks = [];
            while (true) {
              var chunk = await natives.fsReadChunk(h, 65536);
              if (chunk === undefined) break;
              chunks.push(chunk);
            }
            if (chunks.length === 0) return enc ? "" : globalThis.Buffer.alloc(0);
            var total = 0;
            for (var i = 0; i < chunks.length; i++) total += chunks[i].length;
            var buf = globalThis.Buffer.alloc(total);
            var off = 0;
            for (var i = 0; i < chunks.length; i++) {
              buf.set(chunks[i], off);
              off += chunks[i].length;
            }
            return enc ? buf.toString(enc) : buf;
          },
          writeFile: async function (data, options) {
            var enc = (options && typeof options === "object") ? options.encoding : (typeof options === "string" ? options : "utf8");
            if (typeof data === "string") data = globalThis.Buffer.from(data, enc);
            await natives.fsWriteChunk(h, data);
          },
          write: async function (buffer, offset, length, position) {
            if (typeof buffer === "string") buffer = globalThis.Buffer.from(buffer);
            var slice = (offset != null || length != null) ? buffer.subarray(offset || 0, length != null ? (offset || 0) + length : undefined) : buffer;
            await natives.fsWriteChunk(h, slice);
            return { bytesWritten: slice.length, buffer: buffer };
          },
          read: async function (buffer, offset, length, position) {
            var chunk = await natives.fsReadChunk(h, length || 65536);
            if (chunk === undefined) return { bytesRead: 0, buffer: buffer };
            if (buffer) {
              var dest = new Uint8Array(buffer.buffer || buffer, (buffer.byteOffset || 0) + (offset || 0));
              dest.set(chunk);
            }
            return { bytesRead: chunk.length, buffer: buffer || globalThis.Buffer.from(chunk) };
          },
          stat: async function () {
            return wrapStat(await natives.fsStat(String(path), false));
          },
          close: async function () {
            if (!closed) {
              closed = true;
              await Promise.resolve(natives.fsClose(h)).catch(function () {});
            }
          },
          [Symbol.asyncDispose]: async function () {
            await fh.close();
          },
        };
        return fh;
      },
      constants: {
        F_OK: 0, X_OK: 1, W_OK: 2, R_OK: 4,
        O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2,
        ...platformOFlags(natives.platform), O_SYNC: 1052672,
        O_DIRECTORY: 65536, O_NOFOLLOW: 131072,
        S_IFMT: 61440, S_IFREG: 32768, S_IFDIR: 16384, S_IFLNK: 40960,
        S_IFBLK: 24576, S_IFCHR: 8192, S_IFIFO: 4096, S_IFSOCK: 49152,
        S_IRUSR: 256, S_IWUSR: 128, S_IXUSR: 64,
        S_IRGRP: 32, S_IWGRP: 16, S_IXGRP: 8,
        S_IROTH: 4, S_IWOTH: 2, S_IXOTH: 1,
        COPYFILE_EXCL: 1, COPYFILE_FICLONE: 2, COPYFILE_FICLONE_FORCE: 4,
        UV_FS_SYMLINK_DIR: 1, UV_FS_SYMLINK_JUNCTION: 2,
      },
      Dirent,
    };
  };

  registry.factories.fs = (natives) => {
    const promises = registry.get("fs/promises");

    // Callback forms delegate to the promise forms (Node-style (err, value)).
    function callbackify1(promiseFn) {
      return (...args) => {
        const cb = args.pop();
        if (typeof cb !== "function") {
          throw new TypeError("Callback must be a function");
        }
        promiseFn(...args).then(
          (value) => queueMicrotask(() => cb(null, value)),
          (err) => queueMicrotask(() => cb(err)),
        );
      };
    }

    function fsWatch(filename, options, listener) {
      if (typeof options === "function") {
        listener = options;
        options = {};
      }
      var opts = options || {};
      var EventEmitter = registry.get("events");
      var watcher = new EventEmitter();
      var filePath = String(filename);
      var prevMtime = 0;
      var closed = false;
      try {
        prevMtime = natives.fsStatSync(filePath, false).mtimeMs;
      } catch (e) {
        // file may not exist yet
      }
      var pollInterval = opts.interval || 100;
      var poll = setInterval(function () {
        if (closed) return;
        natives.fsStat(filePath, false).then(function (stat) {
          if (stat.mtimeMs !== prevMtime) {
            prevMtime = stat.mtimeMs;
            var parts = filePath.replace(/\\/g, "/").split("/");
            var base = parts[parts.length - 1] || filePath;
            watcher.emit("change", "change", base);
            if (listener) listener("change", base);
          }
        }, function (e) {
          watcher.emit("error", e);
        });
      }, pollInterval);
      watcher.close = function () {
        closed = true;
        clearInterval(poll);
        watcher.emit("close");
      };
      watcher.ref = function () { return watcher; };
      watcher.unref = function () { return watcher; };
      return watcher;
    }

    function fsWatchFile(filename, options, listener) {
      if (typeof options === "function") {
        listener = options;
        options = {};
      }
      var opts = options || {};
      var filePath = String(filename);
      var interval = opts.interval || 5007;
      var prev;
      try {
        prev = wrapStat(natives.fsStatSync(filePath, false));
      } catch (e) {
        prev = wrapStat({ kind: "file", size: 0, mtime: 0, atime: 0, mode: 0 });
      }
      var poll = setInterval(function () {
        natives.fsStat(filePath, false).then(function (raw) {
          var curr = wrapStat(raw);
          if (curr.mtimeMs !== prev.mtimeMs) {
            if (listener) listener(curr, prev);
            prev = curr;
          }
        }, function () {});
      }, interval);
      return {
        close: function () { clearInterval(poll); },
        ref: function () { return this; },
        unref: function () { return this; },
      };
    }

    // Map numeric O_* open flags to the fopen-style string natives.fsOpen
    // takes. Most callers pass a string ("r"/"w"/...); chokidar does.
    function numericOpenFlags(n) {
      var acc = n & 3; // O_RDONLY=0, O_WRONLY=1, O_RDWR=2
      var append = (n & platformOFlags(natives.platform).O_APPEND) !== 0; // O_APPEND
      if (acc === 1) return append ? "a" : "w";
      if (acc === 2) return append ? "a+" : "r+";
      return "r";
    }

    // ReadStream / WriteStream as real Readable / Writable subclasses, built
    // lazily on first use (avoids a stream<->fs require cycle at fs-factory
    // time). Node exposes these as CONSTRUCTORS with a real `.prototype`;
    // graceful-fs does `Object.create(fs.ReadStream.prototype)`, which the
    // previous arrow-function alias (no `.prototype`) broke.
    let _rwStreams;
    function rwStreams() {
      if (_rwStreams) return _rwStreams;
      const { Readable, Writable } = registry.get("stream");
      class ReadStream extends Readable {
        constructor(path, options) {
          const opts = readOptions(options);
          const highWaterMark = opts.highWaterMark ?? 65536;
          const endByte = typeof opts.end === "number" ? opts.end : Infinity;
          const startByte = typeof opts.start === "number" ? opts.start : 0;
          const maxBytes = endByte === Infinity ? Infinity : endByte - startByte + 1;
          let handle = null;
          let totalRead = 0;
          super({
            highWaterMark,
            encoding: opts.encoding ?? null,
            async read(size) {
              try {
                if (handle === null) {
                  handle = (await natives.fsOpen(String(path), "r")).handle;
                  this.emit("open", handle);
                  this.emit("ready");
                }
                const remaining = maxBytes - totalRead;
                if (remaining <= 0) {
                  await Promise.resolve(natives.fsClose(handle)).catch(() => {});
                  handle = null;
                  this.push(null);
                  process.nextTick(() => this.emit("close"));
                  return;
                }
                const want = Math.min(size || highWaterMark, remaining);
                const chunk = await natives.fsReadChunk(handle, want);
                if (chunk === undefined) {
                  await Promise.resolve(natives.fsClose(handle)).catch(() => {});
                  handle = null;
                  this.push(null);
                  process.nextTick(() => this.emit("close"));
                } else {
                  const buf = new globalThis.Buffer(chunk.buffer, chunk.byteOffset, chunk.length);
                  totalRead += buf.length;
                  this.bytesRead = totalRead;
                  this.push(buf);
                }
              } catch (e) {
                this.destroy(e);
              }
            },
            destroy(err, cb) {
              if (handle !== null) {
                Promise.resolve(natives.fsClose(handle)).catch(() => {});
                handle = null;
              }
              cb(err);
            },
          });
          this.path = path;
          this.bytesRead = 0;
        }
      }
      class WriteStream extends Writable {
        constructor(path, options) {
          const opts = readOptions(options);
          const flags = opts.flags === "a" ? "a" : "w";
          let handle = null;
          let totalWritten = 0;
          super({
            highWaterMark: opts.highWaterMark ?? 65536,
            async write(chunk, _encoding, cb) {
              try {
                if (handle === null) {
                  handle = (await natives.fsOpen(String(path), flags)).handle;
                  this.emit("open", handle);
                  this.emit("ready");
                }
                await natives.fsWriteChunk(handle, chunk);
                totalWritten += chunk.length;
                this.bytesWritten = totalWritten;
                cb();
              } catch (e) {
                cb(e);
              }
            },
            async final(cb) {
              try {
                // Zero-write stream: the file must still exist afterwards.
                if (handle === null) {
                  handle = (await natives.fsOpen(String(path), flags)).handle;
                }
                natives.fsClose(handle);
                handle = null;
                cb();
                this.emit("close");
              } catch (e) {
                cb(e);
              }
            },
            destroy(err, cb) {
              if (handle !== null) {
                natives.fsClose(handle);
                handle = null;
              }
              cb(err);
            },
          });
          this.path = path;
          this.bytesWritten = 0;
        }
      }
      _rwStreams = { ReadStream, WriteStream };
      return _rwStreams;
    }

    const fs = {
      promises,
      constants: {
        F_OK: 0, X_OK: 1, W_OK: 2, R_OK: 4,
        O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2,
        ...platformOFlags(natives.platform), O_SYNC: 1052672,
        O_DIRECTORY: 65536, O_NOFOLLOW: 131072,
        S_IFMT: 61440, S_IFREG: 32768, S_IFDIR: 16384, S_IFLNK: 40960,
        S_IFBLK: 24576, S_IFCHR: 8192, S_IFIFO: 4096, S_IFSOCK: 49152,
        S_IRUSR: 256, S_IWUSR: 128, S_IXUSR: 64,
        S_IRGRP: 32, S_IWGRP: 16, S_IXGRP: 8,
        S_IROTH: 4, S_IWOTH: 2, S_IXOTH: 1,
        COPYFILE_EXCL: 1, COPYFILE_FICLONE: 2, COPYFILE_FICLONE_FORCE: 4,
        UV_FS_SYMLINK_DIR: 1, UV_FS_SYMLINK_JUNCTION: 2,
      },

      readFileSync: (path, options) => {
        const enc = readOptions(options).encoding;
        if (enc === "utf8" || enc === "utf-8") {
          return natives.fsReadFileUtf8Sync(String(path));
        }
        const bytes = natives.fsReadFileSync(String(path));
        return decodeRead(bytes, enc ?? null);
      },
      writeFileSync: (path, data, options) => {
        natives.fsWriteFileSync(String(path), encodeWrite(data, options), false);
      },
      appendFileSync: (path, data, options) => {
        natives.fsWriteFileSync(String(path), encodeWrite(data, options), true);
      },
      existsSync: (path) => natives.fsExistsSync(String(path)),
      statSync: (path) => wrapStat(natives.fsStatSync(String(path), false)),
      lstatSync: (path) => wrapStat(natives.fsStatSync(String(path), true)),
      readdirSync: (path, options) => {
        const { withFileTypes } = readOptions(options);
        return wrapDirents(
          String(path),
          natives.fsReaddirSync(String(path)),
          withFileTypes === true,
        );
      },
      mkdirSync: (path, options) => {
        natives.fsMkdirSync(String(path), readOptions(options).recursive === true);
      },
      rmSync: (path, options = {}) => {
        natives.fsRmSync(String(path), options.recursive === true, options.force === true);
      },
      rmdirSync: (path) => {
        const raw = natives.fsStatSync(String(path), true);
        if (raw.kind !== "dir") {
          const isWin = natives.platform === "win32";
          throw makeNodeError(
            isWin ? "ENOENT" : "ENOTDIR",
            `${isWin ? "ENOENT" : "ENOTDIR"}: not a directory, rmdir '${path}'`,
          );
        }
        natives.fsRmSync(String(path), false, false);
      },
      unlinkSync: (path) => natives.fsUnlinkSync(String(path)),
      renameSync: (from, to) => natives.fsRenameSync(String(from), String(to)),
      copyFileSync: (from, to) => natives.fsCopyFileSync(String(from), String(to)),
      accessSync: (path, mode) => natives.fsAccessSync(String(path), mode ?? 0),
      realpathSync: (path) => natives.fsRealpathSync(String(path)),
      mkdtempSync: (prefix) => natives.fsMkdtempSync(String(prefix)),
      symlinkSync: (target, path) => natives.fsSymlinkSync(String(target), String(path)),
      readlinkSync: (path) => natives.fsReadlinkSync(String(path)),
      linkSync: (existing, newPath) => natives.fsLinkSync(String(existing), String(newPath)),
      chmodSync: (path, mode) => natives.fsChmodSync(String(path), mode),
      truncateSync: (path, len) => natives.fsTruncateSync(String(path), len ?? 0),
      // Classic synchronous fd ops. The native handle (a number) IS the fd.
      // graceful-fs / playwright probe locks via openSync(path,"r+") and rely
      // on err.code === "ENOENT" to tell missing from locked.
      openSync: (path, flags, _mode) =>
        natives.fsOpenSync(String(path), typeof flags === "number" ? numericOpenFlags(flags) : (flags ?? "r")),
      closeSync: (fd) => { natives.fsCloseSync(fd); },
      fstatSync: (fd) => wrapStat(natives.fsFstatSync(fd)),
      readSync: (fd, buffer, offset, length, position) => {
        // (fd, buffer, {offset,length,position}) object form.
        if (offset !== null && typeof offset === "object") {
          const o = offset;
          offset = o.offset ?? 0; length = o.length ?? (buffer ? buffer.length - offset : 0); position = o.position ?? null;
        }
        return natives.fsReadSync(fd, buffer, offset ?? 0, length ?? (buffer ? buffer.length - (offset ?? 0) : 0), position ?? null);
      },
      writeSync: (fd, data, offsetOrPosition, length, position) => {
        // Buffer form: (fd, buffer, offset, length, position).
        // String form: (fd, string, position, encoding).
        if (typeof data === "string") {
          const enc = typeof length === "string" ? length : "utf8";
          const buf = globalThis.Buffer.from(data, enc);
          return natives.fsWriteSync(fd, buf, typeof offsetOrPosition === "number" ? offsetOrPosition : null);
        }
        const offset = typeof offsetOrPosition === "number" ? offsetOrPosition : 0;
        const len = typeof length === "number" ? length : (data.length - offset);
        const slice = (offset !== 0 || len !== data.length) ? data.subarray(offset, offset + len) : data;
        return natives.fsWriteSync(fd, slice, typeof position === "number" ? position : null);
      },
      opendirSync: function (path) {
        var dirPath = String(path);
        var entries = natives.fsReaddirSync(dirPath);
        var idx = 0;
        return {
          path: dirPath,
          readSync: function () {
            if (idx >= entries.length) return null;
            return makeDirent(dirPath, entries[idx++]);
          },
          closeSync: function () { idx = entries.length; },
          [Symbol.iterator]: function () {
            return {
              next: function () {
                var d = this.outer.readSync();
                if (d === null) return { done: true, value: undefined };
                return { done: false, value: d };
              },
              outer: this
            };
          }
        };
      },
      cpSync: function cpSyncRecursive(src, dest, options) {
        var srcStr = String(src);
        var destStr = String(dest);
        var opts = options || {};
        var raw;
        try { raw = natives.fsStatSync(srcStr, false); } catch (e) { throw e; }
        if (raw.kind === "dir") {
          if (!opts.recursive) throw makeNodeError("ERR_FS_CP_DIR_TO_NON_DIR", "cpSync: -r not specified; omitting directory '" + srcStr + "'");
          try { natives.fsMkdirSync(destStr, true); } catch (e) {}
          var entries = natives.fsReaddirSync(srcStr);
          for (var i = 0; i < entries.length; i++) {
            var sep = srcStr.endsWith("/") || srcStr.endsWith("\\") ? "" : "/";
            cpSyncRecursive(srcStr + sep + entries[i].name, destStr + sep + entries[i].name, opts);
          }
        } else {
          natives.fsCopyFileSync(srcStr, destStr);
        }
      },

      readFile: callbackify1(promises.readFile),
      writeFile: callbackify1(promises.writeFile),
      appendFile: callbackify1(promises.appendFile),
      stat: callbackify1(promises.stat),
      lstat: callbackify1(promises.lstat),
      readdir: callbackify1(promises.readdir),
      mkdir: callbackify1(promises.mkdir),
      rm: callbackify1(promises.rm),
      rmdir: callbackify1(promises.rmdir),
      unlink: callbackify1(promises.unlink),
      rename: callbackify1(promises.rename),
      copyFile: callbackify1(promises.copyFile),
      access: callbackify1(promises.access),
      realpath: callbackify1(promises.realpath),
      mkdtemp: callbackify1(promises.mkdtemp),
      symlink: callbackify1(promises.symlink),
      readlink: callbackify1(promises.readlink),
      link: callbackify1(promises.link),
      chmod: callbackify1(promises.chmod),
      truncate: callbackify1(promises.truncate),
      opendir: callbackify1(promises.opendir),
      cp: callbackify1(promises.cp),
      exists: (path, cb) => {
        // Deprecated single-arg callback shape, still in the wild.
        cb(natives.fsExistsSync(String(path)));
      },

      // fd-based callback ops (chokidar etc. do promisify(fs.open)). The
      // native open handle (a number) IS the integer fd. fsReadChunk reads
      // sequentially, so `position` is honored only as null/current (no seek).
      open: function (path, flags, mode, cb) {
        if (typeof flags === "function") { cb = flags; flags = "r"; }
        else if (typeof mode === "function") { cb = mode; }
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        var flagStr = typeof flags === "number" ? numericOpenFlags(flags) : (flags || "r");
        Promise.resolve(natives.fsOpen(String(path), String(flagStr))).then(
          function (info) { queueMicrotask(function () { cb(null, info.handle); }); },
          function (err) { queueMicrotask(function () { cb(err); }); },
        );
      },
      close: function (fd, cb) {
        var err = null;
        try { natives.fsClose(fd); } catch (e) { err = e; }
        if (typeof cb === "function") queueMicrotask(function () { cb(err); });
      },
      read: function (fd, buffer, offset, length, position, cb) {
        // Variants: (fd, buffer, offset, length, position, cb),
        // (fd, options, cb), and trailing-callback short forms.
        if (typeof buffer === "function") {
          cb = buffer; buffer = globalThis.Buffer.alloc(16384); offset = 0; length = buffer.length;
        } else if (buffer && typeof buffer === "object" && !ArrayBuffer.isView(buffer)) {
          var o = buffer; cb = offset;
          buffer = o.buffer || globalThis.Buffer.alloc(o.length || 16384);
          offset = o.offset || 0;
          length = o.length != null ? o.length : buffer.length - offset;
        }
        if (typeof offset === "function") { cb = offset; offset = 0; length = buffer ? buffer.length : 16384; }
        if (typeof length === "function") { cb = length; length = buffer ? buffer.length - (offset || 0) : 16384; }
        if (typeof position === "function") { cb = position; }
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        var want = length != null ? length : (buffer ? buffer.length - (offset || 0) : 16384);
        Promise.resolve(natives.fsReadChunk(fd, want)).then(
          function (chunk) {
            if (chunk === undefined || chunk === null) {
              queueMicrotask(function () { cb(null, 0, buffer); });
              return;
            }
            if (buffer) {
              var view = new Uint8Array(buffer.buffer || buffer, (buffer.byteOffset || 0) + (offset || 0));
              view.set(chunk.subarray(0, Math.min(chunk.length, view.length)));
            }
            queueMicrotask(function () { cb(null, chunk.length, buffer); });
          },
          function (err) { queueMicrotask(function () { cb(err); }); },
        );
      },

      createReadStream: (path, options) => new (rwStreams().ReadStream)(path, options),
      createWriteStream: (path, options) => new (rwStreams().WriteStream)(path, options),
      watch: fsWatch,
      watchFile: fsWatchFile,
    };
    fs.realpathSync.native = fs.realpathSync;
    fs.Dirent = Dirent;
    fs.Stats = function Stats() {};
    // Real constructors (lazy) with a `.prototype`, exposed as configurable
    // getters so graceful-fs can read fs.ReadStream.prototype AND later
    // redefine fs.ReadStream with its own wrapper.
    for (const [name, kind] of [
      ["ReadStream", "ReadStream"],
      ["WriteStream", "WriteStream"],
      ["FileReadStream", "ReadStream"],
      ["FileWriteStream", "WriteStream"],
    ]) {
      Object.defineProperty(fs, name, {
        get: () => rwStreams()[kind],
        configurable: true,
        enumerable: true,
      });
    }
    return fs;
  };

  // -------------------------------------------------------------- process
  registry.factories.process = (natives) => {
    const EventEmitter = registry.get("events");
    const { Readable, Writable } = registry.get("stream");
    const { Buffer } = registry.get("buffer");
    const process = new EventEmitter();

    // Lazy env: natives.env() crosses the FFI boundary and copies every
    // environment variable into a JS object (50-200 vars on a typical dev
    // machine). For programs that never touch process.env, this is pure
    // waste. A Proxy defers the copy until first property access.
    let envCache = null;
    const ensureEnv = () => (envCache ??= natives.env());
    const env = new Proxy(Object.create(null), {
      get(_, prop) { return ensureEnv()[prop]; },
      set(_, prop, value) { ensureEnv()[prop] = value; return true; },
      has(_, prop) { return prop in ensureEnv(); },
      deleteProperty(_, prop) { return delete ensureEnv()[prop]; },
      ownKeys() { return Reflect.ownKeys(ensureEnv()); },
      getOwnPropertyDescriptor(_, prop) {
        const obj = ensureEnv();
        if (!(prop in obj)) return undefined;
        return { value: obj[prop], writable: true, enumerable: true, configurable: true };
      },
    });
    const stdoutIsTTY = natives.isTTY(1);
    const stderrIsTTY = natives.isTTY(2);

    // argv is LAZY: the embedder declares it (entry path + script args)
    // after this module instantiates, so the first script-time access must
    // read the native, not a construction-time copy.
    let argvCache = null;
    const argv = () => (argvCache ??= natives.argv());
    Object.defineProperty(process, "argv", {
      get: () => argv(),
      enumerable: true,
      configurable: true,
    });
    Object.defineProperty(process, "argv0", {
      get: () => argv()[0],
      enumerable: true,
      configurable: true,
    });
    Object.defineProperty(process, "execPath", {
      get: () => argv()[0],
      enumerable: true,
      configurable: true,
    });

    // Node fires a synchronous 'exit' event at termination -- natural
    // event-loop drain OR process.exit() -- with process._exiting set so
    // handlers can detect it. Emitted exactly once: the Rust runtime calls
    // __oamEmitExit at natural drain (before reading the final exit code), and
    // process.exit() calls it inline before the native exit.
    process._exiting = false;
    // process._exiting is the once-guard, shared with the runtime's
    // natural-drain emit (oam_engine emit_process_exit) so 'exit' fires exactly
    // once whichever path reaches it first. Not exposed as a global.
    function emitProcessExit(explicitCode) {
      if (process._exiting) return;
      process._exiting = true;
      const code =
        typeof explicitCode === "number"
          ? explicitCode
          : typeof process.exitCode === "number"
            ? process.exitCode
            : 0;
      process.emit("exit", code);
    }

    Object.assign(process, {
      env,
      execArgv: [],
      platform: natives.platform,
      arch: natives.arch,
      // Compat claim: packages feature-detect via process.version. oam
      // tracks the Node LTS line its compat layer targets.
      version: "v22.16.0",
      versions: {
        node: "22.16.0",
        oam: natives.oamVersion,
        v8: natives.v8Version,
      },
      pid: natives.pid,
      ppid: natives.ppid,
      title: "oam",
      exitCode: undefined,
      exit(code) {
        const c = code ?? process.exitCode ?? 0;
        process.exitCode = c;
        const numeric = typeof c === "number" ? c : 0;
        emitProcessExit(numeric);
        natives.exit(numeric);
      },
      cwd: () => natives.cwd(),
      chdir: (dir) => natives.chdir(String(dir)),
      nextTick(fn, ...args) {
        if (typeof fn !== "function") {
          throw new TypeError('The "callback" argument must be of type function');
        }
        queueMicrotask(() => fn(...args));
      },
      hrtime: Object.assign(
        (prev) => {
          const ns = natives.hrtimeNanos();
          const total = Number(ns);
          let secs = Math.floor(total / 1e9);
          let nanos = total % 1e9;
          if (prev) {
            secs -= prev[0];
            nanos -= prev[1];
            if (nanos < 0) {
              secs -= 1;
              nanos += 1e9;
            }
          }
          return [secs, nanos];
        },
        { bigint: () => natives.hrtimeNanos() },
      ),
      abort: () => { natives.processExit(134); },
      abort: () => { natives.processExit(134); },
      uptime: () => natives.uptimeMs() / 1000,
      debugPort: 9229,
      connected: false,
      constrainedMemory: () => 0,
      availableMemory: () => 0,
      memoryUsage: Object.assign(
        () => {
          const h = natives.heapStatistics();
          return {
            rss: natives.processRss(),
            heapTotal: h.total_heap_size,
            heapUsed: h.used_heap_size,
            external: h.external_memory,
            arrayBuffers: 0,
          };
        },
        { rss: () => natives.processRss() },
      ),
      stdout: Object.assign(new Writable({
        write(chunk, _enc, cb) { natives.stdoutWrite(chunk); cb(); },
        decodeStrings: false,
      }), { fd: 1, isTTY: stdoutIsTTY, columns: stdoutIsTTY ? 80 : undefined, hasColors: () => stdoutIsTTY }),
      stderr: Object.assign(new Writable({
        write(chunk, _enc, cb) { natives.stderrWrite(chunk); cb(); },
        decodeStrings: false,
      }), { fd: 2, isTTY: stderrIsTTY, columns: stderrIsTTY ? 80 : undefined, hasColors: () => stderrIsTTY }),
      stdin: Object.assign(new Readable({
        read() {
          natives.stdinRead().then(
            (chunk) => {
              if (chunk === undefined || chunk === null || chunk.length === 0) this.push(null);
              else this.push(Buffer.from(chunk));
            },
            () => this.push(null),
          );
        },
      }), { fd: 0, isTTY: natives.isTTY(0) }),
      getBuiltinModule(name) {
        var bare = String(name).replace(/^node:/, "");
        if (registry.factories[bare]) {
          return registry.get(bare);
        }
        return undefined;
      },
      emitWarning(warning) {
        if (globalThis.console) {
          globalThis.console.warn(
            warning instanceof Error ? `${warning.name}: ${warning.message}` : `Warning: ${warning}`,
          );
        }
      },
      cpuUsage: (prev) => {
        var usage = natives.processCpuUsage();
        if (prev) return { user: usage.user - prev.user, system: usage.system - prev.system };
        return usage;
      },
      kill: (pid, signal) => {
        var sigMap = { SIGTERM: 15, SIGKILL: 9, SIGINT: 2, SIGHUP: 1, SIGUSR1: 10, SIGUSR2: 12, SIGPIPE: 13 };
        var sig = typeof signal === "string" ? (sigMap[signal] || 15) : (signal !== undefined ? signal : 15);
        if (sig === 0) { natives.processKill(pid, 0); return true; }
        natives.processKill(pid, sig);
        return true;
      },
      umask: () => 0,
      getuid: () => 0,
      getgid: () => 0,
      geteuid: () => 0,
      getegid: () => 0,
      setuid: () => {},
      setgid: () => {},
      seteuid: () => {},
      setegid: () => {},
      getgroups: () => [0],
      setgroups: () => {},
      initgroups: () => {},
      resourceUsage: () => ({
        userCPUTime: 0, systemCPUTime: 0, maxRSS: 0,
        sharedMemorySize: 0, unsharedDataSize: 0, unsharedStackSize: 0,
        minorPageFault: 0, majorPageFault: 0, swappedOut: 0,
        fsRead: 0, fsWrite: 0, ipcSent: 0, ipcReceived: 0,
        signalsCount: 0, voluntaryContextSwitches: 0, involuntaryContextSwitches: 0,
      }),
      release: { name: "node" },
      config: { variables: {} },
      features: { inspector: false, ipv6: true, tls: true },
      allowedNodeEnvironmentFlags: new Set(),
      report: {
        getReport: () => ({}),
        writeReport: () => "",
        directory: "",
        filename: "",
        compact: false,
        signal: "SIGUSR2",
        reportOnFatalError: false,
        reportOnSignal: false,
        reportOnUncaughtException: false,
      },
      loadEnvFile: function loadEnvFile(path) {
        var fs = registry.get("fs");
        var envPath = path || ".env";
        var text;
        try { text = fs.readFileSync(envPath, "utf8"); }
        catch (e) {
          var err = new Error("Cannot find env file: " + envPath);
          err.code = "ERR_ENV_FILE_NOT_FOUND";
          throw err;
        }
        var lines = text.split("\n");
        for (var li = 0; li < lines.length; li++) {
          var line = lines[li].trim();
          if (!line || line[0] === "#") continue;
          var eqPos = line.indexOf("=");
          if (eqPos === -1) continue;
          var key = line.slice(0, eqPos).trim();
          var val = line.slice(eqPos + 1).trim();
          if ((val[0] === '"' && val[val.length - 1] === '"') ||
              (val[0] === "'" && val[val.length - 1] === "'")) {
            val = val.slice(1, -1);
          }
          process.env[key] = val;
        }
      },
    });
    return process;
  };

  // --------------------------------------------------------------- module
  registry.factories.module = (natives) => {
    class Module {}

    const builtinModules = [
      "assert",
      "assert/strict",
      "async_hooks",
      "buffer",
      "child_process",
      "cluster",
      "console",
      "crypto",
      "dgram",
      "diagnostics_channel",
      "dns",
      "dns/promises",
      "internal/errors",
      "domain",
      "events",
      "fs",
      "fs/promises",
      "http",
      "http2",
      "https",
      "inspector",
      "module",
      "net",
      "os",
      "path",
      "path/posix",
      "path/win32",
      "perf_hooks",
      "process",
      "punycode",
      "querystring",
      "readline",
      "readline/promises",
      "repl",
      "stream",
      "stream/consumers",
      "stream/promises",
      "stream/web",
      "string_decoder",
      "timers",
      "timers/promises",
      "tls",
      "trace_events",
      "tty",
      "url",
      "util",
      "util/types",
      "v8",
      "vm",
      "worker_threads",
      "zlib",
    ];

    return {
      createRequire(filename) {
        let base = String(filename);
        if (base.startsWith("file://")) {
          base = registry.get("url").fileURLToPath(base);
        }
        return natives.makeRequire(base);
      },
      builtinModules,
      isBuiltin: (name) => {
        const bare = String(name).replace(/^node:/, "");
        return builtinModules.includes(bare);
      },
      syncBuiltinESMExports: () => {},
      Module,
    };
  };

  // ---------------------------------------------------------- async_hooks
  // AsyncLocalStorage rides V8's continuation-preserved embedder data
  // (Node 24's AsyncContextFrame model): the "frame" is an IMMUTABLE Map
  // of storage-instance -> store living in the CPED slot. V8 snapshots the
  // frame into every promise reaction at creation and restores it when the
  // reaction runs, so `await` propagation costs nothing extra; timers and
  // friends are bound to their scheduling frame by the wrappers installed
  // in installRuntimeGlobals.
  //
  // Wave-1 divergences (documented): executionAsyncId/triggerAsyncId
  // return 0 (no async-ids machinery); createHook is a warn-once no-op â€”
  // the legacy diagnostics API is deprecated in Node and the packages that
  // matter feature-detect AsyncLocalStorage first.
  registry.factories.async_hooks = (natives) => {
    const getFrame = () => natives.getContinuationData();
    const setFrame = (frame) => natives.setContinuationData(frame);

    class AsyncResource {
      constructor(type, _options) {
        this.type = String(type ?? "AsyncResource");
        this._frame = getFrame();
      }
      runInAsyncScope(fn, thisArg, ...args) {
        const prev = getFrame();
        setFrame(this._frame);
        try {
          return fn.apply(thisArg, args);
        } finally {
          setFrame(prev);
        }
      }
      bind(fn) {
        const resource = this;
        const bound = function (...args) {
          return resource.runInAsyncScope(fn, this, ...args);
        };
        Object.defineProperty(bound, "name", { value: `bound ${fn.name || ""}`.trim() });
        return bound;
      }
      static bind(fn, type, thisArg) {
        const resource = new AsyncResource(type ?? "bound-anonymous-fn");
        const bound = function (...args) {
          return resource.runInAsyncScope(fn, thisArg ?? this, ...args);
        };
        Object.defineProperty(bound, "name", { value: `bound ${fn.name || ""}`.trim() });
        return bound;
      }
      emitDestroy() {
        return this;
      }
      asyncId() {
        return 0;
      }
      triggerAsyncId() {
        return 0;
      }
    }

    class AsyncLocalStorage {
      constructor() {
        this._disabled = false;
      }
      static bind(fn) {
        return AsyncResource.bind(fn);
      }
      static snapshot() {
        return AsyncResource.bind((cb, ...args) => cb(...args), "AsyncLocalStorage.snapshot");
      }
      getStore() {
        if (this._disabled) return undefined;
        const frame = getFrame();
        return frame instanceof Map ? frame.get(this) : undefined;
      }
      run(store, fn, ...args) {
        this._disabled = false;
        const prev = getFrame();
        const next = new Map(prev instanceof Map ? prev : undefined);
        next.set(this, store);
        setFrame(next);
        try {
          return fn(...args);
        } finally {
          setFrame(prev);
        }
      }
      exit(fn, ...args) {
        const prev = getFrame();
        if (!(prev instanceof Map) || !prev.has(this)) return fn(...args);
        const next = new Map(prev);
        next.delete(this);
        setFrame(next);
        try {
          return fn(...args);
        } finally {
          setFrame(prev);
        }
      }
      enterWith(store) {
        this._disabled = false;
        const prev = getFrame();
        const next = new Map(prev instanceof Map ? prev : undefined);
        next.set(this, store);
        setFrame(next);
      }
      disable() {
        this._disabled = true;
      }
    }

    let warnedCreateHook = false;
    function createHook(_callbacks) {
      if (!warnedCreateHook) {
        warnedCreateHook = true;
        if (globalThis.console) {
          globalThis.console.warn(
            "(oam) async_hooks.createHook is a no-op: the legacy hooks API is " +
              "deprecated in Node; use AsyncLocalStorage (fully supported)",
          );
        }
      }
      return {
        enable() {
          return this;
        },
        disable() {
          return this;
        },
      };
    }

    return {
      AsyncLocalStorage,
      AsyncResource,
      createHook,
      executionAsyncId: () => 0,
      triggerAsyncId: () => 0,
      executionAsyncResource: () => ({}),
    };
  };
  // ---------------------------------------------------------- node:stream
  // Streams3 (the EventEmitter streams the CJS ecosystem is built on),
  // wave-1 subset: flowing/paused Readable with backpressured pipe(),
  // queued Writable with drain/finish, Duplex via the same prototype
  // mixin Node uses, Transform/PassThrough, pipeline/finished (+promise
  // forms), async iteration, and web-stream interop (from/toWeb).
  //
  // Documented divergences: setEncoding decodes per-chunk (a multi-byte
  // character split EXACTLY across chunks can mojibake â€” use
  // TextDecoderStream for byte-exact decoding); no _writev/cork batching
  // (cork/uncork are accepted no-ops); 'readable'-event pull scheduling is
  // simplified (emitted on every push).
  registry.factories.stream = () => {
    const EventEmitter = registry.get("events");
    const BufferCtor = globalThis.Buffer;

    const microtask = (fn) => queueMicrotask(fn);

    function destroyImpl(stream, err, state) {
      if (state.destroyed) return stream;
      state.destroyed = true;
      const cb = (finalErr) => {
        const error = finalErr ?? err;
        if (error) microtask(() => stream.emit("error", error));
        microtask(() => stream.emit("close"));
      };
      if (stream._destroy) stream._destroy(err ?? null, cb);
      else cb(null);
      return stream;
    }

    // ------------------------------------------------------------ Readable
    function initReadableState(self, options) {
      // Duplex/Transform can set the sides independently (split2 et al
      // depend on readableObjectMode).
      const objectMode =
        options.objectMode === true || options.readableObjectMode === true;
      self._rState = {
        objectMode,
        highWaterMark:
          options.readableHighWaterMark ??
          options.highWaterMark ??
          (objectMode ? 16 : 65536),
        buffer: [],
        length: 0,
        flowing: null, // null = neither; true = flowing; false = paused
        ended: false,
        endEmitted: false,
        reading: false,
        destroyed: false,
        errored: null,
        encoding: options.encoding ?? null,
        flushing: false,
        pipes: [],
      };
      if (options.read) self._read = options.read;
      if (options.destroy) self._destroy = options.destroy;
    }

    class Readable extends EventEmitter {
      constructor(options = {}) {
        super();
        initReadableState(this, options);
      }

      _read(_size) {
        this.destroy(new Error("The _read() method is not implemented"));
      }

      get readable() {
        const s = this._rState;
        return !s.destroyed && !s.endEmitted;
      }
      get readableEnded() {
        return this._rState.endEmitted;
      }
      get destroyed() {
        return this._rState.destroyed;
      }
      get readableFlowing() {
        return this._rState.flowing;
      }
      get readableLength() {
        return this._rState.length;
      }
      get readableObjectMode() {
        return this._rState.objectMode;
      }
      get readableHighWaterMark() {
        return this._rState.highWaterMark;
      }

      push(chunk, encoding) {
        const s = this._rState;
        if (chunk === null) {
          s.ended = true;
          s.reading = false;
          this._emitEndIfDrained();
          return false;
        }
        let data = chunk;
        if (!s.objectMode && typeof chunk === "string") {
          data = BufferCtor.from(chunk, encoding ?? "utf8");
        }
        s.buffer.push(data);
        s.length += s.objectMode ? 1 : (data.length ?? 1);
        s.reading = false;
        if (s.flowing) this._scheduleFlow();
        else microtask(() => this.emit("readable"));
        return s.length < s.highWaterMark;
      }

      unshift(chunk) {
        const s = this._rState;
        let data = chunk;
        if (!s.objectMode && typeof chunk === "string") {
          data = BufferCtor.from(chunk, "utf8");
        }
        s.buffer.unshift(data);
        s.length += s.objectMode ? 1 : (data.length ?? 1);
        return true;
      }

      _callRead() {
        const s = this._rState;
        if (s.reading || s.ended || s.destroyed) return;
        if (s.length >= s.highWaterMark) return;
        s.reading = true;
        try {
          const result = this._read(s.highWaterMark);
          // Async _read implementations (fs streams) return promises whose
          // rejections must destroy the stream, not vanish.
          if (result && typeof result.then === "function") {
            result.catch((e) => this.destroy(e));
          }
        } catch (e) {
          this.destroy(e);
        }
      }

      _decode(chunk) {
        const s = this._rState;
        if (s.encoding && !s.objectMode && chunk instanceof Uint8Array) {
          return BufferCtor.prototype.toString.call(chunk, s.encoding);
        }
        return chunk;
      }

      read(size) {
        const s = this._rState;
        if (s.destroyed) return null;
        if (s.buffer.length === 0) {
          if (s.ended) this._emitEndIfDrained();
          else this._callRead();
          return null;
        }
        let out;
        if (s.objectMode) {
          out = s.buffer.shift();
          s.length -= 1;
        } else if (size === undefined || size === null) {
          if (s.buffer.length === 1) out = s.buffer.shift();
          else out = BufferCtor.concat(s.buffer.splice(0));
          s.length = 0;
        } else {
          // Byte-exact slicing for sized reads.
          const parts = [];
          let need = size;
          while (need > 0 && s.buffer.length > 0) {
            const head = s.buffer[0];
            if (head.length <= need) {
              parts.push(s.buffer.shift());
              need -= head.length;
            } else {
              parts.push(head.subarray(0, need));
              s.buffer[0] = head.subarray(need);
              need = 0;
            }
          }
          out = parts.length === 1 ? parts[0] : BufferCtor.concat(parts);
          s.length -= out.length;
          if (out.length === 0) out = null;
        }
        if (s.buffer.length === 0 && !s.ended) this._callRead();
        if (s.ended) this._emitEndIfDrained();
        return out === null ? null : this._decode(out);
      }

      _emitEndIfDrained() {
        const s = this._rState;
        if (!s.ended || s.endEmitted || s.buffer.length > 0) return;
        s.endEmitted = true;
        microtask(() => this.emit("end"));
      }

      _scheduleFlow() {
        const s = this._rState;
        if (s.flushing) return;
        s.flushing = true;
        microtask(() => {
          s.flushing = false;
          while (s.flowing && s.buffer.length > 0 && !s.destroyed) {
            const chunk = s.buffer.shift();
            s.length -= s.objectMode ? 1 : (chunk.length ?? 1);
            this.emit("data", this._decode(chunk));
          }
          if (s.flowing && !s.ended && s.buffer.length === 0) this._callRead();
          this._emitEndIfDrained();
        });
      }

      on(type, listener) {
        super.on(type, listener);
        if (type === "data") {
          if (this._rState.flowing !== false) this.resume();
        } else if (type === "readable") {
          if (this._rState.buffer.length === 0) this._callRead();
        }
        return this;
      }

      resume() {
        const s = this._rState;
        if (s.destroyed) return this;
        s.flowing = true;
        this._scheduleFlow();
        return this;
      }

      pause() {
        this._rState.flowing = false;
        return this;
      }

      isPaused() {
        return this._rState.flowing === false;
      }

      setEncoding(encoding) {
        this._rState.encoding = encoding;
        return this;
      }

      pipe(dest, options = {}) {
        const src = this;
        const state = { ondata: null, ondrain: null, onend: null };
        state.ondata = (chunk) => {
          if (dest.write(chunk) === false) src.pause();
        };
        state.ondrain = () => src.resume();
        state.onend = () => {
          if (options.end !== false) dest.end();
        };
        src._rState.pipes.push({ dest, state });
        src.on("data", state.ondata);
        dest.on("drain", state.ondrain);
        src.on("end", state.onend);
        dest.emit("pipe", src);
        // pipe() flows the source even if it was explicitly paused (Node
        // semantics) â€” backpressure re-pauses as needed.
        src.resume();
        return dest;
      }

      unpipe(dest) {
        const s = this._rState;
        const keep = [];
        for (const entry of s.pipes) {
          if (dest !== undefined && entry.dest !== dest) {
            keep.push(entry);
            continue;
          }
          this.removeListener("data", entry.state.ondata);
          this.removeListener("end", entry.state.onend);
          entry.dest.removeListener("drain", entry.state.ondrain);
          entry.dest.emit("unpipe", this);
        }
        s.pipes = keep;
        if (s.pipes.length === 0) this.pause();
        return this;
      }

      destroy(err) {
        const s = this._rState;
        s.errored = err ?? s.errored;
        return destroyImpl(this, err, s);
      }

      [Symbol.asyncIterator]() {
        const stream = this;
        const s = stream._rState;
        const waitForState = () =>
          new Promise((resolve, reject) => {
            const cleanup = () => {
              stream.removeListener("readable", onReadable);
              stream.removeListener("end", onReadable);
              stream.removeListener("close", onReadable);
              stream.removeListener("error", onError);
            };
            const onReadable = () => {
              cleanup();
              resolve();
            };
            const onError = (e) => {
              cleanup();
              reject(e);
            };
            stream.on("readable", onReadable);
            stream.on("end", onReadable);
            stream.on("close", onReadable);
            stream.on("error", onError);
          });
        return {
          async next() {
            for (;;) {
              if (s.errored) throw s.errored;
              const chunk = stream.read();
              if (chunk !== null) return { value: chunk, done: false };
              if (s.endEmitted || (s.ended && s.buffer.length === 0)) {
                return { value: undefined, done: true };
              }
              if (s.destroyed) return { value: undefined, done: true };
              await waitForState();
            }
          },
          async return(value) {
            stream.destroy();
            return { value, done: true };
          },
          [Symbol.asyncIterator]() {
            return this;
          },
        };
      }

      map(fn) {
        const src = this;
        return new Readable({
          objectMode: true,
          async read() {
            for await (const chunk of src) {
              var result = fn(chunk);
              if (result && typeof result.then === "function") result = await result;
              this.push(result);
            }
            this.push(null);
          },
        });
      }

      filter(fn) {
        const src = this;
        return new Readable({
          objectMode: true,
          async read() {
            for await (const chunk of src) {
              var keep = fn(chunk);
              if (keep && typeof keep.then === "function") keep = await keep;
              if (keep) this.push(chunk);
            }
            this.push(null);
          },
        });
      }

      async reduce(fn, initial) {
        var acc = initial;
        var first = true;
        for await (const chunk of this) {
          if (first && acc === undefined) {
            acc = chunk;
            first = false;
            continue;
          }
          first = false;
          acc = fn(acc, chunk);
          if (acc && typeof acc.then === "function") acc = await acc;
        }
        return acc;
      }

      async toArray() {
        const arr = [];
        for await (const chunk of this) arr.push(chunk);
        return arr;
      }

      async forEach(fn) {
        for await (const chunk of this) {
          var r = fn(chunk);
          if (r && typeof r.then === "function") await r;
        }
      }

      async some(fn) {
        for await (const chunk of this) {
          var result = fn(chunk);
          if (result && typeof result.then === "function") result = await result;
          if (result) return true;
        }
        return false;
      }

      async every(fn) {
        for await (const chunk of this) {
          var result = fn(chunk);
          if (result && typeof result.then === "function") result = await result;
          if (!result) return false;
        }
        return true;
      }

      async find(fn) {
        for await (const chunk of this) {
          var result = fn(chunk);
          if (result && typeof result.then === "function") result = await result;
          if (result) return chunk;
        }
        return undefined;
      }

      flatMap(fn) {
        const src = this;
        return new Readable({
          objectMode: true,
          async read() {
            for await (const chunk of src) {
              var result = fn(chunk);
              if (result && typeof result.then === "function") result = await result;
              if (result != null && typeof result[Symbol.asyncIterator] === "function") {
                for await (const item of result) this.push(item);
              } else if (result != null && typeof result[Symbol.iterator] === "function" && typeof result !== "string") {
                for (const item of result) this.push(item);
              } else {
                this.push(result);
              }
            }
            this.push(null);
          },
        });
      }

      drop(n) {
        const src = this;
        return new Readable({
          objectMode: true,
          async read() {
            var skipped = 0;
            for await (const chunk of src) {
              if (skipped < n) { skipped++; continue; }
              this.push(chunk);
            }
            this.push(null);
          },
        });
      }

      take(n) {
        const src = this;
        return new Readable({
          objectMode: true,
          async read() {
            var taken = 0;
            for await (const chunk of src) {
              this.push(chunk);
              taken++;
              if (taken >= n) break;
            }
            this.push(null);
          },
        });
      }

            static from(iterable) {
        const iterator =
          iterable[Symbol.asyncIterator]?.() ?? iterable[Symbol.iterator]?.();
        if (!iterator) throw new TypeError("Readable.from requires an iterable");
        return new Readable({
          objectMode: true,
          async read() {
            try {
              const { value, done } = await iterator.next();
              if (done) this.push(null);
              else this.push(value);
            } catch (e) {
              this.destroy(e);
            }
          },
        });
      }

      static fromWeb(webStream, options = {}) {
        const reader = webStream.getReader();
        return new Readable({
          objectMode: options.objectMode === true,
          async read() {
            try {
              const { value, done } = await reader.read();
              if (done) this.push(null);
              else this.push(value);
            } catch (e) {
              this.destroy(e);
            }
          },
          destroy(err, cb) {
            reader.cancel(err).then(
              () => cb(err),
              () => cb(err),
            );
          },
        });
      }

      static isDisturbed(stream) {
        if (stream && stream._rState) return stream._rState.reading || stream._rState.ended || stream._rState.destroyed;
        return false;
      }

      static isReadable(stream) {
        if (stream && stream._rState) return !stream._rState.destroyed && !stream._rState.endEmitted;
        return false;
      }

      static toWeb(nodeReadable) {
        return new globalThis.ReadableStream({
          start(controller) {
            nodeReadable.on("data", (chunk) => {
              controller.enqueue(chunk);
              if (controller.desiredSize !== null && controller.desiredSize <= 0) {
                nodeReadable.pause();
              }
            });
            nodeReadable.on("end", () => controller.close());
            nodeReadable.on("error", (e) => controller.error(e));
          },
          pull() {
            nodeReadable.resume();
          },
          cancel(reason) {
            nodeReadable.destroy(reason instanceof Error ? reason : undefined);
          },
        });
      }
    }

    // ------------------------------------------------------------ Writable
    function initWritableState(self, options) {
      const objectMode =
        options.objectMode === true || options.writableObjectMode === true;
      self._wState = {
        objectMode,
        highWaterMark:
          options.writableHighWaterMark ??
          options.highWaterMark ??
          (objectMode ? 16 : 65536),
        queue: [], // {chunk, encoding, cb}
        length: 0,
        writing: false,
        ending: false,
        finished: false,
        destroyed: false,
        needDrain: false,
        endCallbacks: [],
        corked: 0,
      };
      if (options.write) self._write = options.write;
      if (options.writev) self._writev = options.writev;
      if (options.final) self._final = options.final;
      if (options.destroy) self._destroy = options.destroy;
    }

    class Writable extends EventEmitter {
      constructor(options = {}) {
        super();
        initWritableState(this, options);
      }
    }

    const writableMethods = {
      _write(_chunk, _encoding, cb) {
        cb(new Error("The _write() method is not implemented"));
      },

      write(chunk, encoding, cb) {
        if (typeof encoding === "function") {
          cb = encoding;
          encoding = undefined;
        }
        const s = this._wState;
        if (s.ending || s.destroyed) {
          const err = new Error("write after end");
          if (cb) microtask(() => cb(err));
          if (this.listenerCount("error") > 0) microtask(() => this.emit("error", err));
          return false;
        }
        let data = chunk;
        if (!s.objectMode && typeof chunk === "string") {
          data = BufferCtor.from(chunk, encoding ?? "utf8");
        }
        s.queue.push({ chunk: data, encoding: encoding ?? "buffer", cb });
        s.length += s.objectMode ? 1 : (data.length ?? 1);
        this._processWrites();
        const below = s.length < s.highWaterMark;
        if (!below) s.needDrain = true;
        return below;
      },

      _processWrites() {
        const s = this._wState;
        if (s.writing || s.destroyed || s.corked > 0) return;
        if (s.queue.length === 0) {
          this._maybeFinish();
          return;
        }
        // If _writev is available and multiple chunks are queued, batch them
        if (this._writev && s.queue.length > 1) {
          const batch = s.queue.splice(0);
          s.writing = true;
          let called = false;
          let batchLen = 0;
          for (let i = 0; i < batch.length; i++) {
            batchLen += s.objectMode ? 1 : (batch[i].chunk.length ?? 1);
          }
          const done = (err) => {
            if (called) return;
            called = true;
            s.writing = false;
            s.length -= batchLen;
            if (err) {
              for (let i = 0; i < batch.length; i++) {
                if (batch[i].cb) batch[i].cb(err);
              }
              this.destroy(err);
              return;
            }
            for (let i = 0; i < batch.length; i++) {
              if (batch[i].cb) batch[i].cb();
            }
            if (s.queue.length === 0 && s.needDrain && !s.ending) {
              s.needDrain = false;
              this.emit("drain");
            }
            this._processWrites();
          };
          try {
            const result = this._writev(batch, done);
            if (result && typeof result.then === "function") {
              result.catch(done);
            }
          } catch (e) {
            done(e);
          }
          return;
        }
        const next = s.queue.shift();
        if (next === undefined) {
          this._maybeFinish();
          return;
        }
        s.writing = true;
        let called = false;
        const done = (err) => {
          if (called) return;
          called = true;
          s.writing = false;
          s.length -= s.objectMode ? 1 : (next.chunk.length ?? 1);
          if (err) {
            if (next.cb) next.cb(err);
            this.destroy(err);
            return;
          }
          if (next.cb) next.cb();
          if (s.queue.length === 0 && s.needDrain && !s.ending) {
            s.needDrain = false;
            this.emit("drain");
          }
          this._processWrites();
        };
        try {
          const result = this._write(next.chunk, next.encoding, done);
          if (result && typeof result.then === "function") {
            result.catch(done);
          }
        } catch (e) {
          done(e);
        }
      },

      _maybeFinish() {
        const s = this._wState;
        if (!s.ending || s.finished || s.writing || s.queue.length > 0 || s.destroyed) {
          return;
        }
        s.finished = true;
        const complete = () => {
          microtask(() => {
            this.emit("finish");
            for (const cb of s.endCallbacks.splice(0)) cb();
          });
        };
        if (this._final) {
          let called = false;
          const cb = (err) => {
            if (called) return;
            called = true;
            if (err) this.destroy(err);
            else complete();
          };
          try {
            const result = this._final(cb);
            if (result && typeof result.then === "function") result.catch(cb);
          } catch (e) {
            cb(e);
          }
        } else {
          complete();
        }
      },

      end(chunk, encoding, cb) {
        if (typeof chunk === "function") {
          cb = chunk;
          chunk = undefined;
          encoding = undefined;
        } else if (typeof encoding === "function") {
          cb = encoding;
          encoding = undefined;
        }
        const s = this._wState;
        if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
        s.ending = true;
        if (cb) {
          if (s.finished) microtask(cb);
          else s.endCallbacks.push(cb);
        }
        this._processWrites();
        return this;
      },

      cork() {
        this._wState.corked++;
      },
      uncork() {
        const s = this._wState;
        if (s.corked > 0) s.corked--;
        if (s.corked === 0) this._processWrites();
      },
      setDefaultEncoding() {
        return this;
      },

      destroy(err) {
        return destroyImpl(this, err, this._wState);
      },
    };

    const writableGetters = {
      writable: {
        get() {
          const s = this._wState;
          return !s.destroyed && !s.ending;
        },
      },
      writableEnded: {
        get() {
          return this._wState.ending;
        },
      },
      writableFinished: {
        get() {
          return this._wState.finished;
        },
      },
      writableLength: {
        get() {
          return this._wState.length;
        },
      },
      writableHighWaterMark: {
        get() {
          return this._wState.highWaterMark;
        },
      },
      writableObjectMode: {
        get() {
          return this._wState.objectMode;
        },
      },
    };

    function applyWritable(proto) {
      for (const [name, fn] of Object.entries(writableMethods)) {
        Object.defineProperty(proto, name, {
          value: fn,
          writable: true,
          configurable: true,
        });
      }
      Object.defineProperties(proto, writableGetters);
    }
    applyWritable(Writable.prototype);
    Object.defineProperty(Writable.prototype, "destroyed", {
      get() {
        return this._wState.destroyed;
      },
      configurable: true,
    });

    Writable.fromWeb = (webStream) => {
      const writer = webStream.getWriter();
      return new Writable({
        async write(chunk, _enc, cb) {
          try {
            await writer.write(chunk);
            cb();
          } catch (e) {
            cb(e);
          }
        },
        async final(cb) {
          try {
            await writer.close();
            cb();
          } catch (e) {
            cb(e);
          }
        },
        destroy(err, cb) {
          writer.abort(err).then(
            () => cb(err),
            () => cb(err),
          );
        },
      });
    };
    Writable.toWeb = (nodeWritable) =>
      new globalThis.WritableStream({
        write(chunk) {
          return new Promise((resolve, reject) => {
            nodeWritable.write(chunk, (err) => (err ? reject(err) : resolve()));
          });
        },
        close() {
          return new Promise((resolve) => nodeWritable.end(resolve));
        },
        abort(reason) {
          nodeWritable.destroy(reason instanceof Error ? reason : undefined);
        },
      });

    // ------------------------------------------------------------- Duplex
    class Duplex extends Readable {
      constructor(options = {}) {
        super(options);
        initWritableState(this, options);
      }
    }
    applyWritable(Duplex.prototype);
    // `destroyed` must consult BOTH sides.
    Object.defineProperty(Duplex.prototype, "destroyed", {
      get() {
        return this._rState.destroyed || this._wState.destroyed;
      },
      configurable: true,
    });
    Object.defineProperty(Duplex.prototype, "destroy", {
      value: function destroy(err) {
        this._rState.destroyed = true;
        return destroyImpl(this, err, this._wState);
      },
      writable: true,
      configurable: true,
    });

    // ----------------------------------------------------------- Transform
    Duplex.from = function duplexFrom(source) {
      if (source && typeof source.pipe === "function" && typeof source.write === "function") return source;
      if (source && typeof source[Symbol.asyncIterator] === "function") {
        return new Duplex({
          objectMode: true,
          write(chunk, enc, cb) { cb(); },
          async read() {
            for await (var v of source) { if (!this.push(v)) break; }
            this.push(null);
          },
        });
      }
      if (source && typeof source.readable === "object" && typeof source.writable === "object") {
        var d = new Duplex({
          write(chunk, enc, cb) { source.writable.write(chunk); cb(); },
          read() {},
        });
        if (source.readable && typeof source.readable.on === "function") {
          source.readable.on("data", function(ch) { d.push(ch); });
          source.readable.on("end", function() { d.push(null); });
        }
        return d;
      }
      throw new TypeError("Duplex.from: unsupported source");
    };
    Duplex.fromWeb = function duplexFromWeb(pair) {
      var readable = pair.readable;
      var writable = pair.writable;
      var reader = readable.getReader();
      var writer = writable.getWriter();
      return new Duplex({
        async read() {
          try {
            var r = await reader.read();
            if (r.done) this.push(null); else this.push(r.value);
          } catch(e) { this.destroy(e); }
        },
        write(chunk, enc, cb) { writer.write(chunk).then(function() { cb(); }, cb); },
        final(cb) { writer.close().then(function() { cb(); }, cb); },
      });
    };
    Duplex.toWeb = function duplexToWeb(duplex) {
      return {
        readable: Readable.toWeb(duplex),
        writable: Writable.toWeb(duplex),
      };
    };

    class Transform extends Duplex {
      constructor(options = {}) {
        super(options);
        if (options.transform) this._transform = options.transform;
        if (options.flush) this._flush = options.flush;
        // Writable side feeds _transform; output goes out the Readable side.
        this._write = (chunk, encoding, cb) => {
          let called = false;
          const tcb = (err, data) => {
            if (called) return;
            called = true;
            if (err) return cb(err);
            if (data !== undefined && data !== null) this.push(data);
            cb();
          };
          try {
            const result = this._transform(chunk, encoding, tcb);
            if (result && typeof result.then === "function") result.catch(tcb);
          } catch (e) {
            tcb(e);
          }
        };
        this._final = (cb) => {
          const finish = (err, data) => {
            if (err) return cb(err);
            if (data !== undefined && data !== null) this.push(data);
            this.push(null);
            cb();
          };
          if (this._flush) {
            try {
              const result = this._flush(finish);
              if (result && typeof result.then === "function") {
                result.catch((e) => cb(e));
              }
            } catch (e) {
              cb(e);
            }
          } else {
            finish();
          }
        };
      }

      _transform(chunk, _encoding, cb) {
        cb(null, chunk);
      }

      // Transforms pull on demand: reading is driven by writes.
      _read() {}
    }

    class PassThrough extends Transform {}

    // -------------------------------------------------- finished/pipeline
    function finished(stream, options, cb) {
      if (typeof options === "function") {
        cb = options;
        options = {};
      }
      const isReadable = typeof stream.on === "function" && stream._rState !== undefined;
      const isWritable = stream._wState !== undefined;
      let done = false;
      const settle = (err) => {
        if (done) return;
        done = true;
        cleanup();
        cb(err ?? undefined);
      };
      const onend = () => {
        if (!isWritable || stream._wState.finished || stream._wState.destroyed) {
          settle();
        }
      };
      const onfinish = () => {
        if (!isReadable || stream._rState.endEmitted || stream._rState.destroyed) {
          settle();
        }
      };
      const onerror = (e) => settle(e);
      const onclose = () => settle(stream._rState?.errored ?? undefined);
      const cleanup = () => {
        stream.removeListener("end", onend);
        stream.removeListener("finish", onfinish);
        stream.removeListener("error", onerror);
        stream.removeListener("close", onclose);
      };
      stream.on("end", onend);
      stream.on("finish", onfinish);
      stream.on("error", onerror);
      stream.on("close", onclose);
      return () => cleanup();
    }

    function pipeline(...args) {
      const cb = typeof args[args.length - 1] === "function" ? args.pop() : () => {};
      if (args.length < 2) {
        throw new TypeError("pipeline requires at least a source and destination");
      }
      let settled = false;
      const settle = (err) => {
        if (settled) return;
        settled = true;
        if (err) for (const s of args) s.destroy?.(err);
        cb(err ?? undefined);
      };
      for (const s of args) {
        s.on?.("error", settle);
      }
      for (let i = 0; i < args.length - 1; i++) {
        args[i].pipe(args[i + 1]);
      }
      const last = args[args.length - 1];
      finished(last, (err) => settle(err));
      return last;
    }

    // require('stream') IS the legacy Stream class in Node (an
    // EventEmitter subclass with .prototype) â€” packages do
    // util.inherits(X, require('stream')) (jws/jsonwebtoken among them),
    // so the module export must be the CLASS, with everything else
    // attached as properties.
    class Stream extends EventEmitter {}
    Object.assign(Stream, {
      Stream,
      Readable,
      Writable,
      Duplex,
      Transform,
      PassThrough,
      pipeline,
      finished,
      promises: {
        pipeline: (...args) =>
          new Promise((resolve, reject) => {
            pipeline(...args, (err) => (err ? reject(err) : resolve()));
          }),
        finished: (s, options) =>
          new Promise((resolve, reject) => {
            finished(s, options ?? {}, (err) => (err ? reject(err) : resolve()));
          }),
      },
      isErrored: (s) => Boolean(s._rState?.errored),
      isReadable: (s) => Boolean(s._rState && !s._rState.destroyed && !s._rState.endEmitted),
      isDisturbed: (s) => Boolean(s._rState?.reading || s._rState?.ended || s._rState?.endEmitted),
      getDefaultHighWaterMark: (objectMode) => objectMode ? 16 : 16384,
      setDefaultHighWaterMark: (objectMode, value) => {
        if (typeof value !== "number" || value < 0 || Number.isNaN(value)) {
          throw new TypeError("The value of highWaterMark is invalid: " + value);
        }
      },
      addAbortSignal: (signal, stream) => {
        if (signal.aborted) {
          stream.destroy(signal.reason ?? new Error("This operation was aborted"));
        } else {
          signal.addEventListener("abort", () => {
            stream.destroy(signal.reason ?? new Error("This operation was aborted"));
          }, { once: true });
        }
        return stream;
      },
      compose: (...streams) => {
        if (streams.length === 0) throw new TypeError("stream.compose requires at least one stream");
        if (streams.length === 1) return streams[0];
        for (let i = 0; i < streams.length - 1; i++) {
          streams[i].pipe(streams[i + 1]);
        }
        var head = streams[0];
        var tail = streams[streams.length - 1];
        var composed = new Duplex({
          write(chunk, enc, cb) { head.write(chunk, enc, cb); },
          read() {},
          final(cb) { head.end(); cb(); },
        });
        tail.on("data", (chunk) => { if (!composed.push(chunk)) tail.pause(); });
        tail.on("end", () => composed.push(null));
        composed._read = () => tail.resume();
        for (var si = 0; si < streams.length; si++) {
          streams[si].on("error", (err) => composed.destroy(err));
        }
        return composed;
      },
    });
    return Stream;
  };
  registry.factories["stream/promises"] = () => registry.get("stream").promises;
  registry.factories["stream/web"] = () => ({
    ReadableStream: globalThis.ReadableStream,
    WritableStream: globalThis.WritableStream,
    TransformStream: globalThis.TransformStream,
    TextDecoderStream: globalThis.TextDecoderStream,
    TextEncoderStream: globalThis.TextEncoderStream,
  });
  registry.factories["stream/consumers"] = () => {
    async function bytesOf(streamLike) {
      const chunks = [];
      let total = 0;
      for await (const chunk of streamLike) {
        const bytes =
          chunk instanceof Uint8Array
            ? chunk
            : globalThis.Buffer.from(String(chunk), "utf8");
        chunks.push(bytes);
        total += bytes.length;
      }
      const out = new Uint8Array(total);
      let offset = 0;
      for (const c of chunks) {
        out.set(c, offset);
        offset += c.length;
      }
      return out;
    }
    return {
      arrayBuffer: async (s) => (await bytesOf(s)).buffer,
      buffer: async (s) => {
        const bytes = await bytesOf(s);
        return globalThis.Buffer.from(bytes.buffer, bytes.byteOffset, bytes.length);
      },
      text: async (s) => new TextDecoder().decode(await bytesOf(s)),
      json: async (s) => JSON.parse(new TextDecoder().decode(await bytesOf(s))),
    };
  };

  // -------------------------------------------------------- string_decoder
  // StringDecoder = boundary-safe Buffer -> string decoding. The utf8
  // family rides TextDecoder's stream:true buffering (multi-byte chars
  // split across chunks reassemble exactly); single-byte encodings decode
  // per chunk. base64/hex per-chunk decoding can tear at chunk boundaries
  // (punch-listed; rare in stream position).
  registry.factories.string_decoder = () => {
    class StringDecoder {
      constructor(encoding = "utf8") {
        this.encoding = String(encoding).toLowerCase();
        this._decoder =
          this.encoding === "utf8" || this.encoding === "utf-8"
            ? new TextDecoder("utf-8", { ignoreBOM: true })
            : null;
      }
      write(buffer) {
        if (typeof buffer === "string") return buffer;
        if (this._decoder) return this._decoder.decode(buffer, { stream: true });
        return globalThis.Buffer.prototype.toString.call(buffer, this.encoding);
      }
      end(buffer) {
        let out = buffer !== undefined && buffer !== null ? this.write(buffer) : "";
        if (this._decoder) out += this._decoder.decode();
        return out;
      }
      text(buffer, offset) {
        return this.write(buffer.subarray(offset ?? 0));
      }
    }
    return { StringDecoder };
  };

  // ------------------------------------------------- URL / URLSearchParams
  // WHATWG URL: parsing and component mutation happen in Rust (servo's
  // url crate â€” the reference implementation, IDNA included); these
  // classes are thin component holders. Setter failures keep the old
  // value silently, per spec.
  {
    const FORM_SAFE = /[A-Za-z0-9*\-._]/;

    function formEncode(text) {
      const bytes = new TextEncoder().encode(String(text));
      let out = "";
      for (const byte of bytes) {
        const ch = String.fromCharCode(byte);
        if (ch === " ") out += "+";
        else if (FORM_SAFE.test(ch)) out += ch;
        else out += "%" + byte.toString(16).toUpperCase().padStart(2, "0");
      }
      return out;
    }

    function formDecode(text) {
      const src = String(text).replaceAll("+", " ");
      const bytes = [];
      // Literal chars are buffered into RUNS before UTF-8 encoding â€”
      // encoding per code UNIT tears astral pairs (emoji) into U+FFFD.
      let literal = "";
      const flush = () => {
        if (literal.length > 0) {
          for (const b of new TextEncoder().encode(literal)) bytes.push(b);
          literal = "";
        }
      };
      for (let i = 0; i < src.length; i++) {
        if (src[i] === "%" && /^[0-9A-Fa-f]{2}$/.test(src.slice(i + 1, i + 3))) {
          flush();
          bytes.push(parseInt(src.slice(i + 1, i + 3), 16));
          i += 2;
        } else {
          literal += src[i];
        }
      }
      flush();
      return new TextDecoder().decode(new Uint8Array(bytes));
    }

    /// USVString conversion at the API boundary (WebIDL): lone surrogates
    /// become U+FFFD on entry, so get/has agree with toString.
    const usv = (value) => String(value).toWellFormed();

    function requireArgs(count, got, method, name) {
      if (got < count) {
        throw new TypeError(`URLSearchParams.${method}: the "${name}" argument must be specified`);
      }
    }

    class URLSearchParams {
      constructor(init) {
        // The list is mutated IN PLACE everywhere: iteration is LIVE and
        // index-based (spec) â€” snapshot iteration diverges on the classic
        // mutate-while-iterating shapes.
        this._list = []; // [name, value] pairs, order-preserving
        this._url = null; // back-reference set by URL
        if (init === undefined || init === null) {
          // empty
        } else if (typeof init === "string") {
          this._parse(init);
        } else if (typeof init[Symbol.iterator] === "function") {
          for (const pair of init) {
            const entry = Array.from(pair);
            if (entry.length !== 2) {
              throw new TypeError("URLSearchParams: each init pair must have 2 items");
            }
            this._list.push([usv(entry[0]), usv(entry[1])]);
          }
        } else if (typeof init === "object") {
          for (const key of Object.keys(init)) {
            this._list.push([usv(key), usv(init[key])]);
          }
        }
      }
      _parse(text) {
        this._list.length = 0;
        const raw = String(text).replace(/^\?/, "");
        if (raw.length === 0) return;
        for (const piece of raw.split("&")) {
          if (piece.length === 0) continue;
          const eq = piece.indexOf("=");
          if (eq === -1) this._list.push([formDecode(piece), ""]);
          else this._list.push([formDecode(piece.slice(0, eq)), formDecode(piece.slice(eq + 1))]);
        }
      }
      _sync() {
        if (this._url) this._url._setSearchFromParams(this.toString());
      }
      get size() {
        return this._list.length;
      }
      append(name, value) {
        requireArgs(2, arguments.length, "append", "value");
        this._list.push([usv(name), usv(value)]);
        this._sync();
      }
      delete(name, value) {
        requireArgs(1, arguments.length, "delete", "name");
        const n = usv(name);
        const matchValue = arguments.length >= 2 ? usv(value) : null;
        for (let i = this._list.length - 1; i >= 0; i--) {
          if (this._list[i][0] === n && (matchValue === null || this._list[i][1] === matchValue)) {
            this._list.splice(i, 1);
          }
        }
        this._sync();
      }
      get(name) {
        requireArgs(1, arguments.length, "get", "name");
        const n = usv(name);
        for (const [k, v] of this._list) if (k === n) return v;
        return null;
      }
      getAll(name) {
        requireArgs(1, arguments.length, "getAll", "name");
        const n = usv(name);
        return this._list.filter(([k]) => k === n).map(([, v]) => v);
      }
      has(name, value) {
        requireArgs(1, arguments.length, "has", "name");
        const n = usv(name);
        const matchValue = arguments.length >= 2 ? usv(value) : null;
        return this._list.some(
          ([k, v]) => k === n && (matchValue === null || v === matchValue),
        );
      }
      set(name, value) {
        requireArgs(2, arguments.length, "set", "value");
        const n = usv(name);
        const v = usv(value);
        let found = false;
        for (let i = 0; i < this._list.length; i++) {
          if (this._list[i][0] === n) {
            if (found) {
              this._list.splice(i, 1);
              i--;
            } else {
              this._list[i][1] = v;
              found = true;
            }
          }
        }
        if (!found) this._list.push([n, v]);
        this._sync();
      }
      sort() {
        // In-place stable sort by name only (Array#sort is spec-stable).
        this._list.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
        this._sync();
      }
      forEach(fn) {
        const thisArg = arguments[1]; // optional thisArg must not count in .length
        for (let i = 0; i < this._list.length; i++) {
          fn.call(thisArg, this._list[i][1], this._list[i][0], this);
        }
      }
      *entries() {
        for (let i = 0; i < this._list.length; i++) {
          yield [this._list[i][0], this._list[i][1]];
        }
      }
      *keys() {
        for (let i = 0; i < this._list.length; i++) yield this._list[i][0];
      }
      *values() {
        for (let i = 0; i < this._list.length; i++) yield this._list[i][1];
      }
      toString() {
        return this._list.map(([k, v]) => `${formEncode(k)}=${formEncode(v)}`).join("&");
      }
    }
    // WebIDL identity: usp[Symbol.iterator] IS usp.entries.
    URLSearchParams.prototype[Symbol.iterator] = URLSearchParams.prototype.entries;

    class URL {
      constructor(input, base) {
        this._href = globalThis.__oam.node.urlParseHref(String(input), base ?? undefined);
        this._c = null;
        this._params = null;
      }
      static canParse(input, base) {
        return globalThis.__oam.node.urlCanParse(String(input), base != null ? String(base) : undefined);
      }
      static parse(input, base) {
        try {
          return new URL(input, base);
        } catch {
          return null;
        }
      }
      _ensure() {
        if (!this._c) this._c = globalThis.__oam.node.urlParse(this._href);
      }
      _update(part, value) {
        this._c = globalThis.__oam.node.urlUpdate(this._href, part, String(value));
        this._href = this._c.href;
        if (this._params && (part === "search" || part === "href")) {
          this._params._parse(this._c.search);
        }
      }
      _setSearchFromParams(serialized) {
        this._c = globalThis.__oam.node.urlUpdate(this._href, "search", serialized);
        this._href = this._c.href;
      }
      get href() {
        return this._href;
      }
      set href(value) {
        this._href = globalThis.__oam.node.urlParseHref(String(value));
        this._c = null;
        if (this._params) {
          this._ensure();
          this._params._parse(this._c.search);
        }
      }
      get origin() {
        this._ensure();
        return this._c.origin;
      }
      get protocol() {
        this._ensure();
        return this._c.protocol;
      }
      set protocol(v) {
        this._update("protocol", v);
      }
      get username() {
        this._ensure();
        return this._c.username;
      }
      set username(v) {
        this._update("username", v);
      }
      get password() {
        this._ensure();
        return this._c.password;
      }
      set password(v) {
        this._update("password", v);
      }
      get host() {
        this._ensure();
        return this._c.host;
      }
      set host(v) {
        this._update("host", v);
      }
      get hostname() {
        this._ensure();
        return this._c.hostname;
      }
      set hostname(v) {
        this._update("hostname", v);
      }
      get port() {
        this._ensure();
        return this._c.port;
      }
      set port(v) {
        this._update("port", v);
      }
      get pathname() {
        this._ensure();
        return this._c.pathname;
      }
      set pathname(v) {
        this._update("pathname", v);
      }
      get search() {
        this._ensure();
        return this._c.search;
      }
      set search(v) {
        this._update("search", v);
      }
      get hash() {
        this._ensure();
        return this._c.hash;
      }
      set hash(v) {
        this._update("hash", v);
      }
      get searchParams() {
        if (this._params === null) {
          this._ensure();
          this._params = new URLSearchParams(this._c.search);
          this._params._url = this;
        }
        return this._params;
      }
      toString() {
        return this._href;
      }
      toJSON() {
        return this._href;
      }
    }

    globalThis.URL = URL;
    globalThis.URLSearchParams = URLSearchParams;
  }

  // ------------------------------------------------------------- node:url
  registry.factories.url = (natives) => {
    const isWin = natives.platform === "win32";

    function fileURLToPath(input) {
      const url = typeof input === "string" ? new globalThis.URL(input) : input;
      if (url.protocol !== "file:") {
        throw makeNodeError(
          "ERR_INVALID_URL_SCHEME",
          "The URL must be of scheme file",
        );
      }
      // Encoded separators would let a URL smuggle path segments past
      // consumers â€” Node throws, so do we.
      if (/%2f|%5c/i.test(url.pathname)) {
        throw makeNodeError(
          "ERR_INVALID_FILE_URL_PATH",
          "File URL path must not include encoded \\ or / characters",
        );
      }
      let pathname = decodeURIComponent(url.pathname);
      if (isWin) {
        pathname = pathname.replaceAll("/", "\\");
        if (url.hostname) {
          // file://server/share -> \\server\share
          return `\\\\${url.hostname}${pathname}`;
        }
        if (!/^\\[A-Za-z]:/.test(pathname)) {
          // A drive-less path would silently resolve against the cwd's
          // drive â€” fail loud like Node.
          throw makeNodeError(
            "ERR_INVALID_FILE_URL_PATH",
            "File URL path must be absolute",
          );
        }
        return pathname.slice(1); // strip the slash before the drive letter
      }
      return pathname;
    }

    function pathToFileURL(path) {
      const pathModule = registry.get("path");
      let p = String(path);
      if (isWin) {
        // \\?\ device paths: resolve the prefix away (Node parity);
        // \\?\UNC\server\share is the long form of \\server\share.
        if (p.startsWith("\\\\?\\UNC\\")) p = "\\\\" + p.slice(8);
        else if (p.startsWith("\\\\?\\")) p = p.slice(4);
      }
      const trailingSep = /[\\/]$/.test(p) && p.length > 1;
      p = pathModule.resolve(p); // relative paths anchor at cwd (Node parity)
      p = p.replaceAll("\\", "/");
      if (trailingSep && !p.endsWith("/")) p += "/";
      // Percent-encode the URL-special characters paths may carry ('%'
      // FIRST â€” later substitutions insert literal % sequences).
      const encoded = p
        .replaceAll("%", "%25")
        .replaceAll("#", "%23")
        .replaceAll("?", "%3F")
        .replaceAll(" ", "%20")
        .replaceAll("~", "%7E")
        .replaceAll("^", "%5E");
      if (isWin && encoded.startsWith("//")) {
        // UNC: \\server\share -> file://server/share
        return new globalThis.URL("file:" + encoded);
      }
      return new globalThis.URL("file://" + (encoded.startsWith("/") ? "" : "/") + encoded);
    }

    function urlToHttpOptions(url) {
      const options = {
        protocol: url.protocol,
        hostname: url.hostname.startsWith("[")
          ? url.hostname.slice(1, -1) // IPv6: net/dns want it bracket-free
          : url.hostname,
        hash: url.hash,
        search: url.search,
        pathname: url.pathname,
        path: `${url.pathname}${url.search}`,
        href: url.href,
      };
      if (url.port !== "") options.port = Number(url.port);
      if (url.username || url.password) {
        options.auth = `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`;
      }
      return options;
    }

    return {
      URL: globalThis.URL,
      URLSearchParams: globalThis.URLSearchParams,
      fileURLToPath,
      pathToFileURL,
      urlToHttpOptions,
      format: (url) => String(url),
      domainToASCII: (domain) => {
        try {
          return new globalThis.URL(`http://${domain}`).hostname;
        } catch {
          return "";
        }
      },
    };
  };

  // ---------------------------------------------------------- node:crypto
  // Wave-1 surface: streaming hashes + HMAC (md5/sha1/sha224-512, the
  // workhorses of etags, cache keys, and HS256 JWTs), OS randomness, and
  // the WebCrypto subset (subtle.digest, getRandomValues, randomUUID).
  // Asymmetric keys / sign-verify / ciphers land with a later wave.
  registry.factories.crypto = (natives) => {
    const BufferCtor = globalThis.Buffer;
    const stream = registry.get("stream");

    const toBytes = (data, encoding) => {
      if (typeof data === "string") return BufferCtor.from(data, encoding ?? "utf8");
      if (data instanceof KeyObject) return data._material;
      if (data instanceof Uint8Array) return data;
      if (ArrayBuffer.isView(data)) {
        return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      }
      if (data instanceof ArrayBuffer) return new Uint8Array(data);
      throw new TypeError("crypto: data must be a string, Buffer, TypedArray, or ArrayBuffer");
    };

    // Secret-key KeyObject subset: jsonwebtoken-class packages route every
    // key through instanceof KeyObject / createSecretKey, with
    // createPrivateKey/createPublicKey probed in try/catch for asymmetric
    // detection â€” those throw until the asymmetric wave lands.
    class KeyObject {
      constructor(type, material) {
        this.type = type;
        this._material = material;
      }
      get symmetricKeySize() {
        return this._material.length;
      }
      export() {
        // Fresh Buffer (not a view): mutating the export must not corrupt
        // the stored key material.
        return BufferCtor.from(
          this._material.buffer.slice(
            this._material.byteOffset,
            this._material.byteOffset + this._material.length,
          ),
        );
      }
    }

    function createSecretKey(key, encoding) {
      // COPY the material (Node parity): toBytes aliases an existing
      // Uint8Array, so without the copy a later caller zeroing the source
      // buffer (standard key-hygiene) would silently corrupt the key.
      // NOTE: Buffer#slice is a VIEW here (Node semantics) — use a real
      // Uint8Array copy.
      if (typeof key === "object" && key !== null && key.kty === "oct") {
        var raw = new Uint8Array(base64urlDecode(key.k));
        return new KeyObject("secret", raw);
      }
      const material = toBytes(key, encoding);
      return new KeyObject("secret", new Uint8Array(material));
    }

    function detectKeyType(pem) {
      if (typeof pem !== "string") return "unknown";
      if (pem.includes("EC PRIVATE") || pem.includes("EC PUBLIC")) return "ec";
      if (pem.includes("ED25519") || pem.includes("ed25519")) return "ed25519";
      if (pem.includes("RSA PRIVATE") || pem.includes("RSA PUBLIC")) return "rsa";
      if (pem.includes("BEGIN PRIVATE KEY") || pem.includes("BEGIN PUBLIC KEY")) {
        var lines = pem.split("\n").filter(function(l) { return l.length > 0 && l.charAt(0) !== "-"; });
        var b64 = lines.join("");
        var der = BufferCtor.from(b64, "base64");
        var hex = "";
        for (var i = 0; i < der.length && i < 20; i++) hex += ("0" + der[i].toString(16)).slice(-2);
        if (hex.indexOf("06032b6570") !== -1) return "ed25519";
        if (hex.indexOf("06032b6571") !== -1) return "ed25519";
        if (hex.indexOf("2a8648ce3d") !== -1) return "ec";
        return "rsa";
      }
      return "rsa";
    }

    function createPrivateKey(input) {
      var pem;
      if (typeof input === "object" && input !== null && input.format === "jwk" && input.key) {
        var jwk = input.key;
        if (jwk.kty === "RSA") {
          pem = derToPem(rsaJwkToPkcs8(jwk), "PRIVATE KEY");
        } else if (jwk.kty === "EC") {
          var crv = jwk.crv;
          var xBytes = new Uint8Array(base64urlDecode(jwk.x));
          var yBytes = new Uint8Array(base64urlDecode(jwk.y));
          var dBytes = jwk.d ? new Uint8Array(base64urlDecode(jwk.d)) : undefined;
          pem = natives.cryptoEcJwkImport(crv, xBytes, yBytes, dBytes);
        } else {
          throw new Error("createPrivateKey: unsupported JWK kty: " + jwk.kty);
        }
      } else if (typeof input === "string") {
        pem = input;
      } else if (input && typeof input === "object") {
        if (input.key instanceof Uint8Array || BufferCtor.isBuffer(input.key)) {
          pem = new TextDecoder().decode(input.key);
        } else {
          pem = String(input.key);
        }
      } else {
        throw new TypeError("createPrivateKey: input must be a string or object");
      }
      var ko = new KeyObject("private", null);
      ko._pem = pem;
      ko._keyType = detectKeyType(pem);
      ko.asymmetricKeyType = ko._keyType === "rsa" ? "rsa" : ko._keyType === "ec" ? "ec" : "ed25519";
      ko.asymmetricKeySize = undefined;
      ko.export = function(options) {
        if (options && options.format === "jwk") {
          if (ko._keyType === "ec") {
            var comps = natives.cryptoEcJwkExport(pem, true);
            return { kty: "EC", crv: comps.crv, x: base64urlEncode(comps.x), y: base64urlEncode(comps.y), d: base64urlEncode(comps.d) };
          }
          if (ko._keyType === "rsa") {
            var rcomps = natives.cryptoRsaJwkComponents(pem, true);
            return { kty: "RSA", n: base64urlEncode(rcomps.n), e: base64urlEncode(rcomps.e), d: base64urlEncode(rcomps.d), p: base64urlEncode(rcomps.p), q: base64urlEncode(rcomps.q), dp: base64urlEncode(rcomps.dp), dq: base64urlEncode(rcomps.dq), qi: base64urlEncode(rcomps.qi) };
          }
          throw new Error("JWK export not supported for key type: " + ko._keyType);
        }
        if (!options || options.type === "pkcs8" || options.format === "pem") return pem;
        return BufferCtor.from(pem);
      };
      return ko;
    }

    function createPublicKey(input) {
      var pem;
      if (typeof input === "object" && input !== null && input.format === "jwk" && input.key) {
        var jwk = input.key;
        if (jwk.kty === "RSA") {
          pem = derToPem(rsaJwkToSpki(jwk), "PUBLIC KEY");
        } else if (jwk.kty === "EC") {
          var crv = jwk.crv;
          var xBytes = new Uint8Array(base64urlDecode(jwk.x));
          var yBytes = new Uint8Array(base64urlDecode(jwk.y));
          pem = natives.cryptoEcJwkImport(crv, xBytes, yBytes);
        } else {
          throw new Error("createPublicKey: unsupported JWK kty: " + jwk.kty);
        }
      } else if (typeof input === "string") {
        pem = input;
      } else if (input && typeof input === "object") {
        if (input instanceof KeyObject && input.type === "private") {
          pem = natives.cryptoExtractPublicPem(input._pem);
        } else if (input.key instanceof Uint8Array || BufferCtor.isBuffer(input.key)) {
          pem = new TextDecoder().decode(input.key);
        } else {
          pem = String(input.key);
        }
      } else {
        throw new TypeError("createPublicKey: input must be a string or object");
      }
      var ko = new KeyObject("public", null);
      ko._pem = pem;
      ko._keyType = detectKeyType(pem);
      ko.asymmetricKeyType = ko._keyType === "rsa" ? "rsa" : ko._keyType === "ec" ? "ec" : "ed25519";
      ko.asymmetricKeySize = undefined;
      ko.export = function(options) {
        if (options && options.format === "jwk") {
          if (ko._keyType === "ec") {
            var comps = natives.cryptoEcJwkExport(pem, false);
            return { kty: "EC", crv: comps.crv, x: base64urlEncode(comps.x), y: base64urlEncode(comps.y) };
          }
          if (ko._keyType === "rsa") {
            var rcomps = natives.cryptoRsaJwkComponents(pem, false);
            return { kty: "RSA", n: base64urlEncode(rcomps.n), e: base64urlEncode(rcomps.e) };
          }
          throw new Error("JWK export not supported for key type: " + ko._keyType);
        }
        if (!options || options.type === "spki" || options.format === "pem") return pem;
        return BufferCtor.from(pem);
      };
      return ko;
    }

    class Sign extends stream.Transform {
      constructor(algorithm) {
        super();
        this._algorithm = algorithm;
        this._data = [];
      }
      update(data, inputEncoding) {
        this._data.push(toBytes(data, inputEncoding));
        return this;
      }
      sign(key, outputEncoding) {
        var pem, keyType;
        if (key instanceof KeyObject) {
          pem = key._pem || new TextDecoder().decode(key._material);
          keyType = key._keyType || "rsa";
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          pem = typeof key.key === "string" ? key.key : new TextDecoder().decode(key.key);
          keyType = detectKeyType(pem);
        } else {
          pem = typeof key === "string" ? key : new TextDecoder().decode(key);
          keyType = detectKeyType(pem);
        }
        var total = 0;
        for (var i = 0; i < this._data.length; i++) total += this._data[i].length;
        var merged = new Uint8Array(total);
        var off = 0;
        for (var i = 0; i < this._data.length; i++) { merged.set(this._data[i], off); off += this._data[i].length; }
        var padding = 1;
        var saltLength = 32;
        if (key instanceof KeyObject) {
          // KeyObject does not carry padding; default PKCS1
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          if (key.padding !== undefined) padding = key.padding;
          if (key.saltLength !== undefined) saltLength = key.saltLength;
        }
        var sig;
        if (padding === 6 && keyType === "rsa") {
          sig = asBuffer(natives.cryptoSignPss(this._algorithm, merged, pem, saltLength));
        } else {
          sig = asBuffer(natives.cryptoSign(this._algorithm, merged, pem, keyType));
        }
        return outputEncoding ? sig.toString(outputEncoding) : sig;
      }
      _transform(chunk, encoding, callback) {
        this.update(chunk, encoding);
        callback();
      }
    }

    class Verify extends stream.Transform {
      constructor(algorithm) {
        super();
        this._algorithm = algorithm;
        this._data = [];
      }
      update(data, inputEncoding) {
        this._data.push(toBytes(data, inputEncoding));
        return this;
      }
      verify(key, signature, signatureEncoding) {
        var pem, keyType;
        if (key instanceof KeyObject) {
          pem = key._pem || new TextDecoder().decode(key._material);
          keyType = key._keyType || "rsa";
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          pem = typeof key.key === "string" ? key.key : new TextDecoder().decode(key.key);
          keyType = detectKeyType(pem);
        } else {
          pem = typeof key === "string" ? key : new TextDecoder().decode(key);
          keyType = detectKeyType(pem);
        }
        var total = 0;
        for (var i = 0; i < this._data.length; i++) total += this._data[i].length;
        var merged = new Uint8Array(total);
        var off = 0;
        for (var i = 0; i < this._data.length; i++) { merged.set(this._data[i], off); off += this._data[i].length; }
        var sigBuf = typeof signature === "string"
          ? BufferCtor.from(signature, signatureEncoding || "base64")
          : toBytes(signature);
        var padding = 1;
        var saltLength = 32;
        if (key instanceof KeyObject) {
          // KeyObject does not carry padding; default PKCS1
        } else if (typeof key === "object" && key !== null && !(key instanceof Uint8Array)) {
          if (key.padding !== undefined) padding = key.padding;
          if (key.saltLength !== undefined) saltLength = key.saltLength;
        }
        if (padding === 6 && keyType === "rsa") {
          return natives.cryptoVerifyPss(this._algorithm, merged, pem, sigBuf, saltLength);
        }
        return natives.cryptoVerify(this._algorithm, merged, pem, sigBuf, keyType);
      }
      _transform(chunk, encoding, callback) {
        this.update(chunk, encoding);
        callback();
      }
    }

    function createSign(algorithm) { return new Sign(normalizeAlgo(algorithm)); }
    function createVerify(algorithm) { return new Verify(normalizeAlgo(algorithm)); }

    function normalizeAlgo(raw) {
      var s = raw.toUpperCase();
      if (s === "ED25519") return "ed25519";
      if (s === "RSA-SHA256" || s === "SHA256WITHRSA") return "SHA256";
      if (s === "RSA-SHA384" || s === "SHA384WITHRSA") return "SHA384";
      if (s === "RSA-SHA512" || s === "SHA512WITHRSA") return "SHA512";
      if (s === "RSA-SHA1" || s === "SHA1WITHRSA") return "SHA1";
      return raw;
    }

    function signOneShot(algorithm, data, key) {
      return createSign(algorithm).update(data).sign(key);
    }
    function verifyOneShot(algorithm, data, key, signature) {
      return createVerify(algorithm).update(data).verify(key, signature);
    }

    const asBuffer = (bytes) =>
      new BufferCtor(bytes.buffer, bytes.byteOffset, bytes.length);

    class Hash {
      constructor(id) {
        this._id = id;
      }
      update(data, inputEncoding) {
        natives.cryptoHashUpdate(this._id, toBytes(data, inputEncoding));
        return this;
      }
      digest(encoding) {
        const bytes = asBuffer(natives.cryptoHashDigest(this._id));
        return encoding ? bytes.toString(encoding) : bytes;
      }
      copy() {
        return new Hash(natives.cryptoHashCopy(this._id));
      }
    }

    class Hmac extends Hash {
      copy() {
        throw new Error("Hmac.copy is not supported (Node parity)");
      }
    }

    function createHash(algorithm) {
      return new Hash(natives.cryptoHashCreate(String(algorithm)));
    }

    function createHmac(algorithm, key) {
      return new Hmac(natives.cryptoHmacCreate(String(algorithm), toBytes(key)));
    }

    function randomBytes(size, callback) {
      // Chunked: the native caps one call at 64KiB.
      const out = BufferCtor.alloc(size);
      let offset = 0;
      while (offset < size) {
        const chunk = natives.cryptoRandomFill(Math.min(65536, size - offset));
        out.set(chunk, offset);
        offset += chunk.length;
      }
      if (typeof callback === "function") {
        queueMicrotask(() => callback(null, out));
        return undefined;
      }
      return out;
    }

    function randomFillSync(buffer, offset = 0, size) {
      const view =
        buffer instanceof Uint8Array
          ? buffer
          : new Uint8Array(buffer.buffer ?? buffer, buffer.byteOffset ?? 0, buffer.byteLength);
      const count = size ?? view.length - offset;
      const bytes = randomBytes(count);
      view.set(bytes, offset);
      return buffer;
    }

    function randomUUID() {
      const bytes = natives.cryptoRandomFill(16);
      bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
      bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
      const hex = asBuffer(bytes).toString("hex");
      return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
    }

    function randomInt(min, max, callback) {
      if (max === undefined || typeof max === "function") {
        callback = max;
        max = min;
        min = 0;
      }
      if (!Number.isSafeInteger(min) || !Number.isSafeInteger(max) || max <= min) {
        throw new RangeError("randomInt: max must be greater than min (safe integers)");
      }
      const range = max - min;
      // Rejection sampling over 48 bits â€” uniform, like Node.
      let value;
      do {
        const bytes = natives.cryptoRandomFill(6);
        value = 0;
        for (const b of bytes) value = value * 256 + b;
      } while (value >= Math.floor(2 ** 48 / range) * range);
      const result = min + (value % range);
      if (typeof callback === "function") {
        queueMicrotask(() => callback(null, result));
        return undefined;
      }
      return result;
    }

    function timingSafeEqual(a, b) {
      return natives.cryptoTimingSafeEqual(toBytes(a), toBytes(b));
    }

    function webcryptoAlgoName(algorithm) {
      if (typeof algorithm === "string") return algorithm;
      return (algorithm && algorithm.name) || "";
    }
    function webcryptoHashName(algorithm) {
      var n = webcryptoAlgoName(algorithm);
      var h = (algorithm && algorithm.hash) ? webcryptoAlgoName(algorithm.hash) : n;
      return h.replace("-", "").toUpperCase();
    }

    var _importedKeys = new Map();
    var _keyIdCounter = 1;

    function CryptoKey(algorithm, type, extractable, usages, _id) {
      this.algorithm = algorithm;
      this.type = type;
      this.extractable = extractable;
      this.usages = usages;
      this._id = _id;
    }

    const subtle = {
      async digest(algorithm, data) {
        const name = typeof algorithm === "string" ? algorithm : algorithm?.name;
        const id = natives.cryptoHashCreate(String(name));
        natives.cryptoHashUpdate(id, toBytes(data));
        const bytes = natives.cryptoHashDigest(id);
        return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length);
      },

      async importKey(format, keyData, algorithm, extractable, keyUsages) {
        var algoObj = typeof algorithm === "string" ? { name: algorithm } : algorithm;
        var algoName = algoObj.name.toUpperCase();
        var id = _keyIdCounter++;
        var keyType;
        var pem;

        if (format === "raw") {
          var material = keyData instanceof ArrayBuffer ? new Uint8Array(keyData) : new Uint8Array(keyData.buffer, keyData.byteOffset, keyData.byteLength);
          _importedKeys.set(id, { format: "raw", data: material, algo: algoObj });
          keyType = "secret";
        } else if (format === "pkcs8") {
          var der = keyData instanceof ArrayBuffer ? new Uint8Array(keyData) : new Uint8Array(keyData.buffer, keyData.byteOffset, keyData.byteLength);
          pem = derToPem(der, "PRIVATE KEY");
          _importedKeys.set(id, { format: "pkcs8", pem: pem, algo: algoObj });
          keyType = "private";
        } else if (format === "spki") {
          var der2 = keyData instanceof ArrayBuffer ? new Uint8Array(keyData) : new Uint8Array(keyData.buffer, keyData.byteOffset, keyData.byteLength);
          pem = derToPem(der2, "PUBLIC KEY");
          _importedKeys.set(id, { format: "spki", pem: pem, algo: algoObj });
          keyType = "public";
        } else if (format === "jwk") {
          var kty = keyData.kty;
          if (kty === "oct") {
            var raw = new Uint8Array(base64urlDecode(keyData.k));
            _importedKeys.set(id, { format: "raw", data: raw, algo: algoObj });
            keyType = "secret";
          } else if (kty === "RSA") {
            if (keyData.d) {
              pem = derToPem(rsaJwkToPkcs8(keyData), "PRIVATE KEY");
              _importedKeys.set(id, { format: "pkcs8", pem: pem, algo: algoObj });
              keyType = "private";
            } else {
              pem = derToPem(rsaJwkToSpki(keyData), "PUBLIC KEY");
              _importedKeys.set(id, { format: "spki", pem: pem, algo: algoObj });
              keyType = "public";
            }
          } else if (kty === "EC") {
            var ecCrv = keyData.crv;
            var ecX = new Uint8Array(base64urlDecode(keyData.x));
            var ecY = new Uint8Array(base64urlDecode(keyData.y));
            if (keyData.d) {
              var ecD = new Uint8Array(base64urlDecode(keyData.d));
              pem = natives.cryptoEcJwkImport(ecCrv, ecX, ecY, ecD);
              _importedKeys.set(id, { format: "pkcs8", pem: pem, algo: algoObj });
              keyType = "private";
            } else {
              pem = natives.cryptoEcJwkImport(ecCrv, ecX, ecY);
              _importedKeys.set(id, { format: "spki", pem: pem, algo: algoObj });
              keyType = "public";
            }
          } else {
            throw new Error("subtle.importKey: unsupported JWK kty: " + kty);
          }
        } else {
          throw new Error("subtle.importKey: unsupported format " + format);
        }

        return new CryptoKey(algoObj, keyType, extractable, keyUsages, id);
      },

      async exportKey(format, key) {
        var stored = _importedKeys.get(key._id);
        if (!stored) throw new Error("subtle.exportKey: unknown key");
        if (!key.extractable) throw new Error("subtle.exportKey: key is not extractable");

        if (format === "raw" && stored.format === "raw") {
          return stored.data.buffer.slice(stored.data.byteOffset, stored.data.byteOffset + stored.data.byteLength);
        }
        if (format === "pkcs8" && stored.pem) {
          return pemToDer(stored.pem);
        }
        if (format === "spki" && stored.pem) {
          return pemToDer(stored.pem);
        }
        throw new Error("subtle.exportKey: unsupported format " + format + " for key type " + stored.format);
      },

      async sign(algorithm, key, data) {
        var algoName = webcryptoAlgoName(algorithm).toUpperCase();
        var stored = _importedKeys.get(key._id);
        if (!stored) throw new Error("subtle.sign: unknown key");

        if (algoName === "HMAC") {
          var hashName = webcryptoHashName(stored.algo);
          var hmacId = natives.cryptoHmacCreate(hashName, stored.data);
          natives.cryptoHashUpdate(hmacId, toBytes(data));
          var result = natives.cryptoHashDigest(hmacId);
          return result.buffer.slice(result.byteOffset, result.byteOffset + result.length);
        }

        if (algoName === "ED25519") {
          var sigBytes = natives.cryptoSign("ed25519", toBytes(data), stored.pem, "ed25519");
          return sigBytes.buffer.slice(sigBytes.byteOffset, sigBytes.byteOffset + sigBytes.length);
        }

        if (algoName === "RSASSA-PKCS1-V1_5") {
          var hashN = webcryptoHashName(algorithm);
          var sigBytes2 = natives.cryptoSign(hashN, toBytes(data), stored.pem, "rsa");
          return sigBytes2.buffer.slice(sigBytes2.byteOffset, sigBytes2.byteOffset + sigBytes2.length);
        }

        if (algoName === "ECDSA") {
          var hashN2 = webcryptoHashName(algorithm);
          var sigBytes3 = natives.cryptoSign(hashN2, toBytes(data), stored.pem, "ec");
          return sigBytes3.buffer.slice(sigBytes3.byteOffset, sigBytes3.byteOffset + sigBytes3.length);
        }

        if (algoName === "RSA-PSS" || algoName === "RSASSA-PSS") {
          var pssHash = webcryptoHashName(stored.algo);
          var pssSaltLen = (algorithm && algorithm.saltLength !== undefined) ? algorithm.saltLength : 32;
          var pssSig = natives.cryptoSignPss(pssHash, toBytes(data), stored.pem, pssSaltLen);
          return pssSig.buffer.slice(pssSig.byteOffset, pssSig.byteOffset + pssSig.length);
        }

        throw new Error("subtle.sign: unsupported algorithm " + algoName);
      },

      async verify(algorithm, key, signature, data) {
        var algoName = webcryptoAlgoName(algorithm).toUpperCase();
        var stored = _importedKeys.get(key._id);
        if (!stored) throw new Error("subtle.verify: unknown key");

        if (algoName === "HMAC") {
          var hashName = webcryptoHashName(stored.algo);
          var hmacId = natives.cryptoHmacCreate(hashName, stored.data);
          natives.cryptoHashUpdate(hmacId, toBytes(data));
          var expected = natives.cryptoHashDigest(hmacId);
          var sigArr = signature instanceof ArrayBuffer ? new Uint8Array(signature) : new Uint8Array(signature.buffer, signature.byteOffset, signature.byteLength);
          if (expected.length !== sigArr.length) return false;
          return natives.cryptoTimingSafeEqual(expected, sigArr);
        }

        if (algoName === "ED25519") {
          var sigBuf = signature instanceof ArrayBuffer ? new Uint8Array(signature) : new Uint8Array(signature.buffer, signature.byteOffset, signature.byteLength);
          return natives.cryptoVerify("ed25519", toBytes(data), stored.pem, sigBuf, "ed25519");
        }

        if (algoName === "RSASSA-PKCS1-V1_5") {
          var hashN = webcryptoHashName(algorithm);
          var sigBuf2 = signature instanceof ArrayBuffer ? new Uint8Array(signature) : new Uint8Array(signature.buffer, signature.byteOffset, signature.byteLength);
          return natives.cryptoVerify(hashN, toBytes(data), stored.pem, sigBuf2, "rsa");
        }

        if (algoName === "ECDSA") {
          var hashN2 = webcryptoHashName(algorithm);
          var sigBuf3 = signature instanceof ArrayBuffer ? new Uint8Array(signature) : new Uint8Array(signature.buffer, signature.byteOffset, signature.byteLength);
          return natives.cryptoVerify(hashN2, toBytes(data), stored.pem, sigBuf3, "ec");
        }

        if (algoName === "RSA-PSS" || algoName === "RSASSA-PSS") {
          var pssHash2 = webcryptoHashName(stored.algo);
          var pssSaltLen2 = (algorithm && algorithm.saltLength !== undefined) ? algorithm.saltLength : 32;
          var pssSigBuf = signature instanceof ArrayBuffer ? new Uint8Array(signature) : new Uint8Array(signature.buffer, signature.byteOffset, signature.byteLength);
          return natives.cryptoVerifyPss(pssHash2, toBytes(data), stored.pem, pssSigBuf, pssSaltLen2);
        }

        throw new Error("subtle.verify: unsupported algorithm " + algoName);
      },

      async generateKey(algorithm, extractable, keyUsages) {
        var algoName = webcryptoAlgoName(algorithm).toUpperCase();
        if (algoName === "ED25519") {
          var pair = natives.cryptoGenerateKeyPair("ed25519");
          var privId = _keyIdCounter++;
          var pubId = _keyIdCounter++;
          _importedKeys.set(privId, { format: "pkcs8", pem: pair.privateKey, algo: { name: "Ed25519" } });
          _importedKeys.set(pubId, { format: "spki", pem: pair.publicKey, algo: { name: "Ed25519" } });
          return {
            privateKey: new CryptoKey({ name: "Ed25519" }, "private", extractable, keyUsages.filter(function(u) { return u === "sign"; }), privId),
            publicKey: new CryptoKey({ name: "Ed25519" }, "public", extractable, keyUsages.filter(function(u) { return u === "verify"; }), pubId),
          };
        }
        if (algoName === "HMAC") {
          var len = (algorithm.length || 256) / 8;
          var raw = natives.cryptoRandomFill(len);
          var kid = _keyIdCounter++;
          _importedKeys.set(kid, { format: "raw", data: raw, algo: algorithm });
          return new CryptoKey(algorithm, "secret", extractable, keyUsages, kid);
        }
        if (algoName === "AES-GCM" || algoName === "AES-CBC" || algoName === "AES-CTR") {
          var aesLen = (algorithm.length || 256) / 8;
          var aesRaw = natives.cryptoRandomFill(aesLen);
          var aesKid = _keyIdCounter++;
          _importedKeys.set(aesKid, { format: "raw", data: aesRaw, algo: algorithm });
          return new CryptoKey(algorithm, "secret", extractable, keyUsages, aesKid);
        }
        throw new Error("subtle.generateKey: unsupported algorithm " + algoName);
      },
      async deriveBits(algorithm, baseKey, length) {
        var algoName = webcryptoAlgoName(typeof algorithm === "string" ? algorithm : algorithm.name);
        var stored = _importedKeys.get(baseKey._id);
        if (!stored) throw new Error("subtle.deriveBits: unknown key");
        var keyData = stored.data instanceof ArrayBuffer ? new Uint8Array(stored.data) : stored.data;
        if (algoName === "PBKDF2") {
          var salt = new Uint8Array(algorithm.salt);
          var iterations = algorithm.iterations;
          var hashName = webcryptoHashName(algorithm.hash);
          var result = natives.cryptoPbkdf2Sync(
            BufferCtor.from(keyData),
            BufferCtor.from(salt),
            iterations,
            Math.ceil(length / 8),
            hashName
          );
          return result.buffer.slice(result.byteOffset, result.byteOffset + result.byteLength);
        }
        if (algoName === "HKDF") {
          var hSalt = new Uint8Array(algorithm.salt);
          var hInfo = new Uint8Array(algorithm.info);
          var hHash = webcryptoHashName(algorithm.hash);
          var result2 = natives.cryptoHkdfSync(
            hHash,
            BufferCtor.from(keyData),
            BufferCtor.from(hSalt),
            BufferCtor.from(hInfo),
            Math.ceil(length / 8)
          );
          return result2.buffer.slice(result2.byteOffset, result2.byteOffset + result2.byteLength);
        }
        throw new Error("subtle.deriveBits: unsupported algorithm " + algoName);
      },
      async deriveKey(algorithm, baseKey, derivedKeyType, extractable, keyUsages) {
        var dkAlgoName = webcryptoAlgoName(typeof derivedKeyType === "string" ? derivedKeyType : derivedKeyType.name);
        var length = derivedKeyType.length;
        if (!length && dkAlgoName === "AES-CBC") length = 256;
        if (!length && dkAlgoName === "AES-GCM") length = 256;
        if (!length && dkAlgoName === "AES-CTR") length = 256;
        if (!length && dkAlgoName === "HMAC") {
          length = 256;
        }
        var bits = await this.deriveBits(algorithm, baseKey, length);
        return this.importKey("raw", bits, derivedKeyType, extractable, keyUsages);
      },
      async encrypt(algorithm, key, data) {
        var algoName = webcryptoAlgoName(algorithm).toUpperCase();
        var stored = _importedKeys.get(key._id);
        if (!stored) throw new Error("subtle.encrypt: unknown key");
        var keyMat = stored.data;
        var plainArr = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
        if (algoName === "AES-GCM") {
          var iv = new Uint8Array(algorithm.iv);
          var cn = keyMat.length === 16 ? "aes-128-gcm" : "aes-256-gcm";
          var h = natives.cryptoCipherCreate(cn, keyMat, iv, true);
          if (algorithm.additionalData) natives.cryptoCipherSetAad(h, new Uint8Array(algorithm.additionalData));
          natives.cryptoCipherUpdate(h, plainArr);
          var ct = natives.cryptoCipherFinalGcm(h);
          var tag = natives.cryptoCipherGetAuthTag(h);
          var out = new Uint8Array(ct.length + tag.length);
          out.set(ct, 0);
          out.set(tag, ct.length);
          return out.buffer;
        }
        if (algoName === "AES-CBC") {
          var iv2 = new Uint8Array(algorithm.iv);
          var cn2 = keyMat.length === 16 ? "aes-128-cbc" : "aes-256-cbc";
          var h2 = natives.cryptoCipherCreate(cn2, keyMat, iv2, true);
          natives.cryptoCipherUpdate(h2, plainArr);
          var ct2 = natives.cryptoCipherFinal(h2);
          return ct2.buffer.slice(ct2.byteOffset, ct2.byteOffset + ct2.byteLength);
        }
        if (algoName === "AES-CTR") {
          var ctr = new Uint8Array(algorithm.counter);
          var cn3 = keyMat.length === 16 ? "aes-128-ctr" : "aes-256-ctr";
          var h3 = natives.cryptoCipherCreate(cn3, keyMat, ctr, true);
          natives.cryptoCipherUpdate(h3, plainArr);
          var ct3 = natives.cryptoCipherFinal(h3);
          return ct3.buffer.slice(ct3.byteOffset, ct3.byteOffset + ct3.byteLength);
        }
        throw new Error("subtle.encrypt: unsupported algorithm " + algoName);
      },
      async decrypt(algorithm, key, data) {
        var algoName = webcryptoAlgoName(algorithm).toUpperCase();
        var stored = _importedKeys.get(key._id);
        if (!stored) throw new Error("subtle.decrypt: unknown key");
        var keyMat = stored.data;
        var cipherArr = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
        if (algoName === "AES-GCM") {
          var iv = new Uint8Array(algorithm.iv);
          var tagLen = (algorithm.tagLength || 128) / 8;
          var cn = keyMat.length === 16 ? "aes-128-gcm" : "aes-256-gcm";
          var ctPart = cipherArr.slice(0, cipherArr.length - tagLen);
          var tagPart = cipherArr.slice(cipherArr.length - tagLen);
          var h = natives.cryptoCipherCreate(cn, keyMat, iv, false);
          if (algorithm.additionalData) natives.cryptoCipherSetAad(h, new Uint8Array(algorithm.additionalData));
          natives.cryptoCipherSetAuthTag(h, tagPart);
          natives.cryptoCipherUpdate(h, ctPart);
          var pt = natives.cryptoCipherFinalGcm(h);
          return pt.buffer.slice(pt.byteOffset, pt.byteOffset + pt.byteLength);
        }
        if (algoName === "AES-CBC") {
          var iv2 = new Uint8Array(algorithm.iv);
          var cn2 = keyMat.length === 16 ? "aes-128-cbc" : "aes-256-cbc";
          var h2 = natives.cryptoCipherCreate(cn2, keyMat, iv2, false);
          natives.cryptoCipherUpdate(h2, cipherArr);
          var pt2 = natives.cryptoCipherFinal(h2);
          return pt2.buffer.slice(pt2.byteOffset, pt2.byteOffset + pt2.byteLength);
        }
        if (algoName === "AES-CTR") {
          var ctr = new Uint8Array(algorithm.counter);
          var cn3 = keyMat.length === 16 ? "aes-128-ctr" : "aes-256-ctr";
          var h3 = natives.cryptoCipherCreate(cn3, keyMat, ctr, false);
          natives.cryptoCipherUpdate(h3, cipherArr);
          var pt3 = natives.cryptoCipherFinal(h3);
          return pt3.buffer.slice(pt3.byteOffset, pt3.byteOffset + pt3.byteLength);
        }
        throw new Error("subtle.decrypt: unsupported algorithm " + algoName);
      },
      async wrapKey(format, key, wrappingKey, wrapAlgorithm) {
        var exported = await this.exportKey(format, key);
        var data = exported instanceof ArrayBuffer ? exported : new Uint8Array(exported).buffer;
        return this.encrypt(wrapAlgorithm, wrappingKey, data);
      },
      async unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm, extractable, keyUsages) {
        var decrypted = await this.decrypt(unwrapAlgorithm, unwrappingKey, wrappedKey);
        return this.importKey(format, decrypted, unwrappedKeyAlgorithm, extractable, keyUsages);
      },
    };

    function derToPem(der, label) {
      var b64 = BufferCtor.from(der).toString("base64");
      var pem = "-----BEGIN " + label + "-----\n";
      for (var i = 0; i < b64.length; i += 64) {
        pem += b64.slice(i, i + 64) + "\n";
      }
      pem += "-----END " + label + "-----\n";
      return pem;
    }

    function pemToDer(pem) {
      var lines = pem.split("\n").filter(function(l) { return l.length > 0 && l.charAt(0) !== "-"; });
      var buf = BufferCtor.from(lines.join(""), "base64");
      return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
    }

    function getRandomValues(typedArray) {
      if (
        !ArrayBuffer.isView(typedArray) ||
        typedArray instanceof Float32Array ||
        typedArray instanceof Float64Array ||
        typedArray instanceof DataView
      ) {
        throw new TypeError("getRandomValues: argument must be an integer TypedArray");
      }
      if (typedArray.byteLength > 65536) {
        throw Object.assign(
          new Error("getRandomValues: requested too many random bytes (max 65536)"),
          { name: "QuotaExceededError" },
        );
      }
      const bytes = natives.cryptoRandomFill(typedArray.byteLength);
      new Uint8Array(typedArray.buffer, typedArray.byteOffset, typedArray.byteLength).set(bytes);
      return typedArray;
    }


    // ---- wave 2: key derivation (pbkdf2 / scrypt / hkdf) ----

    function pbkdf2Sync(password, salt, iterations, keylen, digest) {
      const result = natives.cryptoPbkdf2Sync(
        toBytes(password), toBytes(salt), iterations, keylen, String(digest),
      );
      return asBuffer(result);
    }

    function pbkdf2(password, salt, iterations, keylen, digest, callback) {
      try {
        const result = pbkdf2Sync(password, salt, iterations, keylen, digest);
        queueMicrotask(() => callback(null, result));
      } catch (err) {
        queueMicrotask(() => callback(err));
      }
    }

    function scryptSync(password, salt, keylen, options) {
      const N = options?.N ?? options?.cost ?? 16384;
      const r = options?.r ?? options?.blockSize ?? 8;
      const p = options?.p ?? options?.parallelization ?? 1;
      const result = natives.cryptoScryptSync(
        toBytes(password), toBytes(salt), keylen, N, r, p,
      );
      return asBuffer(result);
    }

    function scrypt(password, salt, keylen, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      try {
        const result = scryptSync(password, salt, keylen, options);
        queueMicrotask(() => callback(null, result));
      } catch (err) {
        queueMicrotask(() => callback(err));
      }
    }

    function hkdfSync(digest, ikm, salt, info, keylen) {
      const result = natives.cryptoHkdfSync(
        String(digest), toBytes(ikm), toBytes(salt), toBytes(info), keylen,
      );
      return result.buffer.slice(result.byteOffset, result.byteOffset + result.length);
    }

    function hkdf(digest, ikm, salt, info, keylen, callback) {
      try {
        const result = hkdfSync(digest, ikm, salt, info, keylen);
        queueMicrotask(() => callback(null, result));
      } catch (err) {
        queueMicrotask(() => callback(err));
      }
    }

    // ---- wave 2: symmetric ciphers (AES-CBC / CTR / GCM) ----

    const SUPPORTED_CIPHERS = [
      "aes-128-cbc", "aes-256-cbc",
      "aes-128-ctr", "aes-256-ctr",
      "aes-128-gcm", "aes-256-gcm",
    ];

    class Cipher {
      constructor(handle, isGcm) {
        this._handle = handle;
        this._isGcm = isGcm;
        this._finalized = false;
        this._authTag = null;
      }
      update(data, inputEncoding, outputEncoding) {
        if (this._finalized) throw new Error("Attempting to use a finalized cipher");
        natives.cryptoCipherUpdate(this._handle, toBytes(data, inputEncoding));
        const empty = BufferCtor.alloc(0);
        return outputEncoding ? empty.toString(outputEncoding) : empty;
      }
      final(outputEncoding) {
        if (this._finalized) throw new Error("Attempting to use a finalized cipher");
        this._finalized = true;
        let result;
        if (this._isGcm) {
          result = asBuffer(natives.cryptoCipherFinalGcm(this._handle));
          this._authTag = asBuffer(natives.cryptoCipherGetAuthTag(this._handle));
        } else {
          result = asBuffer(natives.cryptoCipherFinal(this._handle));
        }
        return outputEncoding ? result.toString(outputEncoding) : result;
      }
      setAutoPadding(autoPadding) {
        natives.cryptoCipherSetAutoPadding(
          this._handle, autoPadding === undefined ? true : !!autoPadding,
        );
        return this;
      }
      setAAD(buffer) {
        natives.cryptoCipherSetAad(this._handle, toBytes(buffer));
        return this;
      }
      getAuthTag() {
        if (!this._authTag) {
          throw new Error("getAuthTag: not available (call final() first for GCM encrypt)");
        }
        return this._authTag;
      }
    }

    class Decipher {
      constructor(handle, isGcm) {
        this._handle = handle;
        this._isGcm = isGcm;
        this._finalized = false;
      }
      update(data, inputEncoding, outputEncoding) {
        if (this._finalized) throw new Error("Attempting to use a finalized decipher");
        natives.cryptoCipherUpdate(this._handle, toBytes(data, inputEncoding));
        const empty = BufferCtor.alloc(0);
        return outputEncoding ? empty.toString(outputEncoding) : empty;
      }
      final(outputEncoding) {
        if (this._finalized) throw new Error("Attempting to use a finalized decipher");
        this._finalized = true;
        let result;
        if (this._isGcm) {
          result = asBuffer(natives.cryptoCipherFinalGcm(this._handle));
        } else {
          result = asBuffer(natives.cryptoCipherFinal(this._handle));
        }
        return outputEncoding ? result.toString(outputEncoding) : result;
      }
      setAutoPadding(autoPadding) {
        natives.cryptoCipherSetAutoPadding(
          this._handle, autoPadding === undefined ? true : !!autoPadding,
        );
        return this;
      }
      setAAD(buffer) {
        natives.cryptoCipherSetAad(this._handle, toBytes(buffer));
        return this;
      }
      setAuthTag(tag) {
        natives.cryptoCipherSetAuthTag(this._handle, toBytes(tag));
        return this;
      }
    }

    function createCipheriv(algorithm, key, iv) {
      const alg = String(algorithm).toLowerCase();
      const isGcm = alg.endsWith("-gcm");
      const handle = natives.cryptoCipherCreate(alg, toBytes(key), toBytes(iv), true);
      return new Cipher(handle, isGcm);
    }

    function createDecipheriv(algorithm, key, iv) {
      const alg = String(algorithm).toLowerCase();
      const isGcm = alg.endsWith("-gcm");
      const handle = natives.cryptoCipherCreate(alg, toBytes(key), toBytes(iv), false);
      return new Decipher(handle, isGcm);
    }

    function getCiphers() {
      return SUPPORTED_CIPHERS.slice();
    }

    function generateKeyPairSync(type, options) {
      const result = natives.cryptoGenerateKeyPair(type, (options && options.modulusLength) || 0, (options && options.namedCurve) || "");
      const format = (options && options.publicKeyEncoding && options.publicKeyEncoding.format) || 'pem';
      const privFormat = (options && options.privateKeyEncoding && options.privateKeyEncoding.format) || 'pem';
      let pubOut = result.publicKey;
      let privOut = result.privateKey;
      if (format === 'der') {
        const lines = pubOut.split('\n').filter(l => !l.startsWith('-----'));
        pubOut = Buffer.from(lines.join(''), 'base64');
      }
      if (privFormat === 'der') {
        const lines = privOut.split('\n').filter(l => !l.startsWith('-----'));
        privOut = Buffer.from(lines.join(''), 'base64');
      }
      if (format === 'jwk') {
        if (type === 'ec') {
          var pubEcComps = natives.cryptoEcJwkExport(result.publicKey, false);
          pubOut = { kty: 'EC', crv: pubEcComps.crv, x: base64urlEncode(pubEcComps.x), y: base64urlEncode(pubEcComps.y) };
        } else {
          var pubComps = natives.cryptoRsaJwkComponents(result.publicKey, false);
          pubOut = { kty: 'RSA', n: base64urlEncode(pubComps.n), e: base64urlEncode(pubComps.e) };
        }
      }
      if (privFormat === 'jwk') {
        if (type === 'ec') {
          var privEcComps = natives.cryptoEcJwkExport(result.privateKey, true);
          privOut = { kty: 'EC', crv: privEcComps.crv, x: base64urlEncode(privEcComps.x), y: base64urlEncode(privEcComps.y), d: base64urlEncode(privEcComps.d) };
        } else {
          var privComps = natives.cryptoRsaJwkComponents(result.privateKey, true);
          privOut = {
            kty: 'RSA',
            n: base64urlEncode(privComps.n),
            e: base64urlEncode(privComps.e),
            d: base64urlEncode(privComps.d),
            p: base64urlEncode(privComps.p),
            q: base64urlEncode(privComps.q),
            dp: base64urlEncode(privComps.dp),
            dq: base64urlEncode(privComps.dq),
            qi: base64urlEncode(privComps.qi),
          };
        }
      }
      return { publicKey: pubOut, privateKey: privOut };
    }
    function generateKeyPair(type, options, callback) {
      if (typeof options === 'function') { callback = options; options = {}; }
      try {
        const result = generateKeyPairSync(type, options);
        if (callback) process.nextTick(() => callback(null, result.publicKey, result.privateKey));
      } catch (err) {
        if (callback) process.nextTick(() => callback(err));
        else throw err;
      }
    }



    class ECDH {
      constructor(curve) {
        this._curve = curve;
        this._publicKey = null;
        this._privateKey = null;
      }
      generateKeys(encoding, format) {
        var result = natives.cryptoEcdhGenerateKeys(this._curve);
        this._publicKey = result.publicKey;
        this._privateKey = result.privateKey;
        return this.getPublicKey(encoding, format);
      }
      computeSecret(otherPublicKey, inputEncoding, outputEncoding) {
        if (!this._privateKey) throw new Error("ECDH: keys have not been generated");
        var otherKey = typeof otherPublicKey === "string"
          ? BufferCtor.from(otherPublicKey, inputEncoding || "utf8")
          : otherPublicKey;
        var secret = natives.cryptoEcdhComputeSecret(this._curve, new Uint8Array(this._privateKey), new Uint8Array(otherKey));
        var buf = BufferCtor.from(secret);
        return outputEncoding ? buf.toString(outputEncoding) : buf;
      }
      getPublicKey(encoding, format) {
        if (!this._publicKey) throw new Error("ECDH: keys have not been generated");
        var buf = BufferCtor.from(this._publicKey);
        if (format === "compressed") {
          var len = (buf.length - 1) / 2;
          var x = buf.subarray(1, 1 + len);
          var prefix = (buf[buf.length - 1] & 1) ? 0x03 : 0x02;
          var out = BufferCtor.alloc(1 + len);
          out[0] = prefix;
          x.copy(out, 1);
          buf = out;
        }
        return encoding ? buf.toString(encoding) : buf;
      }
      getPrivateKey(encoding) {
        if (!this._privateKey) throw new Error("ECDH: keys have not been generated");
        var buf = BufferCtor.from(this._privateKey);
        return encoding ? buf.toString(encoding) : buf;
      }
      setPrivateKey(key, encoding) {
        this._privateKey = typeof key === "string"
          ? new Uint8Array(BufferCtor.from(key, encoding || "utf8"))
          : new Uint8Array(key);
        this._publicKey = natives.cryptoEcdhGetPublicKey(this._curve, this._privateKey);
      }
      setPublicKey(key, encoding) {
        this._publicKey = typeof key === "string"
          ? new Uint8Array(BufferCtor.from(key, encoding || "utf8"))
          : new Uint8Array(key);
      }
    }

    function createECDH(curveName) {
      return new ECDH(curveName);
    }

    // ---- ASN.1 / JWK helpers ----
    function base64urlDecode(str) {
      str = str.replace(/-/g, '+').replace(/_/g, '/');
      while (str.length % 4 !== 0) str += '=';
      return BufferCtor.from(str, 'base64');
    }
    function base64urlEncode(buf) {
      return BufferCtor.from(buf).toString('base64')
        .replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    }
    function asn1Length(len) {
      if (len < 128) return [len];
      var bytes = [];
      var tmp = len;
      while (tmp > 0) { bytes.unshift(tmp & 0xFF); tmp >>= 8; }
      bytes.unshift(0x80 | bytes.length);
      return bytes;
    }
    function asn1Wrap(tag, content) {
      var len = asn1Length(content.length);
      var out = new Uint8Array(1 + len.length + content.length);
      out[0] = tag;
      out.set(len, 1);
      out.set(content, 1 + len.length);
      return out;
    }
    function asn1Int(bytes) {
      if (bytes[0] >= 0x80) {
        var padded = new Uint8Array(bytes.length + 1);
        padded.set(bytes, 1);
        bytes = padded;
      }
      return asn1Wrap(0x02, bytes);
    }
    function asn1Seq(parts) {
      var totalLen = 0;
      for (var i = 0; i < parts.length; i++) totalLen += parts[i].length;
      var content = new Uint8Array(totalLen);
      var off = 0;
      for (var i = 0; i < parts.length; i++) {
        content.set(parts[i], off);
        off += parts[i].length;
      }
      return asn1Wrap(0x30, content);
    }
    var RSA_OID_BYTES = new Uint8Array([0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00]);

    function rsaJwkToSpki(jwk) {
      var n = new Uint8Array(base64urlDecode(jwk.n));
      var e = new Uint8Array(base64urlDecode(jwk.e));
      var pubKeySeq = asn1Seq([asn1Int(n), asn1Int(e)]);
      var bitStr = asn1Wrap(0x03, (function() {
        var bs = new Uint8Array(1 + pubKeySeq.length);
        bs[0] = 0x00;
        bs.set(pubKeySeq, 1);
        return bs;
      })());
      var totalLen = RSA_OID_BYTES.length + bitStr.length;
      var content = new Uint8Array(totalLen);
      content.set(RSA_OID_BYTES, 0);
      content.set(bitStr, RSA_OID_BYTES.length);
      return asn1Wrap(0x30, content);
    }

    function rsaJwkToPkcs8(jwk) {
      var n = new Uint8Array(base64urlDecode(jwk.n));
      var e = new Uint8Array(base64urlDecode(jwk.e));
      var d = new Uint8Array(base64urlDecode(jwk.d));
      var p = new Uint8Array(base64urlDecode(jwk.p));
      var q = new Uint8Array(base64urlDecode(jwk.q));
      var dp = new Uint8Array(base64urlDecode(jwk.dp));
      var dq = new Uint8Array(base64urlDecode(jwk.dq));
      var qi = new Uint8Array(base64urlDecode(jwk.qi));
      var version = asn1Int(new Uint8Array([0]));
      var rsaPriv = asn1Seq([version, asn1Int(n), asn1Int(e), asn1Int(d), asn1Int(p), asn1Int(q), asn1Int(dp), asn1Int(dq), asn1Int(qi)]);
      var octetStr = asn1Wrap(0x04, rsaPriv);
      var pkcs8Version = asn1Int(new Uint8Array([0]));
      return asn1Seq([pkcs8Version, RSA_OID_BYTES, octetStr]);
    }

    function rsaJwkToPkcs1(jwk) {
      var n = new Uint8Array(base64urlDecode(jwk.n));
      var e = new Uint8Array(base64urlDecode(jwk.e));
      return asn1Seq([asn1Int(n), asn1Int(e)]);
    }

    function publicEncrypt(keyOrOpts, buffer) {
      var key, padding = 4, oaepHash = "sha1";
      if (typeof keyOrOpts === "string") {
        key = keyOrOpts;
      } else if (ArrayBuffer.isView(keyOrOpts)) {
        key = new TextDecoder().decode(keyOrOpts);
      } else if (keyOrOpts && typeof keyOrOpts === "object") {
        key = typeof keyOrOpts.key === "string" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);
        if (keyOrOpts.padding !== undefined) padding = keyOrOpts.padding;
        if (keyOrOpts.oaepHash) oaepHash = keyOrOpts.oaepHash;
      } else {
        throw new TypeError("publicEncrypt: key must be a string, Buffer, or object");
      }
      var paddingName = padding === 1 ? "pkcs1" : "oaep";
      var data = typeof buffer === "string" ? BufferCtor.from(buffer) : buffer;
      return BufferCtor.from(natives.cryptoPublicEncrypt(new Uint8Array(data), key, paddingName, oaepHash));
    }

    function privateDecrypt(keyOrOpts, buffer) {
      var key, padding = 4, oaepHash = "sha1";
      if (typeof keyOrOpts === "string") {
        key = keyOrOpts;
      } else if (ArrayBuffer.isView(keyOrOpts)) {
        key = new TextDecoder().decode(keyOrOpts);
      } else if (keyOrOpts && typeof keyOrOpts === "object") {
        key = typeof keyOrOpts.key === "string" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);
        if (keyOrOpts.padding !== undefined) padding = keyOrOpts.padding;
        if (keyOrOpts.oaepHash) oaepHash = keyOrOpts.oaepHash;
      } else {
        throw new TypeError("privateDecrypt: key must be a string, Buffer, or object");
      }
      var paddingName = padding === 1 ? "pkcs1" : "oaep";
      var data = typeof buffer === "string" ? BufferCtor.from(buffer) : buffer;
      return BufferCtor.from(natives.cryptoPrivateDecrypt(new Uint8Array(data), key, paddingName, oaepHash));
    }

    function privateEncrypt(keyOrOpts, buffer) {
      var key;
      if (typeof keyOrOpts === "string") {
        key = keyOrOpts;
      } else if (ArrayBuffer.isView(keyOrOpts)) {
        key = new TextDecoder().decode(keyOrOpts);
      } else if (keyOrOpts && typeof keyOrOpts === "object") {
        key = typeof keyOrOpts.key === "string" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);
      } else {
        throw new TypeError("privateEncrypt: key must be a string, Buffer, or object");
      }
      var data = typeof buffer === "string" ? BufferCtor.from(buffer) : buffer;
      return BufferCtor.from(natives.cryptoPrivateEncrypt(new Uint8Array(data), key));
    }

    function publicDecrypt(keyOrOpts, buffer) {
      var key;
      if (typeof keyOrOpts === "string") {
        key = keyOrOpts;
      } else if (ArrayBuffer.isView(keyOrOpts)) {
        key = new TextDecoder().decode(keyOrOpts);
      } else if (keyOrOpts && typeof keyOrOpts === "object") {
        key = typeof keyOrOpts.key === "string" ? keyOrOpts.key : new TextDecoder().decode(keyOrOpts.key);
      } else {
        throw new TypeError("publicDecrypt: key must be a string, Buffer, or object");
      }
      var data = typeof buffer === "string" ? BufferCtor.from(buffer) : buffer;
      return BufferCtor.from(natives.cryptoPublicDecrypt(new Uint8Array(data), key));
    }


    // ---- Diffie-Hellman (classic, non-EC) ----
    var DH_GROUPS = {
      modp1: "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6BFFFFFFFFFFFFFFFF",
      modp2: "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381FFFFFFFFFFFFFFFF",
      modp5: "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA237327FFFFFFFFFFFFFFFF",
      modp14: "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AACAA68FFFFFFFFFFFFFFFF",
    };

    class DiffieHellman {
      constructor(prime, generator) {
        if (typeof prime === "number") {
          var primeBytes = BufferCtor.from(natives.cryptoGeneratePrime(prime));
          prime = primeBytes;
        }
        this._prime = BufferCtor.isBuffer(prime) ? prime : BufferCtor.from(prime);
        if (!generator) generator = BufferCtor.from([2]);
        else if (typeof generator === "number") generator = BufferCtor.from([generator]);
        else if (!BufferCtor.isBuffer(generator)) generator = BufferCtor.from(generator);
        this._generator = generator;
        this._publicKey = null;
        this._privateKey = null;
      }
      generateKeys(encoding) {
        var result = natives.cryptoDhGenerateKeys(
          new Uint8Array(this._prime),
          new Uint8Array(this._generator)
        );
        this._publicKey = BufferCtor.from(result.publicKey);
        this._privateKey = BufferCtor.from(result.privateKey);
        return this.getPublicKey(encoding);
      }
      computeSecret(otherPublicKey, inputEncoding, outputEncoding) {
        if (!this._privateKey) throw new Error("DH: keys have not been generated");
        var otherKey = typeof otherPublicKey === "string"
          ? BufferCtor.from(otherPublicKey, inputEncoding || "hex")
          : BufferCtor.from(otherPublicKey);
        var secret = natives.cryptoDhComputeSecret(
          new Uint8Array(this._prime),
          new Uint8Array(this._privateKey),
          new Uint8Array(otherKey)
        );
        var buf = BufferCtor.from(secret);
        return outputEncoding ? buf.toString(outputEncoding) : buf;
      }
      getPrime(encoding) {
        var buf = BufferCtor.from(this._prime);
        return encoding ? buf.toString(encoding) : buf;
      }
      getGenerator(encoding) {
        var buf = BufferCtor.from(this._generator);
        return encoding ? buf.toString(encoding) : buf;
      }
      getPublicKey(encoding) {
        if (!this._publicKey) throw new Error("DH: keys have not been generated");
        var buf = BufferCtor.from(this._publicKey);
        return encoding ? buf.toString(encoding) : buf;
      }
      getPrivateKey(encoding) {
        if (!this._privateKey) throw new Error("DH: keys have not been generated");
        var buf = BufferCtor.from(this._privateKey);
        return encoding ? buf.toString(encoding) : buf;
      }
      setPublicKey(key, encoding) {
        this._publicKey = typeof key === "string"
          ? BufferCtor.from(key, encoding || "hex")
          : BufferCtor.from(key);
      }
      setPrivateKey(key, encoding) {
        this._privateKey = typeof key === "string"
          ? BufferCtor.from(key, encoding || "hex")
          : BufferCtor.from(key);
      }
      get verifyError() { return 0; }
    }

    function createDiffieHellman(primeOrLen, primeEncoding, generator, generatorEncoding) {
      if (typeof primeOrLen === "number") {
        var primeBytes = BufferCtor.from(natives.cryptoGeneratePrime(primeOrLen));
        return new DiffieHellman(primeBytes, BufferCtor.from([2]));
      }
      var prime = typeof primeOrLen === "string"
        ? BufferCtor.from(primeOrLen, primeEncoding || "hex")
        : BufferCtor.from(primeOrLen);
      var gen;
      if (generator === undefined || generator === null) {
        gen = BufferCtor.from([2]);
      } else if (typeof generator === "number") {
        gen = BufferCtor.from([generator]);
      } else if (typeof generator === "string") {
        gen = BufferCtor.from(generator, generatorEncoding || "hex");
      } else {
        gen = BufferCtor.from(generator);
      }
      return new DiffieHellman(prime, gen);
    }

    function getDiffieHellman(groupName) {
      var hex = DH_GROUPS[groupName.toLowerCase()];
      if (!hex) throw new Error("Unknown DH group: " + groupName);
      return new DiffieHellman(BufferCtor.from(hex, "hex"), BufferCtor.from([2]));
    }


    // ---- X.509 Certificate ----
    class X509Certificate {
      constructor(buf) {
        if (typeof buf === "string") buf = BufferCtor.from(buf);
        else if (!BufferCtor.isBuffer(buf)) buf = BufferCtor.from(buf);
        var parsed = natives.cryptoX509Parse(new Uint8Array(buf));
        this._subject = parsed.subject;
        this._issuer = parsed.issuer;
        this._serialNumber = parsed.serialNumber;
        this._validFrom = parsed.validFrom;
        this._validTo = parsed.validTo;
        this._fingerprint = parsed.fingerprint;
        this._fingerprint256 = parsed.fingerprint256;
        this._ca = parsed.ca;
        this._subjectAltName = parsed.subjectAltName || "";
        this._keyUsage = parsed.keyUsage || [];
        this._raw = BufferCtor.from(parsed.raw);
      }
      get subject() { return this._subject; }
      get issuer() { return this._issuer; }
      get serialNumber() { return this._serialNumber; }
      get validFrom() { return this._validFrom; }
      get validTo() { return this._validTo; }
      get fingerprint() { return this._fingerprint; }
      get fingerprint256() { return this._fingerprint256; }
      get ca() { return this._ca; }
      get subjectAltName() { return this._subjectAltName; }
      get keyUsage() { return this._keyUsage; }
      get raw() { return this._raw; }
      toString() {
        var b64 = this._raw.toString("base64");
        var out = [];
        for (var i = 0; i < b64.length; i += 64) out.push(b64.slice(i, i + 64));
        return "-----BEGIN CERTIFICATE-----\n" + out.join("\n") + "\n-----END CERTIFICATE-----\n";
      }
      toJSON() { return this.toString(); }
      toLegacyObject() {
        return {
          subject: this._subject,
          issuer: this._issuer,
          serialNumber: this._serialNumber,
          valid_from: this._validFrom,
          valid_to: this._validTo,
          fingerprint: this._fingerprint,
          fingerprint256: this._fingerprint256,
        };
      }
    }

    const webcrypto = { subtle, getRandomValues, randomUUID };

    class Certificate {
      static exportChallenge() { return globalThis.Buffer.alloc(0); }
      static exportPublicKey() { return globalThis.Buffer.alloc(0); }
      static verifySpkac() { return false; }
      exportChallenge() { return Certificate.exportChallenge.apply(null, arguments); }
      exportPublicKey() { return Certificate.exportPublicKey.apply(null, arguments); }
      verifySpkac() { return Certificate.verifySpkac.apply(null, arguments); }
    }

    function generatePrimeSync(size, options) {
      var bigint = options && options.bigint;
      var bytes = natives.cryptoGeneratePrime(size);
      if (bigint) {
        var hex = "";
        for (var i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
        return BigInt("0x" + hex);
      }
      return BufferCtor.from(bytes);
    }

    function generatePrime(size, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      try {
        var result = generatePrimeSync(size, options);
        if (callback) queueMicrotask(function() { callback(null, result); });
        else return result;
      } catch (err) {
        if (callback) queueMicrotask(function() { callback(err); });
        else throw err;
      }
    }

    function checkPrimeSync(candidate, options) {
      var buf;
      if (typeof candidate === "bigint") {
        var hex = candidate.toString(16);
        if (hex.length % 2 !== 0) hex = "0" + hex;
        buf = BufferCtor.from(hex, 'hex');
      } else {
        buf = BufferCtor.from(candidate);
      }
      return natives.cryptoCheckPrime(new Uint8Array(buf));
    }

    function checkPrime(candidate, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      try {
        var result = checkPrimeSync(candidate, options);
        if (callback) queueMicrotask(function() { callback(null, result); });
        else return result;
      } catch (err) {
        if (callback) queueMicrotask(function() { callback(err); });
        else throw err;
      }
    }

    return {
      hash: (algorithm, data, outputEncoding) => {
        var h = createHash(algorithm);
        h.update(data);
        return outputEncoding ? h.digest(outputEncoding) : h.digest();
      },
      createHash,
      createHmac,
      randomBytes,
      pseudoRandomBytes: randomBytes,
      rng: randomBytes,
      prng: randomBytes,
      randomFillSync,
      randomUUID,
      randomInt,
      timingSafeEqual,
      getHashes: () => ["md5", "sha1", "sha224", "sha256", "sha384", "sha512"],
      getCurves: () => ["prime256v1", "secp256k1", "secp384r1", "secp521r1", "ed25519", "ed448", "x25519", "x448"],
      generateKeySync: (type, options) => {
        if (type === "aes") {
          var len = (options && options.length) || 256;
          return createSecretKey(natives.cryptoRandomFill(len / 8));
        }
        if (type === "hmac") {
          var hLen = (options && options.length) || 256;
          return createSecretKey(natives.cryptoRandomFill(hLen / 8));
        }
        throw new Error("generateKeySync: unsupported type " + type);
      },
      generateKey: (type, options, callback) => {
        try {
          var result;
          if (type === "aes") {
            var len = (options && options.length) || 256;
            result = createSecretKey(natives.cryptoRandomFill(len / 8));
          } else if (type === "hmac") {
            var hLen = (options && options.length) || 256;
            result = createSecretKey(natives.cryptoRandomFill(hLen / 8));
          } else {
            throw new Error("generateKey: unsupported type " + type);
          }
          if (callback) queueMicrotask(() => callback(null, result));
          else return result;
        } catch (err) {
          if (callback) queueMicrotask(() => callback(err));
          else throw err;
        }
      },
      getCurves: () => ["prime256v1", "secp256k1", "secp384r1", "secp521r1", "ed25519", "ed448", "x25519", "x448"],
      generateKeySync: (type, options) => {
        if (type === "aes") {
          var len = (options && options.length) || 256;
          return createSecretKey(natives.cryptoRandomFill(len / 8));
        }
        if (type === "hmac") {
          var hLen = (options && options.length) || 256;
          return createSecretKey(natives.cryptoRandomFill(hLen / 8));
        }
        throw new Error("generateKeySync: unsupported type " + type);
      },
      generateKey: (type, options, callback) => {
        try {
          var result;
          if (type === "aes") {
            var len = (options && options.length) || 256;
            result = createSecretKey(natives.cryptoRandomFill(len / 8));
          } else if (type === "hmac") {
            var hLen = (options && options.length) || 256;
            result = createSecretKey(natives.cryptoRandomFill(hLen / 8));
          } else {
            throw new Error("generateKey: unsupported type " + type);
          }
          if (callback) queueMicrotask(() => callback(null, result));
          else return result;
        } catch (err) {
          if (callback) queueMicrotask(() => callback(err));
          else throw err;
        }
      },
      getCiphers,
      getFips: () => 0,
      setFips: () => { throw new Error("Cannot set FIPS mode in this environment"); },
      secureHeapUsed: () => ({ total: 0, min: 0, used: 0 }),
      KeyObject,
      createSecretKey,
      createPrivateKey,
      createPublicKey,
      generateKeyPairSync,
      generateKeyPair,
      createECDH,
      ECDH,
      publicEncrypt,
      privateEncrypt,
      privateDecrypt,
      publicDecrypt,
      createDiffieHellman,
      getDiffieHellman,
      DiffieHellman,
      X509Certificate,
      generatePrime,
      generatePrimeSync,
      checkPrime,
      checkPrimeSync,
      createSign,
      createVerify,
      sign: signOneShot,
      verify: verifyOneShot,
      Sign,
      Verify,
      Certificate,
      constants: {
        RSA_PKCS1_PADDING: 1,
        RSA_NO_PADDING: 3,
        RSA_PKCS1_OAEP_PADDING: 4,
        RSA_PKCS1_PSS_PADDING: 6,
        RSA_PKCS1_SHA256_PADDING: 12,
        POINT_CONVERSION_COMPRESSED: 2,
        POINT_CONVERSION_UNCOMPRESSED: 4,
        SSL_OP_ALL: 0x80000bff,
        SSL_OP_NO_SSLv2: 0x01000000,
        SSL_OP_NO_SSLv3: 0x02000000,
        SSL_OP_NO_TLSv1: 0x04000000,
        SSL_OP_NO_TLSv1_1: 0x10000000,
        SSL_OP_NO_TLSv1_2: 0x08000000,
        SSL_OP_NO_TLSv1_3: 0x20000000,
        defaultCoreCipherList: "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256",
      },
      createCipheriv,
      createDecipheriv,
      pbkdf2Sync,
      pbkdf2,
      scryptSync,
      scrypt,
      hkdfSync,
      hkdf,
      webcrypto,
      subtle,
      getRandomValues,
    };
  };

  // ------------------------------------------------------------ node:zlib
  // gzip/deflate/deflateRaw + unzip auto-detect. Sync forms run on the
  // isolate thread (the API contract); callback forms ride the async op
  // (CPU work on the blocking pool, Node's threadpool model). create*
  // Transform classes BUFFER input and emit on flush â€” wave-1 divergence,
  // documented: true incremental compression streams land later. brotli*
  // is gated with a pointer.
  registry.factories.zlib = (natives) => {
    const BufferCtor = globalThis.Buffer;
    const toBytes = (data) =>
      typeof data === "string"
        ? BufferCtor.from(data, "utf8")
        : data instanceof Uint8Array
          ? data
          : ArrayBuffer.isView(data)
            ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
            : new Uint8Array(data);
    const asBuffer = (bytes) => new BufferCtor(bytes.buffer, bytes.byteOffset, bytes.length);
    const levelOf = (options) => options?.level ?? -1;

    const Z_NO_FLUSH = 0;
    const Z_PARTIAL_FLUSH = 1;
    const Z_SYNC_FLUSH = 2;
    const Z_FULL_FLUSH = 3;
    const Z_FINISH = 4;
    const DEFLATE = 1;
    const INFLATE = 2;
    const DEFLATERAW = 5;
    const INFLATERAW = 6;

    class ZlibHandle {
      constructor(mode) {
        this._mode = mode;
        this._nativeHandle = null;
        this.onerror = null;
        this._owner = null;
      }
      init(windowBits, level, memLevel, strategy, writeState, processCallback, dictionary) {
        this._writeState = writeState;
        this._processCallback = processCallback;
        const effectiveLevel = (this._mode === DEFLATE || this._mode === DEFLATERAW) ? (level != null ? level : -1) : -1;
        this._nativeHandle = natives.zlibHandleCreate(this._mode, effectiveLevel);
      }
      writeSync(flush, chunk, inOff, inLen, buffer, outOff, outLen) {
        const input = (chunk && inLen > 0) ? chunk.subarray(inOff, inOff + inLen) : new Uint8Array(0);
        const result = natives.zlibHandleWriteSync(
          this._nativeHandle, flush, input, buffer, outOff, outLen
        );
        // native returns [availOutAfter, availInAfter]; keep that order in
        // _writeState (Node's documented internal order) but RETURN
        // [availInAfter, availOutAfter] -- the pair a synchronous _processChunk
        // loop consumes as `res = writeSync(...)` then
        // `handleChunk(res[0]=availInAfter, res[1]=availOutAfter)` (pngjs).
        this._writeState[0] = result[0];
        this._writeState[1] = result[1];
        return [result[1], result[0]];
      }
      close() {
        if (this._nativeHandle !== null) {
          natives.zlibStreamClose(this._nativeHandle);
          this._nativeHandle = null;
        }
      }
    }


    const sync = (format, compress) => (data, options) =>
      asBuffer(natives.zlibSync(toBytes(data), format, levelOf(options), compress));
    const callbackForm = (format, compress) => (data, options, callback) => {
      if (typeof options === "function") {
        callback = options;
        options = undefined;
      }
      natives.zlibAsync(toBytes(data), format, levelOf(options), compress).then(
        (bytes) => callback(null, asBuffer(bytes)),
        (err) => callback(err),
      );
    };

    // Incremental streaming Transform: each _transform call feeds one
    // chunk into the Rust-side encoder/decoder immediately via
    // zlibStreamWrite, so memory usage is bounded by the encoder's
    // internal block size (typically 32-128 kB) rather than the whole
    // input. _flush finalizes the stream and emits any trailer bytes.
    //
    // Supported formats (incremental): gzip, deflate, deflateRaw, unzip.
    // Brotli uses the legacy buffering path with a clear error gate.
    //
    // The stream handle is created lazily on the first _transform call so
    // that streams that are only used for sync ops (e.g. pipe target with
    // no actual data) do not allocate a registry slot.
    function streamingTransformClass(format, compress) {
      const { Transform } = registry.get("stream");
      return class extends Transform {
        constructor(options) {
          super({});
          this._zlibLevel = levelOf(options);
          // _zlibHandle is null until the first chunk arrives.
          this._zlibHandle = null;
          // Promise serializing back-to-back _transform calls so we
          // never have two concurrent zlibStreamWrite ops for the same handle.
          this._zlibQueue = Promise.resolve();
          this._handle = Object.create(ZlibHandle.prototype);
        }

        // Lazily allocate the Rust-side stream on first use.
        _ensureStream() {
          if (this._zlibHandle !== null) return Promise.resolve();
          return natives.zlibStreamCreate(format, this._zlibLevel, compress).then(
            (info) => { this._zlibHandle = info.handle; },
          );
        }

        _transform(chunk, _encoding, cb) {
          const bytes = toBytes(chunk);
          // Chain onto the queue so writes stay in order.
          this._zlibQueue = this._zlibQueue.then(() =>
            this._ensureStream().then(() =>
              natives.zlibStreamWrite(this._zlibHandle, bytes)
            )
          ).then((out) => {
            if (out && out.length > 0) this.push(asBuffer(out));
            cb();
          }, cb);
        }

        _flush(cb) {
          this._zlibQueue = this._zlibQueue.then(() => {
            if (this._zlibHandle === null) {
              // No data was ever written -- create+immediately flush an
              // empty stream so the output is a valid (empty) archive.
              return natives.zlibStreamCreate(format, this._zlibLevel, compress)
                .then((info) => {
                  this._zlibHandle = info.handle;
                  return natives.zlibStreamFlush(this._zlibHandle);
                });
            }
            return natives.zlibStreamFlush(this._zlibHandle);
          }).then((tail) => {
            this._zlibHandle = null;
            if (tail && tail.length > 0) cb(null, asBuffer(tail));
            else cb();
          }, cb);
        }

        // destroy() path: clean up the Rust-side slot without flushing.
        _destroy(err, cb) {
          if (this._zlibHandle !== null) {
            natives.zlibStreamClose(this._zlibHandle);
            this._zlibHandle = null;
          }
          cb(err);
        }
      };
    }

    // Wave-1 buffering fallback: kept for brotli (not yet incrementally
    // backed) and any format that is not in the streaming set.
    function bufferingTransformClass(format, compress) {
      const { Transform } = registry.get("stream");
      return class extends Transform {
        constructor(options) {
          super({});
          this._zlibChunks = [];
          this._zlibLevel = levelOf(options);
        }
        _transform(chunk, _encoding, cb) {
          this._zlibChunks.push(toBytes(chunk));
          cb();
        }
        _flush(cb) {
          const total = this._zlibChunks.reduce((n, c) => n + c.length, 0);
          const joined = new Uint8Array(total);
          let offset = 0;
          for (const c of this._zlibChunks) {
            joined.set(c, offset);
            offset += c.length;
          }
          natives.zlibAsync(joined, format, this._zlibLevel, compress).then(
            (bytes) => cb(null, asBuffer(bytes)),
            (err) => cb(err),
          );
        }
      };
    }

    // Route: use incremental streaming for gzip/deflate/deflateRaw/unzip/brotli;
    // fall back to the buffering path for any unrecognized future formats.
    const STREAMING_FORMATS = new Set(["gzip", "deflate", "deflateRaw", "unzip", "brotli"]);
    function transformClass(format, compress) {
      if (STREAMING_FORMATS.has(format)) {
        return streamingTransformClass(format, compress);
      }
      return bufferingTransformClass(format, compress);
    }

    // One-shot brotli: create stream, write all data, flush -- returns Buffer.
    // The streaming Transform class handles incremental paths automatically
    // (brotli is in STREAMING_FORMATS above).
    const brotliOneShot = (data, compress) => {
      const bytes = toBytes(data);
      return natives.zlibStreamCreate("brotli", -1, compress).then((info) => {
        const handle = info.handle;
        return natives.zlibStreamWrite(handle, bytes).then((chunk1) =>
          natives.zlibStreamFlush(handle).then((chunk2) => {
            const total = (chunk1 ? chunk1.length : 0) + (chunk2 ? chunk2.length : 0);
            if (total === 0) return asBuffer(new Uint8Array(0));
            const out = new Uint8Array(total);
            let off = 0;
            if (chunk1 && chunk1.length > 0) { out.set(chunk1, off); off += chunk1.length; }
            if (chunk2 && chunk2.length > 0) { out.set(chunk2, off); }
            return asBuffer(out);
          })
        );
      });
    };
    const brotliCallbackForm = (compress) => (data, options, callback) => {
      if (typeof options === "function") { callback = options; }
      brotliOneShot(data, compress).then(
        (bytes) => callback(null, bytes),
        (err) => callback(err),
      );
    };
    const brotliSyncGate = () => {
      throw new Error(
        "brotliCompressSync/brotliDecompressSync are not supported -- use brotliCompress/brotliDecompress (async) instead"
      );
    };

    // ---- Node-faithful stream CLASS constructors (Inflate/Deflate/...) -----
    // Exported so transpiled CJS that INHERITS from them works -- pngjs does
    //   util.inherits(MyInflate, zlib.Inflate); zlib.Inflate.call(this, opts);
    // then drives this._handle.writeSync(...) from its own sync _processChunk.
    // Both init paths share the streaming Transform prototype, so instances
    // are real Transforms / EventEmitters either way:
    //   new zlib.Inflate(opts)        -> async streaming Transform
    //   zlib.Inflate.call(this, opts) -> Node sync-handle state on `this`
    const Z_DEFAULT_CHUNK = 16384;
    const Z_MIN_CHUNK = 64;
    // The low-level sync handle (flate2) supports only DEFLATE/INFLATE and
    // their raw forms; gzip/unzip/brotli have no sync-handle mode, so the
    // .call(this) path maps them to the zlib-wrapped deflate handle. No known
    // consumer inherits from those -- pngjs only inherits zlib.Inflate.
    const handleModeFor = (format, compress) =>
      format === "deflateRaw"
        ? (compress ? DEFLATERAW : INFLATERAW)
        : (compress ? DEFLATE : INFLATE);
    function initSyncHandleState(self, mode, options) {
      const opts = options || {};
      let chunkSize = opts.chunkSize != null ? opts.chunkSize : Z_DEFAULT_CHUNK;
      if (chunkSize < Z_MIN_CHUNK) chunkSize = Z_MIN_CHUNK;
      self._chunkSize = chunkSize;
      self._writeState = new Uint32Array(2);
      self._offset = 0;
      self._outOffset = 0;
      self._buffer = BufferCtor.allocUnsafe(chunkSize);
      self._outBuffer = self._buffer;
      self._hadError = false;
      self._finishFlushFlag = Z_FINISH;
      const handle = new ZlibHandle(mode);
      handle.init(15, levelOf(opts), 8, 0, self._writeState, () => {}, opts.dictionary);
      self._handle = handle;
      return self;
    }
    function makeZlibClass(format, compress) {
      const Stream = transformClass(format, compress); // class extends Transform
      const mode = handleModeFor(format, compress);
      function ZlibClass(options) {
        // `new zlib.Inflate(opts)` -> a real streaming Transform instance.
        if (new.target) return Reflect.construct(Stream, [options], new.target);
        // `zlib.Inflate.call(this, opts)` -> sync-handle state for inheritance.
        return initSyncHandleState(this, mode, options);
      }
      // Share the streaming Transform prototype so `new` instances get the
      // streaming methods AND util.inherits(Sub, ZlibClass) chains
      // Sub -> Stream.prototype -> Transform.prototype -> ... -> EventEmitter.
      ZlibClass.prototype = Stream.prototype;
      return ZlibClass;
    }
    const Gzip = makeZlibClass("gzip", true);
    const Gunzip = makeZlibClass("gzip", false);
    const Deflate = makeZlibClass("deflate", true);
    const Inflate = makeZlibClass("deflate", false);
    const DeflateRaw = makeZlibClass("deflateRaw", true);
    const InflateRaw = makeZlibClass("deflateRaw", false);
    const Unzip = makeZlibClass("unzip", false);
    const BrotliCompress = makeZlibClass("brotli", true);
    const BrotliDecompress = makeZlibClass("brotli", false);

    return {
      gzipSync: sync("gzip", true),
      gunzipSync: sync("gzip", false),
      deflateSync: sync("deflate", true),
      inflateSync: sync("deflate", false),
      deflateRawSync: sync("deflateRaw", true),
      inflateRawSync: sync("deflateRaw", false),
      unzipSync: sync("unzip", false),
      gzip: callbackForm("gzip", true),
      gunzip: callbackForm("gzip", false),
      deflate: callbackForm("deflate", true),
      inflate: callbackForm("deflate", false),
      deflateRaw: callbackForm("deflateRaw", true),
      inflateRaw: callbackForm("deflateRaw", false),
      unzip: callbackForm("unzip", false),
      createGzip: (o) => new Gzip(o),
      createGunzip: (o) => new Gunzip(o),
      createDeflate: (o) => new Deflate(o),
      createInflate: (o) => new Inflate(o),
      createDeflateRaw: (o) => new DeflateRaw(o),
      createInflateRaw: (o) => new InflateRaw(o),
      createUnzip: (o) => new Unzip(o),
      createBrotliCompress: (o) => new BrotliCompress(o),
      createBrotliDecompress: (o) => new BrotliDecompress(o),
      Gzip, Gunzip, Deflate, Inflate, DeflateRaw, InflateRaw, Unzip,
      BrotliCompress, BrotliDecompress,
      brotliCompressSync: brotliSyncGate,
      brotliDecompressSync: brotliSyncGate,
      brotliCompress: brotliCallbackForm(true),
      brotliDecompress: brotliCallbackForm(false),
      // Top-level chunk/flush constants (Node exposes these on the module
      // itself, not only under `constants`; pngjs reads zlib.Z_MIN_CHUNK).
      Z_MIN_CHUNK,
      Z_DEFAULT_CHUNK,
      Z_MAX_CHUNK: Infinity,
      Z_NO_FLUSH,
      Z_PARTIAL_FLUSH,
      Z_SYNC_FLUSH,
      Z_FULL_FLUSH,
      Z_FINISH,
      constants: {
        Z_MIN_CHUNK,
        Z_DEFAULT_CHUNK,
        Z_MAX_CHUNK: Infinity,
        Z_NO_COMPRESSION: 0,
        Z_BEST_SPEED: 1,
        Z_BEST_COMPRESSION: 9,
        Z_DEFAULT_COMPRESSION: -1,
        Z_OK: 0,
        Z_STREAM_END: 1,
        Z_DATA_ERROR: -3,
        Z_NO_FLUSH: 0,
        Z_PARTIAL_FLUSH: 1,
        Z_SYNC_FLUSH: 2,
        Z_FULL_FLUSH: 3,
        Z_FINISH: 4,
        Z_DEFAULT_WINDOWBITS: 15,
        Z_DEFAULT_MEMLEVEL: 8,
        Z_DEFAULT_STRATEGY: 0,
        DEFLATE: 1,
        INFLATE: 2,
        GZIP: 3,
        GUNZIP: 4,
        DEFLATERAW: 5,
        INFLATERAW: 6,
        BROTLI_OPERATION_PROCESS: 0,
        BROTLI_OPERATION_FLUSH: 1,
        BROTLI_OPERATION_FINISH: 2,
        BROTLI_DEFAULT_QUALITY: 4,
        BROTLI_MIN_QUALITY: 0,
        BROTLI_MAX_QUALITY: 11,
        BROTLI_DEFAULT_WINDOW: 22,
        BROTLI_MIN_WINDOW_BITS: 10,
        BROTLI_MAX_WINDOW_BITS: 24,
      },
    };
  };

  // ----------------------------------------------------- node:querystring
  // Legacy querystring: escape ~= encodeURIComponent (space -> %20),
  // parse decodes '+' as space, repeated keys become arrays, custom
  // separators supported â€” all node-probed semantics.
  registry.factories.querystring = () => {
    const unescape = (text) => {
      try {
        return decodeURIComponent(text);
      } catch {
        // Malformed sequences decode per-piece, bad pieces stay literal.
        return text.replace(/%[0-9A-Fa-f]{2}/g, (m) => {
          try {
            return decodeURIComponent(m);
          } catch {
            return m;
          }
        });
      }
    };
    const escape = (text) => encodeURIComponent(String(text));

    function parse(input, sep = "&", eq = "=", options = {}) {
      const out = Object.create(null);
      if (typeof input !== "string" || input.length === 0) return out;
      const maxKeys = options.maxKeys ?? 1000;
      const pieces = input.split(sep);
      const limit = maxKeys > 0 ? Math.min(pieces.length, maxKeys) : pieces.length;
      for (let i = 0; i < limit; i++) {
        const piece = pieces[i];
        if (piece.length === 0) continue;
        const idx = piece.indexOf(eq);
        const rawKey = idx === -1 ? piece : piece.slice(0, idx);
        const rawValue = idx === -1 ? "" : piece.slice(idx + eq.length);
        const key = unescape(rawKey.replaceAll("+", " "));
        const value = unescape(rawValue.replaceAll("+", " "));
        if (key in out) {
          if (Array.isArray(out[key])) out[key].push(value);
          else out[key] = [out[key], value];
        } else {
          out[key] = value;
        }
      }
      return out;
    }

    function stringify(obj, sep = "&", eq = "=", _options = {}) {
      if (obj === null || typeof obj !== "object") return "";
      const parts = [];
      for (const key of Object.keys(obj)) {
        const escapedKey = escape(key);
        const value = obj[key];
        if (Array.isArray(value)) {
          for (const item of value) parts.push(`${escapedKey}${eq}${escape(stringifyPrimitive(item))}`);
        } else {
          parts.push(`${escapedKey}${eq}${escape(stringifyPrimitive(value))}`);
        }
      }
      return parts.join(sep);
    }

    function stringifyPrimitive(value) {
      if (typeof value === "string") return value;
      if (typeof value === "number" && Number.isFinite(value)) return String(value);
      if (typeof value === "boolean") return String(value);
      if (typeof value === "bigint") return String(value);
      return "";
    }

    return { parse, stringify, escape, unescape, decode: parse, encode: stringify };
  };

  // ------------------------------------------------- node:timers/promises
  registry.factories["timers/promises"] = () => {
    function promisedTimeout(delay, value, options = {}) {
      const signal = options.signal;
      // Already-aborted: reject immediately, never schedule (Node).
      if (signal?.aborted) {
        return Promise.reject(
          signal.reason ??
            new globalThis.DOMException("The operation was aborted", "AbortError"),
        );
      }
      return new Promise((resolve, reject) => {
        const id = globalThis.setTimeout(() => resolve(value), delay ?? 1);
        signal?.addEventListener?.(
          "abort",
          () => {
            globalThis.clearTimeout(id);
            reject(
              signal.reason ??
                new globalThis.DOMException("The operation was aborted", "AbortError"),
            );
          },
          { once: true },
        );
      });
    }
    function promisedImmediate(value) {
      return new Promise((resolve) => globalThis.setTimeout(() => resolve(value), 0));
    }
    async function* intervalIterator(delay, value, options) {
      const signal = options?.signal;
      if (signal?.aborted) {
        const e = new Error("AbortError");
        e.name = "AbortError";
        throw e;
      }
      // Own the timer id so we can clearTimeout on abort -- promisedTimeout
      // does not expose its handle, and a fire-and-forget timer per tick
      // would retain a slot until natural expiry on every aborted interval.
      let timerId = null;
      let abortListener = null;
      try {
        for (;;) {
          const aborted = await new Promise((resolve) => {
            timerId = globalThis.setTimeout(() => {
              timerId = null;
              resolve(false);
            }, delay);
            if (signal) {
              abortListener = () => resolve(true);
              signal.addEventListener("abort", abortListener, { once: true });
            }
          });
          if (signal && abortListener) {
            signal.removeEventListener("abort", abortListener);
            abortListener = null;
          }
          if (aborted) {
            if (timerId !== null) {
              globalThis.clearTimeout(timerId);
              timerId = null;
            }
            const e = new Error("AbortError");
            e.name = "AbortError";
            throw e;
          }
          yield value;
        }
      } finally {
        if (timerId !== null) {
          globalThis.clearTimeout(timerId);
        }
        if (signal && abortListener) {
          signal.removeEventListener("abort", abortListener);
        }
      }
    }
    return {
      setTimeout: promisedTimeout,
      setImmediate: promisedImmediate,
      setInterval: intervalIterator,
      scheduler: {
        wait: (delay) => promisedTimeout(delay, undefined),
        yield: () => promisedImmediate(undefined),
      },
    };
  };

  // ----------------------------------------------------------- node:console
  // require('console') is the global console plus a Console class bound to
  // caller-provided writables.
  registry.factories.console = () => {
    const util = registry.get("util");
    class Console {
      constructor(stdout, stderr) {
        const options = stdout && stdout.write ? { stdout, stderr } : (stdout ?? {});
        this._out = options.stdout;
        this._err = options.stderr ?? options.stdout;
        if (!this._out || typeof this._out.write !== "function") {
          throw new TypeError("Console expects a writable stream instance");
        }
        const writeTo = (stream) => (...args) => {
          stream.write(`${util.format(...args)}\n`);
        };
        this.log = writeTo(this._out);
        this.info = this.log;
        this.debug = this.log;
        this.warn = writeTo(this._err);
        this.error = this.warn;
      }
    }
    const mod = Object.create(globalThis.console);
    mod.Console = Console;
    return mod;
  };

  // ------------------------------------------------------------ node:http
  // Server side over the same natives oam.serve uses. createServer's
  // ServerResponse streams: the first write() opens a chunked response,
  // so res.write per SSE event flushes immediately. The http CLIENT
  // (http.request/get) is gated â€” use fetch (documented).
  registry.factories.http = (natives) => {
    const EventEmitter = registry.get("events");
    const { Readable } = registry.get("stream");

    class IncomingMessage extends Readable {
      constructor(meta) {
        super({});
        this.method = meta.method;
        this.url = meta.uri;
        this.httpVersion = "1.1";
        this.headers = {};
        this.rawHeaders = [];
        for (const [name, value] of meta.headers) {
          const key = name.toLowerCase();
          this.headers[key] = key in this.headers ? `${this.headers[key]}, ${value}` : value;
          this.rawHeaders.push(name, value);
        }
        this.socket = { remoteAddress: "127.0.0.1", encrypted: false };
        this._requestId = meta.requestId;
        this._bodyPushed = false;
      }
      _read() {
        if (!this._bodyPushed) {
          this._bodyPushed = true;
          const body = natives.httpRequestBody(this._requestId);
          if (body.length > 0) {
            this.push(new globalThis.Buffer(body.buffer, body.byteOffset, body.length));
          }
          this.push(null);
        }
      }
    }

    class ServerResponse extends EventEmitter {
      constructor(requestId) {
        super();
        this._requestId = requestId;
        this._headers = new Map();
        this._streamId = null;
        this._ended = false;
        this._chain = Promise.resolve(); // serializes streaming writes
        this.statusCode = 200;
        this.statusMessage = "";
        this.headersSent = false;
      }
      setHeader(name, value) {
        this._headers.set(String(name).toLowerCase(), value);
        return this;
      }
      getHeader(name) {
        return this._headers.get(String(name).toLowerCase());
      }
      getHeaderNames() {
        return [...this._headers.keys()];
      }
      removeHeader(name) {
        this._headers.delete(String(name).toLowerCase());
      }
      hasHeader(name) {
        return this._headers.has(String(name).toLowerCase());
      }
      writeHead(status, message, headers) {
        if (typeof message === "object" && message !== null) {
          headers = message;
          message = undefined;
        }
        this.statusCode = status;
        if (message) this.statusMessage = message;
        if (headers) {
          if (Array.isArray(headers)) {
            for (let i = 0; i + 1 < headers.length; i += 2) this.setHeader(headers[i], headers[i + 1]);
          } else {
            for (const key of Object.keys(headers)) this.setHeader(key, headers[key]);
          }
        }
        return this;
      }
      _headerPairsJson() {
        const pairs = [];
        for (const [key, value] of this._headers) {
          if (Array.isArray(value)) for (const item of value) pairs.push([key, String(item)]);
          else pairs.push([key, String(value)]);
        }
        return JSON.stringify(pairs);
      }
      _toBytes(chunk, encoding) {
        if (chunk === null || chunk === undefined) return new Uint8Array(0);
        if (typeof chunk === "string") return globalThis.Buffer.from(chunk, encoding ?? "utf8");
        if (chunk instanceof Uint8Array) return chunk;
        if (ArrayBuffer.isView(chunk)) {
          return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
        }
        // Node throws ERR_INVALID_ARG_TYPE rather than shipping garbage.
        throw Object.assign(
          new TypeError(
            `The "chunk" argument must be of type string or an instance of Buffer or Uint8Array. Received ${typeof chunk}`,
          ),
          { code: "ERR_INVALID_ARG_TYPE" },
        );
      }
      write(chunk, encoding, cb) {
        if (typeof encoding === "function") {
          cb = encoding;
          encoding = undefined;
        }
        if (this._ended) return false;
        const bytes = this._toBytes(chunk, encoding);
        if (this._streamId === null) {
          this.headersSent = true;
          this._streamId = natives.httpRespondStream(
            this._requestId,
            this.statusCode,
            this._headerPairsJson(),
          );
        }
        // SERIALIZE: each push chains on the previous one. Independent
        // unawaited ops raced (chunks reordered, dropped, and end() pulled
        // the stream out from under in-flight writes), corrupting every
        // multi-chunk response. The chain guarantees byte order.
        const streamId = this._streamId;
        this._chain = this._chain.then(() => natives.httpBodyPush(streamId, bytes)).then(
          () => cb?.(),
          (err) => {
            if (this.listenerCount("error") > 0) this.emit("error", err);
            cb?.(err);
          },
        );
        return true;
      }
      end(chunk, encoding, cb) {
        if (typeof chunk === "function") {
          cb = chunk;
          chunk = undefined;
        } else if (typeof encoding === "function") {
          cb = encoding;
          encoding = undefined;
        }
        if (this._ended) return this;
        if (this._streamId === null) {
          // Single-shot: full body, hyper sets content-length.
          this._ended = true;
          this.headersSent = true;
          natives.httpRespond(
            this._requestId,
            this.statusCode,
            this._headerPairsJson(),
            this._toBytes(chunk, encoding),
          );
          queueMicrotask(() => {
            this.emit("finish");
            cb?.();
          });
        } else {
          // A trailing chunk joins the same serialized chain; the stream
          // closes only AFTER every queued push has flushed, in order.
          if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
          this._ended = true;
          const streamId = this._streamId;
          this._chain = this._chain.then(() => {
            natives.httpBodyEnd(streamId);
            this.emit("finish");
            cb?.();
          });
        }
        return this;
      }
      flushHeaders() {
        if (this._streamId === null && !this._ended) this.write(new Uint8Array(0));
      }
    }

    class Server extends EventEmitter {
      constructor(handler) {
        super();
        if (handler) this.on("request", handler);
        this._serverId = null;
        this._port = null;
        this._host = null;
        this.listening = false;
      }
      listen(port, host, callback) {
        if (typeof port === "object" && port !== null) {
          // listen({ port, host }, cb)
          callback = host;
          host = port.host;
          port = port.port;
        }
        if (typeof host === "function") {
          callback = host;
          host = undefined;
        }
        if (typeof callback === "function") this.once("listening", callback);
        const hostname = host ?? "127.0.0.1";
        natives.httpServe(hostname, port ?? 0).then(
          (bound) => {
            this._serverId = bound.serverId;
            this._port = bound.port;
            this._host = hostname;
            this.listening = true;
            this.emit("listening");
            (async () => {
              for (;;) {
                const meta = await natives.httpAccept(bound.serverId);
                if (meta === undefined) break;
                if (meta.isUpgrade && meta.socketHandle !== undefined) {
                  const NetSocket = registry.get("net").Socket;
                  const socket = new NetSocket({
                    _handle: meta.socketHandle,
                    _remoteAddr: {
                      address: meta.remoteAddress || "127.0.0.1",
                      port: meta.remotePort || 0,
                      family: "IPv4",
                    },
                  });
                  socket._readLoop();
                  const req = new IncomingMessage(meta);
                  this.emit("upgrade", req, socket, globalThis.Buffer.alloc(0));
                } else {
                  const req = new IncomingMessage(meta);
                  const res = new ServerResponse(meta.requestId);
                  this.emit("request", req, res);
                }
              }
              this.emit("close");
            })();
          },
          (err) => this.emit("error", err),
        );
        return this;
      }
      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }
      close(callback) {
        if (this._serverId !== null) {
          natives.httpClose(this._serverId);
          this.listening = false;
        }
        if (callback) this.once("close", callback);
        return this;
      }
    }

    class ClientRequest extends EventEmitter {
      constructor(options, callback) {
        super();
        if (typeof options === "string") options = new URL(options);
        if (options instanceof URL) {
          options = {
            hostname: options.hostname,
            port: options.port || (options.protocol === "https:" ? 443 : 80),
            path: options.pathname + options.search,
            protocol: options.protocol,
          };
        }
        var opts = options || {};
        this.method = (opts.method || "GET").toUpperCase();
        var protocol = opts.protocol || "http:";
        var host = opts.hostname || opts.host || "localhost";
        var port = opts.port || (protocol === "https:" ? 443 : 80);
        var reqPath = opts.path || "/";
        this._url = protocol + "//" + host + ":" + port + reqPath;
        this._headers = {};
        if (opts.headers) {
          var keys = Object.keys(opts.headers);
          for (var i = 0; i < keys.length; i++) {
            this._headers[keys[i].toLowerCase()] = opts.headers[keys[i]];
          }
        }
        this._body = [];
        this._ended = false;
        this._aborted = false;
        this.headersSent = false;
        var self = this;
        this.socket = {
          remoteAddress: host,
          remotePort: Number(port),
          localAddress: "127.0.0.1",
          localPort: 0,
          setTimeout: function(ms, cb) { if (cb) self.once("timeout", cb); return this; },
          setNoDelay: function() { return this; },
          setKeepAlive: function() { return this; },
          ref: function() { return this; },
          unref: function() { return this; },
          destroy: function() { self.destroy(); },
        };
        if (callback) this.once("response", callback);
        process.nextTick(function() {
          self.emit("socket", self.socket);
        });
      }
      setHeader(name, value) { this._headers[name.toLowerCase()] = value; return this; }
      getHeader(name) { return this._headers[name.toLowerCase()]; }
      removeHeader(name) { delete this._headers[name.toLowerCase()]; }
      getHeaders() { return Object.assign({}, this._headers); }
      hasHeader(name) { return name.toLowerCase() in this._headers; }
      flushHeaders() { /* fetch sends headers with the body */ }
      write(chunk, encoding, callback) {
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (typeof chunk === "string") {
          this._body.push(globalThis.Buffer.from(chunk, encoding || "utf8"));
        } else if (chunk instanceof Uint8Array) {
          this._body.push(chunk);
        } else {
          this._body.push(globalThis.Buffer.from(chunk));
        }
        if (callback) queueMicrotask(callback);
        return true;
      }
      end(data, encoding, callback) {
        if (typeof data === "function") { callback = data; data = undefined; }
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (data != null) this.write(data, encoding);
        this._ended = true;
        this.headersSent = true;
        var self = this;
        var bodyData = null;
        if (self._body.length > 0) {
          var totalLen = 0;
          for (var bi = 0; bi < self._body.length; bi++) totalLen += self._body[bi].length;
          var merged = new Uint8Array(totalLen);
          var boff = 0;
          for (var bi = 0; bi < self._body.length; bi++) {
            merged.set(self._body[bi], boff);
            boff += self._body[bi].length;
          }
          bodyData = merged;
        }
        var connHdr = (self._headers["connection"] || "").toLowerCase();
        if (connHdr.indexOf("upgrade") !== -1) {
          self._doUpgradeRequest(bodyData);
        } else {
          self._doFetchRequest(bodyData);
        }
        if (callback) self.once("response", callback);
        return this;
      }
      _doFetchRequest(bodyData) {
        var self = this;
        var fetchOpts = {
          method: self.method,
          headers: self._headers,
        };
        if (bodyData && self.method !== "GET" && self.method !== "HEAD") {
          fetchOpts.body = bodyData;
        }
        globalThis.fetch(self._url, fetchOpts).then(function (resp) {
          if (self._aborted) return;
          var res = new Readable({ read: function () {} });
          res.statusCode = resp.status;
          res.statusMessage = resp.statusText || "";
          res.httpVersion = "1.1";
          res.headers = {};
          res.rawHeaders = [];
          resp.headers.forEach(function (value, name) {
            var key = name.toLowerCase();
            res.headers[key] = key in res.headers ? res.headers[key] + ", " + value : value;
            res.rawHeaders.push(name, value);
          });
          self.emit("response", res);
          resp.arrayBuffer().then(function (ab) {
            if (ab.byteLength > 0) res.push(globalThis.Buffer.from(ab));
            res.push(null);
            process.nextTick(function() { self.emit("close"); });
          }, function (err) { res.destroy(err); });
        }, function (err) {
          self.emit("error", typeof err === "string" ? new Error(err) : err);
        });
      }
      _doUpgradeRequest(bodyData) {
        var self = this;
        var parsed = new URL(self._url);
        var host = parsed.hostname;
        var port = Number(parsed.port) || (parsed.protocol === "https:" ? 443 : 80);
        var reqPath = parsed.pathname + parsed.search;
        natives.tcpConnect(host, port).then(function (result) {
          var handle = result.handle;
          if (!self._headers["host"]) {
            self._headers["host"] = port === 80 ? host : host + ":" + port;
          }
          var reqLine = self.method + " " + reqPath + " HTTP/1.1\r\n";
          var headerStr = "";
          var hkeys = Object.keys(self._headers);
          for (var hi = 0; hi < hkeys.length; hi++) {
            headerStr += hkeys[hi] + ": " + self._headers[hkeys[hi]] + "\r\n";
          }
          var reqBytes = globalThis.Buffer.from(reqLine + headerStr + "\r\n");
          natives.tcpWrite(handle, reqBytes).then(function () {
            var responseBuf = globalThis.Buffer.alloc(0);
            function readMore() {
              natives.tcpRead(handle, 4096).then(function (chunk) {
                if (chunk === undefined) {
                  self.emit("error", new Error("connection closed before upgrade response"));
                  return;
                }
                responseBuf = globalThis.Buffer.concat([responseBuf, globalThis.Buffer.from(chunk)]);
                var headerEnd = -1;
                for (var si = 0; si < responseBuf.length - 3; si++) {
                  if (responseBuf[si] === 13 && responseBuf[si+1] === 10 && responseBuf[si+2] === 13 && responseBuf[si+3] === 10) {
                    headerEnd = si;
                    break;
                  }
                }
                if (headerEnd === -1) { readMore(); return; }
                var headStr = responseBuf.slice(0, headerEnd).toString();
                var headBytes = headerEnd + 4;
                var remaining = responseBuf.slice(headBytes);
                var lines = headStr.split("\r\n");
                var statusLine = lines[0] || "";
                var statusMatch = statusLine.match(/HTTP\/\d\.\d (\d+)/);
                var statusCode = statusMatch ? Number(statusMatch[1]) : 0;
                var resHeaders = {};
                var rawHeaders = [];
                for (var li = 1; li < lines.length; li++) {
                  var colonIdx = lines[li].indexOf(":");
                  if (colonIdx !== -1) {
                    var hname = lines[li].slice(0, colonIdx);
                    var hval = lines[li].slice(colonIdx + 1).trim();
                    var lname = hname.toLowerCase();
                    resHeaders[lname] = lname in resHeaders ? resHeaders[lname] + ", " + hval : hval;
                    rawHeaders.push(hname, hval);
                  }
                }
                if (statusCode === 101) {
                  var NetSocket = registry.get("net").Socket;
                  var socket = new NetSocket({
                    _handle: handle,
                    _remoteAddr: result.remoteAddr,
                  });
                  socket._readLoop();
                  var res = new Readable({ read: function () {} });
                  res.statusCode = statusCode;
                  res.statusMessage = statusLine.slice(statusLine.indexOf(" " + statusCode) + String(statusCode).length + 2) || "";
                  res.httpVersion = "1.1";
                  res.headers = resHeaders;
                  res.rawHeaders = rawHeaders;
                  self.emit("upgrade", res, socket, remaining);
                } else {
                  var res = new Readable({ read: function () {} });
                  res.statusCode = statusCode;
                  res.statusMessage = "";
                  res.httpVersion = "1.1";
                  res.headers = resHeaders;
                  res.rawHeaders = rawHeaders;
                  self.emit("response", res);
                  if (remaining.length > 0) res.push(remaining);
                  natives.tcpRead(handle, 65536).then(function readRest(chunk) {
                    if (chunk === undefined) { res.push(null); return; }
                    res.push(globalThis.Buffer.from(chunk));
                    natives.tcpRead(handle, 65536).then(readRest);
                  });
                }
              }, function (err) { self.emit("error", err); });
            }
            readMore();
          }, function (err) { self.emit("error", err); });
        }, function (err) { self.emit("error", err); });
      }
      abort() {
        this._aborted = true;
        this.emit("abort");
      }
      destroy(err) {
        this._aborted = true;
        if (err) this.emit("error", err);
        return this;
      }
      setTimeout(ms, callback) {
        if (callback) this.once("timeout", callback);
        return this;
      }
    }

    function request(options, callback) {
      return new ClientRequest(options, callback);
    }

    function get(options, callback) {
      var req = request(options, callback);
      req.end();
      return req;
    }

    class Agent {
      constructor(options) {
        this.options = options || {};
        this.maxSockets = this.options.maxSockets || Infinity;
        this.maxFreeSockets = this.options.maxFreeSockets || 256;
        this.keepAlive = this.options.keepAlive || false;
        this.keepAliveMsecs = this.options.keepAliveMsecs || 1000;
        this.sockets = {};
        this.freeSockets = {};
        this.requests = {};
      }
      destroy() { this.sockets = {}; this.freeSockets = {}; this.requests = {}; }
      getName(options) {
        var name = (options.host || "localhost") + ":" + (options.port || 80);
        if (options.localAddress) name += ":" + options.localAddress;
        return name;
      }
    }

    var INVALID_HEADER_CHAR = /[^\t\x20-\x7e\x80-\xff]/;
    function validateHeaderName(name) {
      if (typeof name !== "string" || name.length === 0) throw new TypeError("Header name must be a valid HTTP token [\"" + name + "\"]");
      if (INVALID_HEADER_CHAR.test(name)) throw new TypeError("Header name must be a valid HTTP token [\"" + name + "\"]");
    }
    function validateHeaderValue(name, value) {
      if (value === undefined) throw new TypeError("Invalid value \"undefined\" for header \"" + name + "\"");
    }

    class OutgoingMessage extends EventEmitter {
      constructor() {
        super();
        this.headersSent = false;
        this.sendDate = true;
        this.finished = false;
        this.writableEnded = false;
        this.writableFinished = false;
        this._headers = {};
      }
      setHeader(name, value) { this._headers[name.toLowerCase()] = value; }
      getHeader(name) { return this._headers[name.toLowerCase()]; }
      getHeaderNames() { return Object.keys(this._headers); }
      getHeaders() { return Object.assign({}, this._headers); }
      hasHeader(name) { return name.toLowerCase() in this._headers; }
      removeHeader(name) { delete this._headers[name.toLowerCase()]; }
      flushHeaders() {}
      appendHeader(name, value) {
        var existing = this._headers[name.toLowerCase()];
        if (existing !== undefined) {
          this._headers[name.toLowerCase()] = Array.isArray(existing) ? existing.concat(value) : [existing, value];
        } else {
          this._headers[name.toLowerCase()] = value;
        }
      }
    }
    Object.setPrototypeOf(ServerResponse.prototype, OutgoingMessage.prototype);

    return {
      createServer: (options, handler) =>
        new Server(typeof options === "function" ? options : handler),
      Server,
      IncomingMessage,
      ServerResponse,
      ClientRequest,
      OutgoingMessage,
      request,
      get,
      globalAgent: { maxSockets: Infinity, maxFreeSockets: 256, keepAlive: true, keepAliveMsecs: 1000, options: {} },
      Agent,
      maxHeaderSize: 16384,
      validateHeaderName,
      validateHeaderValue,
      METHODS: ["DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT"],
      STATUS_CODES: {
        100: "Continue", 101: "Switching Protocols", 102: "Processing", 103: "Early Hints",
        200: "OK", 201: "Created", 202: "Accepted", 203: "Non-Authoritative Information",
        204: "No Content", 205: "Reset Content", 206: "Partial Content", 207: "Multi-Status",
        208: "Already Reported", 226: "IM Used",
        300: "Multiple Choices", 301: "Moved Permanently", 302: "Found", 303: "See Other",
        304: "Not Modified", 305: "Use Proxy", 307: "Temporary Redirect", 308: "Permanent Redirect",
        400: "Bad Request", 401: "Unauthorized", 402: "Payment Required", 403: "Forbidden",
        404: "Not Found", 405: "Method Not Allowed", 406: "Not Acceptable",
        407: "Proxy Authentication Required", 408: "Request Timeout", 409: "Conflict",
        410: "Gone", 411: "Length Required", 412: "Precondition Failed",
        413: "Payload Too Large", 414: "URI Too Long", 415: "Unsupported Media Type",
        416: "Range Not Satisfiable", 417: "Expectation Failed", 418: "I'm a Teapot",
        421: "Misdirected Request", 422: "Unprocessable Entity", 423: "Locked",
        424: "Failed Dependency", 425: "Too Early", 426: "Upgrade Required",
        428: "Precondition Required", 429: "Too Many Requests",
        431: "Request Header Fields Too Large", 451: "Unavailable For Legal Reasons",
        500: "Internal Server Error", 501: "Not Implemented", 502: "Bad Gateway",
        503: "Service Unavailable", 504: "Gateway Timeout",
        505: "HTTP Version Not Supported", 506: "Variant Also Negotiates",
        507: "Insufficient Storage", 508: "Loop Detected",
        510: "Not Extended", 511: "Network Authentication Required",
      },
    };
  };

  // ------------------------------------------------------------- node:net
  // TCP sockets and servers backed by __oam.node tcp* ops.
  registry.factories.net = (natives) => {
    const EventEmitter = registry.get("events");

    const V4_SEGMENT = /^(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])$/;
    function isIPv4(input) {
      const parts = String(input).split(".");
      return parts.length === 4 && parts.every((p) => V4_SEGMENT.test(p));
    }
    function isIPv6(input) {
      const text = String(input);
      if (text.length === 0 || text.includes(" ")) return false;
      const sections = text.split("::");
      if (sections.length > 2) return false;
      const check = (part) =>
        part === "" ||
        part
          .split(":")
          .every(
            (group, i, arr) =>
              /^[0-9A-Fa-f]{1,4}$/.test(group) ||
              (i === arr.length - 1 && isIPv4(group)),
          );
      if (sections.length === 2) return check(sections[0]) && check(sections[1]);
      const groups = text.split(":");
      const hasV4Tail = isIPv4(groups[groups.length - 1]);
      const expected = hasV4Tail ? 7 : 8;
      return groups.length === expected && check(text);
    }
    function isIP(input) {
      return isIPv4(input) ? 4 : isIPv6(input) ? 6 : 0;
    }

    // Happy-Eyeballs (autoSelectFamily) default attempt timeout. Node's
    // default is 250ms; setDefault validates an integer >= 1 and clamps the
    // effective floor to 10ms (node/lib/net.js).
    let autoSelectFamilyAttemptTimeoutDefault = 250;
    function getDefaultAutoSelectFamilyAttemptTimeout() {
      return autoSelectFamilyAttemptTimeoutDefault;
    }
    function setDefaultAutoSelectFamilyAttemptTimeout(value) {
      value = Number(value);
      if (!Number.isInteger(value) || value < 1) {
        throw new RangeError(
          `The value of "value" is out of range. It must be an integer >= 1. Received ${value}`,
        );
      }
      if (value < 10) value = 10;
      autoSelectFamilyAttemptTimeoutDefault = value;
    }

    function toBytes(data, encoding) {
      if (data === null || data === undefined) return new Uint8Array(0);
      if (typeof data === "string") return globalThis.Buffer.from(data, encoding || "utf8");
      if (data instanceof Uint8Array) return data;
      if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      return globalThis.Buffer.from(String(data));
    }

    class Socket extends EventEmitter {
      constructor(options) {
        super();
        this._handle = null;
        this._encoding = null;
        this._chain = Promise.resolve();
        this.connecting = false;
        this.destroyed = false;
        this.writable = true;
        this.readable = true;
        this.remoteAddress = undefined;
        this.remotePort = undefined;
        this.remoteFamily = undefined;
        this.localAddress = undefined;
        this.localPort = undefined;
        this.bytesRead = 0;
        this.bytesWritten = 0;
        this.bufferSize = 0;
        this.allowHalfOpen = (options && options.allowHalfOpen) || false;
        this._paused = false;
        this._pipeHandler = null;
        this._timeoutMs = 0;
        this._timeoutId = null;
        if (options && options._handle !== undefined) {
          this._handle = options._handle;
          this.connecting = false;
          if (options._remoteAddr) {
            this.remoteAddress = options._remoteAddr.address;
            this.remotePort = options._remoteAddr.port;
            this.remoteFamily = options._remoteAddr.family;
          }
        }
      }

      connect(...args) {
        let port, host, cb;
        if (typeof args[0] === "object" && args[0] !== null) {
          const opts = args[0];
          port = opts.port;
          host = opts.host || "127.0.0.1";
          cb = args[1];
        } else {
          port = args[0];
          host = typeof args[1] === "string" ? args[1] : "127.0.0.1";
          cb = typeof args[args.length - 1] === "function" ? args[args.length - 1] : undefined;
        }
        if (cb) this.once("connect", cb);
        this.connecting = true;
        natives.tcpConnect(host, port).then(
          (result) => {
            this._handle = result.handle;
            this.connecting = false;
            if (result.remoteAddr) {
              this.remoteAddress = result.remoteAddr.address;
              this.remotePort = result.remoteAddr.port;
              this.remoteFamily = result.remoteAddr.family;
            }
            if (result.localAddr) {
              this.localAddress = result.localAddr.address;
              this.localPort = result.localAddr.port;
            }
            this.emit("connect");
            this.emit("ready");
            this._readLoop();
          },
          (err) => {
            this.connecting = false;
            this.destroy(err);
          },
        );
        return this;
      }

      write(data, encoding, cb) {
        if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        if (this.destroyed || !this.writable) {
          const err = new Error("This socket has been ended");
          if (cb) cb(err);
          else this.emit("error", err);
          return false;
        }
        if (this._timeoutMs > 0) this._resetTimeout();
        const bytes = toBytes(data, encoding);
        this.bytesWritten += bytes.length;
        this._chain = this._chain.then(() => {
          if (this.destroyed) return;
          return natives.tcpWrite(this._handle, bytes).then(
            () => { if (cb) cb(); },
            (err) => { if (cb) cb(err); else this.emit("error", err); },
          );
        });
        return true;
      }

      end(data, encoding, cb) {
        if (typeof data === "function") { cb = data; data = undefined; encoding = undefined; }
        else if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        if (data !== undefined && data !== null) this.write(data, encoding);
        this.writable = false;
        this._chain = this._chain.then(() => {
          if (this._handle !== null) return natives.tcpShutdown(this._handle);
        }).then(() => {
          this.emit("finish");
          if (cb) cb();
          if (!this.readable) this._doClose();
        });
        return this;
      }

      destroy(err) {
        if (this.destroyed) return this;
        this.destroyed = true;
        this.readable = false;
        this.writable = false;
        this.connecting = false;
        if (this._timeoutId !== null) {
          globalThis.clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
        if (this._handle !== null) {
          try { natives.tcpClose(this._handle); } catch (_) { /* noop */ }
          this._handle = null;
        }
        if (err) this.emit("error", err);
        this.emit("close", !!err);
        return this;
      }

      async _readLoop() {
        while (!this.destroyed) {
          if (this._paused) return;
          let chunk;
          try {
            chunk = await natives.tcpRead(this._handle, 65536);
          } catch (err) {
            this.destroy(err);
            return;
          }
          if (chunk === undefined) {
            this.readable = false;
            this.emit("end");
            if (!this.allowHalfOpen) this.end();
            else if (!this.writable) this._doClose();
            break;
          }
          this.bytesRead += chunk.length;
          if (this._timeoutMs > 0) this._resetTimeout();
          if (this._encoding) {
            this.emit("data", new TextDecoder(this._encoding).decode(chunk));
          } else {
            this.emit("data", globalThis.Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
          }
        }
      }

      _doClose() {
        if (this._handle !== null) {
          try { natives.tcpClose(this._handle); } catch (_) { /* noop */ }
          this._handle = null;
        }
        if (!this.destroyed) {
          this.destroyed = true;
          this.emit("close", false);
        }
      }

      setEncoding(encoding) { this._encoding = encoding; return this; }
      setTimeout(ms, cb) {
        if (this._timeoutId !== null) {
          globalThis.clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
        if (cb) this.once("timeout", cb);
        this._timeoutMs = ms || 0;
        if (this._timeoutMs > 0) this._resetTimeout();
        return this;
      }
      _resetTimeout() {
        if (this._timeoutId !== null) globalThis.clearTimeout(this._timeoutId);
        if (this._timeoutMs > 0 && !this.destroyed) {
          this._timeoutId = globalThis.setTimeout(() => {
            this._timeoutId = null;
            this.emit("timeout");
          }, this._timeoutMs);
        }
      }
      setNoDelay() { return this; }
      setKeepAlive() { return this; }
      ref() { return this; }
      unref() { return this; }
      address() {
        return { address: this.localAddress, port: this.localPort, family: this.remoteFamily || "IPv4" };
      }
      get readyState() {
        if (this.connecting) return "opening";
        if (this.readable && this.writable) return "open";
        if (this.readable) return "readOnly";
        if (this.writable) return "writeOnly";
        return "closed";
      }
      get pending() { return this.connecting; }
      pipe(dest) {
        this._pipeHandler = (chunk) => dest.write(chunk);
        this.on("data", this._pipeHandler);
        this.on("end", () => { if (typeof dest.end === "function") dest.end(); });
        return dest;
      }
      unpipe(dest) {
        if (this._pipeHandler) {
          this.removeListener("data", this._pipeHandler);
          this._pipeHandler = null;
        }
        return this;
      }
      pause() { this._paused = true; return this; }
      resume() {
        if (this._paused) {
          this._paused = false;
          this._readLoop();
        }
        return this;
      }
      cork() { return this; }
      uncork() { return this; }
    }

    class Server extends EventEmitter {
      constructor(options, connectionListener) {
        super();
        if (typeof options === "function") { connectionListener = options; options = {}; }
        if (connectionListener) this.on("connection", connectionListener);
        this._serverId = null;
        this._port = null;
        this._host = null;
        this.listening = false;
      }

      listen(...args) {
        let port, host, cb;
        if (typeof args[0] === "object" && args[0] !== null) {
          const opts = args[0];
          port = opts.port;
          host = opts.host;
          cb = typeof args[1] === "function" ? args[1] : undefined;
        } else {
          port = args[0];
          let idx = 1;
          if (typeof args[idx] === "string") { host = args[idx]; idx++; }
          if (typeof args[idx] === "number") { idx++; }
          if (typeof args[idx] === "function") { cb = args[idx]; }
        }
        if (typeof cb === "function") this.once("listening", cb);
        const hostname = host || "0.0.0.0";
        natives.tcpListen(hostname, port || 0).then(
          (bound) => {
            this._serverId = bound.serverId;
            this._port = bound.port;
            this._host = bound.hostname || hostname;
            this.listening = true;
            this.emit("listening");
            this._acceptLoop();
          },
          (err) => this.emit("error", err),
        );
        return this;
      }

      async _acceptLoop() {
        for (;;) {
          let accepted;
          try {
            accepted = await natives.tcpAccept(this._serverId);
          } catch (err) {
            if (this.listening) this.emit("error", err);
            break;
          }
          if (accepted === undefined) break;
          const socket = new Socket({
            _handle: accepted.handle,
            _remoteAddr: accepted.remoteAddr,
          });
          socket._readLoop();
          this.emit("connection", socket);
        }
        this.emit("close");
      }

      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }

      close(cb) {
        if (this._serverId !== null) {
          natives.tcpServerClose(this._serverId);
          this.listening = false;
        }
        if (cb) this.once("close", cb);
        return this;
      }

      getConnections(cb) { if (cb) cb(null, 0); return this; }
      ref() { return this; }
      unref() { return this; }
    }

    function createConnection(options, cb) {
      if (typeof options === "number") {
        const host = typeof arguments[1] === "string" ? arguments[1] : undefined;
        cb = typeof arguments[arguments.length - 1] === "function" ? arguments[arguments.length - 1] : undefined;
        options = { port: options, host: host };
      }
      const socket = new Socket();
      socket.connect(options, cb);
      return socket;
    }

    function createServer(options, connectionListener) {
      if (typeof options === "function") { connectionListener = options; options = {}; }
      return new Server(options, connectionListener);
    }

    class SocketAddress {
      constructor(options) {
        options = options || {};
        this.address = options.address || "127.0.0.1";
        this.port = options.port || 0;
        this.family = options.family || "ipv4";
        this.flowlabel = options.flowlabel || 0;
      }
    }

    class BlockList {
      constructor() { this._rules = []; }
      addAddress(address, family) {
        this._rules.push({ type: "address", address: address, family: family || "ipv4" });
      }
      addRange(start, end, family) {
        this._rules.push({ type: "range", start: start, end: end, family: family || "ipv4" });
      }
      addSubnet(network, prefix, family) {
        this._rules.push({ type: "subnet", network: network, prefix: prefix, family: family || "ipv4" });
      }
      check(address, family) {
        var fam = (family || "ipv4").toLowerCase();
        for (var ri = 0; ri < this._rules.length; ri++) {
          var rule = this._rules[ri];
          if (rule.family !== fam) continue;
          if (rule.type === "address" && rule.address === address) return true;
        }
        return false;
      }
      get rules() { return this._rules.slice(); }
    }

    return {
      isIPv4, isIPv6, isIP,
      getDefaultAutoSelectFamilyAttemptTimeout,
      setDefaultAutoSelectFamilyAttemptTimeout,
      Socket, Server,
      SocketAddress, BlockList,
      createConnection, connect: createConnection, createServer,
    };
  };

  // ------------------------------------------------------------------ tty
  registry.factories.tty = (natives) => ({
    isatty: (fd) => natives.isTTY(Number(fd)),
    ReadStream: class ReadStream {},
    WriteStream: class WriteStream {},
  });

  // --------------------------------------------------------------- buffer
  registry.factories.buffer = () => {
    // isUtf8/isAscii require an ArrayBuffer/Buffer/TypedArray input; Node throws
    // ERR_INVALID_ARG_TYPE otherwise (it does NOT coerce numbers/strings).
    const validateBinaryInput = (input) => {
      if (
        !(input instanceof Uint8Array) &&
        !(input instanceof ArrayBuffer) &&
        !ArrayBuffer.isView(input)
      ) {
        throw argTypeError(
          "input",
          "ArrayBuffer, Buffer, or TypedArray",
          input,
        );
      }
      return input instanceof Uint8Array
        ? input
        : ArrayBuffer.isView(input)
          ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
          : new Uint8Array(input);
    };
    const mod = {
      Buffer: globalThis.Buffer,
      Blob: globalThis.Blob,
      File: globalThis.File || class File extends globalThis.Blob {
        constructor(bits, name, options) { super(bits, options); this.name = name; this.lastModified = (options && options.lastModified) || Date.now(); }
      },
      atob: globalThis.atob,
      btoa: globalThis.btoa,
      constants: { MAX_LENGTH: 9007199254740991, MAX_STRING_LENGTH: 536870888 },
      kMaxLength: 9007199254740991,
      kStringMaxLength: 536870888,
      SlowBuffer: function SlowBuffer(size) { return globalThis.Buffer.allocUnsafe(size); },
      isUtf8: (input) => {
        const bytes = validateBinaryInput(input);
        try {
          new TextDecoder("utf-8", { fatal: true }).decode(bytes);
          return true;
        } catch {
          return false;
        }
      },
      isAscii: (input) => {
        const bytes = validateBinaryInput(input);
        for (let i = 0; i < bytes.length; i++) if (bytes[i] > 0x7f) return false;
        return true;
      },
    };
    // INSPECT_MAX_BYTES is a live, settable property backed by shared state
    // (Buffer.prototype.inspect reads bufferState.INSPECT_MAX_BYTES).
    Object.defineProperty(mod, "INSPECT_MAX_BYTES", {
      enumerable: true,
      configurable: true,
      get: () => bufferState.INSPECT_MAX_BYTES,
      set: (v) => {
        // Node validates: must be a number, integer-or-Infinity, and >= 0.
        if (typeof v !== "number") {
          throw argTypeOfError("INSPECT_MAX_BYTES", "number", v);
        }
        if (Number.isNaN(v) || v < 0) {
          throw codes.ERR_OUT_OF_RANGE("INSPECT_MAX_BYTES", ">= 0", fmtRange(v));
        }
        bufferState.INSPECT_MAX_BYTES = v;
      },
    });
    return mod;
  };

  // --------------------------------------------------------------- timers
  // Node's `timers` module: re-exports the global timer functions so that
  // require('timers') / import * from 'node:timers' works as expected.
  registry.factories.timers = () => ({
    setTimeout: (fn, ms, ...args) => globalThis.setTimeout(fn, ms, ...args),
    clearTimeout: (id) => globalThis.clearTimeout(id),
    setInterval: (fn, ms, ...args) => globalThis.setInterval(fn, ms, ...args),
    clearInterval: (id) => globalThis.clearInterval(id),
    setImmediate: (fn, ...args) => globalThis.setImmediate(fn, ...args),
    clearImmediate: (id) => globalThis.clearImmediate(id),
  });

  // ------------------------------------------------------------- punycode
  // Pure-JS Punycode (RFC 3492). Node marks this deprecated but it is still
  // in widespread use via the `url` module and idn-related libraries.
  registry.factories.punycode = () => {
    const BASE = 36, TMIN = 1, TMAX = 26, SKEW = 38, DAMP = 700, BIAS_INIT = 72;
    const MAXINT = 2147483647;
    const DELIMITER = "\x2D";

    function adapt(delta, numPoints, firstTime) {
      delta = firstTime ? Math.floor(delta / DAMP) : delta >>> 1;
      delta += Math.floor(delta / numPoints);
      let k = 0;
      while (delta > Math.floor(((BASE - TMIN) * TMAX) / 2)) {
        delta = Math.floor(delta / (BASE - TMIN));
        k += BASE;
      }
      return k + Math.floor(((BASE - TMIN + 1) * delta) / (delta + SKEW));
    }

    function digitToBasic(digit) {
      return digit + (digit < 26 ? 97 : 22);
    }

    function basicToDigit(codePoint) {
      if (codePoint >= 48 && codePoint < 58) return codePoint - 22;
      if (codePoint >= 65 && codePoint < 91) return codePoint - 65;
      if (codePoint >= 97 && codePoint < 123) return codePoint - 97;
      return BASE;
    }

    function decode(input) {
      const output = [];
      let i = 0;
      let n = 128;
      let bias = BIAS_INIT;
      const basic = input.lastIndexOf(DELIMITER);
      const start = basic < 0 ? 0 : basic;
      for (let j = 0; j < start; j++) {
        if (input.charCodeAt(j) >= 128) throw new RangeError("Illegal input characters");
        output.push(input.charCodeAt(j));
      }
      let ic = basic > 0 ? basic + 1 : 0;
      while (ic < input.length) {
        const oldi = i;
        let w = 1;
        for (let k = BASE; ; k += BASE) {
          if (ic >= input.length) throw new RangeError("Invalid input");
          const digit = basicToDigit(input.charCodeAt(ic++));
          if (digit >= BASE || digit > Math.floor((MAXINT - i) / w)) throw new RangeError("Overflow");
          i += digit * w;
          const t = k <= bias ? TMIN : k >= bias + TMAX ? TMAX : k - bias;
          if (digit < t) break;
          const baseMinusT = BASE - t;
          if (w > Math.floor(MAXINT / baseMinusT)) throw new RangeError("Overflow");
          w *= baseMinusT;
        }
        const out = output.length + 1;
        bias = adapt(i - oldi, out, oldi === 0);
        if (Math.floor(i / out) > MAXINT - n) throw new RangeError("Overflow");
        n += Math.floor(i / out);
        i %= out;
        output.splice(i++, 0, n);
      }
      return String.fromCodePoint(...output);
    }

    function encode(input) {
      const output = [];
      const cps = [...input].map((c) => c.codePointAt(0));
      const basic = cps.filter((cp) => cp < 128);
      let handled = basic.length;
      for (const cp of basic) output.push(String.fromCodePoint(cp));
      if (handled > 0) output.push(DELIMITER);
      let n = 128;
      let delta = 0;
      let bias = BIAS_INIT;
      while (handled < cps.length) {
        const m = cps.filter((cp) => cp >= n).reduce((a, b) => Math.min(a, b), MAXINT);
        if (m - n > Math.floor((MAXINT - delta) / (handled + 1))) throw new RangeError("Overflow");
        delta += (m - n) * (handled + 1);
        n = m;
        for (const cp of cps) {
          if (cp < n) {
            delta++;
            if (delta > MAXINT) throw new RangeError("Overflow");
          }
          if (cp === n) {
            let q = delta;
            for (let k = BASE; ; k += BASE) {
              const t = k <= bias ? TMIN : k >= bias + TMAX ? TMAX : k - bias;
              if (q < t) break;
              output.push(String.fromCodePoint(digitToBasic(t + ((q - t) % (BASE - t)))));
              q = Math.floor((q - t) / (BASE - t));
            }
            output.push(String.fromCodePoint(digitToBasic(q)));
            bias = adapt(delta, handled + 1, handled === basic.length);
            delta = 0;
            handled++;
          }
        }
        delta++;
        n++;
      }
      return output.join("");
    }

    function toASCII(domain) {
      return domain.split(".").map((label) => {
        if (/[^\x00-\x7F]/.test(label)) return "xn--" + encode(label);
        return label;
      }).join(".");
    }

    function toUnicode(domain) {
      return domain.split(".").map((label) => {
        if (label.startsWith("xn--")) {
          try { return decode(label.slice(4)); } catch { return label; }
        }
        return label;
      }).join(".");
    }

    return {
      decode, encode, toASCII, toUnicode,
      ucs2: {
        decode: (str) => [...str].map((c) => c.codePointAt(0)),
        encode: (cps) => String.fromCodePoint(...cps),
      },
    };
  };

  // ----------------------------------------------------------- perf_hooks
  // Wrap globalThis.performance (installed post-restore). Factory defers the
  // lookup to call time so requiring perf_hooks before installRuntimeGlobals
  // still works (performance will be live when methods are invoked).
  registry.factories.perf_hooks = () => {
    const _marks = [];
    const _measures = [];
    const _observers = [];

    class PerformanceEntry {
      constructor(name, entryType, startTime, duration, detail) {
        this.name = name || "";
        this.entryType = entryType || "";
        this.startTime = startTime || 0;
        this.duration = duration || 0;
        this.detail = detail !== undefined ? detail : null;
      }
      toJSON() {
        return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration, detail: this.detail };
      }
    }

    class PerformanceObserverEntryList {
      constructor(entries) { this._entries = entries || []; }
      getEntries() { return this._entries.slice(); }
      getEntriesByName(name, type) {
        return this._entries.filter(function(e) {
          return e.name === name && (type === undefined || e.entryType === type);
        });
      }
      getEntriesByType(type) { return this._entries.filter(function(e) { return e.entryType === type; }); }
    }

    function _notifyObservers(entry) {
      for (var i = 0; i < _observers.length; i++) {
        var obs = _observers[i];
        if (obs._types && obs._types.indexOf(entry.entryType) !== -1) {
          try {
            obs._cb(new PerformanceObserverEntryList([entry]), obs);
          } catch (e) { /* observer callback errors are swallowed per spec */ }
        }
      }
    }

    class PerformanceObserver {
      constructor(cb) { this._cb = cb; this._types = []; }
      observe(options) {
        if (options && options.entryTypes) {
          this._types = options.entryTypes.slice();
        } else if (options && options.type) {
          if (this._types.indexOf(options.type) === -1) {
            this._types.push(options.type);
          }
        }
        if (options && options.buffered) {
          var existing = [];
          var types = this._types;
          for (var ti = 0; ti < types.length; ti++) {
            var t = types[ti];
            if (t === "mark") {
              for (var i = 0; i < _marks.length; i++) existing.push(_marks[i]);
            } else if (t === "measure") {
              for (var i = 0; i < _measures.length; i++) existing.push(_measures[i]);
            }
          }
          if (existing.length > 0) {
            var self = this;
            try { self._cb(new PerformanceObserverEntryList(existing), self); } catch (e) {}
          }
        }
        if (_observers.indexOf(this) === -1) {
          _observers.push(this);
        }
      }
      disconnect() {
        this._types = [];
        var idx = _observers.indexOf(this);
        if (idx !== -1) _observers.splice(idx, 1);
      }
    }
    PerformanceObserver.supportedEntryTypes = Object.freeze(["mark", "measure"]);

    class PerformanceNodeTiming extends PerformanceEntry {
      constructor() {
        super("node", "node", 0, 0);
        this.nodeStart = 0;
        this.v8Start = 0;
        this.bootstrapComplete = 0;
        this.environment = 0;
        this.loopStart = 0;
        this.loopExit = 0;
        this.idleTime = 0;
      }
    }

    function _findMark(name) {
      for (var i = _marks.length - 1; i >= 0; i--) {
        if (_marks[i].name === name) return _marks[i];
      }
      return null;
    }

    var _nodeTiming = new PerformanceNodeTiming();

    var perf = {
      now: function() { return globalThis.performance.now(); },
      get timeOrigin() { return globalThis.performance.timeOrigin; },

      mark: function(name, options) {
        var startTime = (options && options.startTime !== undefined) ? options.startTime : globalThis.performance.now();
        var detail = (options && options.detail !== undefined) ? options.detail : null;
        var entry = new PerformanceEntry(name, "mark", startTime, 0, detail);
        _marks.push(entry);
        _notifyObservers(entry);
        return entry;
      },

      measure: function(name, startMarkOrOptions, endMark) {
        var startTime = 0;
        var endTime = globalThis.performance.now();
        var detail = null;
        var duration;

        if (typeof startMarkOrOptions === "string") {
          var sm = _findMark(startMarkOrOptions);
          if (!sm) throw new Error("Failed to execute 'measure': The mark '" + startMarkOrOptions + "' does not exist.");
          startTime = sm.startTime;
          if (typeof endMark === "string") {
            var em = _findMark(endMark);
            if (!em) throw new Error("Failed to execute 'measure': The mark '" + endMark + "' does not exist.");
            endTime = em.startTime;
          }
          duration = endTime - startTime;
        } else if (startMarkOrOptions && typeof startMarkOrOptions === "object") {
          var opts = startMarkOrOptions;
          detail = opts.detail !== undefined ? opts.detail : null;
          if (opts.start !== undefined) {
            if (typeof opts.start === "string") {
              var smk = _findMark(opts.start);
              if (!smk) throw new Error("Failed to execute 'measure': The mark '" + opts.start + "' does not exist.");
              startTime = smk.startTime;
            } else {
              startTime = opts.start;
            }
          }
          if (opts.end !== undefined) {
            if (typeof opts.end === "string") {
              var emk = _findMark(opts.end);
              if (!emk) throw new Error("Failed to execute 'measure': The mark '" + opts.end + "' does not exist.");
              endTime = emk.startTime;
            } else {
              endTime = opts.end;
            }
          }
          if (opts.duration !== undefined) {
            duration = opts.duration;
          } else {
            duration = endTime - startTime;
          }
        } else {
          duration = endTime - startTime;
        }

        var entry = new PerformanceEntry(name, "measure", startTime, duration, detail);
        _measures.push(entry);
        _notifyObservers(entry);
        return entry;
      },

      getEntries: function() { return _marks.concat(_measures).sort(function(a,b){return a.startTime-b.startTime;}); },
      getEntriesByName: function(name, type) {
        return _marks.concat(_measures).sort(function(a,b){return a.startTime-b.startTime;}).filter(function(e) {
          return e.name === name && (type === undefined || e.entryType === type);
        });
      },
      getEntriesByType: function(type) {
        if (type === "mark") return _marks.slice();
        if (type === "measure") return _measures.slice();
        return [];
      },

      clearMarks: function(name) {
        if (name !== undefined) {
          for (var i = _marks.length - 1; i >= 0; i--) {
            if (_marks[i].name === name) _marks.splice(i, 1);
          }
        } else {
          _marks.length = 0;
        }
      },
      clearMeasures: function(name) {
        if (name !== undefined) {
          for (var i = _measures.length - 1; i >= 0; i--) {
            if (_measures[i].name === name) _measures.splice(i, 1);
          }
        } else {
          _measures.length = 0;
        }
      },

      toJSON: function() {
        return { timeOrigin: globalThis.performance.timeOrigin, nodeTiming: {} };
      },

      eventLoopUtilization: function() {
        return { idle: 0, active: 0, utilization: 0 };
      }
    };

    return {
      performance: perf,
      PerformanceObserver,
      PerformanceEntry,
      PerformanceObserverEntryList,
      PerformanceNodeTiming,
      nodeTiming: _nodeTiming,
      createHistogram: () => ({
        record: () => {},
        percentile: () => 0,
        mean: 0,
        max: 0,
        min: 0,
        count: 0,
      }),
      monitorEventLoopDelay: () => ({
        enable() {},
        disable() {},
        reset() {},
        mean: 0,
        max: 0,
        min: 0,
        stddev: 0,
        percentile: () => 0,
        percentiles: new Map(),
      }),
    };
  };

  // ------------------------------------------------------- diagnostics_channel
  // Minimal pub/sub bus. Used by OpenTelemetry auto-instrumentation and http
  // tracing shims: subscribe/unsubscribe/publish + channel objects.
  registry.factories.diagnostics_channel = () => {
    const _channels = new Map();
    class Channel {
      constructor(name) {
        this.name = name;
        this._subs = [];
      }
      get hasSubscribers() { return this._subs.length > 0; }
      subscribe(fn) { if (typeof fn === "function") this._subs.push(fn); }
      unsubscribe(fn) {
        const i = this._subs.indexOf(fn);
        if (i >= 0) this._subs.splice(i, 1);
        return i >= 0;
      }
      publish(message) {
        const subs = this._subs.slice();
        for (const fn of subs) { try { fn(message, this.name); } catch {} }
        return subs.length > 0;
      }
      bindStore() {}
      runStores(context, fn) { if (typeof fn === "function") fn(); }
    }
    function channel(name) {
      if (!_channels.has(name)) _channels.set(name, new Channel(String(name)));
      return _channels.get(name);
    }
    function hasSubscribers(name) {
      return _channels.has(name) && _channels.get(name).hasSubscribers;
    }
    function subscribe(name, fn) { channel(name).subscribe(fn); }
    function unsubscribe(name, fn) { return channel(name).unsubscribe(fn); }
    function tracingChannel(nameOrChannel) {
      var name = typeof nameOrChannel === "string" ? nameOrChannel : nameOrChannel.name;
      // Pin the five sub-channels so the trace methods publish to the SAME
      // Channel instances the caller subscribed to.
      var startCh = channel(name + ":start");
      var endCh = channel(name + ":end");
      var asyncStartCh = channel(name + ":asyncStart");
      var asyncEndCh = channel(name + ":asyncEnd");
      var errorCh = channel(name + ":error");
      return {
        start: startCh,
        end: endCh,
        asyncStart: asyncStartCh,
        asyncEnd: asyncEndCh,
        error: errorCh,
        get hasSubscribers() {
          return (
            startCh.hasSubscribers ||
            endCh.hasSubscribers ||
            asyncStartCh.hasSubscribers ||
            asyncEndCh.hasSubscribers ||
            errorCh.hasSubscribers
          );
        },
        subscribe: (handlers) => {
          if (handlers.start) startCh.subscribe(handlers.start);
          if (handlers.end) endCh.subscribe(handlers.end);
          if (handlers.asyncStart) asyncStartCh.subscribe(handlers.asyncStart);
          if (handlers.asyncEnd) asyncEndCh.subscribe(handlers.asyncEnd);
          if (handlers.error) errorCh.subscribe(handlers.error);
        },
        unsubscribe: (handlers) => {
          if (handlers.start) startCh.unsubscribe(handlers.start);
          if (handlers.end) endCh.unsubscribe(handlers.end);
          if (handlers.asyncStart) asyncStartCh.unsubscribe(handlers.asyncStart);
          if (handlers.asyncEnd) asyncEndCh.unsubscribe(handlers.asyncEnd);
          if (handlers.error) errorCh.unsubscribe(handlers.error);
        },
        // Node's TracingChannel trace API. The no-subscriber fast path
        // (the common case -- no tracer attached) just runs fn, matching
        // Node, so the publish bookkeeping never runs when nobody listens.
        traceSync(fn, context, thisArg, ...args) {
          context = context || {};
          if (!startCh.hasSubscribers) return Reflect.apply(fn, thisArg, args);
          startCh.publish(context);
          try {
            const result = Reflect.apply(fn, thisArg, args);
            context.result = result;
            return result;
          } catch (err) {
            context.error = err;
            errorCh.publish(context);
            throw err;
          } finally {
            endCh.publish(context);
          }
        },
        tracePromise(fn, context, thisArg, ...args) {
          context = context || {};
          if (!startCh.hasSubscribers) return Reflect.apply(fn, thisArg, args);
          startCh.publish(context);
          let promise;
          try {
            promise = Reflect.apply(fn, thisArg, args);
          } catch (err) {
            context.error = err;
            errorCh.publish(context);
            endCh.publish(context);
            throw err;
          }
          endCh.publish(context);
          return Promise.resolve(promise).then(
            (result) => {
              context.result = result;
              asyncStartCh.publish(context);
              asyncEndCh.publish(context);
              return result;
            },
            (err) => {
              context.error = err;
              errorCh.publish(context);
              asyncStartCh.publish(context);
              asyncEndCh.publish(context);
              throw err;
            },
          );
        },
        traceCallback(fn, position, context, thisArg, ...args) {
          context = context || {};
          if (!startCh.hasSubscribers) return Reflect.apply(fn, thisArg, args);
          if (position === undefined || position === -1) position = args.length - 1;
          const callback = args[position];
          if (typeof callback === "function") {
            args[position] = function wrapped(err) {
              if (err) {
                context.error = err;
                errorCh.publish(context);
              } else {
                context.result = arguments[1];
              }
              asyncStartCh.publish(context);
              try {
                return Reflect.apply(callback, this, arguments);
              } finally {
                asyncEndCh.publish(context);
              }
            };
          }
          startCh.publish(context);
          try {
            return Reflect.apply(fn, thisArg, args);
          } catch (err) {
            context.error = err;
            errorCh.publish(context);
            throw err;
          } finally {
            endCh.publish(context);
          }
        },
      };
    }
    return { channel, hasSubscribers, subscribe, unsubscribe, Channel, tracingChannel };
  };

  // -------------------------------------------------------------- readline
  // Minimal stub: enough for `readline.createInterface` and the async-
  // iterator line-reading pattern used by CLI utilities.
  registry.factories.readline = () => {
    const EventEmitter = registry.get("events");
    class Interface extends EventEmitter {
      constructor(options) {
        super();
        const opts = options || {};
        this.input = opts.input || null;
        this.output = opts.output || null;
        this.terminal = opts.terminal != null ? opts.terminal === true : !!(opts.output && opts.output.isTTY);
        this._closed = false;
        this._paused = false;
        this._prompt = typeof opts.prompt === "string" ? opts.prompt : "> ";
        this.crlfDelay = typeof opts.crlfDelay === "number" ? opts.crlfDelay : 100;
        this.line = "";
        if (this.input && typeof this.input.on === "function") {
          const dec = new TextDecoder();
          let buf = "";
          this.input.on("data", (chunk) => {
            if (this._closed) return;
            buf += typeof chunk === "string" ? chunk : dec.decode(chunk, { stream: true });
            const parts = buf.split(/\r?\n/);
            buf = parts.pop() || "";
            for (const line of parts) {
              this.line = line;
              this.emit("line", line);
            }
          });
          this.input.on("end", () => {
            if (buf.length) {
              this.line = buf;
              this.emit("line", buf);
              buf = "";
            }
            this.close();
          });
        }
      }
      close() {
        if (this._closed) return;
        this._closed = true;
        if (this.input && typeof this.input.pause === "function") {
          try { this.input.pause(); } catch (_e) { /* ignore */ }
        }
        this.emit("close");
      }
      question(prompt, cb) {
        if (this.output && typeof this.output.write === "function") this.output.write(prompt);
        const cleanup = () => { this.removeListener("line", onLine); };
        const onLine = (line) => { this.removeListener("close", cleanup); cb(line); };
        this.once("line", onLine);
        this.once("close", cleanup);
      }
      setPrompt(prompt) {
        this._prompt = typeof prompt === "string" ? prompt : "> ";
      }
      prompt(preserveCursor) {
        if (this.output && typeof this.output.write === "function") {
          this.output.write(this._prompt);
        }
      }
      write(data, key) {
        if (this.output && typeof this.output.write === "function" && data != null) {
          this.output.write(typeof data === "string" ? data : String(data));
        }
      }
      pause() {
        if (!this._paused) {
          this._paused = true;
          if (this.input && typeof this.input.pause === "function") {
            this.input.pause();
          }
          this.emit("pause");
        }
        return this;
      }
      resume() {
        if (this._paused) {
          this._paused = false;
          if (this.input && typeof this.input.resume === "function") {
            this.input.resume();
          }
          this.emit("resume");
        }
        return this;
      }
      [Symbol.asyncIterator]() {
        const self = this;
        const queue = [];
        let resolver = null;
        let done = false;
        self.on("line", (line) => {
          if (resolver) { const r = resolver; resolver = null; r({ value: line, done: false }); }
          else queue.push(line);
        });
        self.once("close", () => {
          done = true;
          if (resolver) { const r = resolver; resolver = null; r({ value: undefined, done: true }); }
        });
        return {
          next() {
            if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
            if (done) return Promise.resolve({ value: undefined, done: true });
            return new Promise((r) => { resolver = r; });
          },
        };
      }
    }
    function createInterface(input, output) {
      // Node accepts createInterface(options) AND createInterface(input[, output])
      // where the first positional arg is a stream. @puppeteer/browsers (and
      // many CLIs) call createInterface(childProcess.stderr) -- a Readable, not
      // an options object -- to read a subprocess line-by-line.
      if (typeof input === "string") return new Interface({ prompt: input });
      const isOptions =
        input &&
        typeof input === "object" &&
        ("input" in input || "output" in input || "terminal" in input || "prompt" in input);
      if (isOptions) return new Interface(input);
      if (input && typeof input.on === "function") return new Interface({ input, output });
      return new Interface(input || {});
    }
    function clearLine(stream, dir, cb) {
      if (!stream || typeof stream.write !== "function") {
        if (typeof cb === "function") queueMicrotask(cb);
        return false;
      }
      if (dir === -1) {
        stream.write("\x1b[1K");
      } else if (dir === 1) {
        stream.write("\x1b[0K");
      } else {
        stream.write("\x1b[2K");
      }
      if (typeof cb === "function") queueMicrotask(cb);
      return true;
    }
    function clearScreenDown(stream, cb) {
      if (stream && typeof stream.write === "function") {
        stream.write("\x1b[0J");
      }
      if (typeof cb === "function") queueMicrotask(cb);
    }
    function cursorTo(stream, x, y, cb) {
      if (typeof y === "function") { cb = y; y = undefined; }
      if (stream && typeof stream.write === "function") {
        if (typeof x === "number") {
          if (typeof y === "number") {
            stream.write("\x1b[" + (y + 1) + ";" + (x + 1) + "H");
          } else {
            stream.write("\x1b[" + (x + 1) + "G");
          }
        }
      }
      if (typeof cb === "function") queueMicrotask(cb);
      return true;
    }
    function moveCursor(stream, dx, dy, cb) {
      if (stream && typeof stream.write === "function") {
        if (dx !== 0 && typeof dx === "number") {
          if (dx > 0) stream.write("\x1b[" + dx + "C");
          else stream.write("\x1b[" + (-dx) + "D");
        }
        if (dy !== 0 && typeof dy === "number") {
          if (dy > 0) stream.write("\x1b[" + dy + "B");
          else stream.write("\x1b[" + (-dy) + "A");
        }
      }
      if (typeof cb === "function") queueMicrotask(cb);
      return true;
    }
    function emitKeypressEvents() {}
    return {
      Interface, createInterface,
      clearLine, clearScreenDown, cursorTo, moveCursor, emitKeypressEvents,
    };
  };

  registry.factories["readline/promises"] = () => {
    var rl = registry.get("readline");
    class Interface extends rl.Interface {
      question(prompt, options) {
        var signal = options && options.signal ? options.signal : null;
        return new Promise(function (resolve, reject) {
          if (signal && signal.aborted) { reject(new DOMException("The operation was aborted", "AbortError")); return; }
          var onAbort;
          rl.Interface.prototype.question.call(this, prompt, function(answer) {
            if (signal && onAbort) signal.removeEventListener("abort", onAbort);
            resolve(answer);
          });
          if (signal) {
            onAbort = function() { reject(new DOMException("The operation was aborted", "AbortError")); };
            signal.addEventListener("abort", onAbort, { once: true });
          }
        }.bind(this));
      }
    }
    return {
      createInterface: function (options) {
        var iface = rl.createInterface(options);
        Object.setPrototypeOf(iface, Interface.prototype);
        return iface;
      },
      Interface: Interface,
    };
  };

  // --------------------------------------------------------- trace_events
  // Node's trace_events module: used by tooling to conditionally enable
  // category-based tracing. oam has no kernel trace backend yet; this stub
  // keeps category checks and enable/disable calls from throwing.
  registry.factories.trace_events = () => {
    class Tracing {
      constructor(categories) {
        this._categories = Array.isArray(categories) ? categories.slice() : [];
        this._enabled = false;
      }
      get enabled() { return this._enabled; }
      get categories() { return this._categories.join(","); }
      enable() { this._enabled = true; }
      disable() { this._enabled = false; }
    }
    function createTracing(options) {
      return new Tracing((options && options.categories) || []);
    }
    function getEnabledCategories() { return ""; }
    return { createTracing, getEnabledCategories };
  };

  // ----------------------------------------------------------------------- vm
  // vm module: Script.runInThisContext / runInNewContext / runInContext,
  // createContext with WeakSet tracking, expression-first compilation.
  // NOTE: this uses the with(this){...} pattern for sandboxing, which is
  // NOT true V8 context isolation -- it shares the same global heap.
  // Sufficient for template engines, config eval, and most bundler use.
  registry.factories.vm = () => {
    const _vmContexts = new WeakSet();

    class Script {
      constructor(code, options) {
        this._code = String(code);
        const opts = options != null && typeof options === "object" ? options : {};
        if (typeof options === "string") {
          this._filename = options;
        } else {
          this._filename = opts.filename || "evalmachine.<anonymous>";
        }
        this._lineOffset = Number(opts.lineOffset) || 0;
        this._columnOffset = Number(opts.columnOffset) || 0;
        this._fn = null;
      }
      _compile() {
        if (this._fn) return this._fn;
        const code = this._code;
        // Try expression form first so that 'x + 1' works without explicit
        // return.  Fall back to statement form on SyntaxError.
        try {
          // eslint-disable-next-line no-new-func
          this._fn = new Function(`with(this){return(${code})}`);
        } catch (_e) {
          // eslint-disable-next-line no-new-func
          this._fn = new Function(`with(this){${code}}`);
        }
        return this._fn;
      }
      runInThisContext(_options) {
        return this._compile().call(globalThis);
      }
      runInContext(ctx, _options) {
        return this._compile().call(ctx != null ? ctx : globalThis);
      }
      runInNewContext(sandbox, _options) {
        const ctx = createContext(sandbox || {});
        return this._compile().call(ctx);
      }
      createCachedData() { return new Uint8Array(0); }
    }

    function createContext(sandbox, _options) {
      if (sandbox !== undefined && (sandbox === null || typeof sandbox !== "object")) {
        throw new TypeError("The 'sandbox' argument must be of type object. Received " + typeof sandbox);
      }
      const obj = sandbox != null ? sandbox : Object.create(null);
      if (!_vmContexts.has(obj)) {
        _vmContexts.add(obj);
        // Tag the context unless the caller already set Symbol.toStringTag.
        const desc = Object.getOwnPropertyDescriptor(obj, Symbol.toStringTag);
        if (!desc) {
          Object.defineProperty(obj, Symbol.toStringTag, {
            value: "Context",
            writable: false,
            enumerable: false,
            configurable: true,
          });
        }
      }
      return obj;
    }

    function isContext(value) {
      return value !== null && typeof value === "object" && _vmContexts.has(value);
    }

    function runInThisContext(code, _options) {
      return new Script(code, _options).runInThisContext();
    }
    function runInNewContext(code, sandbox, _options) {
      return new Script(code, _options).runInNewContext(sandbox);
    }
    function runInContext(code, ctx, _options) {
      return new Script(code, _options).runInContext(ctx);
    }
    function compileFunction(code, params, options) {
      const p = params || [];
      const opts = options != null && typeof options === "object" ? options : {};
      // eslint-disable-next-line no-new-func
      const fn = new Function(...p, code);
      if (opts.filename) fn._filename = opts.filename;
      return fn;
    }
    function measureMemory() {
      return Promise.resolve({ total: { jsMemoryEstimate: 0 } });
    }
    function createScript(code, options) {
      return new Script(code, options);
    }

    return {
      Script, createContext, isContext, createScript,
      runInThisContext, runInNewContext, runInContext,
      compileFunction, measureMemory,
    };
  };

  // ---------------------------------------------------------------------- v8
  // v8 module stub: expose minimal heap / serialization surface. Real V8
  // bindings land with the diagnostics wave; for now, feature-detect callers
  // get safe defaults and serialization uses JSON under a V8 wire header.
  registry.factories.v8 = (natives) => {
    // V8 serialization wire format sentinel bytes: 0xff 0x0d (version 13).
    function serialize(value) {
      const json = JSON.stringify(value);
      const enc = new TextEncoder().encode(json);
      const out = new Uint8Array(2 + enc.length);
      out[0] = 0xff; out[1] = 0x0d;
      out.set(enc, 2);
      return out;
    }
    function deserialize(buffer) {
      const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
      const start = (bytes.length >= 2 && bytes[0] === 0xff && bytes[1] === 0x0d) ? 2 : 0;
      return JSON.parse(new TextDecoder().decode(bytes.subarray(start)));
    }
    class Serializer {
      constructor() { this._chunks = []; }
      writeHeader() { this._chunks.push(new Uint8Array([0xff, 0x0d])); }
      writeValue(value) { this._chunks.push(new TextEncoder().encode(JSON.stringify(value))); }
      releaseBuffer() {
        let total = 0;
        for (const c of this._chunks) total += c.length;
        const out = new Uint8Array(total);
        let off = 0;
        for (const c of this._chunks) { out.set(c, off); off += c.length; }
        this._chunks = [];
        return out;
      }
    }
    class Deserializer {
      constructor(buffer) {
        const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
        const start = (bytes.length >= 2 && bytes[0] === 0xff && bytes[1] === 0x0d) ? 2 : 0;
        this._text = new TextDecoder().decode(bytes.subarray(start));
      }
      readHeader() { return true; }
      readValue() { return JSON.parse(this._text); }
    }
    function getHeapStatistics() {
      return natives.heapStatistics();
    }
    function getHeapSpaceStatistics() { return []; }
    function getHeapCodeStatistics() {
      return { code_and_metadata_size: 0, bytecode_and_metadata_size: 0, external_script_source_size: 0 };
    }
    function setFlagsFromString() {}
    function writeHeapSnapshot() { return ""; }
    function cachedDataVersionTag() { return 0; }
    function stopCoverage() {}
    function takeCoverage() {}
    const startupSnapshot = {
      addDeserializeCallback() {},
      addSerializeCallback() {},
      isBuildingSnapshot() { return false; },
    };
    return {
      serialize, deserialize, Serializer, Deserializer,
      getHeapStatistics, getHeapSpaceStatistics, getHeapCodeStatistics,
      setFlagsFromString, writeHeapSnapshot, cachedDataVersionTag,
      stopCoverage, takeCoverage, startupSnapshot,
      constants: {
        ALL_PROPERTIES: 0, ONLY_WRITABLE: 1, ONLY_ENUMERABLE: 2, ONLY_CONFIGURABLE: 4,
        SKIP_STRINGS: 8, SKIP_SYMBOLS: 16,
      },
    };
  };

  // -------------------------------------------------------------- https
  // HTTPS server (TLS-wrapped HTTP) and client. The server uses
  // httpsServe (Rust TLS termination) but shares the same accept/respond
  // ops as plain HTTP. Client request/get delegate to fetch (which
  // supports HTTPS natively via reqwest+rustls).
  registry.factories.https = (natives) => {
    const http = registry.get("http");
    const EventEmitter = registry.get("events");

    class Server extends EventEmitter {
      constructor(options, handler) {
        super();
        if (typeof options === "function") {
          handler = options;
          options = {};
        }
        this._options = options || {};
        if (handler) this.on("request", handler);
        this._serverId = null;
        this._port = null;
        this._host = null;
        this.listening = false;
      }
      listen(port, host, callback) {
        if (typeof port === "object" && port !== null) {
          callback = host;
          host = port.host;
          port = port.port;
        }
        if (typeof host === "function") {
          callback = host;
          host = undefined;
        }
        if (typeof callback === "function") this.once("listening", callback);
        var hostname = host || "127.0.0.1";
        var certPem = typeof this._options.cert === "object" && this._options.cert instanceof Uint8Array
          ? new TextDecoder().decode(this._options.cert) : String(this._options.cert || "");
        var keyPem = typeof this._options.key === "object" && this._options.key instanceof Uint8Array
          ? new TextDecoder().decode(this._options.key) : String(this._options.key || "");
        natives.httpsServe(hostname, port || 0, certPem, keyPem).then(
          (bound) => {
            this._serverId = bound.serverId;
            this._port = bound.port;
            this._host = hostname;
            this.listening = true;
            this.emit("listening");
            (async () => {
              for (;;) {
                const meta = await natives.httpAccept(bound.serverId);
                if (meta === undefined) break;
                const req = new http.IncomingMessage(meta);
                req.socket = { remoteAddress: "127.0.0.1", encrypted: true };
                const res = new http.ServerResponse(meta.requestId);
                this.emit("request", req, res);
              }
              this.emit("close");
            })();
          },
          (err) => this.emit("error", typeof err === "string" ? new Error(err) : err),
        );
        return this;
      }
      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }
      close(callback) {
        if (this._serverId !== null) {
          natives.httpClose(this._serverId);
          this.listening = false;
        }
        if (callback) this.once("close", callback);
        return this;
      }
    }

    function createServer(options, handler) {
      return new Server(options, handler);
    }

    function request(url, options, callback) {
      if (typeof url === "string" || url instanceof URL) {
        var parsed = typeof url === "string" ? new URL(url) : url;
        if (typeof options === "function") { callback = options; options = {}; }
        options = options || {};
        options.hostname = options.hostname || parsed.hostname;
        options.port = options.port || parsed.port || 443;
        options.path = options.path || parsed.pathname + parsed.search;
        options.protocol = "https:";
      } else {
        callback = options;
        options = url || {};
        if (!options.protocol) options.protocol = "https:";
        if (!options.port) options.port = 443;
      }
      if (options.rejectUnauthorized === false) {
        return new TlsClientRequest(options, callback);
      }
      return http.request(options, callback);
    }

    class TlsClientRequest extends EventEmitter {
      constructor(options, callback) {
        super();
        this.method = (options.method || "GET").toUpperCase();
        this._options = options;
        this._headers = {};
        if (options.headers) {
          var keys = Object.keys(options.headers);
          for (var i = 0; i < keys.length; i++) this._headers[keys[i].toLowerCase()] = options.headers[keys[i]];
        }
        this._body = [];
        this._ended = false;
        this._aborted = false;
        this.headersSent = false;
        var self = this;
        this.socket = {
          remoteAddress: options.hostname || "localhost",
          remotePort: Number(options.port || 443),
          localAddress: "127.0.0.1", localPort: 0,
          setTimeout: function() { return this; },
          setNoDelay: function() { return this; },
          setKeepAlive: function() { return this; },
          ref: function() { return this; },
          unref: function() { return this; },
          destroy: function() { self.destroy(); },
        };
        if (callback) this.once("response", callback);
      }
      setHeader(name, value) { this._headers[name.toLowerCase()] = value; return this; }
      getHeader(name) { return this._headers[name.toLowerCase()]; }
      removeHeader(name) { delete this._headers[name.toLowerCase()]; }
      write(chunk, encoding, cb) {
        if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        if (typeof chunk === "string") chunk = globalThis.Buffer.from(chunk, encoding || "utf8");
        else if (!(chunk instanceof Uint8Array)) chunk = globalThis.Buffer.from(chunk);
        this._body.push(chunk);
        if (cb) queueMicrotask(cb);
        return true;
      }
      end(data, encoding, cb) {
        if (typeof data === "function") { cb = data; data = undefined; }
        if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        if (data != null) this.write(data, encoding);
        this._ended = true;
        this.headersSent = true;
        this._send();
        if (cb) this.once("response", cb);
        return this;
      }
      destroy() { this._aborted = true; }
      on(ev, fn) { return super.on(ev, fn); }
      _send() {
        var self = this;
        var tls = registry.get("tls");
        var { Readable } = registry.get("stream");
        var host = self._options.hostname || "localhost";
        var port = Number(self._options.port || 443);
        var path = self._options.path || "/";
        var sock = tls.connect({ host: host, port: port, rejectUnauthorized: false });
        sock.on("error", function(err) { self.emit("error", err); });
        sock.on("secureConnect", function() {
          var lc = self._headers; // header names already lowercased
          var bodyBuf = null;
          if (self._body.length > 0) {
            var total = 0;
            for (var i = 0; i < self._body.length; i++) total += self._body[i].length;
            bodyBuf = globalThis.Buffer.alloc(total);
            var off = 0;
            for (var i = 0; i < self._body.length; i++) { bodyBuf.set(self._body[i], off); off += self._body[i].length; }
          }
          var needsBody = bodyBuf && self.method !== "GET" && self.method !== "HEAD";

          // Request line + headers. Dedupe Host / Content-Length / Connection:
          // a caller-supplied value wins and is emitted once (two Host or two
          // Content-Length fields are an RFC 7230 / request-smuggling hazard).
          // We force a single Connection: close (no keep-alive reuse here).
          var reqStr = self.method + " " + path + " HTTP/1.1\r\n";
          reqStr += "Host: " + (lc["host"] != null ? lc["host"] : host) + "\r\n";
          var hkeys = Object.keys(lc);
          for (var i = 0; i < hkeys.length; i++) {
            var hk = hkeys[i];
            if (hk === "host" || hk === "connection" || hk === "content-length") continue;
            reqStr += hk + ": " + lc[hk] + "\r\n";
          }
          if (needsBody) {
            reqStr += "Content-Length: " + (lc["content-length"] != null ? lc["content-length"] : bodyBuf.length) + "\r\n";
          } else if (lc["content-length"] != null) {
            reqStr += "Content-Length: " + lc["content-length"] + "\r\n";
          }
          reqStr += "Connection: close\r\n\r\n";
          sock.write(reqStr);
          if (needsBody) sock.write(bodyBuf);

          // Response parser. Honors Transfer-Encoding: chunked and
          // Content-Length, falling back to read-to-EOF only when neither is
          // present. The header buffer is released once headers are parsed, so
          // body bytes are never retained (no 2x memory / O(n^2) concat).
          var headerBuf = globalThis.Buffer.alloc(0);
          var headersDone = false;
          var res = null;
          var bodyMode = "eof";          // "eof" | "length" | "chunked"
          var remaining = 0;             // bytes left in "length" mode
          var chunkBuf = globalThis.Buffer.alloc(0);
          var chunkState = "size";       // "size" | "data" | "crlf" | "done"
          var chunkRemaining = 0;

          function finishRes() { if (res) { res.push(null); res = null; } }

          function feedBody(buf) {
            if (!res) return;
            if (bodyMode === "eof") {
              if (buf.length > 0) res.push(buf);
              return;
            }
            if (bodyMode === "length") {
              if (remaining <= 0) { finishRes(); return; }
              var take = buf.length <= remaining ? buf : buf.slice(0, remaining);
              if (take.length > 0) res.push(take);
              remaining -= take.length;
              if (remaining <= 0) finishRes();
              return;
            }
            // chunked
            chunkBuf = globalThis.Buffer.concat([chunkBuf, buf]);
            for (;;) {
              if (chunkState === "size") {
                var nl = chunkBuf.indexOf("\r\n");
                if (nl === -1) return;
                var sizeLine = chunkBuf.slice(0, nl).toString().trim();
                var semi = sizeLine.indexOf(";");
                if (semi !== -1) sizeLine = sizeLine.slice(0, semi);
                var size = parseInt(sizeLine, 16);
                chunkBuf = chunkBuf.slice(nl + 2);
                if (isNaN(size)) {
                  // Malformed chunk-size line -- surface a parse error rather
                  // than silently treating it as the 0-terminator.
                  chunkState = "done";
                  var perr = new Error("Parse Error: invalid chunk size");
                  perr.code = "HPE_INVALID_CHUNK_SIZE";
                  self.emit("error", perr);
                  finishRes();
                  return;
                }
                if (size === 0) { chunkState = "done"; finishRes(); return; }
                chunkRemaining = size;
                chunkState = "data";
              } else if (chunkState === "data") {
                if (chunkBuf.length < chunkRemaining) {
                  if (chunkBuf.length > 0 && res) {
                    res.push(chunkBuf);
                    chunkRemaining -= chunkBuf.length;
                    chunkBuf = globalThis.Buffer.alloc(0);
                  }
                  return;
                }
                if (chunkRemaining > 0 && res) res.push(chunkBuf.slice(0, chunkRemaining));
                chunkBuf = chunkBuf.slice(chunkRemaining);
                chunkRemaining = 0;
                chunkState = "crlf";
              } else if (chunkState === "crlf") {
                if (chunkBuf.length < 2) return;
                chunkBuf = chunkBuf.slice(2);
                chunkState = "size";
              } else {
                return;
              }
            }
          }

          sock.on("data", function(chunk) {
            if (self._aborted) return;
            var b = typeof chunk === "string" ? globalThis.Buffer.from(chunk) : chunk;
            if (!headersDone) {
              headerBuf = globalThis.Buffer.concat([headerBuf, b]);
              var idx = headerBuf.indexOf("\r\n\r\n");
              if (idx === -1) return;
              headersDone = true;
              var headerStr = headerBuf.slice(0, idx).toString();
              var bodyStart = headerBuf.slice(idx + 4);
              headerBuf = null;
              var lines = headerStr.split("\r\n");
              var statusParts = lines[0].split(" ");
              var statusCode = parseInt(statusParts[1]) || 200;
              var statusMessage = statusParts.slice(2).join(" ") || "";
              var resHeaders = {};
              var rawHeaders = [];
              for (var i = 1; i < lines.length; i++) {
                var colon = lines[i].indexOf(":");
                if (colon === -1) continue;
                var k = lines[i].slice(0, colon).trim();
                var v = lines[i].slice(colon + 1).trim();
                var lk = k.toLowerCase();
                rawHeaders.push(k, v);
                if (lk === "set-cookie") {
                  if (Array.isArray(resHeaders[lk])) resHeaders[lk].push(v);
                  else resHeaders[lk] = [v];
                } else if (resHeaders[lk] !== undefined) {
                  resHeaders[lk] = resHeaders[lk] + ", " + v;
                } else {
                  resHeaders[lk] = v;
                }
              }
              res = new Readable({ read: function() {} });
              res.statusCode = statusCode;
              res.statusMessage = statusMessage;
              res.httpVersion = "1.1";
              res.headers = resHeaders;
              res.rawHeaders = rawHeaders;
              var te = resHeaders["transfer-encoding"];
              var cl = resHeaders["content-length"];
              if (te && String(te).toLowerCase().indexOf("chunked") !== -1) {
                bodyMode = "chunked";
              } else if (cl !== undefined) {
                bodyMode = "length";
                remaining = parseInt(cl, 10) || 0;
              } else {
                bodyMode = "eof";
              }
              self.emit("response", res);
              if (bodyStart.length > 0) feedBody(bodyStart);
              else if (bodyMode === "length" && remaining <= 0) finishRes();
            } else {
              feedBody(b);
            }
          });
          sock.on("end", function() {
            if (!headersDone) {
              // Peer closed before a full header block: surface an error rather
              // than leaving the caller's callback/promise pending forever.
              var err = new Error("socket hang up");
              err.code = "ECONNRESET";
              self.emit("error", err);
              return;
            }
            // Peer closed mid-body before the declared length / chunk terminator
            // arrived: surface a truncation error instead of ending cleanly
            // (a short body must not look complete).
            if ((bodyMode === "length" && remaining > 0) ||
                (bodyMode === "chunked" && chunkState !== "done")) {
              var terr = new Error("aborted");
              terr.code = "ECONNRESET";
              self.emit("error", terr);
            }
            // Flush end-of-stream (covers eof mode and ends the body stream).
            finishRes();
          });
        });
      }
    }

    function get(url, options, callback) {
      var req = request(url, options, callback);
      req.end();
      return req;
    }

    var merged = {};
    var httpKeys = Object.keys(http);
    for (var i = 0; i < httpKeys.length; i++) merged[httpKeys[i]] = http[httpKeys[i]];
    merged.createServer = createServer;
    merged.Server = Server;
    merged.request = request;
    merged.get = get;
    return merged;
  };

  // --------------------------------------------------------------- domain
  // Node's `domain` module (deprecated since Node 4, still pulled in by
  // legacy error-handling middleware). Minimal bind/intercept API shape
  // without actual uncaught-exception routing.
  registry.factories.domain = () => {
    const EventEmitter = registry.get("events");
    class Domain extends EventEmitter {
      constructor() {
        super();
        this.members = [];
      }
      enter() {}
      exit() {}
      add(obj) { this.members.push(obj); return this; }
      remove(obj) {
        const i = this.members.indexOf(obj);
        if (i >= 0) this.members.splice(i, 1);
        return this;
      }
      bind(fn) {
        const d = this;
        return function (...args) {
          try { return fn.apply(this, args); }
          catch (e) { d.emit("error", e); }
        };
      }
      intercept(fn) {
        const d = this;
        return function (err, ...args) {
          if (err) { d.emit("error", err); return; }
          try { fn.apply(this, args); }
          catch (e) { d.emit("error", e); }
        };
      }
      run(fn, ...args) {
        try { return fn.apply(this, args); }
        catch (e) { this.emit("error", e); }
      }
    }
    function create() { return new Domain(); }
    const active = null;
    return { Domain, create, active };
  };

  // ---------------------------------------------------------------- repl
  // Stub for code that imports the `repl` module for inspection/extension
  // without starting an interactive REPL session.
  registry.factories.repl = () => {
    const EventEmitter = registry.get("events");
    class REPLServer extends EventEmitter {
      constructor() { super(); this.context = globalThis; }
      start() { return this; }
      close() { this.emit("exit"); }
      defineCommand() {}
      displayPrompt() {}
      clearBufferedCommand() {}
      setupHistory(file, cb) { if (typeof cb === "function") queueMicrotask(() => cb(null, this)); }
    }
    function start(_options) {
      const server = new REPLServer();
      queueMicrotask(() => server.emit("exit"));
      return server;
    }
    return { start, REPLServer, builtinModules: registry.get("module").builtinModules };
  };

  // ------------------------------------------------------------ inspector
  // Node's `inspector` module: wire-level CDP is implemented in oam's Rust
  // core; the JS module surface exposes open/close/url and a Session class
  // so library detection code (clinic, node --inspect integrations) works.
  registry.factories.inspector = () => {
    const EventEmitter = registry.get("events");
    class Session extends EventEmitter {
      connect() {}
      connectToMainThread() {}
      disconnect() {}
      post(method, params, cb) {
        if (typeof params === "function") { cb = params; }
        if (typeof cb === "function") queueMicrotask(() => cb(null, {}));
      }
    }
    let _opened = false;
    function open(_port, _host, _wait) { _opened = true; }
    function close() { _opened = false; }
    function url() { return _opened ? "ws://127.0.0.1:9229/0" : undefined; }
    function waitForDebugger() {}
    return { Session, open, close, url, waitForDebugger, console: globalThis.console || {} };
  };

  // ------------------------------------------------------ child_process
  registry.factories.child_process = (natives) => {
    const EventEmitter = registry.get("events");
    const { Readable, Writable } = registry.get("stream");
    const { Buffer } = registry.get("buffer");

    function normalizeArgs(command, args, options) {
      if (args != null && typeof args === "object" && !Array.isArray(args)) {
        options = args;
        args = [];
      }
      return {
        command: String(command),
        args: (args || []).map(String),
        options: options || {},
      };
    }

    function decodeOutput(buf, encoding) {
      if (!encoding || encoding === "buffer") return Buffer.from(buf);
      return Buffer.from(buf).toString(encoding);
    }

    function spawnSync(command, args, options) {
      const norm = normalizeArgs(command, args, options);
      const opts = norm.options;
      const nativeOpts = {
        cwd: opts.cwd || undefined,
        env: opts.env || undefined,
        shell: !!opts.shell,
        clearEnv: false,
        timeout: opts.timeout || 0,
        maxBuffer: opts.maxBuffer || 50 * 1024 * 1024,
        input: opts.input != null
          ? (typeof opts.input === "string" ? Buffer.from(opts.input, opts.encoding || "utf8") : opts.input)
          : undefined,
      };
      const result = natives.spawnSync(norm.command, norm.args, nativeOpts);
      const encoding = opts.encoding || "buffer";
      return {
        pid: result.pid,
        output: [null, decodeOutput(result.stdout, encoding), decodeOutput(result.stderr, encoding)],
        stdout: decodeOutput(result.stdout, encoding),
        stderr: decodeOutput(result.stderr, encoding),
        status: result.status,
        signal: result.signal,
        error: result.error
          ? Object.assign(new Error(result.error.message), { code: result.error.code })
          : undefined,
      };
    }

    function execSync(command, options) {
      const opts = Object.assign({ shell: true }, options);
      const result = spawnSync(command, [], opts);
      if (result.error) throw result.error;
      if (result.status !== 0) {
        const err = new Error(`Command failed: ${command}\n${result.stderr}`);
        err.status = result.status;
        err.signal = result.signal;
        err.stdout = result.stdout;
        err.stderr = result.stderr;
        err.pid = result.pid;
        throw err;
      }
      return result.stdout;
    }

    function execFileSync(file, args, options) {
      const norm = normalizeArgs(file, args, options);
      const result = spawnSync(norm.command, norm.args, norm.options);
      if (result.error) throw result.error;
      if (result.status !== 0) {
        const err = new Error(`Command failed: ${norm.command}`);
        err.status = result.status;
        err.signal = result.signal;
        err.stdout = result.stdout;
        err.stderr = result.stderr;
        err.pid = result.pid;
        throw err;
      }
      return result.stdout;
    }

    class ChildProcess extends EventEmitter {
      constructor() {
        super();
        this.pid = null;
        this.exitCode = null;
        this.signalCode = null;
        this.killed = false;
        this.connected = false;
        this.stdin = null;
        this.stdout = null;
        this.stderr = null;
        this._handle = null;
        this._exited = false;
        this._extra = false;
      }
      kill(signal) {
        if (this._handle != null) {
          if (this._extra) natives.spawnExtraKill(this._handle, signal);
          else natives.spawnKill(this._handle, signal);
          this.killed = true;
        }
        return true;
      }
      ref() { return this; }
      unref() { return this; }
    }

    // Run `onReady` once the child has spawned, or invoke `callback(err)` if
    // the spawn fails -- whichever happens first. Prevents deferred stdin
    // write/final callbacks from dangling forever on a spawn error.
    function deferUntilSpawn(cp, onReady, callback) {
      var done = false;
      var onSpawn = function() {
        if (done) return; done = true;
        cp.removeListener("spawnfail", onFail);
        onReady();
      };
      var onFail = function(err) {
        if (done) return; done = true;
        cp.removeListener("spawn", onSpawn);
        callback(err);
      };
      cp.once("spawn", onSpawn);
      cp.once("spawnfail", onFail);
    }

    // Map a Node stdio entry to the native spawnExtra direction code:
    //   0=ignore 1=inherit 2=child-read(parent writes) 3=child-write(parent reads).
    // For 'pipe', direction is by fd: fd0 child-reads (stdin); fd1/fd2
    // child-write (stdout/stderr); fd3 child-reads, fd4 child-writes -- the CDP
    // pipe-transport convention (Chromium reads commands on 3, writes on 4).
    // Anonymous pipes are unidirectional, so extra fds carry one direction;
    // full-duplex extra fds (named pipes) are a future enhancement.
    function stdioCode(entry, fd) {
      if (entry === "ignore" || entry === null || entry === undefined) return 0;
      if (entry === "pipe" || entry === "overlapped") {
        if (fd === 0) return 2;
        if (fd === 1 || fd === 2) return 3;
        return fd === 3 ? 2 : 3;
      }
      // 'inherit', a numeric fd, or a stream: inherit the parent's fd.
      return 1;
    }

    // Extra-fd spawn: a child with numbered fds beyond 0/1/2 (Chromium
    // --remote-debugging-pipe needs fds 3/4 for CDP). Synchronous like Node's
    // spawn(); exposes cp.stdio[n] as Readable/Writable streams. The native
    // backend is Windows (CreateProcessW) or Unix (Command+pre_exec dup2).
    function spawnExtra(norm, stdioArr) {
      const opts = norm.options;
      const cp = new ChildProcess();
      cp._extra = true;
      const codes = stdioArr.map((e, i) => stdioCode(e, i));
      const nativeOpts = {
        cwd: opts.cwd || undefined,
        env: opts.env || undefined,
        clearEnv: false,
      };

      cp.stdio = new Array(stdioArr.length).fill(null);
      let info;
      try {
        info = JSON.parse(natives.spawnExtra(norm.command, norm.args, nativeOpts, codes));
      } catch (err) {
        // The native op throws with a JSON body {code,message} on spawn failure
        // (child_win.rs / child_unix.rs) so we can surface a Node-shaped error
        // (err.code === "ENOENT" etc.) that ecosystem code branches on.
        let e = typeof err === "string" ? new Error(err) : err;
        const raw = typeof err === "string" ? err : (e && e.message) || "";
        try {
          const parsed = JSON.parse(raw);
          if (parsed && parsed.code) {
            e = new Error(parsed.message || raw);
            e.code = parsed.code;
            e.syscall = "spawn " + norm.command;
            e.path = norm.command;
          }
        } catch {
          /* message was not the JSON body: emit the original error unchanged */
        }
        queueMicrotask(() => cp.emit("error", e));
        return cp;
      }
      cp._handle = info.handle;
      cp.pid = info.pid;

      const reads = [];
      for (let fd = 0; fd < codes.length; fd++) {
        if (codes[fd] === 2) {
          // child reads -> parent writes: a Writable.
          const w = new Writable({
            write(chunk, encoding, callback) {
              const data = typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk;
              natives.spawnExtraWrite(cp._handle, fd, data).then(() => callback(), (er) => callback(er));
            },
            final(callback) {
              natives.spawnExtraCloseFd(cp._handle, fd);
              callback();
            },
          });
          cp.stdio[fd] = w;
        } else if (codes[fd] === 3) {
          // child writes -> parent reads: a Readable + a pump loop.
          const r = new Readable({ read() {} });
          cp.stdio[fd] = r;
          reads.push((async () => {
            while (true) {
              let chunk;
              try {
                chunk = await natives.spawnExtraRead(cp._handle, fd);
              } catch {
                r.push(null);
                break;
              }
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                r.push(null);
                break;
              }
              r.push(Buffer.from(chunk));
            }
          })());
        }
      }
      // Node aliases: stdin/stdout/stderr are stdio[0..2].
      cp.stdin = cp.stdio[0];
      cp.stdout = cp.stdio[1];
      cp.stderr = cp.stdio[2];

      queueMicrotask(() => cp.emit("spawn"));

      natives.spawnExtraWait(cp._handle).then((result) => {
        cp._exited = true;
        cp.exitCode = result.code;
        cp.signalCode = result.signal;
        cp.emit("exit", result.code, result.signal);
        Promise.allSettled(reads).then(() => {
          cp.emit("close", result.code, result.signal);
        });
      });

      return cp;
    }

    function spawn(command, args, options) {
      const norm = normalizeArgs(command, args, options);
      const opts = norm.options;
      // Extra-fd stdio (Chromium CDP pipe): an array with >3 entries routes to
      // the raw extra-fd spawn (CreateProcessW+lpReserved2 on Windows, Command+
      // pre_exec dup2 on Unix). Gated to platforms with a real native backend.
      if (
        Array.isArray(opts.stdio) &&
        opts.stdio.length > 3 &&
        (natives.platform === "win32" ||
          natives.platform === "linux" ||
          natives.platform === "darwin")
      ) {
        return spawnExtra(norm, opts.stdio);
      }
      const cp = new ChildProcess();

      cp.stdout = new Readable({ read() {} });
      cp.stderr = new Readable({ read() {} });
      cp.stdin = new Writable({
        write(chunk, encoding, callback) {
          const data = typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk;
          const doWrite = () => natives.spawnWrite(cp._handle, data).then(() => callback(), (err) => callback(err));
          if (cp._handle === null) {
            // spawn not resolved yet: settle on whichever lands first so a
            // spawn failure never leaves this write callback dangling.
            deferUntilSpawn(cp, doWrite, callback);
            return;
          }
          doWrite();
        },
        final(callback) {
          const doFinal = () => { natives.spawnCloseStdin(cp._handle); callback(); };
          if (cp._handle === null) {
            deferUntilSpawn(cp, doFinal, callback);
            return;
          }
          doFinal();
        },
      });

      const nativeOpts = {
        cwd: opts.cwd || undefined,
        env: opts.env || undefined,
        shell: !!opts.shell,
        clearEnv: false,
      };

      const readStdout = async (handle) => {
        while (true) {
          const chunk = await natives.spawnReadStdout(handle);
          if (chunk === undefined || chunk === null || chunk.length === 0) {
            cp.stdout.push(null);
            break;
          }
          cp.stdout.push(Buffer.from(chunk));
        }
      };
      const readStderr = async (handle) => {
        while (true) {
          const chunk = await natives.spawnReadStderr(handle);
          if (chunk === undefined || chunk === null || chunk.length === 0) {
            cp.stderr.push(null);
            break;
          }
          cp.stderr.push(Buffer.from(chunk));
        }
      };

      natives.spawnAsync(norm.command, norm.args, nativeOpts).then((info) => {
        cp._handle = info.handle;
        cp.pid = info.pid;

        cp.emit("spawn");

        const stdoutDone = readStdout(info.handle);
        const stderrDone = readStderr(info.handle);

        natives.spawnWait(info.handle).then((result) => {
          cp._exited = true;
          cp.exitCode = result.code;
          cp.signalCode = result.signal;
          cp.emit("exit", result.code, result.signal);
          // Node fires 'close' only AFTER the stdio streams reach EOF, not
          // merely when the process exits. Wait for both read loops so a
          // 'data' listener attached in the 'spawn' handler still sees all
          // output before 'close' -- otherwise stdout capture races the
          // close emit and is intermittently empty.
          Promise.allSettled([stdoutDone, stderrDone]).then(() => {
            cp.emit("close", result.code, result.signal);
          });
        });
      }).catch((err) => {
        const e = typeof err === "string" ? new Error(err) : err;
        // Settle deferred stdin write/final waiters, then end the read streams
        // so consumers awaiting completion don't hang on a failed spawn.
        cp.emit("spawnfail", e);
        cp.stdout.push(null);
        cp.stderr.push(null);
        queueMicrotask(() => cp.emit("error", e));
      });

      return cp;
    }

    function exec(command, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      const opts = Object.assign({ shell: true }, options);
      const cp = spawn(command, [], opts);
      const stdout = [];
      const stderr = [];
      const maxBuffer = opts.maxBuffer || 50 * 1024 * 1024;

      cp.on("spawn", () => {
        cp.stdout.on("data", (chunk) => {
          stdout.push(chunk);
          if (Buffer.concat(stdout).length > maxBuffer) {
            cp.kill();
          }
        });
        cp.stderr.on("data", (chunk) => {
          stderr.push(chunk);
        });
      });
      cp.on("close", (code, signal) => {
        const stdoutBuf = Buffer.concat(stdout);
        const stderrBuf = Buffer.concat(stderr);
        const enc = opts.encoding || "utf8";
        const out = enc === "buffer" ? stdoutBuf : stdoutBuf.toString(enc);
        const errOut = enc === "buffer" ? stderrBuf : stderrBuf.toString(enc);
        if (code !== 0 && callback) {
          const err = new Error(`Command failed: ${command}\n${errOut}`);
          err.code = code;
          err.signal = signal;
          callback(err, out, errOut);
        } else if (callback) {
          callback(null, out, errOut);
        }
      });
      cp.on("error", (err) => {
        if (callback) callback(err, "", "");
      });
      return cp;
    }

    function execFile(file, args, options, callback) {
      if (typeof args === "function") {
        callback = args;
        args = [];
        options = {};
      } else if (typeof options === "function") {
        callback = options;
        options = {};
      }
      const norm = normalizeArgs(file, args, options);
      return exec(norm.command + " " + norm.args.join(" "), norm.options, callback);
    }

    function fork(modulePath, args, options) {
      if (typeof args === "object" && !Array.isArray(args)) {
        options = args;
        args = [];
      }
      args = (args || []).map(String);
      const opts = Object.assign({}, options);
      const execPath = opts.execPath || globalThis.process.execPath;
      const execArgv = opts.execArgv || globalThis.process.execArgv || [];
      const silent = !!opts.silent;

      const cp = new ChildProcess();
      cp.connected = true;

      cp.stdout = silent ? new Readable({ read() {} }) : null;
      cp.stderr = silent ? new Readable({ read() {} }) : null;
      cp.stdin = null;

      let ipcSocket = null;
      const pendingSends = [];

      cp.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") { callback = _sendHandle; }
        else if (typeof _options === "function") { callback = _options; }
        if (!cp.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        const line = JSON.stringify(message) + "\n";
        if (ipcSocket) {
          ipcSocket.write(line, "utf8", callback);
        } else {
          pendingSends.push({ line, callback });
        }
        return true;
      };

      cp.disconnect = function disconnect() {
        cp.connected = false;
        if (ipcSocket) {
          var sock = ipcSocket;
          ipcSocket = null;
          sock._chain.then(function() {
            sock.destroy();
            cp.emit("disconnect");
          });
        } else {
          cp.emit("disconnect");
        }
      };

      const net = registry.get("net");
      const ipcServer = net.createServer();

      ipcServer.listen(0, "127.0.0.1", () => {
        const ipcPort = ipcServer.address().port;

        const childEnv = Object.assign({},
          opts.env || globalThis.process.env,
          { OAM_FORK_IPC_PORT: String(ipcPort) },
        );

        const spawnArgs = execArgv.concat(["run", String(modulePath), "--no-check", "--"]).concat(args);
        const nativeOpts = {
          cwd: opts.cwd || undefined,
          env: childEnv,
          shell: false,
          clearEnv: false,
        };

        ipcServer.on("connection", (socket) => {
          ipcSocket = socket;
          ipcServer.close();

          for (const p of pendingSends) {
            socket.write(p.line, "utf8", p.callback);
          }
          pendingSends.length = 0;

          let buf = "";
          socket.setEncoding("utf8");
          socket.on("data", (chunk) => {
            buf += chunk;
            let nl;
            while ((nl = buf.indexOf("\n")) !== -1) {
              const line = buf.slice(0, nl);
              buf = buf.slice(nl + 1);
              try {
                const msg = JSON.parse(line);
                cp.emit("message", msg);
              } catch (_) { /* ignore malformed */ }
            }
          });
          socket.on("end", () => {
            cp.connected = false;
            cp.emit("disconnect");
          });
          socket.on("error", () => {
            cp.connected = false;
          });
        });

        natives.spawnAsync(execPath, spawnArgs, nativeOpts).then((info) => {
          cp._handle = info.handle;
          cp.pid = info.pid;

          if (silent) {
            cp.stdin = new Writable({
              write(chunk, encoding, callback) {
                natives.spawnWrite(info.handle, typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk)
                  .then(() => callback(), (err) => callback(err));
              },
              final(callback) {
                natives.spawnCloseStdin(info.handle);
                callback();
              },
            });
          }

          cp.emit("spawn");

          const readStdout = async (handle) => {
            while (true) {
              const chunk = await natives.spawnReadStdout(handle);
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                if (cp.stdout) cp.stdout.push(null);
                break;
              }
              if (cp.stdout) cp.stdout.push(Buffer.from(chunk));
            }
          };
          const readStderr = async (handle) => {
            while (true) {
              const chunk = await natives.spawnReadStderr(handle);
              if (chunk === undefined || chunk === null || chunk.length === 0) {
                if (cp.stderr) cp.stderr.push(null);
                break;
              }
              if (cp.stderr) cp.stderr.push(Buffer.from(chunk));
            }
          };
          const stdoutDone = readStdout(info.handle);
          const stderrDone = readStderr(info.handle);

          natives.spawnWait(info.handle).then((result) => {
            cp._exited = true;
            cp.exitCode = result.code;
            cp.signalCode = result.signal;
            ipcServer.close();
            if (ipcSocket) {
              var sock = ipcSocket;
              ipcSocket = null;
              sock._chain.then(function() { sock.end(); });
            }
            cp.connected = false;
            cp.emit("exit", result.code, result.signal);
            // 'close' waits for stdio EOF, not just process exit (Node parity;
            // see spawn() above -- avoids the stdout-capture race on 'close').
            Promise.allSettled([stdoutDone, stderrDone]).then(() => {
              cp.emit("close", result.code, result.signal);
            });
          });
        }).catch((err) => {
          ipcServer.close();
          queueMicrotask(() => cp.emit("error", typeof err === "string" ? new Error(err) : err));
        });
      });

      return cp;
    }

    return {
      spawn,
      spawnSync,
      exec,
      execSync,
      execFile,
      execFileSync,
      fork: fork,
      ChildProcess,
    };
  };

  // ---------------------------------------------------------------- cluster
  registry.factories.cluster = (natives) => {
    const EventEmitter = registry.get("events");

    const _isWorker = natives.clusterIsWorker();

    let _nextId = 1;

    class Worker extends EventEmitter {
      constructor(id, handle, pid) {
        super();
        this.id = id;
        this.exitedAfterDisconnect = false;
        this.killed = false;
        this.process = {
          pid,
          kill: (signal) => this.kill(signal),
        };
        this._handle = handle;
        this._dead = false;
        this._pendingKill = null;
      }
      kill(signal) {
        if (this._dead) return;
        this.killed = true;
        // fork() hasn't resolved the real handle yet: remember the request and
        // apply it once the handle lands, so a kill issued right after fork()
        // isn't silently dropped.
        if (this._handle === -1) {
          this._pendingKill = signal || "SIGTERM";
          return;
        }
        natives.clusterWorkerKill(this._handle, signal || "SIGTERM");
      }
      isDead() { return this._dead; }
      isConnected() { return !this._dead; }
      send() { return false; }
      disconnect() {
        this.exitedAfterDisconnect = true;
        this.kill();
        return this;
      }
    }

    class Cluster extends EventEmitter {
      constructor() {
        super();
        this.isWorker = _isWorker;
        this.isPrimary = !_isWorker;
        this.isMaster = !_isWorker;
        this.workers = {};
        this.settings = {};
        this.SCHED_NONE = 1;
        this.SCHED_RR = 2;
        this.schedulingPolicy = this.SCHED_NONE;
        if (_isWorker) {
          this.worker = new Worker(
            parseInt(process.env.OAM_CLUSTER_WORKER, 10) || 0,
            -1,
            process.pid,
          );
        }
      }
      fork(env) {
        if (_isWorker) {
          throw new Error("cluster.fork: cannot fork from a worker process");
        }
        const id = _nextId++;
        const scriptPath = process.argv[1];
        if (!scriptPath) {
          throw new Error("cluster.fork: no entry script (process.argv[1] is empty)");
        }
        const envObj = env || {};
        const worker = new Worker(id, -1, 0);
        this.workers[id] = worker;
        natives.clusterFork(scriptPath, String(id), envObj).then(
          (result) => {
            worker._handle = result.handle;
            worker.process.pid = result.pid;
            // Apply a kill requested before the handle was ready.
            if (worker._pendingKill) {
              natives.clusterWorkerKill(worker._handle, worker._pendingKill);
              worker._pendingKill = null;
            }
            process.nextTick(() => {
              worker.emit("online");
              this.emit("online", worker);
            });
            natives.clusterWorkerWait(result.handle).then(
              (exitResult) => {
                worker._dead = true;
                const code = exitResult && exitResult.code != null ? exitResult.code : null;
                const signal = exitResult && exitResult.signal != null ? exitResult.signal : null;
                process.nextTick(() => {
                  worker.emit("exit", code, signal);
                  this.emit("exit", worker, code, signal);
                  delete this.workers[id];
                });
              },
              (err) => {
                worker._dead = true;
                process.nextTick(() => {
                  worker.emit("error", err instanceof Error ? err : new Error(String(err)));
                  worker.emit("exit", 1, null);
                  this.emit("exit", worker, 1, null);
                  delete this.workers[id];
                });
              },
            );
          },
          (err) => {
            worker._dead = true;
            process.nextTick(() => {
              worker.emit("error", err instanceof Error ? err : new Error(String(err)));
              delete this.workers[id];
            });
          },
        );
        return worker;
      }
      setupMaster() {}
      setupPrimary() {}
      disconnect(cb) {
        for (const id of Object.keys(this.workers)) {
          this.workers[id].disconnect();
        }
        if (typeof cb === "function") queueMicrotask(cb);
        return this;
      }
    }
    return new Cluster();
  };

  // ------------------------------------------------------------------ dgram
  registry.factories.dgram = (natives) => {
    const EventEmitter = registry.get("events");

    // base64 decode helper (browser-compat atob is available in the snapshot)
    function b64ToBuffer(b64) {
      const raw = atob(b64);
      const buf = globalThis.Buffer.alloc(raw.length);
      for (let i = 0; i < raw.length; i++) buf[i] = raw.charCodeAt(i);
      return buf;
    }

    class Socket extends EventEmitter {
      constructor(type, listener) {
        super();
        this._type = type || "udp4";
        this._handle = null;
        this._bound = false;
        this._closed = false;
        this._recvLoop = false;
        this._address = { address: "0.0.0.0", family: "IPv4", port: 0 };
        if (typeof listener === "function") this.on("message", listener);
      }

      bind(...args) {
        let port = 0, address = "0.0.0.0", cb;
        if (typeof args[0] === "number") {
          port = args[0];
          if (typeof args[1] === "string") address = args[1];
          if (typeof args[args.length - 1] === "function") cb = args[args.length - 1];
        } else if (typeof args[0] === "object" && args[0] !== null) {
          const opts = args[0];
          port = opts.port || 0;
          address = opts.address || "0.0.0.0";
          if (typeof args[1] === "function") cb = args[1];
        } else if (typeof args[0] === "function") {
          cb = args[0];
        }

        if (cb) this.once("listening", cb);

        natives.udpBind(address, port).then((result) => {
          if (this._closed) return;
          this._handle = result.handle;
          this._bound = true;
          this._address = {
            address: result.address,
            port: result.port,
            family: result.family,
          };
          this.emit("listening");
          this._startRecv();
        }).catch((err) => {
          this.emit("error", err);
        });

        return this;
      }

      _startRecv() {
        if (this._recvLoop || this._closed) return;
        this._recvLoop = true;
        const loop = async () => {
          while (!this._closed && this._handle !== null) {
            try {
              const result = await natives.udpRecv(this._handle, 65536);
              if (result === undefined || this._closed) break;
              const msg = b64ToBuffer(result.data);
              this.emit("message", msg, result.rinfo);
            } catch (err) {
              if (!this._closed) this.emit("error", err);
              break;
            }
          }
          this._recvLoop = false;
        };
        loop();
      }

      send(msg, ...args) {
        // Signatures:
        //   send(msg, offset, length, port, address, callback)
        //   send(msg, port, address, callback)
        let offset, length, port, address, cb;

        if (typeof args[0] === "number" && typeof args[1] === "number" &&
            typeof args[2] === "number") {
          // send(msg, offset, length, port, address, callback)
          offset = args[0];
          length = args[1];
          port = args[2];
          address = typeof args[3] === "string" ? args[3] : "127.0.0.1";
          cb = typeof args[args.length - 1] === "function" ? args[args.length - 1] : undefined;
        } else {
          // send(msg, port, address, callback)
          port = args[0];
          address = typeof args[1] === "string" ? args[1] : "127.0.0.1";
          cb = typeof args[args.length - 1] === "function" ? args[args.length - 1] : undefined;
          offset = 0;
          length = undefined;
        }

        let data;
        if (typeof msg === "string") {
          data = globalThis.Buffer.from(msg, "utf8");
        } else if (msg instanceof Uint8Array) {
          data = msg;
        } else if (Array.isArray(msg)) {
          data = globalThis.Buffer.concat(msg.map((m) =>
            typeof m === "string" ? globalThis.Buffer.from(m, "utf8") : m
          ));
        } else {
          data = globalThis.Buffer.from(String(msg));
        }

        if (offset !== undefined && offset !== 0 || length !== undefined) {
          data = data.slice(offset || 0, length !== undefined ? (offset || 0) + length : undefined);
        }

        const doSend = () => {
          natives.udpSend(this._handle, data, String(address), port).then((result) => {
            if (cb) cb(null, result.bytesSent);
          }).catch((err) => {
            if (cb) cb(err);
            else this.emit("error", err);
          });
        };

        if (!this._bound) {
          // Auto-bind like Node does when sending without bind
          this.bind(0, () => doSend());
        } else {
          doSend();
        }
      }

      close(cb) {
        if (this._closed) return this;
        this._closed = true;
        if (this._handle !== null) {
          natives.udpClose(this._handle);
          this._handle = null;
        }
        this._bound = false;
        if (typeof cb === "function") this.once("close", cb);
        process.nextTick(() => this.emit("close"));
        return this;
      }

      address() {
        return Object.assign({}, this._address);
      }

      // Stubs for multicast/TTL options -- no-ops but don't throw
      addMembership() {}
      dropMembership() {}
      setBroadcast() {}
      setMulticastLoopback() {}
      setMulticastTTL() {}
      setTTL() {}
      setRecvBufferSize() {}
      setSendBufferSize() {}
      getRecvBufferSize() { return 65536; }
      getSendBufferSize() { return 65536; }

      ref() { return this; }
      unref() { return this; }
    }

    function createSocket(type, listener) {
      if (typeof type === "object") {
        listener = type.listener || listener;
        type = type.type;
      }
      return new Socket(type, listener);
    }

    return { createSocket, Socket };
  };

  // -------------------------------------------------------------------- dns
  registry.factories.dns = (natives) => {
    function lookup(hostname, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      if (typeof options === "number") options = { family: options };
      const opts = options || {};
      const family = opts.family || 0;
      const all = !!opts.all;

      natives.dnsLookup(String(hostname), family, all).then(
        (result) => {
          if (all) {
            callback(null, result);
          } else {
            callback(null, result.address, result.family);
          }
        },
        (err) => callback(err),
      );
    }

    function _resolveNative(hostname, rrtype, callback) {
      natives.dnsResolve(String(hostname), rrtype).then(
        (result) => callback(null, result),
        (err) => callback(err),
      );
    }

    function resolve(hostname, rrtype, callback) {
      if (typeof rrtype === "function") {
        callback = rrtype;
        rrtype = "A";
      }
      rrtype = (rrtype || "A").toUpperCase();
      _resolveNative(hostname, rrtype, callback);
    }

    function resolve4(hostname, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      natives.dnsResolve(String(hostname), "A").then(
        (results) => {
          if (options && options.ttl) {
            callback(null, results.map((r) => ({ address: r, ttl: 0 })));
          } else {
            callback(null, results);
          }
        },
        (err) => callback(err),
      );
    }

    function resolve6(hostname, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      natives.dnsResolve(String(hostname), "AAAA").then(
        (results) => {
          if (options && options.ttl) {
            callback(null, results.map((r) => ({ address: r, ttl: 0 })));
          } else {
            callback(null, results);
          }
        },
        (err) => callback(err),
      );
    }

    function resolveCname(hostname, callback) {
      _resolveNative(hostname, "CNAME", callback);
    }

    function resolveMx(hostname, callback) {
      _resolveNative(hostname, "MX", callback);
    }

    function resolveTxt(hostname, callback) {
      _resolveNative(hostname, "TXT", callback);
    }

    function resolveNs(hostname, callback) {
      _resolveNative(hostname, "NS", callback);
    }

    function resolveSrv(hostname, callback) {
      _resolveNative(hostname, "SRV", callback);
    }

    function resolveSoa(hostname, callback) {
      _resolveNative(hostname, "SOA", callback);
    }

    function resolvePtr(hostname, callback) {
      _resolveNative(hostname, "PTR", callback);
    }

    function resolveCaa(hostname, callback) {
      _resolveNative(hostname, "CAA", callback);
    }

    function resolveNaptr(hostname, callback) {
      _resolveNative(hostname, "NAPTR", callback);
    }

    function resolveAny(hostname, callback) {
      const err = Object.assign(
        new Error("dns.resolveAny is not supported by oam (deprecated in Node.js)"),
        { code: "ENOSYS" },
      );
      if (typeof callback === "function") queueMicrotask(() => callback(err));
      else throw err;
    }

    function reverse(ip, callback) {
      natives.dnsReverse(String(ip)).then(
        (result) => callback(null, result),
        (err) => callback(err),
      );
    }

    const promises = {
      lookup(hostname, options) {
        const opts = typeof options === "number" ? { family: options } : (options || {});
        const family = opts.family || 0;
        const all = !!opts.all;
        return natives.dnsLookup(String(hostname), family, all);
      },
      resolve(hostname, rrtype) {
        rrtype = (rrtype || "A").toUpperCase();
        return natives.dnsResolve(String(hostname), rrtype);
      },
      resolve4(hostname, options) {
        return natives.dnsResolve(String(hostname), "A").then((r) => {
          if (options && options.ttl) return r.map((x) => ({ address: x, ttl: 0 }));
          return r;
        });
      },
      resolve6(hostname, options) {
        return natives.dnsResolve(String(hostname), "AAAA").then((r) => {
          if (options && options.ttl) return r.map((x) => ({ address: x, ttl: 0 }));
          return r;
        });
      },
      resolveCname(hostname) { return natives.dnsResolve(String(hostname), "CNAME"); },
      resolveMx(hostname) { return natives.dnsResolve(String(hostname), "MX"); },
      resolveTxt(hostname) { return natives.dnsResolve(String(hostname), "TXT"); },
      resolveNs(hostname) { return natives.dnsResolve(String(hostname), "NS"); },
      resolveSrv(hostname) { return natives.dnsResolve(String(hostname), "SRV"); },
      resolveSoa(hostname) { return natives.dnsResolve(String(hostname), "SOA"); },
      resolvePtr(hostname) { return natives.dnsResolve(String(hostname), "PTR"); },
      resolveCaa(hostname) { return natives.dnsResolve(String(hostname), "CAA"); },
      resolveNaptr(hostname) { return natives.dnsResolve(String(hostname), "NAPTR"); },
      resolveAny() {
        return Promise.reject(Object.assign(
          new Error("dns.resolveAny is not supported by oam (deprecated in Node.js)"),
          { code: "ENOSYS" },
        ));
      },
      reverse(ip) { return natives.dnsReverse(String(ip)); },
    };

    class Resolver {
      constructor() { this._servers = []; }
      resolve(hostname, rrtype, cb) {
        if (typeof rrtype === "function") { cb = rrtype; rrtype = "A"; }
        resolve(hostname, rrtype, cb);
      }
      resolve4(hostname, opts, cb) { resolve4(hostname, opts, cb); }
      resolve6(hostname, opts, cb) { resolve6(hostname, opts, cb); }
      resolveCname(hostname, cb) { resolveCname(hostname, cb); }
      resolveMx(hostname, cb) { resolveMx(hostname, cb); }
      resolveTxt(hostname, cb) { resolveTxt(hostname, cb); }
      resolveNs(hostname, cb) { resolveNs(hostname, cb); }
      resolveSrv(hostname, cb) { resolveSrv(hostname, cb); }
      resolveSoa(hostname, cb) { resolveSoa(hostname, cb); }
      resolvePtr(hostname, cb) { resolvePtr(hostname, cb); }
      resolveCaa(hostname, cb) { resolveCaa(hostname, cb); }
      resolveNaptr(hostname, cb) { resolveNaptr(hostname, cb); }
      reverse(ip, cb) { reverse(ip, cb); }
      cancel() {}
      getServers() { return this._servers.slice(); }
      setServers(servers) { this._servers = (servers || []).slice(); }
    }

    const ADDRCONFIG = 0;
    const V4MAPPED = 0;
    const ALL = 0;

    return {
      lookup,
      resolve,
      resolve4,
      resolve6,
      resolveCname,
      resolveMx,
      resolveTxt,
      resolveNs,
      resolveSrv,
      resolveSoa,
      resolvePtr,
      resolveCaa,
      resolveNaptr,
      resolveAny,
      reverse,
      Resolver,
      promises,
      setDefaultResultOrder() {},
      setServers() {},
      getServers: () => [],
      ADDRCONFIG,
      V4MAPPED,
      ALL,
      NODATA: "NODATA",
      FORMERR: "FORMERR",
      SERVFAIL: "SERVFAIL",
      NOTFOUND: "NOTFOUND",
      NOTIMP: "NOTIMP",
      REFUSED: "REFUSED",
      BADQUERY: "BADQUERY",
      BADNAME: "BADNAME",
      BADFAMILY: "BADFAMILY",
      BADRESP: "BADRESP",
      CONNREFUSED: "CONNREFUSED",
      TIMEOUT: "TIMEOUT",
      EOF: "EOF",
      FILE: "FILE",
      NOMEM: "NOMEM",
      DESTRUCTION: "DESTRUCTION",
      BADSTR: "BADSTR",
      BADFLAGS: "BADFLAGS",
      NONAME: "NONAME",
      BADHINTS: "BADHINTS",
      NOTINITIALIZED: "NOTINITIALIZED",
      LOADIPHLPAPI: "LOADIPHLPAPI",
      ADDRGETNETWORKPARAMS: "ADDRGETNETWORKPARAMS",
      CANCELLED: "CANCELLED",
    };
  };

  registry.factories["dns/promises"] = () => registry.get("dns").promises;

  // internal/errors -- some packages (readable-stream, undici) import this
  registry.factories["internal/errors"] = () => ({ codes });

  // ------------------------------------------------------------------ http2
  registry.factories.http2 = (natives) => {
    const EventEmitter = registry.get("events");
    const { Duplex } = registry.get("stream");

    class ServerHttp2Stream extends Duplex {
      constructor(requestId, inHeaders) {
        super({ allowHalfOpen: true });
        this._requestId = requestId;
        this._streamId = null;
        this._ended = false;
        this._responded = false;
        this._chain = Promise.resolve();
        this.sentHeaders = null;
        this._inHeaders = inHeaders;
        this.id = requestId;
      }
      respond(headers, options) {
        if (this._responded) return;
        this._responded = true;
        var status = 200;
        var outPairs = [];
        if (headers) {
          var keys = Object.keys(headers);
          for (var i = 0; i < keys.length; i++) {
            var k = keys[i];
            if (k === ":status") {
              status = Number(headers[k]);
            } else if (k.charAt(0) !== ":") {
              outPairs.push([k.toLowerCase(), String(headers[k])]);
            }
          }
        }
        this.sentHeaders = headers || {};
        var endStream = options && options.endStream;
        if (endStream) {
          this._ended = true;
          natives.httpRespond(
            this._requestId,
            status,
            JSON.stringify(outPairs),
            new Uint8Array(0),
          );
          var self = this;
          queueMicrotask(function() { self.emit("finish"); self.push(null); });
        } else {
          this._streamId = natives.httpRespondStream(
            this._requestId,
            status,
            JSON.stringify(outPairs),
          );
        }
      }
      additionalHeaders() {}
      _write(chunk, encoding, callback) {
        if (this._ended) { callback(); return; }
        if (!this._responded) {
          this.respond({ ":status": 200 });
        }
        var bytes;
        if (typeof chunk === "string") {
          bytes = globalThis.Buffer.from(chunk, encoding || "utf8");
        } else {
          bytes = chunk;
        }
        if (this._streamId === null) { callback(); return; }
        var streamId = this._streamId;
        this._chain = this._chain
          .then(function() { return natives.httpBodyPush(streamId, bytes); })
          .then(function() { callback(); }, function(err) { callback(err); });
      }
      _final(callback) {
        if (this._ended) { callback(); return; }
        this._ended = true;
        if (!this._responded) {
          this.respond({ ":status": 200 });
        }
        if (this._streamId !== null) {
          var streamId = this._streamId;
          var self = this;
          this._chain = this._chain.then(function() {
            natives.httpBodyEnd(streamId);
            self.emit("finish");
            callback();
          });
        } else {
          callback();
        }
      }
      _read() {
        if (!this._bodyPushed) {
          this._bodyPushed = true;
          var body = natives.httpRequestBody(this._requestId);
          if (body && body.length > 0) {
            this.push(globalThis.Buffer.from(body.buffer, body.byteOffset, body.length));
          }
          this.push(null);
        }
      }
      close(code, callback) {
        if (typeof code === "function") { callback = code; code = 0; }
        this.end();
        if (callback) this.once("close", callback);
      }
    }

    class Http2Server extends EventEmitter {
      constructor(options, handler) {
        super();
        if (typeof options === "function") {
          handler = options;
          options = {};
        }
        this._options = options || {};
        if (handler) this.on("stream", handler);
        this._serverId = null;
        this._port = null;
        this._host = null;
        this.listening = false;
      }
      listen(port, host, callback) {
        if (typeof port === "object" && port !== null) {
          callback = host;
          host = port.host;
          port = port.port;
        }
        if (typeof host === "function") {
          callback = host;
          host = undefined;
        }
        if (typeof callback === "function") this.once("listening", callback);
        var hostname = host || "127.0.0.1";
        var self = this;
        natives.http2Serve(hostname, port || 0).then(
          function(bound) {
            self._serverId = bound.serverId;
            self._port = bound.port;
            self._host = hostname;
            self.listening = true;
            self.emit("listening");
            (async function() {
              for (;;) {
                var meta = await natives.httpAccept(bound.serverId);
                if (meta === undefined) break;
                var hdrs = {};
                for (var i = 0; i < meta.headers.length; i++) {
                  var key = meta.headers[i][0].toLowerCase();
                  hdrs[key] = meta.headers[i][1];
                }
                hdrs[":method"] = meta.method;
                hdrs[":path"] = meta.uri;
                hdrs[":scheme"] = "http";
                var stream = new ServerHttp2Stream(meta.requestId, hdrs);
                self.emit("stream", stream, hdrs);
              }
              self.emit("close");
            })();
          },
          function(err) { self.emit("error", err); },
        );
        return this;
      }
      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }
      close(callback) {
        if (this._serverId !== null) {
          natives.httpClose(this._serverId);
          this.listening = false;
        }
        if (callback) this.once("close", callback);
        return this;
      }
      setTimeout() { return this; }
    }

    function createServer(options, handler) {
      return new Http2Server(options, handler);
    }

    function createSecureServer(options, handler) {
      return createServer(options, handler);
    }

    class ClientHttp2Stream extends Duplex {
      constructor(session, headers) {
        super({ allowHalfOpen: true });
        this._session = session;
        this._reqHeaders = headers;
        this._bodyChunks = [];
        this._ended = false;
        this.sentHeaders = headers;
        this.id = 1;
        this._responseEmitted = false;
      }
      _write(chunk, encoding, callback) {
        if (typeof chunk === "string") {
          this._bodyChunks.push(globalThis.Buffer.from(chunk, encoding || "utf8"));
        } else {
          this._bodyChunks.push(chunk);
        }
        callback();
      }
      _final(callback) {
        this._ended = true;
        this._doFetch(callback);
      }
      _read() {}
      _doFetch(callback) {
        var self = this;
        var method = this._reqHeaders[":method"] || "GET";
        var path = this._reqHeaders[":path"] || "/";
        var scheme = this._reqHeaders[":scheme"] || "http";
        var authority = this._reqHeaders[":authority"] || this._session._authority;
        var url = scheme + "://" + authority + path;
        var fetchHeaders = {};
        var keys = Object.keys(this._reqHeaders);
        for (var i = 0; i < keys.length; i++) {
          if (keys[i].charAt(0) !== ":") {
            fetchHeaders[keys[i]] = this._reqHeaders[keys[i]];
          }
        }
        var bodyData = null;
        if (this._bodyChunks.length > 0) {
          var totalLen = 0;
          for (var bi = 0; bi < this._bodyChunks.length; bi++) totalLen += this._bodyChunks[bi].length;
          var merged = new Uint8Array(totalLen);
          var boff = 0;
          for (var bi = 0; bi < this._bodyChunks.length; bi++) {
            merged.set(this._bodyChunks[bi], boff);
            boff += this._bodyChunks[bi].length;
          }
          bodyData = merged;
        }
        var fetchOpts = { method: method, headers: fetchHeaders };
        if (bodyData && method !== "GET" && method !== "HEAD") {
          fetchOpts.body = bodyData;
        }
        globalThis.fetch(url, fetchOpts).then(
          function(resp) {
            var respHeaders = { ":status": resp.status };
            resp.headers.forEach(function(value, name) {
              respHeaders[name.toLowerCase()] = value;
            });
            self.emit("response", respHeaders, 0);
            resp.arrayBuffer().then(function(ab) {
              if (ab.byteLength > 0) {
                self.push(globalThis.Buffer.from(ab));
              }
              self.push(null);
              callback();
            }, function(err) {
              self.destroy(err);
              callback(err);
            });
          },
          function(err) {
            self.emit("error", typeof err === "string" ? new Error(err) : err);
            callback(err);
          },
        );
      }
      close(code, callback) {
        if (typeof code === "function") { callback = code; code = 0; }
        this.end();
        if (callback) this.once("close", callback);
      }
    }

    class ClientHttp2Session extends EventEmitter {
      constructor(authority) {
        super();
        this._authority = authority.replace(/^https?:\/\//, "");
        this._scheme = authority.startsWith("https") ? "https" : "http";
        this._closed = false;
        this._destroyed = false;
        this.socket = {};
        this.alpnProtocol = "h2c";
        var self = this;
        process.nextTick(function() { self.emit("connect", self); });
      }
      request(headers) {
        if (this._closed || this._destroyed) {
          throw new Error("Session is closed");
        }
        var merged = {};
        merged[":method"] = "GET";
        merged[":path"] = "/";
        merged[":scheme"] = this._scheme;
        merged[":authority"] = this._authority;
        if (headers) {
          var keys = Object.keys(headers);
          for (var i = 0; i < keys.length; i++) {
            merged[keys[i]] = headers[keys[i]];
          }
        }
        var stream = new ClientHttp2Stream(this, merged);
        return stream;
      }
      close(callback) {
        this._closed = true;
        if (callback) this.once("close", callback);
        var self = this;
        process.nextTick(function() { self.emit("close"); });
      }
      destroy(err) {
        this._destroyed = true;
        this._closed = true;
        if (err) this.emit("error", err);
        var self = this;
        process.nextTick(function() { self.emit("close"); });
      }
      ref() { return this; }
      unref() { return this; }
      ping(payload, callback) {
        if (typeof payload === "function") { callback = payload; payload = undefined; }
        if (callback) process.nextTick(function() { callback(null, 0, globalThis.Buffer.alloc(8)); });
      }
      get closed() { return this._closed; }
      get destroyed() { return this._destroyed; }
    }

    function connect(authority, options) {
      if (typeof options === "function") options = {};
      return new ClientHttp2Session(authority);
    }

    return {
      createServer,
      createSecureServer,
      connect,
      constants: {
        NGHTTP2_SESSION_SERVER: 0,
        NGHTTP2_SESSION_CLIENT: 1,
        NGHTTP2_STREAM_STATE_IDLE: 1,
        NGHTTP2_STREAM_STATE_OPEN: 2,
        NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3,
        NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6,
        NGHTTP2_STREAM_STATE_CLOSED: 7,
        NGHTTP2_NO_ERROR: 0,
        NGHTTP2_PROTOCOL_ERROR: 1,
        NGHTTP2_INTERNAL_ERROR: 2,
        NGHTTP2_FLOW_CONTROL_ERROR: 3,
        NGHTTP2_SETTINGS_TIMEOUT: 4,
        NGHTTP2_STREAM_CLOSED: 5,
        NGHTTP2_FRAME_SIZE_ERROR: 6,
        NGHTTP2_REFUSED_STREAM: 7,
        NGHTTP2_CANCEL: 8,
        NGHTTP2_COMPRESSION_ERROR: 9,
        NGHTTP2_CONNECT_ERROR: 10,
        NGHTTP2_ENHANCE_YOUR_CALM: 11,
        NGHTTP2_INADEQUATE_SECURITY: 12,
        NGHTTP2_HTTP_1_1_REQUIRED: 13,
        NGHTTP2_DEFAULT_WEIGHT: 16,
        HTTP2_HEADER_STATUS: ":status",
        HTTP2_HEADER_METHOD: ":method",
        HTTP2_HEADER_AUTHORITY: ":authority",
        HTTP2_HEADER_SCHEME: ":scheme",
        HTTP2_HEADER_PATH: ":path",
        HTTP2_HEADER_CONTENT_TYPE: "content-type",
        HTTP2_HEADER_CONTENT_LENGTH: "content-length",
        HTTP2_HEADER_ACCEPT_ENCODING: "accept-encoding",
        HTTP2_METHOD_GET: "GET",
        HTTP2_METHOD_POST: "POST",
        HTTP_STATUS_OK: 200,
        HTTP_STATUS_NOT_FOUND: 404,
        HTTP_STATUS_INTERNAL_SERVER_ERROR: 500,
      },
      sensitiveHeaders: Symbol.for("nodejs.http2.sensitiveHeaders"),
    };
  };

  // ------------------------------------------------------------------- tls
  registry.factories.tls = (natives) => {
    const EventEmitter = registry.get("events");
    const { Duplex } = registry.get("stream");

    class TLSSocket extends Duplex {
      constructor(socket, options) {
        super();
        this.encrypted = true;
        this.authorized = false;
        this.authorizationError = null;
        this.alpnProtocol = false;
        // Match net.Socket / Node semantics: when the peer half-closes (our
        // readable hits EOF), auto-end our write side unless allowHalfOpen.
        // Without this the peer's parked read never sees a FIN, so the event
        // loop (which exits on inflight==0) never drains and the process hangs
        // at exit. oam's Duplex does NOT do this automatically (net.Socket
        // does it by hand in _readLoop), so TLSSocket must too.
        this.allowHalfOpen = (options && options.allowHalfOpen) || false;
        this._handle = null;
        this._reading = false;
        this._protocol = null;
        this._cipher = null;
      }
      // On readable EOF, half-close our write side (sends FIN) so the peer's
      // read drains -- mirrors net.Socket._readLoop's allowHalfOpen branch.
      _onReadEof() {
        this.push(null);
        if (!this.allowHalfOpen) {
          try {
            this.end();
          } catch (_) {
            /* already ending/ended */
          }
        }
      }
      _kickRead() {
        // If a consumer attached 'data' (or otherwise started flowing) BEFORE
        // the handle was ready, the initial _read no-op'd. Now that the handle
        // exists, re-pull so data actually flows. No-ops if already reading.
        if (this._handle !== null && !this._reading &&
            (this.readableFlowing === true ||
             (typeof this.listenerCount === "function" && this.listenerCount("data") > 0))) {
          this._read(65536);
        }
      }
      _read(size) {
        if (this._handle === null || this._reading) return;
        this._reading = true;
        natives.tlsRead(this._handle, size || 65536).then(
          (data) => {
            this._reading = false;
            if (data === undefined) {
              this._onReadEof();
            } else {
              this.push(new globalThis.Buffer(data.buffer, data.byteOffset, data.length));
            }
          },
          (err) => {
            this._reading = false;
            var info = typeof err === "string" ? err : ((err && err.code) || "") + " " + ((err && err.message) || "");
            // A peer that closes/resets mid-read should end the stream, not
            // crash an otherwise-healthy server with an unhandled 'error'.
            // Windows surfaces a peer close as WSAECONNRESET (os error 10054);
            // rustls reports an abrupt TLS close (no close_notify) as EIO with
            // a "peer closed connection without sending TLS close_notify"
            // message. Real HTTP-body truncation is still caught at the HTTP
            // layer (Content-Length / chunked checks), so EOF here is safe.
            if (/ECONNRESET|ECONNABORTED|EPIPE|forcibly closed|10054|reset by peer|close_notify|peer closed|unexpected eof/i.test(info)) {
              this._onReadEof();
              return;
            }
            this.destroy(typeof err === "string" ? new Error(err) : err);
          },
        );
      }
      _write(chunk, encoding, callback) {
        if (this._handle === null) {
          callback(new Error("TLSSocket: not connected"));
          return;
        }
        var data = typeof chunk === "string"
          ? globalThis.Buffer.from(chunk, encoding) : chunk;
        natives.tlsWrite(this._handle, data).then(
          () => callback(),
          (err) => callback(typeof err === "string" ? new Error(err) : err),
        );
      }
      _final(callback) {
        if (this._handle !== null) {
          natives.tlsShutdown(this._handle).then(() => callback(), () => callback());
        } else {
          callback();
        }
      }
      _destroy(err, callback) {
        if (this._handle !== null) {
          natives.tlsClose(this._handle);
          this._handle = null;
        }
        callback(err);
      }
      getPeerCertificate() { return {}; }
      getProtocol() { return this._protocol || null; }
      getCipher() {
        return this._cipher ? { name: this._cipher, standardName: this._cipher, version: this._protocol } : null;
      }
      setMaxSendFragment() { return true; }
      enableTrace() {}
      get remoteAddress() { return this._remoteAddress || undefined; }
      get remotePort() { return this._remotePort || undefined; }
    }

    function connect(optionsOrPort, hostOrCb, cb) {
      var options, callback;
      if (typeof optionsOrPort === "number") {
        options = { port: optionsOrPort, host: typeof hostOrCb === "string" ? hostOrCb : "localhost" };
        callback = typeof hostOrCb === "function" ? hostOrCb : cb;
      } else {
        options = optionsOrPort || {};
        callback = typeof hostOrCb === "function" ? hostOrCb : undefined;
      }
      var host = options.host || options.hostname || "localhost";
      var port = options.port || 443;
      var serverName = options.servername || host;
      var ca = options.ca != null ? String(options.ca) : undefined;
      var cert = options.cert != null ? String(options.cert) : undefined;
      var key = options.key != null ? String(options.key) : undefined;
      var rejectUnauthorized = options.rejectUnauthorized !== false;

      var socket = new TLSSocket(null, options);
      socket._remoteAddress = host;
      socket._remotePort = port;
      if (callback) socket.once("secureConnect", callback);

      natives.tlsConnect(host, port, serverName, ca, rejectUnauthorized, cert, key).then(
        (info) => {
          socket._handle = info.handle;
          socket.authorized = info.authorized;
          socket._protocol = info.protocol;
          socket._cipher = info.cipher;
          socket.alpnProtocol = info.alpnProtocol || false;
          socket.emit("secureConnect");
          socket._kickRead();
        },
        (err) => {
          socket.destroy(typeof err === "string" ? new Error(err) : err);
        },
      );

      return socket;
    }

    function createSecureContext(options) {
      return Object.assign({}, options);
    }

    class Server extends EventEmitter {
      constructor(options, connectionListener) {
        super();
        if (typeof options === "function") {
          connectionListener = options;
          options = {};
        }
        this._options = options || {};
        if (connectionListener) this.on("secureConnection", connectionListener);
        this._serverId = null;
        this._port = null;
        this._host = null;
        this.listening = false;
        this._closed = false;
      }
      listen(port, host, callback) {
        if (typeof port === "object" && port !== null) {
          callback = host;
          host = port.host;
          port = port.port;
        }
        if (typeof host === "function") {
          callback = host;
          host = undefined;
        }
        if (typeof callback === "function") this.once("listening", callback);
        var hostname = host || "0.0.0.0";
        var certPem = this._options.cert instanceof Uint8Array
          ? new TextDecoder().decode(this._options.cert) : String(this._options.cert || "");
        var keyPem = this._options.key instanceof Uint8Array
          ? new TextDecoder().decode(this._options.key) : String(this._options.key || "");

        natives.tcpListen(hostname, port || 0).then(
          (bound) => {
            this._serverId = bound.serverId;
            this._port = bound.port;
            this._host = bound.hostname || hostname;
            this.listening = true;
            this.emit("listening");
            this._acceptLoop(bound.serverId, certPem, keyPem);
          },
          (err) => this.emit("error", typeof err === "string" ? new Error(err) : err),
        );
        return this;
      }
      _acceptLoop(serverId, certPem, keyPem) {
        (async () => {
          while (!this._closed) {
            var accepted;
            try {
              accepted = await natives.tcpAccept(serverId);
            } catch (e) {
              // A transient accept() error (EMFILE/ECONNABORTED) rejects but the
              // listener stays bound -- surface it and keep serving rather than
              // tearing the server down. Brief backoff avoids a hot spin.
              if (this._closed) break;
              this.emit("tlsClientError", typeof e === "string" ? new Error(e) : e, null);
              await new Promise((r) => setTimeout(r, 10));
              continue;
            }
            if (accepted === undefined) break;
            var tcpHandle = accepted.handle;
            try {
              var info = await natives.tlsAcceptWrap(tcpHandle, certPem, keyPem);
              var socket = new TLSSocket(null, {});
              socket._handle = info.handle;
              socket.authorized = true;
              socket._protocol = info.protocol;
              socket._cipher = info.cipher;
              socket.alpnProtocol = info.alpnProtocol || false;
              socket.encrypted = true;
              if (accepted.remoteAddr) {
                socket._remoteAddress = accepted.remoteAddr.address;
                socket._remotePort = accepted.remoteAddr.port;
              }
              this.emit("secureConnection", socket);
              socket._kickRead();
            } catch (e) {
              this.emit("tlsClientError", typeof e === "string" ? new Error(e) : e, null);
            }
          }
          this.emit("close");
        })();
      }
      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }
      close(callback) {
        this._closed = true;
        if (this._serverId !== null) {
          natives.tcpServerClose(this._serverId);
          this.listening = false;
        }
        if (callback) this.once("close", callback);
        return this;
      }
      getTicketKeys() { return Buffer.alloc(48); }
      setTicketKeys() {}
    }

    function createServer(options, connectionListener) {
      return new Server(options, connectionListener);
    }

    return {
      connect,
      createServer,
      createSecureContext,
      Server,
      TLSSocket,
      DEFAULT_ECDH_CURVE: "auto",
      DEFAULT_MAX_VERSION: "TLSv1.3",
      DEFAULT_MIN_VERSION: "TLSv1.2",
      rootCertificates: [],
      getCiphers: () => ["TLS_AES_128_GCM_SHA256", "TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256"],
      checkServerIdentity: () => undefined,
    };
  };

  // --------------------------------------------------------- worker_threads
  // MessageChannel / MessagePort are fully functional (in-process). Worker
  // class has native-backed thread spawning via workerNew/workerRecvMessage/
  // workerPostMessage/workerTerminate ops.
  registry.factories.worker_threads = () => {
    const EventEmitter = registry.get("events");

    // SHARE_ENV sentinel -- when passed as opts.env, workers share the
    // process environment (which they do by default in oam anyway).
    const SHARE_ENV = Symbol("nodejs.worker_threads.SHARE_ENV");

    // Module-level environment data store shared across threads in-process.
    const _envData = new Map();

    // WeakSet for markAsUntransferable -- marks objects so transfer attempts
    // can check (actual enforcement is future; the mark is the contract).
    const _untransferableSet = new WeakSet();

    class MessagePort extends EventEmitter {
      constructor() { super(); this._twin = null; this._active = true; }
      postMessage(data) {
        const twin = this._twin;
        if (twin && twin._active) {
          const cloned = (typeof data === "object" && data !== null) ? structuredClone(data) : data;
          queueMicrotask(() => twin.emit("message", cloned));
        }
      }
      start() { return this; }
      close() { this._active = false; this.emit("close"); }
      ref() { return this; }
      unref() { return this; }
      addEventListener(type, fn) { this.on(type, fn); }
      removeEventListener(type, fn) { this.off(type, fn); }
    }
    class MessageChannel {
      constructor() {
        this.port1 = new MessagePort();
        this.port2 = new MessagePort();
        this.port1._twin = this.port2;
        this.port2._twin = this.port1;
      }
    }
    const natives = globalThis.__oam.node;
    const pathMod = registry.get("path");
    class Worker extends EventEmitter {
      constructor(filename, opts = {}) {
        super();
        if (typeof filename !== "string") throw new TypeError("Worker requires a filename");
        const resolved = pathMod.resolve(filename);
        const wd = opts.workerData !== undefined ? JSON.stringify(opts.workerData) : null;

        // Store worker name (Node >=12.11 option)
        this.name = opts.name || "";

        // Note SHARE_ENV usage (workers share process env by default in oam)
        this._shareEnv = opts.env === SHARE_ENV;

        const result = natives.workerNew(resolved, wd);
        this._workerId = result.workerId;
        this.threadId = result.threadId;

        // Expose performance reference (mirrors Node behavior)
        this.performance = globalThis.performance;

        this._recvLoop();

        // Emit 'online' asynchronously after construction, matching Node
        queueMicrotask(() => this.emit("online"));
      }
      async _recvLoop() {
        while (true) {
          let raw;
          try { raw = await natives.workerRecvMessage(this._workerId); } catch { break; }
          if (raw === undefined) { this.emit("exit", 0); break; }
          if (raw instanceof Uint8Array) {
            const text = new TextDecoder().decode(raw);
            try {
              const data = JSON.parse(text);
              this.emit("message", data);
            } catch {
              this.emit("message", text);
            }
          } else if (typeof raw === "object" && raw !== null) {
            if (raw.type === "error") { this.emit("error", new Error(raw.message)); }
            if (raw.type === "exit") { this.emit("exit", raw.code); break; }
          }
        }
      }
      postMessage(data) {
        natives.workerPostMessage(this._workerId, JSON.stringify(data));
      }
      terminate() {
        natives.workerTerminate(this._workerId);
        return Promise.resolve(0);
      }
      ref() { return this; }
      unref() { return this; }
      getHeapSnapshot() { return Promise.resolve(new Uint8Array(0)); }
    }

    // Symbol.dispose support for the 'using' keyword
    if (typeof Symbol.dispose !== "undefined") {
      Worker.prototype[Symbol.dispose] = function() { this.terminate(); };
    }

    function getEnvironmentData(key) {
      const val = _envData.get(key);
      if (val === undefined) return undefined;
      if (val === null || typeof val !== "object") return val;
      return structuredClone(val);
    }

    function setEnvironmentData(key, value) {
      if (value === undefined) {
        _envData.delete(key);
      } else {
        _envData.set(key, (typeof value === "object" && value !== null) ? structuredClone(value) : value);
      }
    }

    function markAsUntransferable(obj) {
      if (typeof obj !== "object" || obj === null) {
        throw new TypeError("markAsUntransferable expects an object");
      }
      _untransferableSet.add(obj);
    }

    const isMain = natives.workerIsMainThread();
    const threadId = natives.workerThreadId();
    const workerData = natives.workerGetData();

    let parentPort = null;
    if (!isMain) {
      parentPort = new MessagePort();
      parentPort.postMessage = function postMessage(data) {
        natives.parentPortPostMessage(JSON.stringify(data));
      };
      (async () => {
        while (true) {
          let raw;
          try { raw = await natives.parentPortRecvMessage(); } catch { break; }
          if (raw === undefined) break;
          if (raw instanceof Uint8Array) {
            const text = new TextDecoder().decode(raw);
            try { parentPort.emit("message", JSON.parse(text)); }
            catch { parentPort.emit("message", text); }
          }
        }
      })();
    }

    const _bc = typeof globalThis.BroadcastChannel !== "undefined" ? globalThis.BroadcastChannel : null;
    return {
      isMainThread: isMain,
      parentPort,
      workerData,
      threadId,
      resourceLimits: {},
      SHARE_ENV,
      MessageChannel,
      MessagePort,
      Worker,
      BroadcastChannel: _bc || class BroadcastChannel { constructor() { throw new Error("BroadcastChannel not available"); } },
      receiveMessageOnPort: () => undefined,
      markAsUntransferable,
      moveMessagePortToContext: () => { throw new Error("moveMessagePortToContext is not supported in oam"); },
      getEnvironmentData,
      setEnvironmentData,
    };
  };

  // ------------------------------------------------------------- node:test
  // Node's built-in test runner (focused subset): describe/suite/it/test +
  // before/after/beforeEach/afterEach + a minimal TestContext. Auto-runs the
  // registered tests after the file finishes evaluating (scheduled via
  // queueMicrotask, which runs once the synchronous top-level registration is
  // done) and sets process.exitCode = 1 on any failure -- which the harness
  // oracle (exit 0 == pass) and the natural-'exit' path then surface. TAP-lite
  // output to stdout. Prefix-only: only require('node:test') resolves here.
  registry.factories["test"] = () => {
    const assert = registry.get("assert");
    const log = (s) => globalThis.console.log(s);

    const root = makeTestSuite("", null);
    let current = root;
    let scheduled = false;
    let hasOnly = false;
    let passed = 0;
    let failed = 0;
    let skipped = 0;
    let counter = 0;

    function makeTestSuite(name, parent) {
      return {
        name,
        parent,
        children: [],
        before: [],
        after: [],
        beforeEach: [],
        afterEach: [],
        mode: "normal",
      };
    }
    function lineage(s) {
      const c = [];
      for (; s; s = s.parent) c.unshift(s);
      return c;
    }
    function inheritedSkip(s) {
      for (; s; s = s.parent) if (s.mode === "skip") return true;
      return false;
    }
    function hasOnlyAncestor(s) {
      for (; s; s = s.parent) if (s.mode === "only") return true;
      return false;
    }

    // Node's (name?, options?, fn?) overload soup, normalized.
    function norm(name, options, fn) {
      if (typeof name === "function") {
        fn = name;
        options = undefined;
        name = fn.name || "<anonymous>";
      } else if (typeof options === "function") {
        fn = options;
        options = undefined;
      }
      if (typeof name !== "string") name = (fn && fn.name) || "<anonymous>";
      return { name, options: options || {}, fn };
    }

    function schedule() {
      if (scheduled) return;
      scheduled = true;
      // Runs after the file's synchronous top-level registration completes.
      queueMicrotask(() => {
        runRoot();
      });
    }

    function addSuite(name, options, fn, forced) {
      const a = norm(name, options, fn);
      const s = makeTestSuite(a.name, current);
      if (forced) s.mode = forced;
      else if (a.options.only) {
        s.mode = "only";
        hasOnly = true;
      } else if (a.options.skip || a.options.todo) s.mode = "skip";
      current.children.push({ kind: "suite", suite: s });
      const prev = current;
      current = s;
      try {
        if (a.fn) a.fn();
      } finally {
        current = prev;
      }
      schedule();
    }

    function addTest(name, options, fn, forced) {
      const a = norm(name, options, fn);
      let mode = forced || "normal";
      if (!forced) {
        if (a.options.only) {
          mode = "only";
          hasOnly = true;
        } else if (a.options.todo) mode = "todo";
        else if (a.options.skip) mode = "skip";
      }
      const entry = {
        kind: "test",
        name: a.name,
        fn: a.fn,
        mode,
        suite: current,
        resolve: null,
      };
      current.children.push(entry);
      schedule();
      // Thenable resolving when this test finishes (covers `await test(...)`).
      return new Promise((res) => {
        entry.resolve = res;
      });
    }

    function describe(n, o, f) {
      return addSuite(n, o, f);
    }
    describe.skip = (n, o, f) => addSuite(n, o, f, "skip");
    describe.todo = (n, o, f) => addSuite(n, o, f, "skip");
    describe.only = (n, o, f) => {
      hasOnly = true;
      return addSuite(n, o, f, "only");
    };
    const suite = describe;

    function test(n, o, f) {
      return addTest(n, o, f);
    }
    test.skip = (n, o, f) => addTest(n, o, f, "skip");
    test.todo = (n, o, f) => addTest(n, o, f, "todo");
    test.only = (n, o, f) => {
      hasOnly = true;
      return addTest(n, o, f, "only");
    };
    const it = test;

    const before = (fn) => current.before.push(fn);
    const after = (fn) => current.after.push(fn);
    const beforeEach = (fn) => current.beforeEach.push(fn);
    const afterEach = (fn) => current.afterEach.push(fn);

    function diag(e) {
      const msg = e instanceof Error ? e.stack || e.message : String(e);
      log("  ---");
      for (const line of String(msg).split("\n")) log("  " + line);
      log("  ...");
    }

    function makeContext(name) {
      const ctx = {
        name,
        assert,
        diagnostic: (msg) => log("# " + msg),
        skip(msg) {
          ctx._skipped = msg ?? true;
        },
        todo(msg) {
          ctx._todo = msg ?? true;
        },
        before,
        after,
        beforeEach,
        afterEach,
        signal: undefined,
        runOnly() {},
        // Subtests run inline within the parent test, awaited.
        test(n, o, f) {
          return runSubtest(norm(n, o, f));
        },
      };
      return ctx;
    }

    async function runSubtest(a) {
      const id = ++counter;
      if (a.options.skip) {
        skipped++;
        log(`ok ${id} - ${a.name} # SKIP`);
        return;
      }
      if (a.options.todo) {
        log(`ok ${id} - ${a.name} # TODO`);
        return;
      }
      const ctx = makeContext(a.name);
      try {
        await a.fn?.(ctx);
        if (ctx._skipped) {
          skipped++;
          log(`ok ${id} - ${a.name} # SKIP`);
        } else {
          passed++;
          log(`ok ${id} - ${a.name}`);
        }
      } catch (e) {
        failed++;
        log(`not ok ${id} - ${a.name}`);
        diag(e);
      }
    }

    async function runTestEntry(entry) {
      const id = ++counter;
      if (entry.mode === "todo") {
        log(`ok ${id} - ${entry.name} # TODO`);
        entry.resolve?.();
        return;
      }
      const skip =
        entry.mode === "skip" ||
        inheritedSkip(entry.suite) ||
        (hasOnly && entry.mode !== "only" && !hasOnlyAncestor(entry.suite));
      if (skip) {
        skipped++;
        log(`ok ${id} - ${entry.name} # SKIP`);
        entry.resolve?.();
        return;
      }
      const ctx = makeContext(entry.name);
      let err = null;
      try {
        for (const s of lineage(entry.suite)) for (const h of s.beforeEach) await h(ctx);
        try {
          await entry.fn?.(ctx);
        } catch (e) {
          err = e;
        }
        for (const s of lineage(entry.suite).reverse())
          for (const h of s.afterEach) {
            try {
              await h(ctx);
            } catch (e) {
              err ??= e;
            }
          }
      } catch (e) {
        err = e;
      }
      if (err) {
        failed++;
        log(`not ok ${id} - ${entry.name}`);
        diag(err);
      } else if (ctx._skipped) {
        skipped++;
        log(`ok ${id} - ${entry.name} # SKIP`);
      } else {
        passed++;
        log(`ok ${id} - ${entry.name}`);
      }
      entry.resolve?.();
    }

    async function runSuiteTree(s) {
      for (const h of s.before) await h();
      for (const child of s.children) {
        if (child.kind === "suite") await runSuiteTree(child.suite);
        else await runTestEntry(child);
      }
      for (const h of s.after) {
        try {
          await h();
        } catch (e) {
          failed++;
          diag(e);
        }
      }
    }

    async function runRoot() {
      log("TAP version 13");
      try {
        await runSuiteTree(root);
      } catch (e) {
        failed++;
        diag(e);
      }
      const total = passed + failed + skipped;
      log(`1..${total}`);
      log(`# tests ${total}`);
      log(`# pass ${passed}`);
      log(`# fail ${failed}`);
      log(`# skipped ${skipped}`);
      if (failed > 0) globalThis.process.exitCode = 1;
    }

    const mock = {
      fn: (impl) => {
        const f = (...args) => {
          f.mock.calls.push({ arguments: args });
          return impl ? impl(...args) : undefined;
        };
        f.mock = { calls: [] };
        return f;
      },
    };

    return {
      test,
      it,
      describe,
      suite,
      before,
      after,
      beforeEach,
      afterEach,
      mock,
      default: test,
    };
  };

  // ------------------------------------------------- runtime global setup
  // Called from Rust after snapshot restore + native install. Installs the
  // globals that need runtime data, and upgrades console with the
  // util.inspect-powered formatter (the M0 native console stringified
  // objects to '[object Object]').
  registry.installRuntimeGlobals = function installRuntimeGlobals() {
    const natives = globalThis.__oam.node;
    globalThis.process = registry.get("process");
    // Node defines `global` as an alias for the global object (global === globalThis).
    // Transpiled CJS deps (e.g. node-postgres) reference bare `global`; without
    // this they throw "global is not defined" the moment that path runs.
    if (typeof globalThis.global === "undefined") globalThis.global = globalThis;

    // Fork IPC child side: store port for lazy connect.
    // Cannot connect during installRuntimeGlobals because CoreRuntime
    // (tokio, TCP ops) is not installed until execute_module/reset_run_slots.
    // Instead, store the port and connect lazily on first process.on('message').
    const _ipcPort = globalThis.process.env.OAM_FORK_IPC_PORT;
    if (_ipcPort) {
      globalThis.process.connected = true;
      let _ipcSock = null;
      let _ipcReady = false;
      const _ipcPending = [];
      let _ipcConnecting = false;

      function _ipcEnsureConnect() {
        if (_ipcConnecting || _ipcReady) return;
        _ipcConnecting = true;
        const _net = registry.get("net");
        _ipcSock = new _net.Socket();
        let _ipcBuf = "";

        _ipcSock.connect(parseInt(_ipcPort, 10), "127.0.0.1", () => {
          _ipcReady = true;
          _ipcSock.setEncoding("utf8");

          for (const p of _ipcPending) {
            _ipcSock.write(p.line, "utf8", p.callback);
          }
          _ipcPending.length = 0;

          _ipcSock.on("data", (chunk) => {
            _ipcBuf += chunk;
            let nl;
            while ((nl = _ipcBuf.indexOf("\n")) !== -1) {
              const line = _ipcBuf.slice(0, nl);
              _ipcBuf = _ipcBuf.slice(nl + 1);
              try {
                const msg = JSON.parse(line);
                globalThis.process.emit("message", msg);
              } catch (_) { /* ignore malformed */ }
            }
          });
          _ipcSock.on("end", () => {
            globalThis.process.connected = false;
            globalThis.process.emit("disconnect");
          });
          _ipcSock.on("error", () => {
            globalThis.process.connected = false;
          });
        });
      }

      const _origOn = globalThis.process.on.bind(globalThis.process);
      globalThis.process.on = function on(event, listener) {
        if (event === "message") _ipcEnsureConnect();
        return _origOn(event, listener);
      };

      globalThis.process.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") { callback = _sendHandle; }
        else if (typeof _options === "function") { callback = _options; }
        if (!globalThis.process.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        _ipcEnsureConnect();
        const line = JSON.stringify(message) + "\n";
        if (_ipcReady && _ipcSock) {
          _ipcSock.write(line, "utf8", callback);
        } else {
          _ipcPending.push({ line, callback });
        }
        return true;
      };

      globalThis.process.disconnect = function disconnect() {
        globalThis.process.connected = false;
        if (_ipcSock) {
          var sock = _ipcSock;
          _ipcSock = null;
          _ipcReady = false;
          sock._chain.then(function() {
            sock.destroy();
            globalThis.process.emit("disconnect");
          });
        } else {
          globalThis.process.emit("disconnect");
        }
      };
    }
    var _perfEntries = [];
    globalThis.performance = {
      now: () => natives.nowMs(),
      timeOrigin: Date.now() - natives.nowMs(),
      mark: function mark(name, options) {
        var entry = {
          name: name,
          entryType: "mark",
          startTime: (options && options.startTime !== undefined) ? options.startTime : natives.nowMs(),
          duration: 0,
          detail: (options && options.detail) || null,
        };
        _perfEntries.push(entry);
        return entry;
      },
      measure: function measure(name, startOrOptions, endMark) {
        var startTime = 0;
        var endTime = natives.nowMs();
        if (typeof startOrOptions === "string") {
          for (var i = _perfEntries.length - 1; i >= 0; i--) {
            if (_perfEntries[i].name === startOrOptions && _perfEntries[i].entryType === "mark") {
              startTime = _perfEntries[i].startTime;
              break;
            }
          }
          if (typeof endMark === "string") {
            for (var j = _perfEntries.length - 1; j >= 0; j--) {
              if (_perfEntries[j].name === endMark && _perfEntries[j].entryType === "mark") {
                endTime = _perfEntries[j].startTime;
                break;
              }
            }
          }
        } else if (startOrOptions && typeof startOrOptions === "object") {
          if (startOrOptions.start !== undefined) {
            if (typeof startOrOptions.start === "string") {
              for (var k = _perfEntries.length - 1; k >= 0; k--) {
                if (_perfEntries[k].name === startOrOptions.start && _perfEntries[k].entryType === "mark") {
                  startTime = _perfEntries[k].startTime;
                  break;
                }
              }
            } else {
              startTime = startOrOptions.start;
            }
          }
          if (startOrOptions.end !== undefined) {
            if (typeof startOrOptions.end === "string") {
              for (var m = _perfEntries.length - 1; m >= 0; m--) {
                if (_perfEntries[m].name === startOrOptions.end && _perfEntries[m].entryType === "mark") {
                  endTime = _perfEntries[m].startTime;
                  break;
                }
              }
            } else {
              endTime = startOrOptions.end;
            }
          }
          if (startOrOptions.duration !== undefined) {
            endTime = startTime + startOrOptions.duration;
          }
        }
        var entry = {
          name: name,
          entryType: "measure",
          startTime: startTime,
          duration: endTime - startTime,
          detail: (startOrOptions && typeof startOrOptions === "object" && startOrOptions.detail) || null,
        };
        _perfEntries.push(entry);
        return entry;
      },
      getEntries: function getEntries() { return _perfEntries.slice(); },
      getEntriesByName: function getEntriesByName(name, type) {
        return _perfEntries.filter(function(e) {
          return e.name === name && (!type || e.entryType === type);
        });
      },
      getEntriesByType: function getEntriesByType(type) {
        return _perfEntries.filter(function(e) { return e.entryType === type; });
      },
      clearMarks: function clearMarks(name) {
        if (name === undefined) {
          _perfEntries = _perfEntries.filter(function(e) { return e.entryType !== "mark"; });
        } else {
          _perfEntries = _perfEntries.filter(function(e) { return !(e.entryType === "mark" && e.name === name); });
        }
      },
      clearMeasures: function clearMeasures(name) {
        if (name === undefined) {
          _perfEntries = _perfEntries.filter(function(e) { return e.entryType !== "measure"; });
        } else {
          _perfEntries = _perfEntries.filter(function(e) { return !(e.entryType === "measure" && e.name === name); });
        }
      },
      clearResourceTimings: function clearResourceTimings() {},
      toJSON: function toJSON() {
        return { timeOrigin: this.timeOrigin };
      },
    };
    // Lazy: registry.get('crypto') instantiates the ENTIRE crypto factory
    // (KeyObject, Hash, Hmac, createHash, ...) when only the webcrypto
    // sub-object {subtle, getRandomValues, randomUUID} is consumed at
    // boot. Deferring via a getter avoids ~1-2ms of factory init for
    // programs that never touch globalThis.crypto.
    Object.defineProperty(globalThis, 'crypto', {
      get() {
        const value = registry.get('crypto').webcrypto;
        Object.defineProperty(globalThis, 'crypto', { value, writable: true, configurable: true });
        return value;
      },
      configurable: true,
    });

  // -------------------------------------------------------------- navigator
  globalThis.navigator = globalThis.navigator || {
    userAgent: "oam/" + (globalThis.__oam?.version || "0.0.0"),
    language: "en",
    languages: ["en"],
    onLine: true,
    hardwareConcurrency: 1,
  };

    // oam.serve: defined in bootstrap (snapshot), attached here because
    // the `oam` namespace object is a post-restore native install.
    if (globalThis.oam && globalThis.__oamServe) {
      globalThis.oam.serve = globalThis.__oamServe;
    }

    // AsyncLocalStorage across macrotasks: V8's CPED only travels with
    // promise continuations, so timer-family callbacks are bound to the
    // frame current at SCHEDULING time (Node semantics). Promise paths
    // need no wrapper â€” V8 handles them.
    {
      const bindToCurrentFrame = (fn) => {
        if (typeof fn !== "function") return fn;
        const frame = natives.getContinuationData();
        return function (...args) {
          const prev = natives.getContinuationData();
          natives.setContinuationData(frame);
          try {
            return fn.apply(this, args);
          } finally {
            natives.setContinuationData(prev);
          }
        };
      };
      // setImmediate delegates to globalThis.setTimeout at call time, so
      // wrapping the timer pair covers it â€” no double-bind.
      for (const name of ["setTimeout", "setInterval", "queueMicrotask"]) {
        const native = globalThis[name];
        if (typeof native !== "function") continue;
        const wrapped = function (fn, ...rest) {
          return native(bindToCurrentFrame(fn), ...rest);
        };
        Object.defineProperty(wrapped, "name", { value: name });
        globalThis[name] = wrapped;
      }
    }

    const util = registry.get("util");
    const fmt = (args) =>
      args.length > 0 && typeof args[0] === "string"
        ? util.format(...args)
        : args.map((a) => (typeof a === "string" ? a : util.inspect(a))).join(" ");
    const writeOut = (args) => natives.stdoutWrite(fmt(args) + "\n");
    const writeErr = (args) => natives.stderrWrite(fmt(args) + "\n");

    const timers = new Map();
    const counters = new Map();
    globalThis.console = {
      log: (...args) => writeOut(args),
      info: (...args) => writeOut(args),
      debug: (...args) => writeOut(args),
      warn: (...args) => writeErr(args),
      error: (...args) => writeErr(args),
      trace: (...args) => {
        const stack = new Error().stack?.split("\n").slice(1).join("\n") ?? "";
        writeErr([`Trace: ${fmt(args)}\n${stack}`]);
      },
      assert: (cond, ...args) => {
        if (!cond) writeErr([`Assertion failed${args.length ? ": " + fmt(args) : ""}`]);
      },
      dir: (obj, options) => writeOut([util.inspect(obj, options ?? { depth: 2 })]),
      table: (data) => writeOut([util.inspect(data)]),
      time: (label = "default") => timers.set(label, natives.nowMs()),
      timeEnd: (label = "default") => {
        const start = timers.get(label);
        timers.delete(label);
        writeOut([`${label}: ${start === undefined ? "NaN" : (natives.nowMs() - start).toFixed(3)}ms`]);
      },
      count: (label = "default") => {
        const next = (counters.get(label) ?? 0) + 1;
        counters.set(label, next);
        writeOut([`${label}: ${next}`]);
      },
      countReset: (label = "default") => counters.delete(label),
      timeLog: (label = "default", ...extra) => {
        const start = timers.get(label);
        if (start === undefined) {
          writeErr([`Timer '${label}' does not exist`]);
          return;
        }
        const elapsed = `${label}: ${(natives.nowMs() - start).toFixed(3)}ms`;
        writeOut(extra.length ? [elapsed, ...extra] : [elapsed]);
      },
      group: (...args) => {
        if (args.length) writeOut(args);
      },
      groupCollapsed: (...args) => {
        if (args.length) writeOut(args);
      },
      groupEnd: () => {},
      table: (data, columns) => {
        if (data === null || data === undefined || typeof data !== "object") {
          writeOut([String(data)]);
          return;
        }
        if (Array.isArray(data)) {
          if (data.length === 0) { writeOut(["[]"]); return; }
          if (typeof data[0] === "object" && data[0] !== null) {
            const cols = columns || Object.keys(data[0]);
            writeOut(["(index) | " + cols.join(" | ")]);
            for (let ri = 0; ri < data.length; ri++) {
              const vals = cols.map(c => String(data[ri][c] === undefined ? "" : data[ri][c]));
              writeOut([ri + "       | " + vals.join(" | ")]);
            }
          } else {
            writeOut(["(index) | Values"]);
            for (let vi = 0; vi < data.length; vi++) {
              writeOut([vi + "       | " + String(data[vi])]);
            }
          }
        } else {
          const keys = Object.keys(data);
          if (keys.length === 0) { writeOut(["{}"]); return; }
          writeOut(["(index) | Values"]);
          for (const k of keys) writeOut([k + " | " + String(data[k])]);
        }
      },
      clear: () => {},
    };

    // Match Node's globalThis property attributes. Node defines web globals
    // (and process) as NON-enumerable, so `for (const k in globalThis)` is
    // nearly empty; oam installed several via plain assignment (enumerable),
    // which diverged from Node AND tripped Node test/common's global-leak check.
    // Flip them to non-enumerable (value/getter preserved). The handful that are
    // genuinely enumerable in Node -- timers, atob/btoa, structuredClone,
    // performance, fetch, navigator, queueMicrotask, global -- are left as-is.
    for (const name of [
      "DOMException", "Event", "EventTarget", "AbortSignal", "AbortController",
      "Headers", "Response", "Request", "Blob", "File", "MessagePort", "FormData",
      "BroadcastChannel", "MessageEvent", "CloseEvent", "WebSocket",
      "TextEncoder", "TextDecoder", "Buffer", "URL", "URLSearchParams",
      "ReadableStream", "WritableStream", "TransformStream",
      "TextDecoderStream", "TextEncoderStream", "process",
      "oam", "__oam", "__oamServe", "__oamTestRun",
    ]) {
      const desc = Object.getOwnPropertyDescriptor(globalThis, name);
      if (desc && desc.enumerable && desc.configurable) {
        desc.enumerable = false;
        Object.defineProperty(globalThis, name, desc);
      }
    }
  };
})();
