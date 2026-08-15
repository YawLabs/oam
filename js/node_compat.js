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
// - process.nextTick is a FIFO queue drained BY THE HOST at every tick
//   point (drain -> microtask checkpoint, looped until both empty -- Node's
//   processTicksAndRejections; docs/design/nexttick-engine.md). Ticks run
//   ahead of already-queued promise jobs, and ticks scheduled from
//   promise-job contexts run after full microtask exhaustion,
//   Node-identical in both directions.
// - node:stream is NOT defined here: js/vendor/oam-shims/register.js
//   installs the vendored Node v22 port (docs/design/streams-port.md); the
//   hand-rolled wave-1 streams were deleted in slice 5. fs/net/http streams
//   in this file build on registry.get("stream") = the vendored classes.
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
            // WHATWG "maximal subpart": one U+FFFD stands for the whole
            // invalid prefix, so resume PAST the bytes that were valid.
            // Advancing a single byte re-decoded the continuation bytes as
            // fresh leads and emitted extra replacement characters.
            fail(i);
            i += i + 1 < bytes.length && b1 >= lower && b1 <= upper ? 2 : 1;
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
            if (!(i + 1 < bytes.length && b1 >= lower && b1 <= upper)) i += 1;
            else if (!(i + 2 < bytes.length && (b2 & 0xc0) === 0x80)) i += 2;
            else i += 3;
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
      if (v === -1) continue; // Node SKIPS junk (only '=' terminates), not break
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

  /// A SYNTHESISED system error carrying node's full shape: code, syscall,
  /// path, errno and node's message wording.
  ///
  /// For failures oam decides itself rather than reading off a syscall --
  /// rmdir refusing a non-directory is the case that needs it. Those used
  /// makeNodeError, which sets `code` alone, so `err.syscall`/`errno`/`path`
  /// came back undefined where every syscall-backed error in the runtime
  /// sets all four.
  ///
  /// The errno table is deliberately tiny: only the codes actually raised
  /// this way, with libuv's negative numbering (the Windows block is offset,
  /// POSIX is plain -errno) to match what the Rust side reports.
  function makeSystemError(code, syscall, path) {
    const isWin = globalThis.__oam.node.platform === "win32";
    const ERRNO = isWin
      ? { ENOENT: -4058, ENOTDIR: -4052, EEXIST: -4075, EPERM: -4048 }
      : { ENOENT: -2, ENOTDIR: -20, EEXIST: -17, EPERM: -1 };
    const TEXT = {
      ENOENT: "no such file or directory",
      ENOTDIR: "not a directory",
      EEXIST: "file already exists",
      EPERM: "operation not permitted",
    };
    const err = new Error(`${code}: ${TEXT[code] ?? code}, ${syscall} '${path}'`);
    err.code = code;
    err.syscall = syscall;
    err.path = String(path);
    if (ERRNO[code] !== undefined) err.errno = ERRNO[code];
    return err;
  }

  /// ERR_INVALID_ARG_TYPE and friends are TypeErrors in node, not plain
  /// Errors -- `instanceof TypeError` and assert.throws({ name: "TypeError" })
  /// both key off that, so makeNodeError (which builds an Error) is the wrong
  /// tool for an argument-validation failure.
  function nodeTypeError(message, code) {
    var err = new TypeError(message);
    applyNodeErrorShape(err, code ?? "ERR_INVALID_ARG_TYPE");
    return err;
  }

  /// node's "Received ..." tail on an ERR_INVALID_ARG_TYPE message.
  function describeArg(value) {
    if (value === null) return "null";
    if (value === undefined) return "undefined";
    if (typeof value === "object") {
      var name = value.constructor && value.constructor.name;
      return `an instance of ${name || "Object"}`;
    }
    if (typeof value === "string") return `type string ('${value}')`;
    return `type ${typeof value} (${String(value)})`;
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
    // Node renders the code into the stack header too. Anchor the rewrite to
    // the first line only -- never risk hitting a "Name:" substring in the
    // message or a deeper frame.
    if (typeof inst.stack === "string") {
      const nl = inst.stack.indexOf("\n");
      const head = nl === -1 ? inst.stack : inst.stack.slice(0, nl);
      const rest = nl === -1 ? "" : inst.stack.slice(nl);
      inst.stack = head.replace(baseName + ":", baseName + " [" + code + "]:") + rest;
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

  // Builtins node exposes ONLY under the 'node:' prefix (bare require/
  // getBuiltinModule of these returns undefined).
  const PREFIX_ONLY_BUILTINS = new Set(["test", "test/reporters", "sea", "sqlite"]);

  var codes = {};

  // Faithful port of Node's internal `lib/internal/errors.js`
  // invalidArgTypeHelper -- renders the "Received ..." tail of an
  // ERR_INVALID_ARG_TYPE message the way Node does (type + inspected value,
  // class names, function names, negative zero, etc.). Many conformance tests
  // assert on this exact text, so it must match byte-for-byte.
  function invalidArgTypeHelper(input) {
    if (input == null) {
      return ` Received ${input}`;
    }
    // No name gate: anonymous functions render "Received function " (empty
    // name, trailing space) -- probe-verified against real node v22.22.2,
    // and test/common invalidArgTypeHelper does the same unconditionally.
    if (typeof input === "function") {
      return ` Received function ${input.name}`;
    }
    if (typeof input === "object") {
      if (input.constructor && input.constructor.name) {
        return ` Received an instance of ${input.constructor.name}`;
      }
      try {
        return ` Received ${nodeInspect(input, { depth: -1 })}`;
      } catch {
        return ` Received an object`;
      }
    }
    let inspected;
    try {
      inspected = nodeInspect(input, { colors: false });
    } catch {
      inspected = String(input);
    }
    if (inspected.length > 28) {
      inspected = `${inspected.slice(0, 25)}...`;
    }
    return ` Received type ${typeof input} (${inspected})`;
  }

  // Build the "must be of type X" / "must be one of type X, Y, or Z" clause and
  // pick "argument" vs "property" exactly as Node does (a name containing a dot
  // is treated as a property path).
  function buildArgTypeMessage(name, expected, actual) {
    if (!Array.isArray(expected)) expected = [expected];
    let msg = "The ";
    if (name.endsWith(" argument")) {
      msg += `${name} `;
    } else {
      const type = name.includes(".") ? "property" : "argument";
      msg += `"${name}" ${type} `;
    }
    msg += "must be ";

    const types = [];
    const instances = [];
    const other = [];
    for (const value of expected) {
      const low = String(value).toLowerCase();
      if (
        ["string", "number", "bigint", "boolean", "symbol", "undefined", "object", "function"].includes(low)
      ) {
        types.push(low);
      } else if (/^[A-Z]/.test(String(value))) {
        instances.push(String(value));
      } else {
        other.push(String(value));
      }
    }
    if (instances.length > 0) {
      const pos = types.indexOf("object");
      if (pos !== -1) {
        types.splice(pos, 1);
        instances.push("Object");
      }
    }
    if (types.length > 0) {
      msg += `${types.length > 1 ? "one of type" : "of type"} ${formatList(types, "or")}`;
      if (instances.length > 0 || other.length > 0) msg += " or ";
    }
    if (instances.length > 0) {
      msg += `an instance of ${formatList(instances, "or")}`;
      if (other.length > 0) msg += " or ";
    }
    if (other.length > 0) {
      if (other.length > 1) msg += `one of ${formatList(other, "or")}`;
      else {
        if (other[0].toLowerCase() !== other[0]) msg += "an ";
        msg += `${other[0]}`;
      }
    }
    return `${msg}.${invalidArgTypeHelper(actual)}`;
  }

  // Node's formatList helper: join with commas + the final conjunction.
  function formatList(arr, type) {
    if (arr.length <= 2) return arr.join(` ${type} `);
    return `${arr.slice(0, -1).join(", ")}, ${type} ${arr[arr.length - 1]}`;
  }

  // util.inspect bound late (util factory may not be built yet when an error is
  // thrown). Resolve lazily so the helper works during early bootstrap.
  function nodeInspect(value, opts) {
    try {
      return registry.get("util").inspect(value, opts);
    } catch {
      return String(value);
    }
  }

  // ---- TypeError family ----
  codes.ERR_INVALID_ARG_TYPE = E("ERR_INVALID_ARG_TYPE", TypeError, function(name, expected, actual) {
    return buildArgTypeMessage(name, expected, actual);
  });
  codes.ERR_INVALID_ARG_VALUE = E("ERR_INVALID_ARG_VALUE", TypeError, function(name, value, reason) {
    // Node's exact shape: `The ${type} '${name}' ${reason}. Received
    // ${inspect(value)}` -- 'property' when the name contains a dot,
    // 'argument' otherwise, and INSPECT rather than String() so a string
    // shows its quotes and an object shows its contents. oam previously
    // emitted `The argument "x" is invalid. Received <String(v)>. <reason>`,
    // which got the quoting, the order and the value rendering wrong at
    // all 15 call sites.
    let inspected;
    try {
      inspected = nodeInspect(value);
    } catch {
      inspected = String(value);
    }
    if (inspected.length > 128) inspected = `${inspected.slice(0, 128)}...`;
    const type = String(name).includes(".") ? "property" : "argument";
    return `The ${type} '${name}' ${reason ?? "is invalid"}. Received ${inspected}`;
  });
  codes.ERR_INVALID_CALLBACK = E("ERR_INVALID_CALLBACK", TypeError, function(name) {
    return 'Callback must be a function. Received ' + String(name);
  });
  codes.ERR_INVALID_THIS = E("ERR_INVALID_THIS", TypeError, function(expected) {
    return 'Value of "this" must be of type ' + expected;
  });
  // Node determineSpecificType: "undefined" / "an instance of Map" /
  // "function foo" / "type string ('x')".
  function determineSpecificType(value) {
    if (value === null || value === undefined) return String(value);
    if (typeof value === "function" && value.name) return "function " + value.name;
    if (typeof value === "object") {
      if (value.constructor && value.constructor.name) return "an instance of " + value.constructor.name;
      try {
        return require("util").inspect(value, { depth: -1 });
      } catch {
        return "an instance of Object";
      }
    }
    let inspected;
    try {
      inspected = require("util").inspect(value, { colors: false });
    } catch {
      inspected = String(value);
    }
    if (inspected.length > 28) inspected = inspected.slice(0, 25) + "...";
    return "type " + typeof value + " (" + inspected + ")";
  }
  codes.ERR_INVALID_RETURN_VALUE = E("ERR_INVALID_RETURN_VALUE", TypeError, function(input, name, value) {
    return 'Expected ' + input + ' to be returned from the "' + name + '" function but got ' +
      determineSpecificType(value) + ".";
  });
  codes.ERR_MISSING_ARGS = E("ERR_MISSING_ARGS", TypeError, function() {
    var args = Array.prototype.slice.call(arguments);
    return 'The ' + args.map(function(a) { return '"' + a + '"'; }).join(", ") + ' argument' + (args.length > 1 ? 's' : '') + ' must be specified';
  });
  codes.ERR_UNKNOWN_ENCODING = E("ERR_UNKNOWN_ENCODING", TypeError, function(enc) {
    return 'Unknown encoding: ' + enc;
  });
  codes.ERR_CONSTRUCT_CALL_REQUIRED = E("ERR_CONSTRUCT_CALL_REQUIRED", TypeError, function(name) {
    return 'Class constructor ' + name + ' cannot be invoked without `new`';
  });
  // Node v22 shape: message is exactly "Invalid URL"; the offending string
  // rides on err.input (set by the throw sites), not in the message.
  codes.ERR_INVALID_URL = E("ERR_INVALID_URL", TypeError, function() {
    return 'Invalid URL';
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
    return name ? '"' + name + '" is outside of buffer bounds' : 'Attempt to access memory outside buffer bounds';
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
        // Node's ascii ENCODE writes the low 8 bits (charCode & 0xff), same as
        // latin1 -- it is the DECODE side (toString) that masks to 7 bits.
        const out = new Uint8Array(str.length);
        for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0xff;
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

  // Brand checks for ArrayBuffer / SharedArrayBuffer that, unlike `instanceof`,
  // are NOT fooled by prototype tampering and DO recognize cross-realm buffers
  // (Node uses the internal-slot brand). Each calls the prototype byteLength
  // getter, which throws unless the receiver has the matching internal slot.
  const _abByteLength = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "byteLength").get;
  // SharedArrayBuffer's byteLength getter is resolved LAZILY: this module is
  // evaluated while building the V8 snapshot, where SharedArrayBuffer is not
  // yet a global, so capturing it here would pin it to null forever and make
  // every SAB look like a non-buffer. Resolve on first use (runtime), cache it.
  let _sabByteLength;
  let _sabResolved = false;
  function sabByteLengthGetter() {
    if (!_sabResolved) {
      _sabResolved = true;
      _sabByteLength =
        typeof SharedArrayBuffer !== "undefined"
          ? Object.getOwnPropertyDescriptor(SharedArrayBuffer.prototype, "byteLength").get
          : null;
    }
    return _sabByteLength;
  }
  function isAnyArrayBuffer(v) {
    if (typeof v !== "object" || v === null) return false;
    try {
      _abByteLength.call(v);
      return true;
    } catch {
      /* not a real ArrayBuffer */
    }
    const sabGet = sabByteLengthGetter();
    if (sabGet) {
      try {
        sabGet.call(v);
        return true;
      } catch {
        /* not a real SharedArrayBuffer */
      }
    }
    return false;
  }

  // Brand checks for the built-ins whose identity has to survive a realm
  // boundary. `instanceof` compares against THIS realm's constructor, so a Map
  // handed back from a `vm` context fails it and gets treated as a plain
  // object -- wrong for deepEqual, util.types and inspect alike. Calling the
  // original accessor works instead: it only succeeds on the real internal
  // slot, so it is both realm-agnostic and un-spoofable by a borrowed
  // prototype or a faked Symbol.toStringTag.
  function slotProbe(accessor, ...args) {
    return (v) => {
      if (v === null || typeof v !== "object") return false;
      try {
        accessor.call(v, ...args);
        return true;
      } catch {
        return false;
      }
    };
  }
  const isRealDate = slotProbe(Date.prototype.getTime);
  const isRealRegExp = slotProbe(
    Object.getOwnPropertyDescriptor(RegExp.prototype, "source").get,
  );
  const isRealMap = slotProbe(Object.getOwnPropertyDescriptor(Map.prototype, "size").get);
  const isRealSet = slotProbe(Object.getOwnPropertyDescriptor(Set.prototype, "size").get);
  // has() is the only WeakMap/WeakSet method that brand-checks without
  // mutating; the probe key is discarded either way.
  const isRealWeakMap = slotProbe(WeakMap.prototype.has, {});
  const isRealWeakSet = slotProbe(WeakSet.prototype.has, {});
  const isRealDataView = slotProbe(
    Object.getOwnPropertyDescriptor(DataView.prototype, "byteLength").get,
  );
  const isSharedArrayBufferValue =
    typeof SharedArrayBuffer === "function"
      ? slotProbe(Object.getOwnPropertyDescriptor(SharedArrayBuffer.prototype, "byteLength").get)
      : () => false;
  // %TypedArray%.prototype[Symbol.toStringTag] reads the [[TypedArrayName]]
  // slot, so it names a foreign typed array and answers undefined for anything
  // that merely looks like one.
  const TYPED_ARRAY_TAG = Object.getOwnPropertyDescriptor(
    Object.getPrototypeOf(Uint8Array.prototype),
    Symbol.toStringTag,
  ).get;
  function typedArrayName(v) {
    try {
      return TYPED_ARRAY_TAG.call(v);
    } catch {
      return undefined;
    }
  }

  // Node's validateInteger: number-typed integer >= min (copyBytesFrom offsets).
  function validateInteger(value, name, min) {
    if (typeof value !== "number") {
      throw argTypeOfError(name, "number", value);
    }
    if (!Number.isInteger(value)) {
      throw codes.ERR_OUT_OF_RANGE(name, "an integer", fmtRange(value));
    }
    if (min !== undefined && value < min) {
      throw codes.ERR_OUT_OF_RANGE(name, ">= " + min, fmtRange(value));
    }
  }

  const poolSizeRef = { value: 8192 };
  // Node's lib/buffer.js pool. Small allocations are carved out of one
  // shared ArrayBuffer, so an allocUnsafe Buffer normally has a NON-ZERO
  // byteOffset and a .buffer much larger than its length. Userland observes
  // this (it is why .buffer must never be handed out unsliced), and code
  // that assumes otherwise is already wrong against Node.
  let allocPool = null;
  let poolOffset = 0;
  // Objects that must never appear in a postMessage transfer list. The
  // Buffer pool is the reason this exists: transferring it would detach the
  // backing store of every small Buffer in the process at once.
  const untransferable = new WeakSet();
  function createPool() {
    allocPool = new ArrayBuffer(poolSizeRef.value);
    untransferable.add(allocPool);
    poolOffset = 0;
  }
  function alignPool() {
    // Ensure aligned slices, exactly as Node does.
    if (poolOffset & 0x7) poolOffset = (poolOffset | 0x7) + 1;
  }
  // Requests at or above half the pool get their own ArrayBuffer (Node's
  // rule), so a large buffer never evicts the pool.
  function poolAllocate(BufferCtor, size) {
    if (size <= 0) return new BufferCtor(0);
    if (size >= (poolSizeRef.value >>> 1)) return new BufferCtor(size);
    if (allocPool === null) createPool();
    if (size > poolSizeRef.value - poolOffset) createPool();
    const b = new BufferCtor(allocPool, poolOffset, size);
    poolOffset += size;
    alignPool();
    return b;
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
      // --zero-fill-buffers turns the whole unsafe family into alloc(): the
      // point of the flag is that NO buffer can hand back another
      // allocation's leftover bytes, which the pooled path otherwise does
      // by design.
      if (globalThis.__oamZeroFillBuffers) return Buffer.alloc(size);
      return poolAllocate(Buffer, size);
    }

    static allocUnsafeSlow(size) {
      validateSize(size);
      if (globalThis.__oamZeroFillBuffers) return Buffer.alloc(size);
      return new Buffer(size);
    }

    // Node >= 19: copy the raw BYTES of a TypedArray into a new Buffer (a true
    // byte reinterpretation, unlike Buffer.from(typedArray) which copies the
    // ELEMENTS mod 256). offset/length are in TypedArray ELEMENTS.
    static copyBytesFrom(view, offset, length) {
      if (!ArrayBuffer.isView(view) || view instanceof DataView) {
        throw codes.ERR_INVALID_ARG_TYPE("view", "TypedArray", view);
      }
      const viewLength = view.length;
      if (viewLength === 0) return Buffer.alloc(0);
      if (offset !== undefined || length !== undefined) {
        if (offset === undefined) {
          offset = 0;
        } else {
          validateInteger(offset, "offset", 0);
          if (offset >= viewLength) return Buffer.alloc(0);
        }
        let end;
        if (length === undefined) {
          end = viewLength;
        } else {
          validateInteger(length, "length", 0);
          end = offset + length;
        }
        view = view.subarray(offset, end);
      }
      const bytes = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
      const buf = new Buffer(bytes.length);
      buf.set(bytes);
      return buf;
    }

    static from(value, encodingOrOffset, length) {
      if (typeof value === "string") {
        // Node fromString: a non-string (or empty-string) encoding silently
        // defaults to utf8 (`Buffer.from('asd', 1)` is allowed); a non-empty
        // unknown string encoding throws ERR_UNKNOWN_ENCODING.
        let enc = encodingOrOffset;
        if (typeof enc !== "string" || enc.length === 0) enc = undefined;
        const bytes = bytesFromString(value, enc);
        const out = poolAllocate(Buffer, bytes.length);
        out.set(bytes);
        return out;
      }
      if (value instanceof Uint8Array) {
        // Buffer / Uint8Array: COPIES its bytes, per Node (from(typedArray)
        // copies; length === element count === byte count).
        const buf = new Buffer(value.length);
        buf.set(value);
        return buf;
      }
      if (isAnyArrayBuffer(value)) {
        // Views the SAME memory, per Node. Brand-checked (not instanceof) so
        // a prototype-spoofed object is rejected and cross-realm buffers work.
        // Coerce byteOffset/length exactly as Node's fromArrayBuffer does.
        let off;
        if (encodingOrOffset === undefined) {
          off = 0;
        } else {
          off = +encodingOrOffset;
          if (Number.isNaN(off)) off = 0;
        }
        const maxLength = value.byteLength - off;
        if (maxLength < 0) throw codes.ERR_BUFFER_OUT_OF_BOUNDS("offset");
        let len;
        if (length === undefined) {
          len = maxLength;
        } else {
          len = +length;
          if (len > 0) {
            if (len > maxLength) throw codes.ERR_BUFFER_OUT_OF_BOUNDS("length");
          } else {
            len = 0;
          }
        }
        // A RESIZABLE ArrayBuffer with no explicit length yields a
        // LENGTH-TRACKING view: passing a computed length pins it, so a
        // later ab.resize() left the Buffer reporting the old byteLength
        // (and reading past the live end after a shrink). Omitting the
        // length is what makes the view track, per the spec and Node.
        if (length === undefined && value.resizable === true) {
          return new Buffer(value, off);
        }
        return new Buffer(value, off, len);
      }
      if (ArrayBuffer.isView(value)) {
        // Any OTHER TypedArray (Uint16Array, Uint32Array, ...) is treated as an
        // array-like of its ELEMENTS, each coerced mod 256 -- NOT a raw byte
        // reinterpretation (that is Buffer.copyBytesFrom). A DataView has no
        // numeric .length and yields an empty Buffer.
        const n = typeof value.length === "number" ? value.length : 0;
        const buf = new Buffer(n);
        if (n > 0) buf.set(value);
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
      // Array-like (objects with a `.length`): fromArrayLike. Per Node, a
      // NON-number length yields an empty Buffer; a number <= 0 yields empty;
      // otherwise allocate (NaN -> 0, fractional truncates) and copy elements
      // mod 256. Functions have a numeric .length but are typeof "function", so
      // they never reach this object-only branch (Node rejects them).
      if (value != null && typeof value === "object" && value.length !== undefined) {
        if (typeof value.length !== "number" || value.length <= 0) return new Buffer(0);
        const len = value.length >>> 0;
        const buf = new Buffer(len);
        for (let i = 0; i < len; i++) buf[i] = value[i] & 0xff;
        return buf;
      }
      // Node fromObject: an object whose `.buffer` is an (any) ArrayBuffer but
      // with no numeric length yields an empty Buffer (test-buffer-sharedarray
      // buffer does Buffer.from({ buffer: sab }) and expects no throw). Brand-
      // checked so a cross-realm ArrayBuffer is recognized.
      if (value != null && typeof value === "object" && isAnyArrayBuffer(value.buffer)) {
        return new Buffer(0);
      }
      throw argTypeFromError(value);
    }

    static byteLength(value, encoding) {
      if (typeof value !== "string") {
        // instanceof is realm-bound, and an ArrayBuffer handed over from a
        // `vm` context is a genuine cross-realm object. Both checks here are
        // brand checks instead: ArrayBuffer.isView is a V8-level test, and
        // isAnyArrayBuffer calls the byteLength getter.
        if (ArrayBuffer.isView(value) || isAnyArrayBuffer(value)) {
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
      return Buffer.prototype.compare.call(a, b);
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
      // Node refuses UP FRONT when the result cannot fit in a V8 string
      // (kStringMaxLength = 0x1fffffe8): without this guard the decode is
      // attempted and the process dies with a V8 heap OOM instead of
      // throwing a catchable error -- e.g. readFileSync(bigFile).toString().
      // The check is on the BYTE span, matching node: a multi-byte encoding
      // can only produce fewer characters, and node throws on the span too.
      if (e - s > 0x1fffffe8) {
        const err = new Error("Cannot create a string longer than 0x1fffffe8 characters");
        err.code = "ERR_STRING_TOO_LONG";
        throw err;
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
      // Node's argument dispatch (lib/buffer.js):
      //   write(string)                          -> whole buffer, utf8
      //   write(string, encoding)                -- ONLY when length is omitted
      //   write(string, offset[, length][, encoding])
      // A STRING in the `offset` slot is the encoding ONLY when `length` is
      // absent; otherwise `offset` must be a number (a string there is an
      // ERR_INVALID_ARG_TYPE, e.g. b.write('test', 'utf8', 0)).
      if (offset === undefined) {
        offset = 0;
        length = this.length;
      } else if (length === undefined && typeof offset === "string") {
        encoding = offset;
        offset = 0;
        length = this.length;
      } else {
        validateOffsetRange(offset, "offset", this.length);
        const remaining = this.length - offset;
        if (length === undefined) {
          length = remaining;
        } else if (typeof length === "string") {
          encoding = length;
          length = remaining;
        } else {
          validateOffsetRange(length, "length", this.length);
          if (length > remaining) length = remaining;
        }
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
      // Node value coercion: undefined/null -> 0; boolean -> 0/1; number -> &255.
      if (value === undefined || value === null) {
        Uint8Array.prototype.fill.call(this, 0, start, end);
        return this;
      }
      if (typeof value === "boolean") value = +value;
      if (typeof value === "number") {
        Uint8Array.prototype.fill.call(this, value & 0xff, start, end);
        return this;
      }
      if (typeof value === "string") {
        // Node validates the encoding here: a non-string encoding is an
        // ERR_INVALID_ARG_TYPE, an unrecognized one is ERR_UNKNOWN_ENCODING.
        const norm = normalizeEncoding(encoding);
        if (norm === undefined) {
          if (typeof encoding !== "string") throw argTypeOfError("encoding", "string", encoding);
          throw codes.ERR_UNKNOWN_ENCODING(encoding);
        }
        if (value.length === 0) {
          Uint8Array.prototype.fill.call(this, 0, start, end);
          return this;
        }
        const pattern = bytesFromString(value, encoding);
        // A non-empty string that decodes to zero bytes (bad/odd hex, a single
        // base64 char) is an invalid fill value, per Node.
        if (pattern.length === 0) throw codes.ERR_INVALID_ARG_VALUE("value", value);
        for (let i = start; i < end; i++) this[i] = pattern[(i - start) % pattern.length];
        return this;
      }
      // Buffer / Uint8Array / array-like fill value: tile its bytes. An empty
      // one is an invalid fill value, per Node.
      const pattern = Buffer.from(value);
      if (pattern.length === 0) throw codes.ERR_INVALID_ARG_VALUE("value", value);
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

    // Node aliases buf.parent -> buf.buffer (legacy pre-Uint8Array name). The
    // getter must not throw when read off the prototype (Buffer.prototype.parent
    // === undefined), so guard the receiver.
    get parent() {
      return this instanceof Buffer ? this.buffer : undefined;
    }

    // Node exposes buf.offset === buf.byteOffset (same prototype-read guard).
    get offset() {
      return this instanceof Buffer ? this.byteOffset : undefined;
    }
  }

  // Node's Buffer.prototype.lastIndexOf is a plain function (not a class method),
  // so it is constructable: `new buffer.Buffer.prototype.lastIndexOf(1, 'str')`
  // reaches the receiver guard and throws ERR_INVALID_ARG_TYPE for "buffer"
  // (see test-buffer-indexof). A class method is non-constructable and would
  // instead throw "is not a constructor". Re-wrap as a named function expression
  // so `new` works and the constructed instance reports name "lastIndexOf".
  {
    const classLastIndexOf = Buffer.prototype.lastIndexOf;
    Buffer.prototype.lastIndexOf = function lastIndexOf(needle, byteOffset, encoding) {
      return classLastIndexOf.call(this, needle, byteOffset, encoding);
    };
  }

  // Node aliases Buffer.prototype.toLocaleString to the SAME function as
  // toString (test-buffer-alloc asserts reference equality); otherwise it would
  // inherit %TypedArray%.prototype.toLocaleString.
  Buffer.prototype.toLocaleString = Buffer.prototype.toString;

  // Node's per-encoding raw write helpers (asciiWrite/latin1Write/utf8Write,
  // exposed on Buffer.prototype). offset/length are validated against the
  // buffer bounds and throw ERR_BUFFER_OUT_OF_BOUNDS when out of range (a
  // negative length, an offset past the end, ...) -- see test-buffer-write.
  {
    function rawEncWrite(buf, encoding, string, offset, length) {
      if (offset === undefined) {
        offset = 0;
      } else {
        offset = +offset;
        if (Number.isNaN(offset)) offset = 0;
      }
      if (offset < 0 || offset > buf.length || Math.floor(offset) !== offset) {
        throw codes.ERR_BUFFER_OUT_OF_BOUNDS();
      }
      const remaining = buf.length - offset;
      if (length === undefined) {
        length = remaining;
      } else {
        length = +length;
        if (Number.isNaN(length)) length = 0;
      }
      if (length < 0 || Math.floor(length) !== length) {
        throw codes.ERR_BUFFER_OUT_OF_BOUNDS();
      }
      if (length > remaining) length = remaining;
      const bytes = bytesFromString(String(string), encoding);
      let writable = Math.min(bytes.length, length);
      // Never split a multi-byte UTF-8 sequence (1 byte/char for ascii/latin1).
      if (writable < bytes.length && encoding === "utf8") {
        while (writable > 0 && (bytes[writable] & 0xc0) === 0x80) writable--;
      }
      buf.set(bytes.subarray(0, writable), offset);
      return writable;
    }
    Buffer.prototype.asciiWrite = function asciiWrite(string, offset, length) {
      return rawEncWrite(this, "ascii", string, offset, length);
    };
    Buffer.prototype.latin1Write = function latin1Write(string, offset, length) {
      return rawEncWrite(this, "latin1", string, offset, length);
    };
    Buffer.prototype.utf8Write = function utf8Write(string, offset, length) {
      return rawEncWrite(this, "utf8", string, offset, length);
    };
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
  Object.defineProperty(Buffer, "poolSize", {
    get: () => poolSizeRef.value,
    set: (v) => {
      poolSizeRef.value = v;
    },
    enumerable: true,
    configurable: true,
  });

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

  // Node's `Buffer` is a FUNCTION callable WITHOUT `new` (legacy/deprecated --
  // DEP0005), not an ES6 class. `class Buffer extends Uint8Array` above rejects
  // `Buffer([1,2,3])` / `Buffer(10)` with "Class constructor cannot be invoked
  // without 'new'". The vendored Node buffer tests (alloc / slice / new /
  // zero-fill / no-negative-allocation / over-max-length) exercise the callable
  // form. Mirror the EventEmitter / zlib.Inflate "callable as plain function AND
  // as class" pattern: expose a plain function that shares the real class's
  // `.prototype` (so `instanceof` + every prototype method survives) and all
  // static methods, dispatching args exactly as Node's legacy constructor does:
  //   Buffer(number)            -> alloc(number)  (zero-filled, Node v22)
  //   Buffer(string[, enc])     -> from(string, enc)
  //   Buffer(array)             -> from(array)
  //   Buffer(buffer/TypedArray) -> from(buffer)
  //   Buffer(arrayBuffer[, off[, len]]) -> from(arrayBuffer, off, len)
  // `new Buffer(...)` behaves identically. The internal `new Buffer(...)` call
  // sites above keep using the raw class binding (lexical), so the fast
  // allocation/view paths are untouched -- only the PUBLIC `globalThis.Buffer`
  // becomes the callable shim. `Symbol.species` still returns the raw class so
  // subarray/slice construction stays on the raw path.
  const RealBuffer = Buffer;
  let buffer_dep0005_warned = false;
  // Node suppresses DEP0005 when the CALLER sits inside node_modules
  // (isInsideNodeModules: the innermost stack frame with a real USER filename
  // decides). Runtime frames are not user frames and must all be skipped:
  // `oam:` and `node:` are builtins, `[eval]` is the eval wrapper, and
  // `<anonymous>` covers anything still unnamed. --pending-deprecation
  // overrides.
  function isCallerInsideNodeModules() {
    try {
      const lines = (new Error().stack || "").split("\n");
      for (let i = 1; i < lines.length; i++) {
        const m = /\(([^)]+):\d+:\d+\)/.exec(lines[i]) || / at ([^( ][^ ]*):\d+:\d+/.exec(lines[i]);
        const file = m && (m[1] || m[2]);
        if (
          !file ||
          file === "<anonymous>" ||
          file === "[eval]" ||
          file.startsWith("node:") ||
          file.startsWith("oam:")
        ) {
          continue;
        }
        return /[\\/]node_modules[\\/]/.test(file);
      }
    } catch { /* fall through: treat as app code */ }
    return false;
  }
  function emitBufferCtorDeprecation() {
    if (buffer_dep0005_warned) return;
    // Suppression does NOT latch: a later call from app code still warns.
    if (!globalThis.__oamPendingDeprecation && isCallerInsideNodeModules()) return;
    buffer_dep0005_warned = true;
    // Node emits DEP0005 once. Route through process.emitWarning if the process
    // factory is already realized; never throw if it isn't (Buffer is top-level,
    // process is a lazy registry factory built later).
    try {
      const proc = globalThis.process;
      if (proc && typeof proc.emitWarning === "function") {
        proc.emitWarning(
          "Buffer() is deprecated due to security and usability issues. Please use " +
            "the Buffer.alloc(), Buffer.allocUnsafe(), or Buffer.from() methods instead.",
          { type: "DeprecationWarning", code: "DEP0005" },
        );
      }
    } catch {
      // Best-effort: a missing/partial process must not break Buffer().
    }
  }
  function CallableBuffer(arg, encodingOrOffset, length) {
    emitBufferCtorDeprecation();
    if (typeof arg === "number") {
      // Node legacy Buffer(number): an allocation (Node v22 zero-fills). BUT a
      // STRING 2nd arg means the caller used the string-constructor form -- Node
      // routes there and throws the "string" arg-type error for the number.
      if (typeof encodingOrOffset === "string") {
        throw applyNodeErrorShape(
          new TypeError(
            'The "string" argument must be of type string.' + receivedSuffix(arg),
          ),
          "ERR_INVALID_ARG_TYPE",
        );
      }
      return RealBuffer.alloc(arg);
    }
    if (typeof arg === "string") {
      return RealBuffer.from(arg, encodingOrOffset);
    }
    // array / Buffer / TypedArray / ArrayBuffer(+offset/length) / array-like.
    return RealBuffer.from(arg, encodingOrOffset, length);
  }
  // Share the real class's prototype so `instanceof Buffer` and every prototype
  // method resolve identically for instances produced by either binding.
  CallableBuffer.prototype = RealBuffer.prototype;
  Object.defineProperty(CallableBuffer.prototype, "constructor", {
    value: CallableBuffer,
    writable: true,
    configurable: true,
    enumerable: false,
  });
  Object.setPrototypeOf(CallableBuffer, RealBuffer); // inherit statics on the fn object too
  // Copy every own (enumerable) static across so `for..in` cloners
  // (safe-buffer / iconv-lite) see them on the callable shim as well.
  for (const name of Object.getOwnPropertyNames(RealBuffer)) {
    if (name === "prototype" || name === "name" || name === "length") continue;
    const descriptor = Object.getOwnPropertyDescriptor(RealBuffer, name);
    if (descriptor) Object.defineProperty(CallableBuffer, name, descriptor);
  }
  // Buffer.of: the inherited Uint8Array.of does `new this(len)` == new Buffer(n),
  // which fires the DEP0005 deprecation. Node defines its own non-deprecating
  // Buffer.of (test-buffer-of-no-deprecation asserts no 'warning' fires).
  CallableBuffer.of = function of(...items) {
    return RealBuffer.from(items);
  };
  // `Buffer.name` should read "Buffer" (some libs assert it).
  Object.defineProperty(CallableBuffer, "name", { value: "Buffer", configurable: true });

  globalThis.Buffer = CallableBuffer;
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
    // Active-resource introspection tables (process.getActiveResourcesInfo /
    // process._getActiveHandles / process._getActiveRequests). Declared here
    // rather than inside a module factory because producers (net, fs) and the
    // consumer (process) are built independently and in either order.
    //   _activeHandles:  handle object -> libuv-ish type tag ("TCPServerWrap",
    //                    "TCPSocketWrap"). Node's _getActiveHandles() returns
    //                    the JS wrappers (net.Server / net.Socket), never
    //                    timers -- probe: a pending setTimeout yields [].
    //   _activeRequests: in-flight request tokens ({ type }) for the fs
    //                    CALLBACK layer ("FSReqCallback"). Only the callback
    //                    layer is instrumented: node tags fs/promises work
    //                    "FSReqPromise" and oam's promise forms are the layer
    //                    the callback forms delegate to, so instrumenting both
    //                    would double-count every fs.open()/fs.readFile().
    //   _activeTimers:   installed by the timers block (id -> Timeout handle).
    _activeHandles: new Map(),
    _activeRequests: new Set(),
    /// Re-entry guard for internal/* resolution (see get()).
    _internalResolving: new Set(),
    get(name) {
      if (registry.cache.has(name)) return registry.cache.get(name);
      // internal/* (only reachable under --expose-internals; the resolver
      // refuses the specifier otherwise) comes from the vendored internal
      // registry -- the SAME modules the streams port runs on, not a
      // parallel set written to satisfy tests. A name that registry does
      // not define throws, which is the honest answer for an internal oam
      // genuinely does not have.
      // NOTE the order: a registered factory WINS. `node:internal/errors`
      // already resolved to node_compat's own codes, and intercepting
      // internal/* ahead of the factory lookup silently swapped it for the
      // vendored shim's class-based codes -- same names, different calling
      // convention, so existing callers broke.
      if (name.startsWith("internal/") && !registry.factories[name]) {
        const vendor = globalThis.__oamVendor;
        if (!vendor || typeof vendor.require !== "function") {
          throw new Error(`Cannot find module '${name}'`);
        }
        // The vendor loader falls back to THIS function for ids it has no
        // factory for, so a name neither side defines would bounce between
        // them until the stack blew. The guard turns that into the honest
        // "Cannot find module", which is also what the suite reclassifies
        // on.
        if (registry._internalResolving.has(name)) {
          throw new Error(`Cannot find module '${name}'`);
        }
        registry._internalResolving.add(name);
        try {
          return vendor.require(name);
        } finally {
          registry._internalResolving.delete(name);
        }
      }
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
    // captureRejections: when set on an instance (via opts.captureRejections),
    // emit() routes a listener's rejected thenable to the emitter's
    // nodejs.rejection handler instead of letting it become unhandled.
    const kCapture = Symbol("kCapture");

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
    function EventEmitter(opts) {
      // Node v22 EventEmitter.init guard: the vendored streams pre-shape
      // `_events` (readable.js:332, writable.js:404, duplex.js:76 -- hidden-
      // class stability + Node's documented eventNames() ORDER) BEFORE
      // calling this constructor. Only reset when unset or inherited from
      // the prototype; a pre-shaped own object is preserved. _eventsCount
      // still needs initializing on that path (streams set only _events).
      if (
        this._events === undefined ||
        this._events === Object.getPrototypeOf(this)?._events
      ) {
        this._events = { __proto__: null };
        this._eventsCount = 0;
      } else {
        this._eventsCount ??= 0;
      }
      if (opts && opts.captureRejections) this[kCapture] = true;
    }
    EventEmitter.prototype[kCapture] = false;
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
      // Prototype method, not this.listenerCount -- an instance may override/delete it
      // (test-stream-pipe-without-listenerCount sets it undefined); internal accounting
      // must not depend on the instance method.
      const count = EventEmitter.prototype.listenerCount.call(this, type);
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
      if (typeof listener !== "function") {
        throw new TypeError(`The "listener" argument must be a function`);
      }
      const wrapper = (...args) => {
        this.removeListener(type, wrapper);
        return listener.apply(this, args);
      };
      wrapper.listener = listener;
      this.on(type, wrapper);
      return this;
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
      const events = eventsOf(this);
      // Node emits 'removeListener' for every listener it drops here, and
      // that is not cosmetic: oam's own signal watchers DISARM on that event
      // (see the SIGNAL_NAMES wiring in the process factory). Dropping them
      // silently left the native SIGINT watcher armed after
      // removeAllListeners('SIGINT'), so the next SIGINT was swallowed
      // instead of killing the process -- an unkillable hang.
      //
      // Fast path when nobody is listening for removals, exactly as Node
      // does: no observable difference, no per-listener emit cost.
      if (events.removeListener === undefined) {
        if (type === undefined) {
          this._events = { __proto__: null };
          this._eventsCount = 0;
        } else if (events[type] !== undefined) {
          delete this._events[type];
          this._eventsCount--;
        }
        return this;
      }

      if (type === undefined) {
        // 'removeListener' itself goes LAST, so removals of the other
        // events are still observable while they happen.
        for (const key of Reflect.ownKeys(events)) {
          if (key === "removeListener") continue;
          this.removeAllListeners(key);
        }
        this.removeAllListeners("removeListener");
        this._events = { __proto__: null };
        this._eventsCount = 0;
        return this;
      }

      const listeners = events[type];
      if (typeof listeners === "function") {
        this.removeListener(type, listeners);
      } else if (listeners !== undefined) {
        // LIFO, and over a COPY: each removeListener() mutates the live
        // array (and a handler may add or remove more).
        const copy = listeners.slice();
        for (let i = copy.length - 1; i >= 0; i--) {
          this.removeListener(type, copy[i]);
        }
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
    EventEmitter.prototype.listenerCount = function (type, listener) {
      const existing = eventsOf(this)[type];
      if (existing === undefined) return 0;
      if (typeof existing === "function") {
        // 2-arg form: count only matching listeners (incl. once-wrappers).
        if (listener !== undefined) {
          return existing === listener || existing.listener === listener ? 1 : 0;
        }
        return 1;
      }
      if (listener !== undefined) {
        let count = 0;
        for (let i = 0; i < existing.length; i++) {
          if (existing[i] === listener || existing[i].listener === listener) count++;
        }
        return count;
      }
      return existing.length;
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
      const capture = this[kCapture] === true;
      for (const listener of list) {
        const result = listener.apply(this, args);
        // captureRejections: a listener returning a rejecting thenable is routed
        // to the emitter's nodejs.rejection handler (streams override it to
        // destroy(err)) instead of surfacing as an unhandled rejection.
        if (capture && result != null && typeof result.then === "function") {
          result.then(undefined, (err) => {
            queueMicrotask(() => {
              const onRejection = this[EventEmitter.captureRejectionSymbol];
              if (typeof onRejection === "function") {
                onRejection.call(this, err, type, ...args);
              } else {
                // Disable capture while emitting 'error' to avoid recursion.
                const prev = this[kCapture];
                try {
                  this[kCapture] = false;
                  this.emit("error", err);
                } finally {
                  this[kCapture] = prev;
                }
              }
            });
          });
        }
      }
      return true;
    };
    EventEmitter.defaultMaxListeners = 10;
    EventEmitter.errorMonitor = errorMonitor;

    function once(emitter, type, options) {
      // Node's once() is an async function, so a bad `options` surfaces as a
      // REJECTION, never a synchronous throw -- callers write
      // `await rejects(once(ee, 'x', 1), ...)`. Anything non-object (and
      // null, and arrays) is rejected rather than silently ignored, which is
      // what oam did: `once(ee, 'x', {signl: sig})` typo'd its way into a
      // promise that never settled.
      if (
        options !== undefined &&
        (options === null || typeof options !== "object" || Array.isArray(options))
      ) {
        return Promise.reject(
          new codes.ERR_INVALID_ARG_TYPE("options", "Object", options),
        );
      }
      const signal = options ? options.signal : undefined;
      // Node's validateAbortSignal, and it runs BEFORE the aborted check:
      // duck-typed on 'aborted' rather than instanceof, so a signal from
      // another realm still works while `{}` or a stray number does not.
      if (
        signal !== undefined &&
        (signal === null || typeof signal !== "object" || !("aborted" in signal))
      ) {
        return Promise.reject(
          new codes.ERR_INVALID_ARG_TYPE("options.signal", "AbortSignal", signal),
        );
      }
      if (signal && signal.aborted) {
        return Promise.reject(
          new (globalThis.DOMException ?? Error)(
            "The operation was aborted",
            "AbortError",
          ),
        );
      }
      // The emitter has to be one or the other. An AbortController, say,
      // is neither (its SIGNAL is the target), and oam used to return a
      // promise that simply never settled.
      if (
        typeof emitter?.once !== "function" &&
        typeof emitter?.addEventListener !== "function"
      ) {
        return Promise.reject(
          new codes.ERR_INVALID_ARG_TYPE(
            "emitter",
            ["EventEmitter", "EventTarget"],
            emitter,
          ),
        );
      }
      // Support BOTH EventEmitter (.once/.removeListener) and EventTarget
      // (AbortSignal et al, .addEventListener/.removeEventListener), the way
      // Node's events.once does -- the stream operator tests await
      // once(abortSignal, 'abort') inside a mapper.
      const isEventTarget =
        typeof emitter.once !== "function" &&
        typeof emitter.addEventListener === "function";
      return new Promise((resolve, reject) => {
        // EVERY settle path has to unhook EVERYTHING -- the event listener,
        // the error listener, and the signal's 'abort' listener. Leaving the
        // abort listener attached kept the promise's closure (and the
        // emitter) reachable from a long-lived AbortSignal for as long as
        // the signal itself lived: a leak for any caller that reuses one
        // signal across many once() calls, which is the normal pattern.
        let onAbort;
        const cleanup = () => {
          if (isEventTarget) {
            emitter.removeEventListener(type, onEvent);
            if (typeof emitter.removeEventListener === "function") {
              emitter.removeEventListener("error", onError);
            }
          } else {
            emitter.removeListener(type, onEvent);
            emitter.removeListener("error", onError);
          }
          if (signal && onAbort) signal.removeEventListener("abort", onAbort);
        };
        const onEvent = (...args) => {
          cleanup();
          resolve(args);
        };
        const onError = (err) => {
          cleanup();
          reject(err);
        };
        if (isEventTarget) {
          emitter.addEventListener(type, onEvent, { once: true });
        } else {
          emitter.once(type, onEvent);
          if (type !== "error") emitter.once("error", onError);
        }
        if (signal) {
          onAbort = () => {
            cleanup();
            reject(
              new (globalThis.DOMException ?? Error)(
                "The operation was aborted",
                "AbortError",
              ),
            );
          };
          // kResistStopPropagation: an UNRELATED abort listener calling
          // stopImmediatePropagation must not starve this one -- otherwise
          // the promise never settles and the caller hangs forever. Node
          // marks its own internal abort listeners the same way.
          signal.addEventListener("abort", onAbort, {
            once: true,
            [Symbol.for("oam.kResistStopPropagation")]: true,
          });
        }
      });
    }

    function on(emitter, event, options) {
      // Same validation contract as once(): a plain object, a bad options
      // bag, or a non-AbortSignal signal used to surface as whatever
      // TypeError happened to fall out first ("emitter.on is not a
      // function"), with no code for a caller to branch on.
      if (
        options !== undefined &&
        (options === null || typeof options !== "object" || Array.isArray(options))
      ) {
        throw new codes.ERR_INVALID_ARG_TYPE("options", "Object", options);
      }
      var signal = options && options.signal;
      if (
        signal !== undefined &&
        (signal === null || typeof signal !== "object" || !("aborted" in signal))
      ) {
        throw new codes.ERR_INVALID_ARG_TYPE("options.signal", "AbortSignal", signal);
      }
      if (
        typeof emitter?.on !== "function" &&
        typeof emitter?.addEventListener !== "function"
      ) {
        throw new codes.ERR_INVALID_ARG_TYPE(
          "emitter",
          ["EventEmitter", "EventTarget"],
          emitter,
        );
      }
      var unconsumed = [];
      var waiting = [];
      var error = null;
      var done = false;
      // An error is reported to exactly one consumer; after that the
      // iterator simply reads as finished.
      var errorDelivered = false;

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
        // The iterator is finished the moment it errors: unhook now rather
        // than waiting for a return() the consumer will never make.
        cleanup();
        var w = waiting.slice();
        waiting.length = 0;
        // The error is delivered ONCE, to the FIRST waiter; everyone else
        // sees a finished iterator. Rejecting every pending next() with the
        // same error made three concurrent next() calls produce three
        // rejections where node produces one rejection and two dones.
        for (var i = 0; i < w.length; i++) {
          if (i === 0 && !errorDelivered) {
            errorDelivered = true;
            w[i].reject(err);
          } else {
            w[i].resolve({ value: undefined, done: true });
          }
        }
        done = true;
      }

      function makeAbortErrorForOn(reason) {
        var err = new Error("The operation was aborted");
        err.code = "ABORT_ERR";
        err.name = "AbortError";
        if (reason !== undefined) err.cause = reason;
        return err;
      }
      function abortHandler() {
        errorHandler(makeAbortErrorForOn(signal && signal.reason));
      }

      // ONE teardown for every way this iterator can finish. It used to
      // live only in return()/throw(), and a rejected next() does NOT
      // trigger return() -- so the ERROR path left both listeners attached
      // to the emitter forever. The signal's abort listener was never
      // removed on any path.
      // EventTarget has no .on/.removeListener. on() only ever spoke the
      // EventEmitter dialect, so passing an EventTarget (an AbortSignal,
      // say) died with "emitter.on is not a function" -- even though the
      // async-iterator contract is identical for both.
      var isTarget = typeof emitter.on !== "function";
      var attach = isTarget
        ? function (type, fn) { emitter.addEventListener(type, fn); }
        : function (type, fn) { emitter.on(type, fn); };
      var detach = isTarget
        ? function (type, fn) { emitter.removeEventListener(type, fn); }
        : function (type, fn) { emitter.removeListener(type, fn); };

      var cleanedUp = false;
      function cleanup() {
        if (cleanedUp) return;
        cleanedUp = true;
        detach(event, eventHandler);
        if (event !== "error") detach("error", errorHandler);
        if (signal) signal.removeEventListener?.("abort", abortHandler);
      }

      attach(event, eventHandler);
      if (event !== "error") attach("error", errorHandler);
      if (signal) {
        if (signal.aborted) {
          // Already aborted: throw SYNCHRONOUSLY. Routing it through the
          // async error path handed back an iterator that looked usable
          // and only failed on first next(), so `assert.throws(() => on(...))`
          // saw nothing thrown.
          cleanup();
          throw makeAbortErrorForOn(signal.reason);
        }
        signal.addEventListener("abort", abortHandler, { once: true });
      }

      var iterator = {
        next: function() {
          if (unconsumed.length > 0) {
            return Promise.resolve({ value: unconsumed.shift(), done: false });
          }
          if (error && !errorDelivered) {
            errorDelivered = true;
            return Promise.reject(error);
          }
          if (done || error) {
            return Promise.resolve({ value: undefined, done: true });
          }
          return new Promise(function(resolve, reject) {
            waiting.push({ resolve: resolve, reject: reject });
          });
        },
        return: function() {
          done = true;
          cleanup();
          var w = waiting.slice();
          waiting.length = 0;
          for (var i = 0; i < w.length; i++) w[i].resolve({ value: undefined, done: true });
          return Promise.resolve({ value: undefined, done: true });
        },
        throw: function(err) {
          // throw() takes an ERROR. Accepting undefined finished the
          // iterator with a rejection nobody could interpret; node
          // validates synchronously and says what it wanted.
          if (!(err instanceof Error)) {
            throw new codes.ERR_INVALID_ARG_TYPE(
              "EventEmitter.AsyncIterator",
              "Error",
              err,
            );
          }
          // Route through the SAME error path a real 'error' event takes:
          // that is what rejects the already-pending next(), which is how
          // the for-await consumer actually observes the throw. Setting
          // `error` and returning a rejected promise left the pending
          // waiter hanging (and produced a rejection nobody was there to
          // handle). Returns undefined, as node's does.
          errorHandler(err);
          return undefined;
        },
      };
      iterator[Symbol.asyncIterator] = function() { return iterator; };
      return iterator;
    }

    function getEventListeners(emitter, name) {
      // Validate before touching it. A non-emitter used to reach the property
      // reads below and come back as `[]` -- "nothing is listening" is a
      // plausible-looking answer to a question that was never askable, and a
      // typo'd argument silently passed a leak check.
      if (
        emitter === null ||
        (typeof emitter !== "object" && typeof emitter !== "function") ||
        (typeof emitter.listeners !== "function" && !emitter._listeners)
      ) {
        throw new codes.ERR_INVALID_ARG_TYPE(
          "emitter",
          ["EventEmitter", "EventTarget"],
          emitter,
        );
      }
      if (typeof emitter.listeners === "function") return emitter.listeners(name);
      // EventTarget (AbortSignal and friends) has no .listeners(): read the
      // internal registry the bootstrap EventTarget keeps. Without this the
      // function silently returned [] for EVERY EventTarget, so callers
      // auditing listener counts -- leak checks especially -- were told
      // "none attached" no matter what was actually registered.
      const registry = emitter?._listeners;
      if (registry && typeof registry.get === "function") {
        // Entry shape is bootstrap's EventTarget: `fn` when held strongly,
        // `ref` (a WeakRef) when registered with kWeakHandler. A weak listener
        // that has already been collected is reported as gone rather than as
        // an `undefined` sitting in the array.
        return (registry.get(name) ?? [])
          .map((e) => (e.ref ? e.ref.deref() : e.fn))
          .filter((fn) => fn !== undefined);
      }
      return [];
    }

    // Internal slot used to attach a max-listeners value to a native
    // EventTarget (which has no get/setMaxListeners method of its own), the way
    // Node stores it via kMaxEventTargetListeners.
    const kMaxEventTargetListeners = Symbol("kMaxEventTargetListeners");

    function setMaxListeners(n) {
      if (arguments.length > 1) {
        for (var i = 1; i < arguments.length; i++) {
          const target = arguments[i];
          if (typeof target.setMaxListeners === "function") {
            target.setMaxListeners(n);
          } else {
            // Native EventTarget / AbortSignal: stash the value in a slot.
            Object.defineProperty(target, kMaxEventTargetListeners, {
              value: n,
              writable: true,
              configurable: true,
              enumerable: false,
            });
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
    EventEmitter.listenerCount = (emitter, type, listener) =>
      EventEmitter.prototype.listenerCount.call(emitter, type, listener);
    EventEmitter.getMaxListeners = function getMaxListeners(emitterOrTarget) {
      if (typeof emitterOrTarget.getMaxListeners === "function") {
        return emitterOrTarget.getMaxListeners();
      }
      if (
        emitterOrTarget != null &&
        emitterOrTarget[kMaxEventTargetListeners] !== undefined
      ) {
        return emitterOrTarget[kMaxEventTargetListeners];
      }
      // An AbortSignal defaults to 0 listeners; a bare EventTarget to the
      // module default.
      if (typeof AbortSignal === "function" && emitterOrTarget instanceof AbortSignal) {
        return 0;
      }
      return EventEmitter.defaultMaxListeners;
    };
    EventEmitter.addAbortListener = function addAbortListener(signal, listener) {
      // Duck-typed like Node's validateAbortSignal ('aborted' in signal), NOT
      // instanceof: polyfilled signals (abort-controller npm pkg) must pass,
      // and the vendored streams' every {signal} entry point funnels here.
      if (signal === null || typeof signal !== "object" || !("aborted" in signal)) {
        throw new codes.ERR_INVALID_ARG_TYPE("signal", "AbortSignal", signal);
      }
      if (typeof listener !== "function") {
        throw new codes.ERR_INVALID_ARG_TYPE("listener", "function", listener);
      }
      if (signal.aborted) {
        queueMicrotask(() => listener());
        return { [Symbol.dispose]() {} };
      }
      // Resist flag: Node registers this listener with kResistStopPropagation
      // so it still fires after stopImmediatePropagation() (test-events-
      // add-abort-listener subtest 6). Shared symbol consumed by the
      // bootstrap EventTarget.
      signal.addEventListener("abort", listener, {
        once: true,
        [Symbol.for("oam.kResistStopPropagation")]: true,
      });
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
  // Faithful port of Node v22's lib/path.js (win32 + posix), char-code driven
  // so the many edge cases (UNC roots, '\\?\' device paths, drive-relative
  // 'C:.', trailing './' preservation, '..' above-root handling) match Node
  // exactly. The two platform objects share normalizeString().
  function makePathModule(isWin, natives) {
    const CHAR_DOT = 46;
    const CHAR_FORWARD_SLASH = 47;
    const CHAR_BACKWARD_SLASH = 92;
    const CHAR_COLON = 58;
    const CHAR_QUESTION_MARK = 63;
    const CHAR_UPPERCASE_A = 65;
    const CHAR_LOWERCASE_A = 97;
    const CHAR_UPPERCASE_Z = 90;
    const CHAR_LOWERCASE_Z = 122;
    const cc = (s, i) => s.charCodeAt(i);

    const isPathSeparator = isWin
      ? (code) => code === CHAR_FORWARD_SLASH || code === CHAR_BACKWARD_SLASH
      : (code) => code === CHAR_FORWARD_SLASH;
    const isPosixPathSeparator = (code) => code === CHAR_FORWARD_SLASH;
    const isWindowsDeviceRoot = (code) =>
      (code >= CHAR_UPPERCASE_A && code <= CHAR_UPPERCASE_Z) ||
      (code >= CHAR_LOWERCASE_A && code <= CHAR_LOWERCASE_Z);

    // Faithful port of Node's internal isWindowsReservedName. `index` is the
    // length of the device-name slice to inspect (the position of ':').
    // Reserved: CON/PRN/AUX/NUL and COM/LPT followed by a single 1-9 or the
    // superscript digits 0xB2/0xB3/0xB9. COM0, COM10, COMa, CONOUT, etc. are
    // NOT reserved.
    function isWindowsReservedName(str, index) {
      if (index !== 3 && index !== 4) return false;
      let i = 0;
      const first = str.charCodeAt(0) | 0x20; // lowercase
      const second = str.charCodeAt(1) | 0x20;
      const third = str.charCodeAt(2) | 0x20;
      // CON / PRN / AUX / NUL (index === 3)
      if (index === 3) {
        if (first === 0x63 && second === 0x6f && third === 0x6e) return true; // con
        if (first === 0x70 && second === 0x72 && third === 0x6e) return true; // prn
        if (first === 0x61 && second === 0x75 && third === 0x78) return true; // aux
        if (first === 0x6e && second === 0x75 && third === 0x6c) return true; // nul
        return false;
      }
      // COM<n> / LPT<n>  (index === 4)
      const isCom = first === 0x63 && second === 0x6f && third === 0x6d; // com
      const isLpt = first === 0x6c && second === 0x70 && third === 0x74; // lpt
      if (!isCom && !isLpt) return false;
      const fourth = str.charCodeAt(3);
      // '1'..'9'
      if (fourth >= 0x31 && fourth <= 0x39) return true;
      // superscript 1 (0xB9), 2 (0xB2), 3 (0xB3)
      if (fourth === 0xb9 || fourth === 0xb2 || fourth === 0xb3) return true;
      void i;
      return false;
    }

    function assertPath(p, name) {
      if (typeof p !== "string") {
        throw new codes.ERR_INVALID_ARG_TYPE(name || "path", "string", p);
      }
    }

    // Resolves . and .. elements in a path with directory names. `separator`
    // is the single-char separator to emit; `isSep` tests for a boundary.
    function normalizeString(path, allowAboveRoot, separator, isSep) {
      let res = "";
      let lastSegmentLength = 0;
      let lastSlash = -1;
      let dots = 0;
      let code = 0;
      for (let i = 0; i <= path.length; ++i) {
        if (i < path.length) code = cc(path, i);
        else if (isSep(code)) break;
        else code = CHAR_FORWARD_SLASH;

        if (isSep(code)) {
          if (lastSlash === i - 1 || dots === 1) {
            // NOOP
          } else if (dots === 2) {
            if (
              res.length < 2 ||
              lastSegmentLength !== 2 ||
              cc(res, res.length - 1) !== CHAR_DOT ||
              cc(res, res.length - 2) !== CHAR_DOT
            ) {
              if (res.length > 2) {
                const lastSlashIndex = res.lastIndexOf(separator);
                if (lastSlashIndex === -1) {
                  res = "";
                  lastSegmentLength = 0;
                } else {
                  res = res.slice(0, lastSlashIndex);
                  lastSegmentLength = res.length - 1 - res.lastIndexOf(separator);
                }
                lastSlash = i;
                dots = 0;
                continue;
              } else if (res.length !== 0) {
                res = "";
                lastSegmentLength = 0;
                lastSlash = i;
                dots = 0;
                continue;
              }
            }
            if (allowAboveRoot) {
              res += res.length > 0 ? `${separator}..` : "..";
              lastSegmentLength = 2;
            }
          } else {
            if (res.length > 0) res += `${separator}${path.slice(lastSlash + 1, i)}`;
            else res = path.slice(lastSlash + 1, i);
            lastSegmentLength = i - lastSlash - 1;
          }
          lastSlash = i;
          dots = 0;
        } else if (code === CHAR_DOT && dots !== -1) {
          ++dots;
        } else {
          dots = -1;
        }
      }
      return res;
    }

    function formatExt(ext) {
      return ext ? `${ext[0] === "." ? "" : "."}${ext}` : "";
    }

    function _format(sep, pathObject) {
      if (pathObject === null || typeof pathObject !== "object") {
        throw new codes.ERR_INVALID_ARG_TYPE("pathObject", "object", pathObject);
      }
      const dir = pathObject.dir || pathObject.root;
      const base =
        pathObject.base || `${pathObject.name || ""}${formatExt(pathObject.ext)}`;
      if (!dir) return base;
      return dir === pathObject.root ? `${dir}${base}` : `${dir}${sep}${base}`;
    }

    // Node's win32 path.resolve reads process.cwd() (the patchable JS
    // function), not an internal syscall -- same contract as the posix
    // side's posixCwd. The default process.cwd IS natives.cwd(), so this
    // only differs once someone patches it.
    const win32ProcessCwd = () => {
      const proc = globalThis.process;
      if (proc && typeof proc.cwd === "function") return proc.cwd();
      return natives ? natives.cwd() : "/";
    };

    let mod;
    if (isWin) {
      mod = {
        resolve(...args) {
          let resolvedDevice = "";
          let resolvedTail = "";
          let resolvedAbsolute = false;
          for (let i = args.length - 1; i >= -1; i--) {
            let path;
            if (i >= 0) {
              path = args[i];
              assertPath(path, `paths[${i}]`);
              if (path.length === 0) continue;
            } else if (resolvedDevice.length === 0) {
              // process.cwd(), not natives.cwd(): Node reads the patchable
              // JS function here too (see the posix posixCwd note).
              path = win32ProcessCwd();
            } else {
              // Windows has per-drive CWDs; oam tracks only the process CWD,
              // so fall back to it when the device matches, else the drive root.
              const envCwd = win32ProcessCwd();
              path =
                envCwd.slice(0, 2).toLowerCase() === resolvedDevice.toLowerCase()
                  ? envCwd
                  : `${resolvedDevice}\\`;
              if (path === undefined) path = `${resolvedDevice}\\`;
            }

            const len = path.length;
            let rootEnd = 0;
            let device = "";
            let isAbsolute = false;
            const code = cc(path, 0);
            if (len === 1) {
              if (isPathSeparator(code)) {
                rootEnd = 1;
                isAbsolute = true;
              }
            } else if (isPathSeparator(code)) {
              isAbsolute = true;
              if (isPathSeparator(cc(path, 1))) {
                let j = 2;
                let last = j;
                while (j < len && !isPathSeparator(cc(path, j))) j++;
                if (j < len && j !== last) {
                  const firstPart = path.slice(last, j);
                  last = j;
                  while (j < len && isPathSeparator(cc(path, j))) j++;
                  if (j < len && j !== last) {
                    last = j;
                    while (j < len && !isPathSeparator(cc(path, j))) j++;
                    if (j === len || j !== last) {
                      if (firstPart !== "." && firstPart !== "?") {
                        // UNC root
                        device = `\\\\${firstPart}\\${path.slice(last, j)}`;
                        rootEnd = j;
                      } else {
                        // device root (e.g. \\.\PHYSICALDRIVE0)
                        device = `\\\\${firstPart}`;
                        rootEnd = 4;
                      }
                    }
                  }
                }
              } else {
                rootEnd = 1;
              }
            } else if (isWindowsDeviceRoot(code) && cc(path, 1) === CHAR_COLON) {
              device = path.slice(0, 2);
              rootEnd = 2;
              if (len > 2 && isPathSeparator(cc(path, 2))) {
                isAbsolute = true;
                rootEnd = 3;
              }
            }

            if (device.length > 0) {
              if (resolvedDevice.length > 0) {
                if (device.toLowerCase() !== resolvedDevice.toLowerCase()) continue;
              } else {
                resolvedDevice = device;
              }
            }

            if (resolvedAbsolute) {
              if (resolvedDevice.length > 0) break;
            } else {
              resolvedTail = `${path.slice(rootEnd)}\\${resolvedTail}`;
              resolvedAbsolute = isAbsolute;
              if (isAbsolute && resolvedDevice.length > 0) break;
            }
          }

          resolvedTail = normalizeString(
            resolvedTail,
            !resolvedAbsolute,
            "\\",
            isPathSeparator,
          );

          return resolvedAbsolute
            ? `${resolvedDevice}\\${resolvedTail}`
            : `${resolvedDevice}${resolvedTail}` || ".";
        },

        normalize(path) {
          assertPath(path);
          const len = path.length;
          if (len === 0) return ".";
          let rootEnd = 0;
          let device;
          let isAbsolute = false;
          const code = cc(path, 0);

          if (len === 1) {
            return isPosixPathSeparator(code) ? "\\" : path;
          }

          if (isPathSeparator(code)) {
            isAbsolute = true;
            if (isPathSeparator(cc(path, 1))) {
              let j = 2;
              let last = j;
              while (j < len && !isPathSeparator(cc(path, j))) j++;
              if (j < len && j !== last) {
                const firstPart = path.slice(last, j);
                last = j;
                while (j < len && isPathSeparator(cc(path, j))) j++;
                if (j < len && j !== last) {
                  last = j;
                  while (j < len && !isPathSeparator(cc(path, j))) j++;
                  if (j === len || j !== last) {
                    if (firstPart === "." || firstPart === "?") {
                      // device root (e.g. \\.\PHYSICALDRIVE0)
                      device = `\\\\${firstPart}`;
                      rootEnd = 4;
                      const colonIndex = path.indexOf(":");
                      const possibleDevice = path.slice(4, colonIndex + 1);
                      if (
                        isWindowsReservedName(possibleDevice, possibleDevice.length - 1)
                      ) {
                        device = `\\\\?\\${possibleDevice}`;
                        rootEnd = 4 + possibleDevice.length;
                      }
                    } else if (j === len) {
                      // UNC root only -- nothing left to process
                      return `\\\\${firstPart}\\${path.slice(last)}\\`;
                    } else {
                      // UNC root with leftovers
                      device = `\\\\${firstPart}\\${path.slice(last, j)}`;
                      rootEnd = j;
                    }
                  }
                }
              }
            } else {
              rootEnd = 1;
            }
          } else {
            const colonIndex = path.indexOf(":");
            if (colonIndex > 0) {
              if (isWindowsDeviceRoot(code) && colonIndex === 1) {
                device = path.slice(0, 2);
                rootEnd = 2;
                if (len > 2 && isPathSeparator(cc(path, 2))) {
                  isAbsolute = true;
                  rootEnd = 3;
                }
              } else if (isWindowsReservedName(path, colonIndex)) {
                device = path.slice(0, colonIndex + 1);
                rootEnd = colonIndex + 1;
              }
            }
          }

          let tail =
            rootEnd < len
              ? normalizeString(path.slice(rootEnd), !isAbsolute, "\\", isPathSeparator)
              : "";
          if (tail.length === 0 && !isAbsolute) tail = ".";
          if (tail.length > 0 && isPathSeparator(cc(path, len - 1))) tail += "\\";
          if (!isAbsolute && device === undefined && path.includes(":")) {
            // CVE-2024-36139: keep a relative path from becoming something
            // Windows could read as absolute.
            if (
              tail.length >= 2 &&
              isWindowsDeviceRoot(cc(tail, 0)) &&
              cc(tail, 1) === CHAR_COLON
            ) {
              return `.\\${tail}`;
            }
            let index = path.indexOf(":");
            do {
              if (index === len - 1 || isPathSeparator(cc(path, index + 1))) {
                return `.\\${tail}`;
              }
            } while ((index = path.indexOf(":", index + 1)) !== -1);
          }
          const colonIndex2 = path.indexOf(":");
          if (isWindowsReservedName(path, colonIndex2)) {
            return `.\\${device ?? ""}${tail}`;
          }
          if (device === undefined) {
            return isAbsolute ? `\\${tail}` : tail;
          }
          return isAbsolute ? `${device}\\${tail}` : `${device}${tail}`;
        },

        isAbsolute(path) {
          assertPath(path);
          const len = path.length;
          if (len === 0) return false;
          const code = cc(path, 0);
          return (
            isPathSeparator(code) ||
            (len > 2 &&
              isWindowsDeviceRoot(code) &&
              cc(path, 1) === CHAR_COLON &&
              isPathSeparator(cc(path, 2)))
          );
        },

        join(...args) {
          if (args.length === 0) return ".";
          let joined;
          let firstPart;
          for (let i = 0; i < args.length; ++i) {
            const arg = args[i];
            assertPath(arg);
            if (arg.length > 0) {
              if (joined === undefined) joined = firstPart = arg;
              else joined += `\\${arg}`;
            }
          }
          if (joined === undefined) return ".";

          let needsReplace = true;
          let slashCount = 0;
          if (isPathSeparator(cc(firstPart, 0))) {
            ++slashCount;
            const firstLen = firstPart.length;
            if (firstLen > 1 && isPathSeparator(cc(firstPart, 1))) {
              ++slashCount;
              if (firstLen > 2) {
                if (isPathSeparator(cc(firstPart, 2))) ++slashCount;
                else {
                  needsReplace = false;
                }
              }
            }
          }
          if (needsReplace) {
            while (slashCount < joined.length && isPathSeparator(cc(joined, slashCount)))
              slashCount++;
            if (slashCount >= 2) joined = `\\${joined.slice(slashCount)}`;
          }

          // Skip normalization when a reserved device name (CON, COM1, ...)
          // appears in any segment -- normalize would otherwise eat it.
          const parts = [];
          let part = "";
          for (let i = 0; i < joined.length; i++) {
            if (joined[i] === "\\") {
              if (part) parts.push(part);
              part = "";
              while (i + 1 < joined.length && joined[i + 1] === "\\") i++;
            } else {
              part += joined[i];
            }
          }
          if (part) parts.push(part);
          if (
            parts.some((p) => {
              const colonIndex = p.indexOf(":");
              return colonIndex !== -1 && isWindowsReservedName(p, colonIndex);
            })
          ) {
            let result = "";
            for (let i = 0; i < joined.length; i++) {
              result += joined[i] === "/" ? "\\" : joined[i];
            }
            return result;
          }

          return mod.normalize(joined);
        },

        relative(from, to) {
          assertPath(from, "from");
          assertPath(to, "to");
          if (from === to) return "";

          const fromOrig = mod.resolve(from);
          const toOrig = mod.resolve(to);
          if (fromOrig === toOrig) return "";

          from = fromOrig.toLowerCase();
          to = toOrig.toLowerCase();
          if (from === to) return "";

          // When lowercasing changed the string LENGTH (e.g. the Turkish
          // dotted-I), the char-index arithmetic below would be off, so fall
          // back to a segment-wise comparison on the original-cased paths.
          if (fromOrig.length !== from.length || toOrig.length !== to.length) {
            const fromSplit = fromOrig.split("\\");
            const toSplit = toOrig.split("\\");
            if (fromSplit[fromSplit.length - 1] === "") fromSplit.pop();
            if (toSplit[toSplit.length - 1] === "") toSplit.pop();
            const fLen = fromSplit.length;
            const tLen = toSplit.length;
            const lim = fLen < tLen ? fLen : tLen;
            let k;
            for (k = 0; k < lim; k++) {
              if (fromSplit[k].toLowerCase() !== toSplit[k].toLowerCase()) break;
            }
            if (k === 0) return toOrig;
            if (k === lim) {
              if (tLen > lim) return toSplit.slice(k).join("\\");
              if (fLen > lim) return "..\\".repeat(fLen - 1 - k) + "..";
              return "";
            }
            return "..\\".repeat(fLen - k) + toSplit.slice(k).join("\\");
          }

          let fromStart = 0;
          while (
            fromStart < from.length &&
            cc(from, fromStart) === CHAR_BACKWARD_SLASH
          )
            fromStart++;
          let fromEnd = from.length;
          while (
            fromEnd - 1 > fromStart &&
            cc(from, fromEnd - 1) === CHAR_BACKWARD_SLASH
          )
            fromEnd--;
          const fromLen = fromEnd - fromStart;

          let toStart = 0;
          while (toStart < to.length && cc(to, toStart) === CHAR_BACKWARD_SLASH)
            toStart++;
          let toEnd = to.length;
          while (toEnd - 1 > toStart && cc(to, toEnd - 1) === CHAR_BACKWARD_SLASH)
            toEnd--;
          const toLen = toEnd - toStart;

          const length = fromLen < toLen ? fromLen : toLen;
          let lastCommonSep = -1;
          let i = 0;
          for (; i < length; i++) {
            const fromCode = cc(from, fromStart + i);
            if (fromCode !== cc(to, toStart + i)) break;
            else if (fromCode === CHAR_BACKWARD_SLASH) lastCommonSep = i;
          }

          if (i !== length) {
            if (lastCommonSep === -1) return toOrig;
          } else {
            if (toLen > length) {
              if (cc(to, toStart + i) === CHAR_BACKWARD_SLASH) {
                return toOrig.slice(toStart + i + 1);
              }
              if (i === 2) {
                return toOrig.slice(toStart + i);
              }
            }
            if (fromLen > length) {
              if (cc(from, fromStart + i) === CHAR_BACKWARD_SLASH) lastCommonSep = i;
              else if (i === 2) lastCommonSep = 3;
            }
            if (lastCommonSep === -1) lastCommonSep = 0;
          }

          let out = "";
          for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
            if (i === fromEnd || cc(from, i) === CHAR_BACKWARD_SLASH) {
              out += out.length === 0 ? ".." : "\\..";
            }
          }

          toStart += lastCommonSep;
          if (out.length > 0) {
            return `${out}${toOrig.slice(toStart, toEnd)}`;
          }
          if (cc(toOrig, toStart) === CHAR_BACKWARD_SLASH) ++toStart;
          return toOrig.slice(toStart, toEnd);
        },

        toNamespacedPath(path) {
          if (typeof path !== "string" || path.length === 0) return path;
          const resolvedPath = mod.resolve(path);
          if (resolvedPath.length <= 2) return path;

          if (cc(resolvedPath, 0) === CHAR_BACKWARD_SLASH) {
            if (cc(resolvedPath, 1) === CHAR_BACKWARD_SLASH) {
              const code = cc(resolvedPath, 2);
              if (code !== CHAR_QUESTION_MARK && code !== CHAR_DOT) {
                return `\\\\?\\UNC\\${resolvedPath.slice(2)}`;
              }
            }
          } else if (
            isWindowsDeviceRoot(cc(resolvedPath, 0)) &&
            cc(resolvedPath, 1) === CHAR_COLON &&
            cc(resolvedPath, 2) === CHAR_BACKWARD_SLASH
          ) {
            return `\\\\?\\${resolvedPath}`;
          }
          return resolvedPath;
        },

        dirname(path) {
          assertPath(path);
          const len = path.length;
          if (len === 0) return ".";
          let rootEnd = -1;
          let offset = 0;
          const code = cc(path, 0);

          if (len === 1) {
            return isPathSeparator(code) ? path : ".";
          }

          if (isPathSeparator(code)) {
            rootEnd = offset = 1;
            if (isPathSeparator(cc(path, 1))) {
              let j = 2;
              let last = j;
              while (j < len && !isPathSeparator(cc(path, j))) j++;
              if (j < len && j !== last) {
                last = j;
                while (j < len && isPathSeparator(cc(path, j))) j++;
                if (j < len && j !== last) {
                  last = j;
                  while (j < len && !isPathSeparator(cc(path, j))) j++;
                  if (j === len) return path;
                  if (j !== last) rootEnd = offset = j + 1;
                }
              }
            }
          } else if (isWindowsDeviceRoot(code) && cc(path, 1) === CHAR_COLON) {
            rootEnd = len > 2 && isPathSeparator(cc(path, 2)) ? 3 : 2;
            offset = rootEnd;
          }

          let end = -1;
          let matchedSlash = true;
          for (let i = len - 1; i >= offset; --i) {
            if (isPathSeparator(cc(path, i))) {
              if (!matchedSlash) {
                end = i;
                break;
              }
            } else {
              matchedSlash = false;
            }
          }

          if (end === -1) {
            if (rootEnd === -1) return ".";
            end = rootEnd;
          }
          return path.slice(0, end);
        },

        basename(path, suffix) {
          if (suffix !== undefined) assertPath(suffix, "suffix");
          assertPath(path);
          let start = 0;
          let end = -1;
          let matchedSlash = true;

          if (
            path.length >= 2 &&
            isWindowsDeviceRoot(cc(path, 0)) &&
            cc(path, 1) === CHAR_COLON
          ) {
            start = 2;
          }

          if (suffix !== undefined && suffix.length > 0 && suffix.length <= path.length) {
            if (suffix === path) return "";
            let extIdx = suffix.length - 1;
            let firstNonSlashEnd = -1;
            for (let i = path.length - 1; i >= start; --i) {
              const code = cc(path, i);
              if (isPathSeparator(code)) {
                if (!matchedSlash) {
                  start = i + 1;
                  break;
                }
              } else {
                if (firstNonSlashEnd === -1) {
                  matchedSlash = false;
                  firstNonSlashEnd = i + 1;
                }
                if (extIdx >= 0) {
                  if (code === cc(suffix, extIdx)) {
                    if (--extIdx === -1) {
                      end = i;
                    }
                  } else {
                    extIdx = -1;
                    end = firstNonSlashEnd;
                  }
                }
              }
            }
            if (start === end) end = firstNonSlashEnd;
            else if (end === -1) end = path.length;
            return path.slice(start, end);
          }
          for (let i = path.length - 1; i >= start; --i) {
            if (isPathSeparator(cc(path, i))) {
              if (!matchedSlash) {
                start = i + 1;
                break;
              }
            } else if (end === -1) {
              matchedSlash = false;
              end = i + 1;
            }
          }
          if (end === -1) return "";
          return path.slice(start, end);
        },

        extname(path) {
          assertPath(path);
          let start = 0;
          let startDot = -1;
          let startPart = 0;
          let end = -1;
          let matchedSlash = true;
          let preDotState = 0;

          if (
            path.length >= 2 &&
            cc(path, 1) === CHAR_COLON &&
            isWindowsDeviceRoot(cc(path, 0))
          ) {
            start = startPart = 2;
          }

          for (let i = path.length - 1; i >= start; --i) {
            const code = cc(path, i);
            if (isPathSeparator(code)) {
              if (!matchedSlash) {
                startPart = i + 1;
                break;
              }
              continue;
            }
            if (end === -1) {
              matchedSlash = false;
              end = i + 1;
            }
            if (code === CHAR_DOT) {
              if (startDot === -1) startDot = i;
              else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
              preDotState = -1;
            }
          }

          if (
            startDot === -1 ||
            end === -1 ||
            preDotState === 0 ||
            (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
          ) {
            return "";
          }
          return path.slice(startDot, end);
        },

        format: _format.bind(null, "\\"),

        parse(path) {
          assertPath(path);
          const ret = { root: "", dir: "", base: "", ext: "", name: "" };
          if (path.length === 0) return ret;
          const len = path.length;
          let rootEnd = 0;
          let code = cc(path, 0);

          if (len === 1) {
            if (isPathSeparator(code)) {
              ret.root = ret.dir = path;
              return ret;
            }
            ret.base = ret.name = path;
            return ret;
          }

          if (isPathSeparator(code)) {
            rootEnd = 1;
            if (isPathSeparator(cc(path, 1))) {
              let j = 2;
              let last = j;
              while (j < len && !isPathSeparator(cc(path, j))) j++;
              if (j < len && j !== last) {
                last = j;
                while (j < len && isPathSeparator(cc(path, j))) j++;
                if (j < len && j !== last) {
                  last = j;
                  while (j < len && !isPathSeparator(cc(path, j))) j++;
                  if (j === len) rootEnd = j;
                  else if (j !== last) rootEnd = j + 1;
                }
              }
            }
          } else if (isWindowsDeviceRoot(code) && cc(path, 1) === CHAR_COLON) {
            if (len <= 2) {
              ret.root = ret.dir = path;
              return ret;
            }
            rootEnd = 2;
            if (isPathSeparator(cc(path, 2))) {
              if (len === 3) {
                ret.root = ret.dir = path;
                return ret;
              }
              rootEnd = 3;
            }
          }
          if (rootEnd > 0) ret.root = path.slice(0, rootEnd);

          let startDot = -1;
          let startPart = rootEnd;
          let end = -1;
          let matchedSlash = true;
          let i = len - 1;
          let preDotState = 0;

          for (; i >= rootEnd; --i) {
            code = cc(path, i);
            if (isPathSeparator(code)) {
              if (!matchedSlash) {
                startPart = i + 1;
                break;
              }
              continue;
            }
            if (end === -1) {
              matchedSlash = false;
              end = i + 1;
            }
            if (code === CHAR_DOT) {
              if (startDot === -1) startDot = i;
              else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
              preDotState = -1;
            }
          }

          if (end !== -1) {
            if (
              startDot === -1 ||
              preDotState === 0 ||
              (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
            ) {
              ret.base = ret.name = path.slice(startPart, end);
            } else {
              ret.name = path.slice(startPart, startDot);
              ret.base = path.slice(startPart, end);
              ret.ext = path.slice(startDot, end);
            }
          }

          if (startPart > 0 && startPart !== rootEnd) {
            ret.dir = path.slice(0, startPart - 1);
          } else {
            ret.dir = ret.root;
          }
          return ret;
        },

        matchesGlob: pathMatchesGlob,
        sep: "\\",
        delimiter: ";",
      };
    } else {
      // Node's internal posixCwd: on Windows, expose the process CWD in POSIX
      // form (drive letter dropped, '\' -> '/') so path.posix.* on a Windows
      // host behaves like the test corpus expects.
      // Node's path module reads `process.cwd()` -- the PATCHABLE JS
      // function -- never an internal syscall. Going straight to
      // natives.cwd() made a patched cwd unobservable, so `path.resolve()`
      // could not degrade to '.' the way test-path-resolve requires when
      // process.cwd() is made to fail (it patches it to return ''). The
      // default process.cwd IS natives.cwd(), so this is behavior-neutral
      // until someone patches it.
      const processCwd = () => {
        const proc = globalThis.process;
        if (proc && typeof proc.cwd === "function") return proc.cwd();
        return natives ? natives.cwd() : "/";
      };
      const posixCwd = () => {
        const raw = processCwd();
        if (natives && natives.platform === "win32") {
          const slashed = raw.replace(/\\/g, "/");
          const idx = slashed.indexOf("/");
          return idx === -1 ? slashed : slashed.slice(idx);
        }
        return raw;
      };

      mod = {
        resolve(...args) {
          let resolvedPath = "";
          let resolvedAbsolute = false;
          for (let i = args.length - 1; i >= -1 && !resolvedAbsolute; i--) {
            const path = i >= 0 ? args[i] : posixCwd();
            assertPath(path, `paths[${i}]`);
            if (path.length === 0) continue;
            resolvedPath = `${path}/${resolvedPath}`;
            resolvedAbsolute = cc(path, 0) === CHAR_FORWARD_SLASH;
          }
          resolvedPath = normalizeString(
            resolvedPath,
            !resolvedAbsolute,
            "/",
            isPosixPathSeparator,
          );
          if (resolvedAbsolute) return `/${resolvedPath}`;
          return resolvedPath.length > 0 ? resolvedPath : ".";
        },

        normalize(path) {
          assertPath(path);
          if (path.length === 0) return ".";
          const isAbsolute = cc(path, 0) === CHAR_FORWARD_SLASH;
          const trailingSeparator = cc(path, path.length - 1) === CHAR_FORWARD_SLASH;
          path = normalizeString(path, !isAbsolute, "/", isPosixPathSeparator);
          if (path.length === 0) {
            if (isAbsolute) return "/";
            return trailingSeparator ? "./" : ".";
          }
          if (trailingSeparator) path += "/";
          return isAbsolute ? `/${path}` : path;
        },

        isAbsolute(path) {
          assertPath(path);
          return path.length > 0 && cc(path, 0) === CHAR_FORWARD_SLASH;
        },

        join(...args) {
          if (args.length === 0) return ".";
          let joined;
          for (let i = 0; i < args.length; ++i) {
            const arg = args[i];
            assertPath(arg);
            if (arg.length > 0) {
              if (joined === undefined) joined = arg;
              else joined += `/${arg}`;
            }
          }
          if (joined === undefined) return ".";
          return mod.normalize(joined);
        },

        relative(from, to) {
          assertPath(from, "from");
          assertPath(to, "to");
          if (from === to) return "";

          from = mod.resolve(from);
          to = mod.resolve(to);
          if (from === to) return "";

          const fromStart = 1;
          const fromEnd = from.length;
          const fromLen = fromEnd - fromStart;
          const toStart = 1;
          const toLen = to.length - toStart;

          const length = fromLen < toLen ? fromLen : toLen;
          let lastCommonSep = -1;
          let i = 0;
          for (; i < length; i++) {
            const fromCode = cc(from, fromStart + i);
            if (fromCode !== cc(to, toStart + i)) break;
            else if (fromCode === CHAR_FORWARD_SLASH) lastCommonSep = i;
          }
          if (i === length) {
            if (toLen > length) {
              if (cc(to, toStart + i) === CHAR_FORWARD_SLASH) {
                return to.slice(toStart + i + 1);
              }
              if (i === 0) {
                return to.slice(toStart + i);
              }
            } else if (fromLen > length) {
              if (cc(from, fromStart + i) === CHAR_FORWARD_SLASH) lastCommonSep = i;
              else if (i === 0) lastCommonSep = 0;
            }
          }

          let out = "";
          for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
            if (i === fromEnd || cc(from, i) === CHAR_FORWARD_SLASH) {
              out += out.length === 0 ? ".." : "/..";
            }
          }
          return `${out}${to.slice(toStart + lastCommonSep)}`;
        },

        toNamespacedPath(path) {
          return path;
        },

        dirname(path) {
          assertPath(path);
          if (path.length === 0) return ".";
          const hasRoot = cc(path, 0) === CHAR_FORWARD_SLASH;
          let end = -1;
          let matchedSlash = true;
          for (let i = path.length - 1; i >= 1; --i) {
            if (cc(path, i) === CHAR_FORWARD_SLASH) {
              if (!matchedSlash) {
                end = i;
                break;
              }
            } else {
              matchedSlash = false;
            }
          }
          if (end === -1) return hasRoot ? "/" : ".";
          if (hasRoot && end === 1) return "//";
          return path.slice(0, end);
        },

        basename(path, suffix) {
          if (suffix !== undefined) assertPath(suffix, "suffix");
          assertPath(path);
          let start = 0;
          let end = -1;
          let matchedSlash = true;

          if (suffix !== undefined && suffix.length > 0 && suffix.length <= path.length) {
            if (suffix === path) return "";
            let extIdx = suffix.length - 1;
            let firstNonSlashEnd = -1;
            for (let i = path.length - 1; i >= 0; --i) {
              const code = cc(path, i);
              if (code === CHAR_FORWARD_SLASH) {
                if (!matchedSlash) {
                  start = i + 1;
                  break;
                }
              } else {
                if (firstNonSlashEnd === -1) {
                  matchedSlash = false;
                  firstNonSlashEnd = i + 1;
                }
                if (extIdx >= 0) {
                  if (code === cc(suffix, extIdx)) {
                    if (--extIdx === -1) {
                      end = i;
                    }
                  } else {
                    extIdx = -1;
                    end = firstNonSlashEnd;
                  }
                }
              }
            }
            if (start === end) end = firstNonSlashEnd;
            else if (end === -1) end = path.length;
            return path.slice(start, end);
          }
          for (let i = path.length - 1; i >= 0; --i) {
            if (cc(path, i) === CHAR_FORWARD_SLASH) {
              if (!matchedSlash) {
                start = i + 1;
                break;
              }
            } else if (end === -1) {
              matchedSlash = false;
              end = i + 1;
            }
          }
          if (end === -1) return "";
          return path.slice(start, end);
        },

        extname(path) {
          assertPath(path);
          let startDot = -1;
          let startPart = 0;
          let end = -1;
          let matchedSlash = true;
          let preDotState = 0;
          for (let i = path.length - 1; i >= 0; --i) {
            const code = cc(path, i);
            if (code === CHAR_FORWARD_SLASH) {
              if (!matchedSlash) {
                startPart = i + 1;
                break;
              }
              continue;
            }
            if (end === -1) {
              matchedSlash = false;
              end = i + 1;
            }
            if (code === CHAR_DOT) {
              if (startDot === -1) startDot = i;
              else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
              preDotState = -1;
            }
          }
          if (
            startDot === -1 ||
            end === -1 ||
            preDotState === 0 ||
            (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
          ) {
            return "";
          }
          return path.slice(startDot, end);
        },

        format: _format.bind(null, "/"),

        parse(path) {
          assertPath(path);
          const ret = { root: "", dir: "", base: "", ext: "", name: "" };
          if (path.length === 0) return ret;
          const isAbsolute = cc(path, 0) === CHAR_FORWARD_SLASH;
          let start;
          if (isAbsolute) {
            ret.root = "/";
            start = 1;
          } else {
            start = 0;
          }
          let startDot = -1;
          let startPart = 0;
          let end = -1;
          let matchedSlash = true;
          let i = path.length - 1;
          let preDotState = 0;

          for (; i >= start; --i) {
            const code = cc(path, i);
            if (code === CHAR_FORWARD_SLASH) {
              if (!matchedSlash) {
                startPart = i + 1;
                break;
              }
              continue;
            }
            if (end === -1) {
              matchedSlash = false;
              end = i + 1;
            }
            if (code === CHAR_DOT) {
              if (startDot === -1) startDot = i;
              else if (preDotState !== 1) preDotState = 1;
            } else if (startDot !== -1) {
              preDotState = -1;
            }
          }

          if (end !== -1) {
            const startVal = startPart === 0 && isAbsolute ? 1 : startPart;
            if (
              startDot === -1 ||
              preDotState === 0 ||
              (preDotState === 1 && startDot === end - 1 && startDot === startPart + 1)
            ) {
              ret.base = ret.name = path.slice(startVal, end);
            } else {
              ret.name = path.slice(startVal, startDot);
              ret.base = path.slice(startVal, end);
              ret.ext = path.slice(startDot, end);
            }
          }

          if (startPart > 0) ret.dir = path.slice(0, startPart - 1);
          else if (isAbsolute) ret.dir = "/";
          return ret;
        },

        matchesGlob: pathMatchesGlob,
        sep: "/",
        delimiter: ":",
      };
    }

    return mod;
  }

  registry.factories.path = (natives) => {
    const win32 = makePathModule(true, natives);
    const posix = makePathModule(false, natives);
    win32.win32 = win32;
    win32.posix = posix;
    posix.win32 = win32;
    posix.posix = posix;
    // The default path module IS the platform's module object (same reference),
    // so `require('path') === require('path').win32` on Windows (test-path).
    return natives.platform === "win32" ? win32 : posix;
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
    // O_NOATIME is Linux-only and must be ABSENT elsewhere -- Node's test
    // asserts both directions, and code feature-detects with `in`.
    return { O_CREAT: 64, O_EXCL: 128, O_TRUNC: 512, O_APPEND: 1024, O_NOATIME: 0x40000 };
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
    const INSPECT_STYLES = { special: "cyan", number: "yellow", bigint: "yellow", boolean: "yellow", undefined: "grey", null: "bold", string: "green", symbol: "green", date: "magenta", regexp: "red", module: "underline" };
    const INSPECT_COLORS = { bold: [1, 22], italic: [3, 23], underline: [4, 24], inverse: [7, 27], white: [37, 39], grey: [90, 39], black: [30, 39], blue: [34, 39], cyan: [36, 39], green: [32, 39], magenta: [35, 39], red: [31, 39], yellow: [33, 39] };
    // Snapshot of capitalized global names (Node inspect's `builtInObjects`):
    // the showHidden prototype-property walk stops at these built-in layers.
    const builtInObjects = new Set(
      Object.getOwnPropertyNames(globalThis).filter((e) => /^[A-Z][a-zA-Z0-9]+$/.test(e)),
    );
    // Node inspect `meta` table: escapes for C0 controls, the single quote,
    // the backslash, DEL, and the C1 range (0x80-0x9F). BS is the backslash
    // character; built via fromCharCode so the table reads unambiguously.
    const INSPECT_BS = String.fromCharCode(92);
    const INSPECT_CUSTOM_SYMBOL = Symbol.for("nodejs.util.inspect.custom");
    const strEscapeMeta = (() => {
      const hex = (i) => `${INSPECT_BS}x${i.toString(16).toUpperCase().padStart(2, "0")}`;
      const m = [];
      for (let i = 0; i < 32; i++) m.push(hex(i));
      m[8] = INSPECT_BS + "b";
      m[9] = INSPECT_BS + "t";
      m[10] = INSPECT_BS + "n";
      m[12] = INSPECT_BS + "f";
      m[13] = INSPECT_BS + "r";
      for (let i = 32; i < 127; i++) m.push("");
      m[39] = INSPECT_BS + "'";
      m[92] = INSPECT_BS + INSPECT_BS;
      m.push(hex(127));
      for (let i = 128; i < 160; i++) m.push(hex(i));
      return m;
    })();
    // Node strEscape: choose the quote style (' -> " -> `), escape control
    // chars / backslash / the active quote, and backslash-u-escape lone
    // surrogates.
    function strEscape(str) {
      let quoteCode = 39;
      let quoteChar = "'";
      if (str.includes("'")) {
        if (!str.includes('"')) {
          quoteCode = -1;
          quoteChar = '"';
        } else if (!str.includes("`") && !str.includes("${")) {
          quoteCode = -2;
          quoteChar = "`";
        }
      }
      let result = "";
      let last = 0;
      for (let i = 0; i < str.length; i++) {
        const point = str.charCodeAt(i);
        if (point === quoteCode || point === 92 || point < 32 || (point > 126 && point < 160)) {
          result += str.slice(last, i) + strEscapeMeta[point];
          last = i + 1;
        } else if (point >= 0xd800 && point <= 0xdfff) {
          // Paired surrogates pass through; lone ones get escaped.
          if (point <= 0xdbff && i + 1 < str.length) {
            const next = str.charCodeAt(i + 1);
            if (next >= 0xdc00 && next <= 0xdfff) {
              i++;
              continue;
            }
          }
          result += `${str.slice(last, i)}${INSPECT_BS}u${point.toString(16)}`;
          last = i + 1;
        }
      }
      result += str.slice(last);
      return quoteChar + result + quoteChar;
    }
    function inspect(value, options = {}) {
      // `depth` is mutable: the Node output-budget clamp sets it to -1 when a
      // pathological object accumulates ~2^27 chars at one indentation level.
      let depth = options.depth === undefined ? 2 : options.depth;
      const showHidden = options.showHidden === undefined ? false : !!options.showHidden;
      const getters = options.getters === undefined ? false : options.getters;
      const breakLength = options.breakLength === undefined ? 80 : options.breakLength;
      const compact = options.compact === undefined ? 3 : options.compact;
      const maxArrayLength =
        options.maxArrayLength === null ? Infinity : options.maxArrayLength === undefined ? 100 : options.maxArrayLength;
      const maxStringLength =
        options.maxStringLength === null ? Infinity : options.maxStringLength === undefined ? 10000 : options.maxStringLength;
      // Node sorts the FORMATTED entry strings (not the keys). For object-type
      // renders the whole output is sorted; for array-type only the trailing
      // non-index key entries are.
      const sorted = options.sorted === undefined ? false : options.sorted;
      const sortCmp = sorted === true ? undefined : typeof sorted === "function" ? sorted : undefined;
      // `customInspect: false` suppresses the built-in custom renderers too --
      // Buffer's `<Buffer ..>` form is Buffer.prototype[inspect.custom] in Node.
      const customInspect = options.customInspect === undefined ? true : !!options.customInspect;
      const seen = [];
      // object -> ref id; minted the first time a revisit is detected
      // (Node's deferred `<ref *N>` / `[Circular *N]` anchor scheme).
      const circular = new Map();
      const ictx = { indentationLvl: 0, currentDepth: 0, budget: {} };
      const stylize = options.colors
        ? (str, type) => {
            const name = INSPECT_STYLES[type];
            if (!name) return str;
            const c = INSPECT_COLORS[name];
            return `\u001b[${c[0]}m${str}\u001b[${c[1]}m`;
          }
        : (str) => str;
      const ansiRe = /\u001b\[\d{1,3}m/g;
      const width = (s) => (options.colors ? s.replace(ansiRe, "").length : s.length);
      // Node keyStrRegExp -- note: no `$`, so `$foo` keys are quoted.
      const identKeyRe = /^[a-zA-Z_][a-zA-Z_0-9]*$/;
      // Port of Node getUserOptions: the options bag handed to a user's
      // [util.inspect.custom] hook as its second argument.
      function userOptionsSnapshot() {
        return {
          stylize,
          showHidden,
          depth,
          colors: !!options.colors,
          customInspect,
          showProxy: options.showProxy === undefined ? false : options.showProxy,
          maxArrayLength,
          maxStringLength,
          breakLength,
          compact,
          sorted,
          getters,
          numericSeparator: options.numericSeparator,
        };
      }

      function markCircular(v) {
        let id = circular.get(v);
        if (id === undefined) {
          id = circular.size + 1;
          circular.set(v, id);
        }
        return stylize(`[Circular *${id}]`, "special");
      }
      function refPrefix(v, base) {
        const id = circular.get(v);
        if (id === undefined) return base;
        const ref = stylize(`<ref *${id}>`, "special");
        return base === "" ? ref : `${ref} ${base}`;
      }
      function safeCtorName(v) {
        // Node getConstructorName core: own-descriptor walk up the prototype
        // chain; never invokes accessors (a throwing `constructor` getter must
        // not detonate the render).
        let obj = v;
        while (obj) {
          const d = Object.getOwnPropertyDescriptor(obj, "constructor");
          if (d !== undefined && typeof d.value === "function" && d.value.name !== "") {
            try {
              if (v instanceof d.value) return String(d.value.name);
            } catch {
              // Symbol.hasInstance shenanigans -- keep walking.
            }
          }
          obj = Object.getPrototypeOf(obj);
        }
        return undefined;
      }
      // Node dispatches on un-spoofable INTERNAL SLOTS, not the prototype
      // chain, so a value wearing a borrowed prototype (or a faked
      // Symbol.toStringTag) renders as the plain object it really is. Each
      // probe calls the original accessor, which throws on a wrong receiver.
      const hasSlot = (getter, v) => {
        try {
          getter.call(v);
          return true;
        } catch {
          return false;
        }
      };
      // isRealDate / isRealRegExp / isRealMap / isRealSet / isRealDataView are
      // the module-level probes -- shared so inspect and deepEqual agree on
      // what a foreign Map is.
      const AB_BYTELENGTH = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "byteLength").get;
      // SharedArrayBuffer has its OWN byteLength getter -- ArrayBuffer's throws
      // on it -- so both slots must be probed.
      const SAB_BYTELENGTH =
        typeof SharedArrayBuffer === "function"
          ? Object.getOwnPropertyDescriptor(SharedArrayBuffer.prototype, "byteLength").get
          : null;
      const isRealArrayBuffer = (v) =>
        hasSlot(AB_BYTELENGTH, v) || (SAB_BYTELENGTH !== null && hasSlot(SAB_BYTELENGTH, v));
      function tagOf(v) {
        // Port of Node's formatRaw tag gate: only surface Symbol.toStringTag
        // when it is a string AND not an own (enumerable) property, otherwise
        // it would be printed twice.
        let tag;
        try {
          tag = v[Symbol.toStringTag];
        } catch {
          return "";
        }
        if (typeof tag !== "string") return "";
        if (tag !== "") {
          const own = showHidden
            ? Object.prototype.hasOwnProperty.call(v, Symbol.toStringTag)
            : Object.prototype.propertyIsEnumerable.call(v, Symbol.toStringTag);
          if (own) return "";
        }
        return tag;
      }
      function typedArrayTag(v) {
        const tag = tagOf(v);
        if (tag !== "") return tag;
        const cn = safeCtorName(v);
        return cn || "TypedArray";
      }
      function inspectPrefix(constructor, tag, fallback, size = "") {
        // Port of Node getPrefix.
        if (constructor === undefined || constructor === null) {
          if (tag !== "" && fallback !== tag) return `[${fallback}${size}: null prototype] [${tag}] `;
          return `[${fallback}${size}: null prototype] `;
        }
        let result = `${constructor}${size} `;
        if (tag !== "") {
          const position = constructor.indexOf(tag);
          if (position === -1) {
            result += `[${tag}] `;
          } else {
            const endPos = position + tag.length;
            if (endPos !== constructor.length && constructor[endPos] === constructor[endPos].toLowerCase()) {
              result += `[${tag}] `;
            }
          }
        }
        return result;
      }
      function getKeys(v, hidden) {
        const symbols = Object.getOwnPropertySymbols(v);
        let keys;
        if (hidden) {
          keys = Object.getOwnPropertyNames(v);
          if (symbols.length !== 0) keys.push(...symbols);
        } else {
          try {
            keys = Object.keys(v);
          } catch {
            keys = Object.getOwnPropertyNames(v);
          }
          if (symbols.length !== 0) {
            keys.push(...symbols.filter((k) => Object.prototype.propertyIsEnumerable.call(v, k)));
          }
        }
        return keys;
      }
      function formatString(s) {
        let trailer = "";
        if (s.length > maxStringLength) {
          const remaining = s.length - maxStringLength;
          s = s.slice(0, maxStringLength);
          trailer = `... ${remaining} more character${remaining > 1 ? "s" : ""}`;
        }
        if (compact !== true && s.length > 16 && s.length > breakLength - ictx.indentationLvl - 4) {
          // Node splits multi-line strings at their newlines and joins the
          // quoted chunks with " +".
          return (
            s
              .split(/(?<=\n)/)
              .map((line) => stylize(strEscape(line), "string"))
              .join(` +\n${" ".repeat(ictx.indentationLvl + 2)}`) + trailer
          );
        }
        return stylize(strEscape(s), "string") + trailer;
      }
      function isBelowBreakLength(output, start, base) {
        // Port of Node isBelowBreakLength (entry count approximates the ", "
        // separators; colors are stripped before measuring).
        let totalLength = output.length + start;
        if (totalLength + output.length > breakLength) return false;
        for (let i = 0; i < output.length; i++) {
          totalLength += width(output[i]);
          if (totalLength > breakLength) return false;
        }
        return base === "" || !base.includes("\n");
      }
      function groupArrayElements(output, value) {
        // Port of Node groupArrayElements: column-pack short array entries.
        let totalLength = 0;
        let maxLength = 0;
        let i = 0;
        let outputLength = output.length;
        if (maxArrayLength < output.length) {
          // Ignore the "... n more items" tail.
          outputLength--;
        }
        const separatorSpace = 2; // " " + ","
        const dataLen = new Array(outputLength);
        for (; i < outputLength; i++) {
          const len = width(output[i]);
          dataLen[i] = len;
          totalLength += len + separatorSpace;
          if (maxLength < len) maxLength = len;
        }
        const actualMax = maxLength + separatorSpace;
        if (
          actualMax * 3 + ictx.indentationLvl < breakLength &&
          (totalLength / actualMax > 5 || maxLength <= 6)
        ) {
          const approxCharHeights = 2.5;
          const averageBias = Math.sqrt(actualMax - totalLength / output.length);
          const biasedMax = Math.max(actualMax - 3 - averageBias, 1);
          const columns = Math.min(
            Math.round(Math.sqrt(approxCharHeights * biasedMax * outputLength) / biasedMax),
            Math.floor((breakLength - ictx.indentationLvl) / actualMax),
            compact * 4,
            15,
          );
          if (columns <= 1) return output;
          const tmp = [];
          const maxLineLength = [];
          for (let c = 0; c < columns; c++) {
            let lineMaxLength = 0;
            for (let j = c; j < output.length; j += columns) {
              if (dataLen[j] > lineMaxLength) lineMaxLength = dataLen[j];
            }
            maxLineLength[c] = lineMaxLength + separatorSpace;
          }
          let padStart = true;
          if (value !== undefined) {
            for (let j = 0; j < output.length; j++) {
              if (typeof value[j] !== "number" && typeof value[j] !== "bigint") {
                padStart = false;
                break;
              }
            }
          }
          for (let row = 0; row < outputLength; row += columns) {
            const max = Math.min(row + columns, outputLength);
            let str = "";
            let j = row;
            for (; j < max - 1; j++) {
              const padding = maxLineLength[j - row] + output[j].length - dataLen[j];
              const cell = `${output[j]}, `;
              str += padStart ? cell.padStart(padding, " ") : cell.padEnd(padding, " ");
            }
            if (padStart) {
              const padding = maxLineLength[j - row] + output[j].length - dataLen[j] - separatorSpace;
              str += output[j].padStart(padding, " ");
            } else {
              str += output[j];
            }
            tmp.push(str);
          }
          if (maxArrayLength < output.length) tmp.push(output[outputLength]);
          output = tmp;
        }
        return output;
      }
      function reduceToSingleString(output, base, braces, level, arrayish, value) {
        // Port of Node reduceToSingleString. `level` is the post-increment
        // recurse count (owner level + 1), matching ictx.currentDepth.
        if (compact !== true) {
          if (typeof compact === "number" && compact >= 1) {
            const entries = output.length;
            if (arrayish && entries > 6) output = groupArrayElements(output, value);
            if (ictx.currentDepth - level < compact && entries === output.length) {
              const start = output.length + ictx.indentationLvl + braces[0].length + base.length + 10;
              if (isBelowBreakLength(output, start, base)) {
                const joined = output.join(", ");
                if (!joined.includes("\n")) {
                  return `${base ? `${base} ` : ""}${braces[0]} ${joined} ${braces[1]}`;
                }
              }
            }
          }
          const indentation = `\n${" ".repeat(ictx.indentationLvl)}`;
          return `${base ? `${base} ` : ""}${braces[0]}${indentation}  ${output.join(`,${indentation}  `)}${indentation}${braces[1]}`;
        }
        // compact === true
        if (isBelowBreakLength(output, 0, base)) {
          return `${braces[0]}${base ? ` ${base}` : ""} ${output.join(", ")} ${braces[1]}`;
        }
        const indentation = " ".repeat(ictx.indentationLvl);
        const ln =
          base === "" && braces[0].length === 1 ? " " : `${base ? ` ${base}` : ""}\n${indentation}  `;
        return `${braces[0]}${ln}${output.join(`,\n${indentation}  `)} ${braces[1]}`;
      }
      function reduce(output, base, braces, level, arrayish, value, sortKeysLen) {
        // Port of Node's ctx.sorted block in formatRaw: object-type renders sort
        // the entire formatted output; array-type renders sort only the trailing
        // `keys.length` non-index entries.
        if (sorted) {
          if (!arrayish) {
            output.sort(sortCmp);
          } else if (sortKeysLen > 1) {
            const head = output.slice(0, output.length - sortKeysLen);
            const tail = output.slice(output.length - sortKeysLen).sort(sortCmp);
            output = head.concat(tail);
          }
        }
        const res = reduceToSingleString(output, base, braces, level, arrayish, value);
        // Node's output budget: clamp depth once ~2^27 chars accumulate at one
        // indentation level so huge objects cannot OOM the isolate.
        const budget = ictx.budget[ictx.indentationLvl] || 0;
        const newLength = budget + res.length;
        ictx.budget[ictx.indentationLvl] = newLength;
        if (newLength > 2 ** 27) depth = -1;
        return res;
      }
      function collapsed(v) {
        // Depth-limit collapse text (Node getCtxStyle bracket form).
        if (Object.getPrototypeOf(v) === null) {
          const cn = natives.getConstructorName(v) || "Object";
          return Reflect.ownKeys(v).length === 0
            ? `[${cn}: null prototype] {}`
            : `[${cn}: null prototype]`;
        }
        const cn = safeCtorName(v);
        if (Array.isArray(v)) return stylize(cn && cn !== "Array" ? `[${cn}]` : "[Array]", "special");
        return stylize(cn && cn !== "Object" ? `[${cn}]` : "[Object]", "special");
      }
      function formatGetterPrimitive(tmp) {
        // Node formatPrimitive reached from a getter invocation. Anything that
        // is not a listed primitive falls into Symbol.prototype.toString and
        // throws -- exactly like Node (the caller's catch renders it).
        if (typeof tmp === "string") return formatString(tmp);
        if (typeof tmp === "number") return stylize(Object.is(tmp, -0) ? "-0" : String(tmp), "number");
        if (typeof tmp === "bigint") return stylize(`${tmp}n`, "bigint");
        if (typeof tmp === "boolean") return stylize(String(tmp), "boolean");
        if (typeof tmp === "undefined") return stylize("undefined", "undefined");
        return stylize(Symbol.prototype.toString.call(tmp), "symbol");
      }
      function formatProperty(owner, key, desc, level, arrayElem, receiver) {
        // Port of Node formatProperty. `level` is the level the value walks at
        // (owner+1 for own props, owner level for prototype props).
        let str;
        desc = desc || Object.getOwnPropertyDescriptor(owner, key) || { value: owner[key], enumerable: true };
        if (desc.value !== undefined) {
          ictx.indentationLvl += 2;
          str = walk(desc.value, level);
          ictx.indentationLvl -= 2;
        } else if (desc.get !== undefined) {
          const label = desc.set !== undefined ? "Getter/Setter" : "Getter";
          const wantInvoke =
            getters === true ||
            (getters === "get" && desc.set === undefined) ||
            (getters === "set" && desc.set !== undefined);
          if (getters && wantInvoke) {
            try {
              const tmp = desc.get.call(receiver);
              ictx.indentationLvl += 2;
              if (tmp === null) {
                str = `${stylize(`[${label}:`, "special")} ${stylize("null", "null")}${stylize("]", "special")}`;
              } else if (typeof tmp === "object") {
                str = `${stylize(`[${label}]`, "special")} ${walk(tmp, level)}`;
              } else {
                const primitive = formatGetterPrimitive(tmp);
                str = `${stylize(`[${label}:`, "special")} ${primitive}${stylize("]", "special")}`;
              }
              ictx.indentationLvl -= 2;
            } catch (err) {
              str = `${stylize(`[${label}:`, "special")} <Inspection threw (${err.message})>${stylize("]", "special")}`;
            }
          } else {
            str = stylize(`[${label}]`, "special");
          }
        } else if (desc.set !== undefined) {
          str = stylize("[Setter]", "special");
        } else {
          str = stylize("undefined", "undefined");
        }
        if (arrayElem) return str;
        let name;
        if (typeof key === "symbol") {
          name = `[${stylize(key.toString(), "symbol")}]`;
        } else if (key === "__proto__") {
          name = "['__proto__']";
        } else if (desc.enumerable === false) {
          name = `[${key}]`;
        } else if (identKeyRe.test(key)) {
          name = key;
        } else {
          name = stylize(strEscape(key), "string");
        }
        return `${name}: ${str}`;
      }
      function addProtoProps(main, obj, level, out) {
        // Port of Node addPrototypeProperties: pull non-function properties
        // (accessors and data) from up to three user prototype layers.
        let layer = 0;
        let keys = null;
        let keySet = null;
        do {
          if (layer !== 0 || main === obj) {
            obj = Object.getPrototypeOf(obj);
            if (obj === null) return;
            const d = Object.getOwnPropertyDescriptor(obj, "constructor");
            if (d !== undefined && typeof d.value === "function" && builtInObjects.has(d.value.name)) {
              return;
            }
          }
          if (layer === 0) {
            keySet = new Set();
          } else {
            for (const k of keys) keySet.add(k);
          }
          keys = Reflect.ownKeys(obj);
          seen.push(main);
          for (const key of keys) {
            if (
              key === "constructor" ||
              Object.prototype.hasOwnProperty.call(main, key) ||
              (layer !== 0 && keySet.has(key))
            ) {
              continue;
            }
            const desc = Object.getOwnPropertyDescriptor(obj, key);
            if (typeof desc.value === "function") continue;
            const entry = formatProperty(obj, key, desc, level, false, main);
            if (options.colors) out.push(`\u001b[2m${entry}\u001b[22m`);
            else out.push(entry);
          }
          seen.pop();
        } while (++layer !== 3);
      }
      function protoPropsFor(v, level) {
        if (!showHidden || (depth !== null && level > depth)) return undefined;
        // Node getConstructorName side effect: locate the constructor layer,
        // then collect prototype properties unless it is a direct built-in.
        const out = [];
        let obj = v;
        let firstProto;
        while (obj) {
          const d = Object.getOwnPropertyDescriptor(obj, "constructor");
          if (d !== undefined && typeof d.value === "function" && d.value.name !== "") {
            let isInst = false;
            try {
              isInst = v instanceof d.value;
            } catch {
              // Keep walking.
            }
            if (isInst) {
              if (firstProto !== obj || !builtInObjects.has(d.value.name)) {
                addProtoProps(v, firstProto || v, level, out);
              }
              break;
            }
          }
          obj = Object.getPrototypeOf(obj);
          if (firstProto === undefined) firstProto = obj;
        }
        return out.length > 0 ? out : undefined;
      }
      function entriesTail(v, keys, base, level) {
        seen.push(v);
        ictx.currentDepth = level + 1;
        const items = [];
        try {
          for (const k of keys) items.push(formatProperty(v, k, undefined, level + 1, false, v));
        } finally {
          seen.pop();
        }
        return reduce(items, refPrefix(v, base), ["{", "}"], level + 1, false, v);
      }
      function formatFunction(v, level) {
        let sig = "";
        try {
          sig = Function.prototype.toString.call(v);
        } catch {
          // Exotic callables: fall through to the [Function ...] base.
        }
        let base = null;
        if (sig.startsWith("class") && sig.endsWith("}")) {
          const body = sig.slice(5, -1);
          const braceIdx = body.indexOf("{");
          if (braceIdx !== -1 && !body.slice(0, braceIdx).includes("(")) {
            const hasName = Object.prototype.hasOwnProperty.call(v, "name");
            const name = (hasName && v.name) || "(anonymous)";
            let text = `class ${name}`;
            const superCls = Object.getPrototypeOf(v);
            if (superCls && superCls.name) text += ` extends ${superCls.name}`;
            base = `[${text}]`;
          }
        }
        if (base === null) {
          let type = "Function";
          try {
            const proto = Object.getPrototypeOf(v);
            const d = proto && Object.getOwnPropertyDescriptor(proto, "constructor");
            const pcn = d && typeof d.value === "function" ? d.value.name : undefined;
            if (pcn === "AsyncFunction" || pcn === "GeneratorFunction" || pcn === "AsyncGeneratorFunction") {
              type = pcn;
            }
          } catch {
            // Function base stays plain.
          }
          base = v.name ? `[${type}: ${v.name}]` : `[${type} (anonymous)]`;
        }
        const keys = getKeys(v, showHidden);
        if (keys.length === 0) return stylize(base, "special");
        if (depth !== null && level > depth) return stylize("[Function]", "special");
        return entriesTail(v, keys, base, level);
      }

      function walk(v, level) {
        if (v === null) return stylize("null", "null");
        const t = typeof v;
        if (t === "string") {
          return level === 0 && options.bare ? stylize(v, "string") : formatString(v);
        }
        if (t === "number") return stylize(Object.is(v, -0) ? "-0" : String(v), "number");
        if (t === "boolean") return stylize(String(v), "boolean");
        if (t === "undefined") return stylize("undefined", "undefined");
        if (t === "bigint") return stylize(`${v}n`, "bigint");
        if (t === "symbol") return stylize(v.toString(), "symbol");
        // ---------------- object-like values from here on ----------------
        if (seen.includes(v)) return markCircular(v);
        // User-supplied [util.inspect.custom] hook (Node formatValue). Skipped
        // when customInspect is off, when the hook IS util.inspect, and on the
        // prototype object itself (circular-render guard).
        if (customInspect) {
          let maybeCustom;
          try {
            maybeCustom = v[INSPECT_CUSTOM_SYMBOL];
          } catch {
            maybeCustom = undefined;
          }
          if (
            typeof maybeCustom === "function" &&
            maybeCustom !== inspect &&
            Object.getOwnPropertyDescriptor(v, "constructor")?.value?.prototype !== v
          ) {
            const customDepth = depth === null ? null : depth - level;
            const ret = maybeCustom.call(v, customDepth, userOptionsSnapshot(), inspect);
            // Returning `this` means "render me normally" -- avoids recursion.
            if (ret !== v) {
              if (typeof ret !== "string") return walk(ret, level);
              return ret.split("\n").join(`\n${" ".repeat(ictx.indentationLvl)}`);
            }
          }
        }
        if (t === "function") return formatFunction(v, level);
        if (isRealDate(v)) {
          // Node prefixes a Date SUBCLASS with its constructor name.
          let base = Number.isNaN(v.getTime()) ? "Invalid Date" : v.toISOString();
          const prefix = inspectPrefix(safeCtorName(v), tagOf(v), "Date");
          if (prefix !== "Date ") base = `${prefix}${base}`;
          const keys = getKeys(v, showHidden);
          if (keys.length === 0 || (depth !== null && level > depth)) return stylize(base, "date");
          return entriesTail(v, keys, stylize(base, "date"), level);
        }
        if (isRealRegExp(v)) {
          let base = v.toString();
          const prefix = inspectPrefix(safeCtorName(v), tagOf(v), "RegExp");
          if (prefix !== "RegExp ") base = `${prefix}${base}`;
          base = stylize(base, "regexp");
          const keys = getKeys(v, showHidden);
          if (keys.length === 0 || (depth !== null && level > depth)) return base;
          return entriesTail(v, keys, base, level);
        }
        if (v instanceof Error) {
          let base;
          try {
            base = v.stack ? String(v.stack) : Error.prototype.toString.call(v);
          } catch {
            base = "[Error]";
          }
          let msg;
          try {
            msg = v.message;
          } catch {
            msg = undefined;
          }
          // Node wraps the render in brackets when the stack carries no
          // call-frame lines (searching past the message).
          let pos = (msg && base.indexOf(msg)) || -1;
          if (pos !== -1) pos += msg.length;
          if (base.indexOf("\n    at", pos) === -1) base = `[${base}]`;
          // Nested errors get their stack lines indented (Node formatError).
          if (ictx.indentationLvl !== 0) {
            base = base.split("\n").join(`\n${" ".repeat(ictx.indentationLvl)}`);
          }
          let keys = getKeys(v, showHidden);
          if (!showHidden && keys.length !== 0) {
            // Node removeDuplicateErrorKeys: hide stack/message/name entries
            // already mirrored in the stack render.
            keys = keys.filter((k) => {
              if (k === "stack") return false;
              if (k === "message" || k === "name") {
                try {
                  const val = v[k];
                  return typeof val === "string" && !base.includes(val);
                } catch {
                  return true;
                }
              }
              return true;
            });
          }
          // Node surfaces a non-enumerable own `cause` (what `new Error(m, {cause})`
          // installs) and AggregateError's `errors` as bracketed entries.
          if (Object.prototype.hasOwnProperty.call(v, "cause") && !keys.includes("cause")) {
            keys.push("cause");
          }
          try {
            if (
              Array.isArray(v.errors) &&
              Object.prototype.hasOwnProperty.call(v, "errors") &&
              !keys.includes("errors")
            ) {
              keys.push("errors");
            }
          } catch {
            // A throwing `errors` getter is ignored, exactly like Node.
          }
          if (keys.length === 0) return base;
          if (depth !== null && level > depth) {
            return stylize(`[${safeCtorName(v) || "Error"}]`, "special");
          }
          return entriesTail(v, keys, base, level);
        }
        if (customInspect && globalThis.Buffer && v instanceof globalThis.Buffer) {
          // Node formats Buffers inline as <Buffer hex ...>; it does NOT call a
          // user .inspect() (that legacy hook is gone) -- an own `inspect` prop
          // is shown like any other property.
          const bufMod = registry.get("buffer");
          const max = bufMod.INSPECT_MAX_BYTES;
          const shown = Math.min(v.length, max);
          let hex = "";
          for (let i = 0; i < shown; i++) hex += (i ? " " : "") + v[i].toString(16).padStart(2, "0");
          if (v.length > max) {
            const more = v.length - max;
            hex += (hex ? " " : "") + `... ${more} more byte${more === 1 ? "" : "s"}`;
          }
          const parts = [];
          if (hex) parts.push(hex);
          for (const k of Object.keys(v)) {
            if (String(k >>> 0) === k) continue; // skip array-index keys
            const kt = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k) ? k : `'${k}'`;
            parts.push(`${kt}: ${walk(v[k], level + 1)}`);
          }
          return `<Buffer ${parts.join(", ")}>`;
        }
        if (isRealArrayBuffer(v)) {
          const label = v instanceof ArrayBuffer ? "ArrayBuffer" : "SharedArrayBuffer";
          const pfx = inspectPrefix(safeCtorName(v), tagOf(v), label);
          const items = [];
          let bytes;
          try {
            bytes = new Uint8Array(v);
          } catch {
            bytes = null;
          }
          if (bytes === null) {
            items.push(stylize("(detached)", "special"));
          } else {
            const shown = Math.min(maxArrayLength, bytes.length);
            let hex = "";
            for (let i = 0; i < shown; i++) hex += (i ? " " : "") + bytes[i].toString(16).padStart(2, "0");
            const remaining = bytes.length - maxArrayLength;
            if (remaining > 0) hex += ` ... ${remaining} more byte${remaining > 1 ? "s" : ""}`;
            items.push(`${stylize("[Uint8Contents]", "special")}: <${hex}>`);
          }
          items.push(`[byteLength]: ${stylize(String(v.byteLength), "number")}`);
          return reduce(items, refPrefix(v, ""), [`${pfx}{`, "}"], level + 1, false, v);
        }
        if (Array.isArray(v)) {
          // Subclass prefix: Node tags an Array subclass "Name(len) [ ... ]".
          const cn = safeCtorName(v);
          const prefix = cn && cn !== "Array" ? `${cn}(${v.length}) ` : "";
          // Non-index own keys; showHidden adds non-enumerables ([length]).
          const extraKeys = [];
          for (const k of Reflect.ownKeys(v)) {
            if (typeof k === "string" && String(k >>> 0) === k && (k >>> 0) < v.length) continue;
            if (!showHidden) {
              const d = Object.getOwnPropertyDescriptor(v, k);
              if (!d || !d.enumerable) continue;
            }
            extraKeys.push(k);
          }
          const protoProps = protoPropsFor(v, level);
          if (v.length === 0 && extraKeys.length === 0 && protoProps === undefined) return `${prefix}[]`;
          if (depth !== null && level > depth) return collapsed(v);
          seen.push(v);
          ictx.currentDepth = level + 1;
          const items = [];
          try {
            let emptyRun = 0;
            const flushEmpty = () => {
              if (emptyRun > 0) {
                items.push(`<${emptyRun} empty item${emptyRun === 1 ? "" : "s"}>`);
                emptyRun = 0;
              }
            };
            const limit = Math.min(v.length, maxArrayLength);
            for (let idx = 0; idx < limit; idx++) {
              // A hole (sparse) is rendered as a coalesced "<N empty items>" run.
              const d = Object.getOwnPropertyDescriptor(v, idx);
              if (d === undefined) {
                emptyRun++;
                continue;
              }
              flushEmpty();
              items.push(formatProperty(v, idx, d, level + 1, true, v));
            }
            flushEmpty();
            if (v.length > limit) {
              const remaining = v.length - limit;
              items.push(`... ${remaining} more item${remaining > 1 ? "s" : ""}`);
            }
            for (const k of extraKeys) items.push(formatProperty(v, k, undefined, level + 1, false, v));
            if (protoProps !== undefined) items.push(...protoProps);
          } finally {
            seen.pop();
          }
          return reduce(items, refPrefix(v, ""), [`${prefix}[`, "]"], level + 1, true, v, extraKeys.length);
        }
        if (isRealMap(v)) {
          const cn = safeCtorName(v);
          const prefix = `${cn && cn !== "Map" ? cn : "Map"}(${v.size}) `;
          if (v.size === 0) return `${prefix}{}`;
          if (depth !== null && level > depth) return collapsed(v);
          seen.push(v);
          ictx.currentDepth = level + 1;
          const items = [];
          ictx.indentationLvl += 2;
          try {
            for (const [k, val] of v) {
              items.push(`${walk(k, level + 1)} => ${walk(val, level + 1)}`);
            }
          } finally {
            ictx.indentationLvl -= 2;
            seen.pop();
          }
          return reduce(items, refPrefix(v, ""), [`${prefix}{`, "}"], level + 1, false, v);
        }
        if (isRealSet(v)) {
          const cn = safeCtorName(v);
          const prefix = `${cn && cn !== "Set" ? cn : "Set"}(${v.size}) `;
          if (v.size === 0) return `${prefix}{}`;
          if (depth !== null && level > depth) return collapsed(v);
          seen.push(v);
          ictx.currentDepth = level + 1;
          const items = [];
          ictx.indentationLvl += 2;
          try {
            for (const val of v) items.push(walk(val, level + 1));
          } finally {
            ictx.indentationLvl -= 2;
            seen.pop();
          }
          return reduce(items, refPrefix(v, ""), [`${prefix}{`, "}"], level + 1, false, v);
        }
        if (Symbol.iterator in v && ArrayBuffer.isView(v) && typeof v.subarray === "function") {
          // Typed arrays (incl. Buffer when customInspect is off). Node routes
          // these through the normal array-extras render: element entries first,
          // then non-index own keys, all under `Ctor(len) [ ... ]`.
          const cn = safeCtorName(v);
          const tag = typedArrayTag(v);
          const taLen = v.length;
          const prefix = inspectPrefix(cn, tag, tag, `(${taLen})`);
          const extraKeys = [];
          for (const k of Reflect.ownKeys(v)) {
            if (typeof k === "string" && String(k >>> 0) === k && (k >>> 0) < taLen) continue;
            if (!showHidden) {
              const d = Object.getOwnPropertyDescriptor(v, k);
              if (!d || !d.enumerable) continue;
            }
            extraKeys.push(k);
          }
          if (taLen === 0 && extraKeys.length === 0) return `${prefix}[]`;
          if (depth !== null && level > depth) return collapsed(v);
          seen.push(v);
          ictx.currentDepth = level + 1;
          const items = [];
          try {
            const limit = Math.min(taLen, maxArrayLength);
            const isBig = typeof v[0] === "bigint";
            for (let idx = 0; idx < limit; idx++) {
              const el = v[idx];
              items.push(
                isBig
                  ? stylize(`${el}n`, "bigint")
                  : stylize(Object.is(el, -0) ? "-0" : String(el), "number"),
              );
            }
            if (taLen > limit) {
              const remaining = taLen - limit;
              items.push(`... ${remaining} more item${remaining > 1 ? "s" : ""}`);
            }
            for (const k of extraKeys) items.push(formatProperty(v, k, undefined, level + 1, false, v));
          } finally {
            seen.pop();
          }
          return reduce(items, refPrefix(v, ""), [`${prefix}[`, "]"], level + 1, true, v, extraKeys.length);
        }
        if (isRealDataView(v)) {
          // DataView: Node renders the three hidden descriptors unconditionally.
          const cn = safeCtorName(v);
          const prefix = inspectPrefix(cn, tagOf(v), "DataView");
          if (depth !== null && level > depth) return collapsed(v);
          seen.push(v);
          ictx.currentDepth = level + 1;
          const items = [];
          ictx.indentationLvl += 2;
          try {
            items.push(`[byteLength]: ${stylize(String(v.byteLength), "number")}`);
            items.push(`[byteOffset]: ${stylize(String(v.byteOffset), "number")}`);
            items.push(`[buffer]: ${walk(v.buffer, level + 1)}`);
            for (const k of getKeys(v, showHidden)) {
              items.push(formatProperty(v, k, undefined, level + 1, false, v));
            }
          } finally {
            ictx.indentationLvl -= 2;
            seen.pop();
          }
          return reduce(items, refPrefix(v, ""), [`${prefix}{`, "}"], level + 1, false, v);
        }
        if (v instanceof WeakMap || v instanceof WeakSet) {
          // Node cannot enumerate a weak collection without showHidden.
          const label = v instanceof WeakMap ? "WeakMap" : "WeakSet";
          const pfx = inspectPrefix(safeCtorName(v), tagOf(v), label);
          if (depth !== null && level > depth) return collapsed(v);
          return reduce([stylize("<items unknown>", "special")], refPrefix(v, ""), [`${pfx}{`, "}"], level + 1, false, v);
        }
        {
          // Boxed primitives render as `[Boolean: false]`, not `Boolean {}`.
          const boxType =
            v instanceof Number
              ? "Number"
              : v instanceof String
                ? "String"
                : v instanceof Boolean
                  ? "Boolean"
                  : null;
          if (boxType !== null) {
            const prim = boxType === "Number" ? Number.prototype.valueOf.call(v)
              : boxType === "String" ? String.prototype.valueOf.call(v)
                : Boolean.prototype.valueOf.call(v);
            const ctor = safeCtorName(v);
            let base = `[${boxType}`;
            if (boxType !== ctor) base += ctor === undefined ? " (null prototype)" : ` (${ctor})`;
            base += `: ${typeof prim === "string" ? formatString(prim) : stylize(String(prim), boxType.toLowerCase())}]`;
            const tg = tagOf(v);
            if (tg !== "" && tg !== ctor) base += ` [${tg}]`;
            let bkeys = getKeys(v, showHidden);
            // Drop the 0..n-1 index entries a boxed String exposes.
            if (boxType === "String") bkeys = bkeys.slice(prim.length);
            if (bkeys.length === 0) return base;
            if (depth !== null && level > depth) return collapsed(v);
            return entriesTail(v, bkeys, base, level);
          }
        }
        // Plain objects, class instances, null-prototype objects, and the
        // remaining object-shaped values (Promise et al).
        let prefix;
        let nullProto = false;
        if (Object.getPrototypeOf(v) === null) {
          // V8 GetConstructorName recovers the original ctor ("Foo") even after
          // the prototype was nulled (Node "[Foo: null prototype]").
          const cn = natives.getConstructorName(v) || "Object";
          prefix = `[${cn}: null prototype] `;
          nullProto = true;
        } else {
          const ctor = safeCtorName(v);
          const tg = tagOf(v);
          if (ctor === "Object" || ctor === undefined) {
            // Node tags an arguments object and surfaces a non-own toStringTag.
            prefix =
              Object.prototype.toString.call(v) === "[object Arguments]"
                ? "[Arguments] "
                : tg !== ""
                  ? inspectPrefix(ctor ?? "Object", tg, "Object")
                  : "";
          } else {
            // Node getCtxStyle: the tag rides along unless it already prefixes
            // the constructor name (so `Buffer` + `Uint8Array` renders as
            // `Buffer [Uint8Array] `, but `ArrayBuffer` + `ArrayBuffer` does not).
            prefix = inspectPrefix(ctor, tg, "Object");
          }
        }
        // A module namespace is `[Module: null prototype]`, not
        // `[Object: null prototype] [Module]`: the tag IS the type here.
        const namespaceObject = natives.v8Is(v, "moduleNamespaceObject");
        if (namespaceObject) prefix = inspectPrefix(undefined, "", "Module");
        const keys = getKeys(v, showHidden);
        const protoProps = nullProto ? undefined : protoPropsFor(v, level);
        if (keys.length === 0 && protoProps === undefined) return `${prefix}{}`;
        if (depth !== null && level > depth) return collapsed(v);
        seen.push(v);
        ictx.currentDepth = level + 1;
        const items = [];
        try {
          for (const k of keys) {
            if (namespaceObject) {
              // Reading an export that is still in its temporal dead zone
              // THROWS -- so does asking for its descriptor. That is a state
              // to report, not an inspection failure: a linked-but-not-yet-
              // evaluated module legitimately has uninitialized bindings.
              try {
                items.push(formatProperty(v, k, undefined, level + 1, false, v));
              } catch (err) {
                if (!(err instanceof ReferenceError)) throw err;
                // Formatted through a stand-in so the key styling, quoting and
                // line breaking stay identical to every other entry.
                const placeholder = formatProperty({ [k]: "" }, k, undefined, level + 1, false, v);
                const cut = placeholder.lastIndexOf(" ");
                items.push(placeholder.slice(0, cut + 1) + stylize("<uninitialized>", "special"));
              }
              continue;
            }
            items.push(formatProperty(v, k, undefined, level + 1, false, v));
          }
          if (protoProps !== undefined) items.push(...protoProps);
        } finally {
          seen.pop();
        }
        return reduce(items, refPrefix(v, ""), [`${prefix}{`, "}"], level + 1, false, v);
      }
      return walk(value, 0);
    }

    // util.inspect.defaultOptions / .custom -- present so code that reads or
    // mutates them (the conformance corpus does) doesn't crash. Not all of
    // these options are honored by the walker yet.
    inspect.defaultOptions = {
      showHidden: false,
      depth: 2,
      colors: false,
      customInspect: true,
      showProxy: false,
      maxArrayLength: 100,
      maxStringLength: 10000,
      breakLength: 80,
      compact: 3,
      sorted: false,
      getters: false,
      numericSeparator: false,
    };
    inspect.custom = Symbol.for("nodejs.util.inspect.custom");

    // Number -> string preserving negative zero ("-0"), the way Node's
    // formatters do (String(-0) is "0", which loses the sign).
    function numToStr(n) {
      if (Object.is(n, -0)) return "-0";
      return String(n);
    }

    // Node's numericSeparator: group integer digits in 3s (from the right) and
    // fractional digits in 3s (from the left) with '_'. Exponential / non-finite
    // strings are left untouched. Gated by inspect.defaultOptions.numericSeparator.
    function numSep(str) {
      if (typeof str !== "string") return str;
      let neg = false;
      let body = str;
      if (body[0] === "-") { neg = true; body = body.slice(1); }
      const dot = body.indexOf(".");
      const intPart = dot >= 0 ? body.slice(0, dot) : body;
      const fracPart = dot >= 0 ? body.slice(dot + 1) : "";
      // Only plain decimal digit runs are groupable (no e/E, NaN, Infinity).
      const allDigits = (t) => t.length > 0 && [...t].every((c) => c >= "0" && c <= "9");
      if (!allDigits(intPart) || (dot >= 0 && !allDigits(fracPart))) return str;
      let gi = "";
      for (let i = 0; i < intPart.length; i++) {
        if (i > 0 && (intPart.length - i) % 3 === 0) gi += "_";
        gi += intPart[i];
      }
      let out = gi;
      if (dot >= 0) {
        let gf = "";
        for (let i = 0; i < fracPart.length; i++) {
          if (i > 0 && i % 3 === 0) gf += "_";
          gf += fracPart[i];
        }
        out += "." + gf;
      }
      return (neg ? "-" : "") + out;
    }
    let _fmtOpts = {};
    function maybeSep(str) {
      return _fmtOpts.numericSeparator ? numSep(str) : str;
    }

    function formatValue(v) {
      if (typeof v === "string") return v;
      return inspect(v);
    }

    // Node tryStringify: %j narrows its catch to circular-structure
    // TypeErrors (matched by first message line against a probe) and
    // rethrows everything else (e.g. a throwing toJSON, BigInt).
    let circularErrorMessage;
    const firstErrorLine = (err) =>
      err && err.message !== undefined ? String(err.message).split("\n", 1)[0] : "";
    function tryStringify(arg) {
      try {
        return JSON.stringify(arg);
      } catch (err) {
        if (circularErrorMessage === undefined) {
          try {
            const a = {};
            a.a = a;
            JSON.stringify(a);
          } catch (circularError) {
            circularErrorMessage = firstErrorLine(circularError);
          }
        }
        if (err && err.name === "TypeError" && firstErrorLine(err) === circularErrorMessage) {
          return "[Circular]";
        }
        throw err;
      }
    }

    function format(f, ...args) {
      _fmtOpts = inspect.defaultOptions;
      // util.format() with no arguments returns '' (Node parity).
      if (arguments.length === 0) return "";
      return formatBody(f, args);
    }
    function formatBody(f, args) {
      const fmtInspect = (x) => (typeof x === "string" ? x : inspect(x, _fmtOpts));
      if (typeof f !== "string") {
        return [f, ...args].map(fmtInspect).join(" ");
      }
      // Node fast-path: a lone format string with NO substitution args is
      // returned verbatim ('%%' stays '%%').
      if (args.length === 0) return f;
      let i = 0;
      let out = f.replace(/%[sdifjoOc%]/g, (spec) => {
        if (spec === "%%") return "%";
        if (i >= args.length) return spec;
        const arg = args[i++];
        switch (spec) {
          case "%s": {
            if (typeof arg === "number") return maybeSep(numToStr(arg));
            if (typeof arg === "bigint") return (maybeSep(String(arg)) + "n");
            // Node %s: only an object with a BUILT-IN toString (plain object /
            // array) inspects at depth 0; everything else (primitives, functions,
            // symbols, objects with a custom toString) is String()-coerced.
            const isObj = arg !== null && typeof arg === "object";
            let builtIn = false;
            if (isObj) {
              const ts = arg.toString;
              // Date has a Symbol.toPrimitive but Node %s INSPECTS it (ISO form),
              // so it counts as built-in here.
              const hasToPrim = typeof arg[Symbol.toPrimitive] === "function" && !(arg instanceof Date);
              builtIn = !hasToPrim &&
                (typeof ts !== "function" || ts === Object.prototype.toString || ts === Array.prototype.toString || arg instanceof Date);
            }
            if (!builtIn) return String(arg);
            return inspect(arg, { ..._fmtOpts, depth: 0, colors: false, compact: 3, bare: true });
          }
          case "%d":
            if (typeof arg === "symbol") return "NaN"; // Number(Symbol) throws; Node prints NaN
            return typeof arg === "bigint" ? (maybeSep(String(arg)) + "n") : maybeSep(numToStr(Number(arg)));
          case "%i":
            if (typeof arg === "symbol") return "NaN";
            return typeof arg === "bigint" ? (maybeSep(String(arg)) + "n") : maybeSep(numToStr(parseInt(arg, 10)));
          case "%f":
            if (typeof arg === "symbol") return "NaN";
            return maybeSep(numToStr(parseFloat(arg)));
          case "%j":
            return tryStringify(arg);
          case "%o":
            // Node %o: showHidden + showProxy at depth 4 regardless of the
            // surrounding inspect options.
            return inspect(arg, { ..._fmtOpts, showHidden: true, showProxy: true, depth: 4 });
          case "%O":
            return inspect(arg, _fmtOpts);
          case "%c":
            // console.spec %c (CSS directive): consumes the arg, renders nothing.
            return "";
          default:
            return spec;
        }
      });
      for (; i < args.length; i++) out += " " + fmtInspect(args[i]);
      return out;
    }

    const customPromisify = Symbol.for("nodejs.util.promisify.custom");
    // Node's kCustomPromisifyArgs: names for a callback that yields MORE
    // than one value, so the promise resolves an object instead of
    // silently dropping every argument after the first (this is how
    // promisified child_process.exec resolves { stdout, stderr }).
    const customPromisifyArgs = Symbol.for("nodejs.util.promisify.customArgs");
    function promisify(original) {
      if (typeof original !== "function") {
        // node's validateFunction: a coded ERR_INVALID_ARG_TYPE carrying the
        // "Received ..." tail, not a bare TypeError. Callers branch on the
        // code, and the tail is what names the value that was actually passed.
        throw new codes.ERR_INVALID_ARG_TYPE("original", "Function", original);
      }
      if (original[customPromisify]) {
        const fn = original[customPromisify];
        if (typeof fn !== "function") {
          throw new codes.ERR_INVALID_ARG_TYPE("util.promisify.custom", "Function", fn);
        }
        // Mark the custom function as its OWN promisified form, so
        // promisify() is idempotent: promisify(promisify(fn)) === fn.
        // Without it the second call wrapped the custom function again and
        // returned a different object than the first call did.
        Object.defineProperty(fn, customPromisify, {
          value: fn,
          enumerable: false,
          writable: false,
          configurable: true,
        });
        return fn;
      }
      // DEP0174: promisifying something that already returns a promise is
      // almost always a mistake -- the wrapper's promise resolves with
      // whatever the callback got, which is nothing. Asked of V8, not of
      // `original.constructor`, which any function can be given.
      if (natives.v8Is(original, "asyncFunction")) {
        process.emitWarning(
          "Calling promisify on a function that returns a Promise is likely a mistake.",
          "DeprecationWarning",
          "DEP0174",
        );
      }
      const argNames = original[customPromisifyArgs];
      function promisified(...args) {
        return new Promise((resolve, reject) => {
          original.call(this, ...args, (err, ...values) => {
            if (err) {
              reject(err);
              return;
            }
            // The named-object form is for callbacks that hand back SEVERAL
            // values (fs.read's bytesRead + buffer). A lone value stays the
            // value -- wrapping it would change the shape of what the promise
            // resolves to, which is a wrong answer rather than a missing one.
            if (argNames !== undefined && values.length > 1) {
              const obj = {};
              for (let i = 0; i < argNames.length; i++) obj[argNames[i]] = values[i];
              resolve(obj);
              return;
            }
            resolve(values[0]);
          });
        });
      }
      // The wrapper stands in for the original, so it inherits the original's
      // prototype rather than this realm's Function.prototype. That is
      // load-bearing across a realm boundary: promisifying a function from a
      // `vm` context must not silently re-home it into ours.
      Object.setPrototypeOf(promisified, Object.getPrototypeOf(original));
      // Mark the generated wrapper as its own promisified form, so promisify
      // is idempotent on it the same way it already is on a custom one.
      Object.defineProperty(promisified, customPromisify, {
        value: promisified,
        enumerable: false,
        writable: false,
        configurable: true,
      });
      // Carry over EVERY own property, not just `name` -- `length`, and
      // whatever the original was decorated with, are part of standing in for
      // it. Descriptors are copied so non-enumerable stays non-enumerable.
      const descriptors = Object.getOwnPropertyDescriptors(original);
      // Node null-prototypes each descriptor first so a mutated
      // %Object.prototype% cannot smuggle in `get`/`set`/`writable` keys that
      // defineProperties would then honor.
      for (const key of Reflect.ownKeys(descriptors)) {
        Object.setPrototypeOf(descriptors[key], null);
      }
      return Object.defineProperties(promisified, descriptors);
    }
    promisify.custom = customPromisify;

    // Node v22 lib/util.js callbackify shape: arg validation, this-bound cb,
    // nextTick delivery (not queueMicrotask), ERR_FALSY_VALUE_REJECTION for
    // ALL falsy rejections (?? missed false/0/''), and descriptor copying
    // with length+1 / name+'Callbackified'.
    // Node's lib/util.js hoists this so the pruned stack has a real frame to
    // cut at. Capturing against `callbackified` (which has already RETURNED by
    // the time the tick runs) leaves V8 no matching frame, so the whole stack
    // is pruned away and err.stack is just the header.
    function callbackifyOnRejected(reason, cb) {
      if (!reason) {
        const err = new Error("Promise was rejected with falsy value");
        err.reason = reason;
        // Capture BEFORE shaping: applyNodeErrorShape rewrites the current
        // stack's first line into the "Error [ERR_FALSY_VALUE_REJECTION]:"
        // header node renders -- capturing afterward would regenerate an
        // unshaped stack.
        Error.captureStackTrace(err, callbackifyOnRejected);
        applyNodeErrorShape(err, "ERR_FALSY_VALUE_REJECTION");
        reason = err;
      }
      return cb(reason);
    }

    function callbackify(original) {
      if (typeof original !== "function") {
        throw new codes.ERR_INVALID_ARG_TYPE("original", "Function", original);
      }
      function callbackified(...args) {
        const maybeCb = args.pop();
        if (typeof maybeCb !== "function") {
          // "last argument" -- buildArgTypeMessage prepends "The " for
          // ' argument'-suffixed names, matching node's rendered message.
          throw new codes.ERR_INVALID_ARG_TYPE("last argument", "Function", maybeCb);
        }
        const cb = maybeCb.bind(this);
        Reflect.apply(original, this, args).then(
          (ret) => process.nextTick(cb, null, ret),
          (rej) => process.nextTick(callbackifyOnRejected, rej, cb),
        );
      }
      const descriptors = Object.getOwnPropertyDescriptors(original);
      if (typeof descriptors.length.value === "number") descriptors.length.value++;
      if (typeof descriptors.name.value === "string") descriptors.name.value += "Callbackified";
      for (const d of Object.values(descriptors)) Object.setPrototypeOf(d, null);
      Object.defineProperties(callbackified, descriptors);
      return callbackified;
    }

    function inherits(ctor, superCtor) {
      if (ctor === undefined || ctor === null) {
        throw new codes.ERR_INVALID_ARG_TYPE("ctor", "Function", ctor);
      }
      if (superCtor === undefined || superCtor === null) {
        throw new codes.ERR_INVALID_ARG_TYPE("superCtor", "Function", superCtor);
      }
      if (superCtor.prototype === undefined) {
        throw new codes.ERR_INVALID_ARG_TYPE(
          "superCtor.prototype",
          "Object",
          superCtor.prototype,
        );
      }
      Object.defineProperty(ctor, "super_", { value: superCtor, writable: true, configurable: true });
      Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
    }

    // Codes that have already warned, shared across every deprecate() call:
    // Node warns once per CODE, not once per wrapper.
    const deprecationCodesWarned = new Set();
    function deprecate(fn, msg, code) {
      if (code !== undefined && typeof code !== "string") {
        throw new codes.ERR_INVALID_ARG_TYPE("code", "string", code);
      }
      let warned = false;
      function deprecated(...args) {
        if (!warned) {
          warned = true;
          // process.emitWarning, NOT console.warn: this is what makes
          // --no-deprecation suppress it, --throw-deprecation turn it
          // fatal, and process.on('warning') see it. Writing straight to
          // the console bypassed all three.
          if (code !== undefined) {
            if (!deprecationCodesWarned.has(code)) {
              deprecationCodesWarned.add(code);
              process.emitWarning(msg, "DeprecationWarning", code, deprecated);
            }
          } else {
            process.emitWarning(msg, "DeprecationWarning", deprecated);
          }
        }
        // A deprecated CONSTRUCTOR must still construct: applying it would
        // lose new.target and return the wrong thing entirely.
        if (new.target) return Reflect.construct(fn, args, new.target);
        return Reflect.apply(fn, this, args);
      }
      // Node copies the wrapped function's arity onto the wrapper; the rest
      // parameter above otherwise reports length 0, and callers do inspect
      // fn.length to decide how to call things.
      Object.defineProperty(deprecated, "length", {
        value: fn.length,
        configurable: true,
      });
      Object.setPrototypeOf(deprecated, fn);
      if (fn.prototype) {
        // ASSIGNED, not defineProperty'd: a function declaration's
        // `prototype` is non-configurable, so redefining it throws. Sharing
        // it is what makes an instance from the wrapper still read as an
        // instanceof the wrapped constructor.
        deprecated.prototype = fn.prototype;
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

    // Identify a boxed-primitive's TRUE internal class by probing each
    // wrapper's valueOf (throws on a mismatched receiver) -- robust against a
    // Symbol.toStringTag / prototype override. Returns null for non-wrappers.
    function boxedKind(v) {
      if (v === null || typeof v !== "object") return null;
      try {
        Number.prototype.valueOf.call(v);
        return "Number";
      } catch {}
      try {
        String.prototype.valueOf.call(v);
        return "String";
      } catch {}
      try {
        Boolean.prototype.valueOf.call(v);
        return "Boolean";
      } catch {}
      try {
        Symbol.prototype.valueOf.call(v);
        return "Symbol";
      } catch {}
      try {
        BigInt.prototype.valueOf.call(v);
        return "BigInt";
      } catch {}
      return null;
    }
    function boxedValue(v, kind) {
      switch (kind) {
        case "Number":
          return Number.prototype.valueOf.call(v);
        case "String":
          return String.prototype.valueOf.call(v);
        case "Boolean":
          return Boolean.prototype.valueOf.call(v);
        case "Symbol":
          return Symbol.prototype.valueOf.call(v);
        case "BigInt":
          return BigInt.prototype.valueOf.call(v);
        default:
          return v;
      }
    }

    const TA_TAG_GETTER = Object.getOwnPropertyDescriptor(
      Object.getPrototypeOf(Object.getPrototypeOf(new Int8Array(0))),
      Symbol.toStringTag,
    ).get;
    const taKind = (v) => {
      try {
        return TA_TAG_GETTER.call(v);
      } catch {
        return "DataView";
      }
    };
    const origGetTime = Date.prototype.getTime;
    // Un-spoofable [[ErrorData]] probe. `instanceof Error` misses an error whose
    // prototype was nulled, and Object.prototype.toString can be forged with
    // Symbol.toStringTag -- Error.isError reads the internal slot directly.
    const isNativeErrorValue =
      typeof Error.isError === "function"
        ? (v) => Error.isError(v)
        : (v) => v instanceof Error || Object.prototype.toString.call(v) === "[object Error]";
    const anyArrayBuffer = (v) =>
      v instanceof ArrayBuffer ||
      (typeof SharedArrayBuffer !== "undefined" && v instanceof SharedArrayBuffer);

    function deepEqualImpl(a, b, strict, memo) {
      // Strict is SameValue (Object.is): NaN equals NaN, +0 does NOT
      // equal -0 â€” exactly Node's deepStrictEqual primitive rule.
      const primitiveEqual = strict
        ? Object.is
        : // eslint-disable-next-line eqeqeq
          (x, y) => x == y || (Number.isNaN(x) && Number.isNaN(y));
      // Node's innerDeepEqual treats null and every non-object (including
      // functions) as a primitive, and a primitive is NEVER deep-equal to an
      // object in either mode -- `==` coercion must not leak in here
      // (`'a' == ['a']` is true, but deepEqual('a', ['a']) is false).
      const aPrim = a === null || typeof a !== "object";
      const bPrim = b === null || typeof b !== "object";
      if (aPrim || bPrim) {
        if (aPrim !== bPrim) return false;
        return primitiveEqual(a, b);
      }
      if (a === b) return true;
      if (strict && Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
      // Boxed primitives (new Number/String/Boolean/Symbol/BigInt). Detect the
      // TRUE internal class by probing each wrapper's valueOf (which throws on a
      // mismatched receiver), so a Symbol.toStringTag override can't disguise
      // the slot. Two wrappers are equal only if the same kind AND the same
      // primitive value AND their own enumerable keys match (the key check runs
      // below after this branch falls through for equal-valued wrappers).
      {
        const ka = boxedKind(a);
        const kb = boxedKind(b);
        if (ka !== null || kb !== null) {
          if (ka !== kb) return false;
          if (!primitiveEqual(boxedValue(a, ka), boxedValue(b, kb))) return false;
          // fall through to the own-enumerable-key comparison below.
        }
      }
      // Type tag must match symmetrically. Node uses the un-spoofable internal
      // class, so an object vs a Date/RegExp/Array/Error/arguments (or two
      // typed arrays of different element kind) is never equal even when their
      // own keys line up. The old code only branched on `a instanceof X`, which
      // silently accepted a type-mismatched `b`.
      if (Array.isArray(a) !== Array.isArray(b)) return false;
      if (
        (Object.prototype.toString.call(a) === "[object Arguments]") !==
        (Object.prototype.toString.call(b) === "[object Arguments]")
      ) {
        return false;
      }
      if (isRealDate(a) !== isRealDate(b)) return false;
      if (isRealRegExp(a) !== isRealRegExp(b)) return false;
      if (isNativeErrorValue(a) !== isNativeErrorValue(b)) return false;
      if (isRealMap(a) !== isRealMap(b)) return false;
      if (isRealSet(a) !== isRealSet(b)) return false;
      if (ArrayBuffer.isView(a) !== ArrayBuffer.isView(b)) return false;
      if (anyArrayBuffer(a) !== anyArrayBuffer(b)) return false;
      // WeakMap/WeakSet cannot be compared by content; only reference equality
      // (already handled by the `a === b` check above) -- distinct => not equal.
      if (a instanceof WeakMap || a instanceof WeakSet) return false;
      if (isRealDate(a)) {
        // Read the internal [[DateValue]] via the original getTime so an
        // overridden own `getTime` cannot fool the comparison; a non-Date
        // receiver (a fake clone with Date.prototype) throws => not equal.
        let ta, tb;
        try {
          ta = origGetTime.call(a);
        } catch {
          return false;
        }
        try {
          tb = origGetTime.call(b);
        } catch {
          return false;
        }
        if (!Object.is(ta, tb)) return false;
        // fall through to own-key comparison (extra props still count)
      } else if (isRealRegExp(a)) {
        try {
          if (
            a.source !== b.source ||
            a.flags !== b.flags ||
            a.lastIndex !== b.lastIndex
          ) {
            return false;
          }
        } catch {
          return false;
        }
        // fall through to own-key comparison
      }
      if (ArrayBuffer.isView(a) || ArrayBuffer.isView(b)) {
        if (!ArrayBuffer.isView(a) || !ArrayBuffer.isView(b)) return false;
        // Element kind must match (Int8 != Uint8 even with equal bytes). Read
        // the true kind via the %TypedArray% toStringTag getter, which ignores
        // own-toStringTag / prototype spoofing.
        if (taKind(a) !== taKind(b)) return false;
        const kind = taKind(a);
        const isFloatArray =
          kind === "Float16Array" || kind === "Float32Array" || kind === "Float64Array";
        if (!strict && isFloatArray) {
          // LOOSE mode compares float arrays ELEMENT-wise, not byte-wise:
          // +0 and -0 have different bytes but are ==, and loose equality
          // says they match. (Strict stays byte-wise, where they differ --
          // which is the whole distinction between the two APIs.)
          if (a.length !== b.length) return false;
          for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
        } else {
          const ua = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
          const ub = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
          if (ua.length !== ub.length) return false;
          for (let i = 0; i < ua.length; i++) if (ua[i] !== ub[i]) return false;
        }
        // Extra own enumerable (non-index) properties still count. Symbols are
        // compared only in strict mode (loose deepEqual ignores symbol keys).
        const extraKeys = (o) =>
          [
            ...Object.keys(o).filter((k) => !/^\d+$/.test(k)),
            ...(strict
              ? Object.getOwnPropertySymbols(o).filter(
                  (s) => Object.getOwnPropertyDescriptor(o, s)?.enumerable,
                )
              : []),
          ];
        const ea = extraKeys(a);
        const eb = extraKeys(b);
        if (ea.length !== eb.length) return false;
        for (const k of ea) {
          if (!Object.prototype.hasOwnProperty.call(b, k)) return false;
          if (!deepEqualImpl(a[k], b[k], strict, memo ?? new Map())) return false;
        }
        return true;
      }
      if (anyArrayBuffer(a)) {
        // An ArrayBuffer and a SharedArrayBuffer are DIFFERENT types even
        // with identical bytes -- one is shareable across threads and the
        // other is not, which is exactly the distinction a test comparing
        // them cares about. Comparing bytes alone called them equal.
        const shared = (v) =>
          typeof SharedArrayBuffer !== "undefined" && v instanceof SharedArrayBuffer;
        if (shared(a) !== shared(b)) return false;
        if (a.byteLength !== b.byteLength) return false;
        const ua = new Uint8Array(a);
        const ub = new Uint8Array(b);
        for (let i = 0; i < ua.length; i++) if (ua[i] !== ub[i]) return false;
        return true;
      }
      memo = memo ?? new Map();
      const prior = memo.get(a);
      if (prior && prior.has(b)) return true;
      if (prior) prior.add(b);
      else memo.set(a, new Set([b]));

      if (Array.isArray(a)) {
        if (a.length !== b.length) return false;
        // Do NOT return here: fall through to the own-key comparison so extra
        // own props, sparse holes (Object.keys skips them), and (strict only)
        // symbol keys are all checked like Node. Index values are compared by
        // the bottom key loop.
      }
      if (isRealMap(a)) {
        if (a.size !== b.size) return false;
        const entries = [...b];
        const used = new Array(entries.length).fill(false);
        mapOuter: for (const [k, v] of a) {
          for (let i = 0; i < entries.length; i++) {
            if (used[i]) continue;
            if (
              deepEqualImpl(k, entries[i][0], strict, memo) &&
              deepEqualImpl(v, entries[i][1], strict, memo)
            ) {
              used[i] = true;
              continue mapOuter;
            }
          }
          return false;
        }
        // fall through: own enumerable props on the Map object are compared too
      }
      if (isRealSet(a)) {
        if (a.size !== b.size) return false;
        const items = [...b];
        const used = new Array(items.length).fill(false);
        setOuter: for (const v of a) {
          for (let i = 0; i < items.length; i++) {
            if (used[i]) continue;
            if (deepEqualImpl(v, items[i], strict, memo)) {
              used[i] = true;
              continue setOuter;
            }
          }
          return false;
        }
        // fall through: own enumerable props on the Set object are compared too
      }
      if (a instanceof Error) {
        if (!(b instanceof Error) || a.name !== b.name || a.message !== b.message) {
          return false;
        }
        // Node compares the (non-enumerable) `cause` too: errors with
        // differing causes are not deep-equal.
        const aHasCause = "cause" in a;
        const bHasCause = "cause" in b;
        if (aHasCause !== bHasCause) return false;
        if (aHasCause && !deepEqualImpl(a.cause, b.cause, strict, memo)) return false;
        // AggregateError carries a non-enumerable `errors` array that Node
        // compares as part of deep equality.
        if (Array.isArray(a.errors) || Array.isArray(b.errors)) {
          if (!deepEqualImpl(a.errors, b.errors, strict, memo)) return false;
        }
      }
      // Own ENUMERABLE keys, string + symbol (Node compares both; non-
      // enumerable symbols are ignored).
      const ownEnumerableKeys = (obj) => {
        const out = Object.keys(obj);
        // Node compares symbol-keyed props only in STRICT mode; loose deepEqual
        // ignores symbols entirely.
        if (strict) {
          for (const s of Object.getOwnPropertySymbols(obj)) {
            if (Object.getOwnPropertyDescriptor(obj, s)?.enumerable) out.push(s);
          }
        }
        return out;
      };
      const aKeys = ownEnumerableKeys(a);
      const bKeys = ownEnumerableKeys(b);
      if (aKeys.length !== bKeys.length) return false;
      for (const key of aKeys) {
        if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
        const bDesc = Object.getOwnPropertyDescriptor(b, key);
        if (!bDesc || !bDesc.enumerable) return false;
        if (!deepEqualImpl(a[key], b[key], strict, memo)) return false;
      }
      return true;
    }

    // Partial deep-strict-equal: every own enumerable key of `expected` must
    // deep-strict-equal the same key in `actual`; `actual` may have extras.
    // Used by assert.partialDeepStrictEqual.
    function partialDeepEqualImpl(actual, expected, memo) {
      if (
        expected === null ||
        typeof expected !== "object" ||
        actual === null ||
        typeof actual !== "object"
      ) {
        return deepEqualImpl(actual, expected, true);
      }
      if (actual === expected) return true;
      memo = memo ?? new Map();
      const prior = memo.get(expected);
      if (prior && prior.has(actual)) return true;
      if (prior) prior.add(actual);
      else memo.set(expected, new Set([actual]));

      // Node's partial mode DROPS the prototype/constructor check but keeps the
      // internal type-tag check -- that tag is what rejects arguments-vs-object,
      // Error-vs-object, boxed-Symbol-vs-object and Uint8Array-vs-Int8Array.
      const tagOfVal = Object.prototype.toString;
      if (tagOfVal.call(actual) !== tagOfVal.call(expected)) return false;
      // ...and, because Symbol.toStringTag / the prototype can both be spoofed,
      // the same un-spoofable internal-type battery deepEqualImpl uses.
      if (Array.isArray(actual) !== Array.isArray(expected)) return false;
      if (isRealDate(actual) !== isRealDate(expected)) return false;
      if (isRealRegExp(actual) !== isRealRegExp(expected)) return false;
      if (isRealMap(actual) !== isRealMap(expected)) return false;
      if (isRealSet(actual) !== isRealSet(expected)) return false;
      if (ArrayBuffer.isView(actual) !== ArrayBuffer.isView(expected)) return false;
      if (anyArrayBuffer(actual) !== anyArrayBuffer(expected)) return false;
      if (ArrayBuffer.isView(actual) && taKind(actual) !== taKind(expected)) return false;
      if (isNativeErrorValue(actual) !== isNativeErrorValue(expected)) return false;

      // Weak collections expose no contents; only reference equality can hold
      // (and that was already handled above).
      if (
        expected instanceof WeakMap || expected instanceof WeakSet ||
        actual instanceof WeakMap || actual instanceof WeakSet
      ) {
        return false;
      }

      if (Array.isArray(expected)) {
        if (!Array.isArray(actual)) return false;
        if (actual.length < expected.length) return false;
        if (!partialArrayEquiv(actual, expected, memo)) return false;
        return partialKeyEquiv(actual, expected, memo, true);
      }

      if (ArrayBuffer.isView(expected)) {
        if (!ArrayBuffer.isView(actual)) return false;
        const va = new Uint8Array(actual.buffer, actual.byteOffset, actual.byteLength);
        const vb = new Uint8Array(expected.buffer, expected.byteOffset, expected.byteLength);
        if (actual.byteLength === expected.byteLength) {
          for (let i = 0; i < va.length; i++) if (va[i] !== vb[i]) return false;
        } else if (!isPartialUint8Array(va, vb)) {
          return false;
        }
        return partialKeyEquiv(actual, expected, memo, true);
      }

      if (
        expected instanceof ArrayBuffer ||
        (typeof SharedArrayBuffer !== "undefined" && expected instanceof SharedArrayBuffer)
      ) {
        const va = new Uint8Array(actual);
        const vb = new Uint8Array(expected);
        if (va.length === vb.length) {
          for (let i = 0; i < va.length; i++) if (va[i] !== vb[i]) return false;
        } else if (!isPartialUint8Array(va, vb)) {
          return false;
        }
        return partialKeyEquiv(actual, expected, memo, false);
      }

      if (isRealSet(expected)) {
        if (!isRealSet(actual) || actual.size < expected.size) return false;
        if (!partialSetEquiv(actual, expected, memo)) return false;
        return partialKeyEquiv(actual, expected, memo, false);
      }

      if (isRealMap(expected)) {
        if (!isRealMap(actual) || actual.size < expected.size) return false;
        if (!partialMapEquiv(actual, expected, memo)) return false;
        return partialKeyEquiv(actual, expected, memo, false);
      }

      if (isRealDate(expected)) {
        // Read [[DateValue]] through the original getTime so an own override
        // cannot fool the check; a fake clone carrying Date.prototype throws.
        let ta, tb;
        try {
          ta = origGetTime.call(actual);
          tb = origGetTime.call(expected);
        } catch {
          return false;
        }
        if (!Object.is(ta, tb)) return false;
        return partialKeyEquiv(actual, expected, memo, false);
      }

      if (isRealRegExp(expected)) {
        // `source`/`flags` are prototype accessors: a fake clone wearing
        // RegExp.prototype throws instead of comparing, so treat that as
        // "not equal" rather than letting the TypeError escape.
        try {
          if (!isRealRegExp(actual) || actual.source !== expected.source || actual.flags !== expected.flags) {
            return false;
          }
        } catch {
          return false;
        }
        return partialKeyEquiv(actual, expected, memo, false);
      }

      {
        const ke = boxedKind(expected);
        if (ke !== null) {
          const ka = boxedKind(actual);
          if (ka !== ke) return false;
          if (!Object.is(boxedValue(actual, ka), boxedValue(expected, ke))) return false;
          return partialKeyEquiv(actual, expected, memo, false);
        }
      }

      if (isNativeErrorValue(expected)) {
        if (!isNativeErrorValue(actual)) return false;
        // message/name/cause/errors are non-enumerable, so the key loop below
        // never sees them. Node compares them explicitly, treating an absent
        // (undefined) or empty-string `message` on the expected side as "any".
        for (const p of ["name", "message", "cause", "errors"]) {
          if (Object.prototype.propertyIsEnumerable.call(expected, p)) continue;
          const want = expected[p];
          if (want === undefined || (p === "message" && want === "")) continue;
          if (!partialDeepEqualImpl(actual[p], want, memo)) return false;
        }
        // An expected `cause` requires an actual one; the reverse is fine.
        if (
          Object.prototype.hasOwnProperty.call(expected, "cause") &&
          !Object.prototype.hasOwnProperty.call(actual, "cause")
        ) {
          return false;
        }
        return partialKeyEquiv(actual, expected, memo, false);
      }

      return partialKeyEquiv(actual, expected, memo, false);
    }

    // Every own ENUMERABLE key (string + symbol) of `expected` must be an own
    // enumerable key of `actual` and match partially. Extra keys on `actual`
    // are what makes the comparison "partial".
    function partialKeyEquiv(actual, expected, memo, skipIndexKeys) {
      const keys = Object.keys(expected);
      for (const sym of Object.getOwnPropertySymbols(expected)) {
        if (Object.prototype.propertyIsEnumerable.call(expected, sym)) keys.push(sym);
      }
      for (const key of keys) {
        if (skipIndexKeys && typeof key === "string" && String(key >>> 0) === key) continue;
        const d = Object.getOwnPropertyDescriptor(actual, key);
        if (d === undefined || d.enumerable !== true) return false;
        if (!partialDeepEqualImpl(actual[key], expected[key], memo)) return false;
      }
      return true;
    }

    function isArrayHole(arr, i) {
      return arr[i] === undefined && !Object.prototype.hasOwnProperty.call(arr, i);
    }

    // Port of node partialSparseArrayEquiv: once either side is sparse, only the
    // DEFINED indices participate, still as an in-order subsequence.
    function partialSparseArrayEquiv(a, b, memo, startA, startB) {
      let aPos = 0;
      const keysA = Object.keys(a).slice(startA);
      const keysB = Object.keys(b).slice(startB);
      if (keysA.length < keysB.length) return false;
      for (let i = 0; i < keysB.length; i++) {
        const keyB = keysB[i];
        while (!partialDeepEqualImpl(a[keysA[aPos]], b[keyB], memo)) {
          aPos++;
          if (aPos > keysA.length - keysB.length + i) return false;
        }
        aPos++;
      }
      return true;
    }

    // Port of node partialArrayEquiv: expected is an in-order subsequence.
    function partialArrayEquiv(a, b, memo) {
      let aPos = 0;
      for (let i = 0; i < b.length; i++) {
        let isSparse = isArrayHole(b, i);
        if (isSparse) return partialSparseArrayEquiv(a, b, memo, aPos, i);
        while (!(isSparse = isArrayHole(a, aPos)) && !partialDeepEqualImpl(a[aPos], b[i], memo)) {
          aPos++;
          if (aPos > a.length - b.length + i) return false;
        }
        if (isSparse) return partialSparseArrayEquiv(a, b, memo, aPos, i);
        aPos++;
      }
      return true;
    }

    // Port of node isPartialUint8Array: byte-level in-order subsequence.
    function isPartialUint8Array(a, b) {
      if (a.length < b.length) return false;
      let offsetA = 0;
      for (let offsetB = 0; offsetB < b.length; offsetB++) {
        while (!Object.is(a[offsetA], b[offsetB])) {
          offsetA++;
          if (offsetA > a.length - b.length + offsetB) return false;
        }
        offsetA++;
      }
      return true;
    }

    // Each expected member must match a DISTINCT actual member.
    function partialSetEquiv(actual, expected, memo) {
      const pool = [...actual];
      const used = new Array(pool.length).fill(false);
      outer: for (const want of expected) {
        // Fast path for primitives held identically by the actual set.
        if ((want === null || typeof want !== "object") && actual.has(want)) {
          for (let i = 0; i < pool.length; i++) {
            if (!used[i] && Object.is(pool[i], want)) {
              used[i] = true;
              continue outer;
            }
          }
        }
        for (let i = 0; i < pool.length; i++) {
          if (!used[i] && partialDeepEqualImpl(pool[i], want, memo)) {
            used[i] = true;
            continue outer;
          }
        }
        return false;
      }
      return true;
    }

    // Each expected entry must match a DISTINCT actual entry (key AND value).
    function partialMapEquiv(actual, expected, memo) {
      const pool = [...actual];
      const used = new Array(pool.length).fill(false);
      outer: for (const [wantKey, wantVal] of expected) {
        for (let i = 0; i < pool.length; i++) {
          if (used[i]) continue;
          const [gotKey, gotVal] = pool[i];
          const keyOk =
            Object.is(gotKey, wantKey) ||
            (gotKey !== null && typeof gotKey === "object" && partialDeepEqualImpl(gotKey, wantKey, memo));
          if (!keyOk) continue;
          if (!partialDeepEqualImpl(gotVal, wantVal, memo)) continue;
          used[i] = true;
          continue outer;
        }
        return false;
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

    let utilGetCallSiteWarned = false;
    return {
      format,
      formatWithOptions: (opts, ...args) => {
        if (opts === null || typeof opts !== "object") {
          throw new codes.ERR_INVALID_ARG_TYPE("inspectOptions", "object", opts);
        }
        _fmtOpts = { ...inspect.defaultOptions, ...opts };
        if (args.length === 0) return "";
        return formatBody(args[0], args.slice(1));
      },
      inspect,
      getCallSites,
      // Deprecated singular alias (renamed to getCallSites): emit the
      // ExperimentalWarning once, then delegate.
      getCallSite(...args) {
        if (!utilGetCallSiteWarned) {
          utilGetCallSiteWarned = true;
          globalThis.process.emitWarning(
            "The `util.getCallSite` API has been renamed to `util.getCallSites()`.",
            "ExperimentalWarning",
          );
        }
        return getCallSites(...args);
      },
      parseArgs,
      aborted: (signal, resource) => {
        return new Promise((resolve) => {
          if (signal.aborted) { resolve(signal.reason); return; }
          signal.addEventListener("abort", () => resolve(signal.reason), { once: true });
        });
      },
      // JS port of Node v22.22.2 Dotenv::ParseContent, tuned against the
      // LIVE binary (the shipped parser trims remaining content after
      // unquoted / unterminated-quote lines -- order-dependent comment
      // behavior). Fuzz-verified byte-identical vs real node v22.22.2:
      // 5 seeds x 433 inputs (random + every adversarial-review repro +
      // the dotenv/valid.env fixture), 0 divergences, including key order
      // (UTF-8 byte sort, std::map) and '__proto__' no-op via plain Set.
      parseEnv: (content) => {
        if (typeof content !== "string") {
          throw new codes.ERR_INVALID_ARG_TYPE("content", "string", content);
        }
        // Node removes EVERY '\r' (std::remove), not just CRLF pairs.
        let s = content.replace(/\r/g, "");
        const store = new Map();
        // node_dotenv.cc trim_spaces: ' ', '\t', '\n' off BOTH ends.
        const trimSpaces = (str) => {
          let a = 0, b = str.length;
          while (a < b && (str[a] === " " || str[a] === "\t" || str[a] === "\n")) a++;
          while (b > a && (str[b - 1] === " " || str[b - 1] === "\t" || str[b - 1] === "\n")) b--;
          return str.slice(a, b);
        };
        s = trimSpaces(s);

        while (s.length > 0) {
          // Empty/comment lines: raw front char only -- leading spaces are
          // NOT skipped here (they were eaten by a preceding trim, or they
          // become part of a key and are trimmed there).
          if (s[0] === "\n" || s[0] === "#") {
            const nl = s.indexOf("\n");
            if (nl === -1) break;
            s = s.slice(nl + 1);
            continue;
          }

          // Next '=' or '\n', whichever first. A no-'=' line is skipped AND
          // the remainder trimmed (eats the next line's leading whitespace).
          let eqOrNl = -1;
          for (let i = 0; i < s.length; i++) {
            if (s[i] === "=" || s[i] === "\n") { eqOrNl = i; break; }
          }
          if (eqOrNl === -1) break;
          if (s[eqOrNl] === "\n") {
            s = trimSpaces(s.slice(eqOrNl + 1));
            continue;
          }

          let key = trimSpaces(s.slice(0, eqOrNl));
          s = s.slice(eqOrNl + 1);

          // Value-not-present: store (even for an empty key: '=' -> {"":""})
          // and leave the '\n' for the skip branch above.
          if (s.length === 0 || s[0] === "\n") {
            store.set(key, "");
            continue;
          }

          // Both-ends trim over ' \t\n': leading whitespace before the value
          // folds ACROSS newlines ('A= \nB=2' takes 'B=2' as A's value).
          s = trimSpaces(s);

          // Empty key with a value present: plain continue -- the rest of
          // the line re-parses as fresh content.
          if (key.length === 0) continue;

          if (key.startsWith("export ")) key = trimSpaces(key.slice(7));

          if (s.length === 0) {
            store.set(key, ""); // trailing 'KEY=   ' at EOF
            break;
          }

          const q = s[0];
          if (q === '"' || q === "'" || q === "`") {
            const close = s.indexOf(q, 1);
            if (close === -1) {
              // Unterminated quote: physical line is the value, verbatim
              // (including the opening quote); trailing trim after consuming.
              const nl = s.indexOf("\n");
              if (nl !== -1) {
                store.set(key, s.slice(0, nl));
                s = trimSpaces(s.slice(nl + 1));
              } else {
                store.set(key, s);
                break;
              }
            } else {
              let value = s.slice(1, close);
              // Only double-quoted values expand literal \n escapes.
              if (q === '"') value = value.replace(/\\n/g, "\n");
              store.set(key, value);
              const nl = s.indexOf("\n", close + 1);
              s = nl !== -1 ? s.slice(nl + 1) : "";
            }
          } else {
            // Unquoted: to newline (or EOF), strip inline #-comment, trim
            // value; the remaining content is ALSO trimmed after consuming.
            const nl = s.indexOf("\n");
            let value = nl !== -1 ? s.slice(0, nl) : s;
            const hash = value.indexOf("#");
            if (hash !== -1) value = value.slice(0, hash);
            store.set(key, trimSpaces(value));
            s = nl !== -1 ? trimSpaces(s.slice(nl + 1)) : "";
          }
        }

        // Node materializes from std::map: keys in UTF-8 byte order, set via
        // v8::Object::Set -- plain assignment, so '__proto__' is a silent
        // no-op exactly like node.
        const enc = new TextEncoder();
        const cmpUtf8 = (a, b) => {
          const ba = enc.encode(a), bb = enc.encode(b);
          const n = Math.min(ba.length, bb.length);
          for (let i = 0; i < n; i++) if (ba[i] !== bb[i]) return ba[i] - bb[i];
          return ba.length - bb.length;
        };
        const result = {};
        for (const k of [...store.keys()].sort(cmpUtf8)) result[k] = store.get(k);
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
      _partialDeepEqual: partialDeepEqualImpl,
      stripVTControlCharacters: (str) => {
        if (typeof str !== "string") {
          throw new codes.ERR_INVALID_ARG_TYPE("str", "string", str);
        }
        // Node's ANSI matcher (lib/internal/util/colors.js): covers CSI
        // escapes AND OSC sequences (\u001b]8;; hyperlinks terminated by BEL),
        // which the old CSI-only regex left behind.
        const ESC = String.fromCharCode(0x1b);
        const CSI8 = String.fromCharCode(0x9b);
        const BEL = String.fromCharCode(0x07);
        const ST9C = String.fromCharCode(0x9c);
        // OSC string terminator: BEL, ESC+'\' (7-bit ST), or 0x9C (8-bit ST).
        const STTERM = "(?:" + BEL + "|" + ESC + "\\\\|" + ST9C + ")";
        const ansiPattern =
          "[" + ESC + CSI8 + "][[\\]()#;?]*" +
          "(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*" +
          "|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?" + STTERM + ")" +
          "|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-ntqry=><~]))";
        // eslint-disable-next-line no-control-regex
        return str.replace(new RegExp(ansiPattern, "g"), "");
      },
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
      styleText: function styleText(format, text, options) {
        // Node's inspect.colors table (open/close SGR pairs).
        const colors = {
          reset: [0, 0], bold: [1, 22], dim: [2, 22], italic: [3, 23],
          underline: [4, 24], blink: [5, 25], inverse: [7, 27], hidden: [8, 28],
          strikethrough: [9, 29], doubleunderline: [21, 24], black: [30, 39],
          red: [31, 39], green: [32, 39], yellow: [33, 39], blue: [34, 39],
          magenta: [35, 39], cyan: [36, 39], white: [37, 39], bgBlack: [40, 49],
          bgRed: [41, 49], bgGreen: [42, 49], bgYellow: [43, 49], bgBlue: [44, 49],
          bgMagenta: [45, 49], bgCyan: [46, 49], bgWhite: [47, 49], framed: [51, 54],
          overlined: [53, 55], gray: [90, 39], redBright: [91, 39],
          greenBright: [92, 39], yellowBright: [93, 39], blueBright: [94, 39],
          magentaBright: [95, 39], cyanBright: [96, 39], whiteBright: [97, 39],
          bgGray: [100, 49], bgRedBright: [101, 49], bgGreenBright: [102, 49],
          bgYellowBright: [103, 49], bgBlueBright: [104, 49],
          bgMagentaBright: [105, 49], bgCyanBright: [106, 49],
          bgWhiteBright: [107, 49],
        };
        const escapeStyleCode = (code) => `\u001b[${code}m`;

        const opts = options || {};
        const validateStream = opts.validateStream === undefined ? true : opts.validateStream;
        const stream = opts.stream === undefined ? globalThis.process?.stdout : opts.stream;
        if (typeof text !== "string") {
          throw new codes.ERR_INVALID_ARG_TYPE("text", "string", text);
        }
        if (typeof validateStream !== "boolean") {
          throw new codes.ERR_INVALID_ARG_TYPE(
            "options.validateStream",
            "boolean",
            validateStream,
          );
        }
        if (validateStream) {
          // A stream-shaped object has write()/on(); reject anything else
          // (e.g. a bare {}). Node validates ReadableStream/WritableStream/
          // Stream; this heuristic covers the corpus cases.
          const looksLikeStream =
            stream != null &&
            (typeof stream.write === "function" ||
              typeof stream.on === "function" ||
              typeof stream.pipe === "function" ||
              typeof stream.getReader === "function" ||
              typeof stream.getWriter === "function");
          if (!looksLikeStream) {
            throw new codes.ERR_INVALID_ARG_TYPE(
              "stream",
              ["ReadableStream", "WritableStream", "Stream"],
              stream,
            );
          }
        }
        // oam cannot meaningfully introspect the stream's TTY-ness here; once a
        // valid stream is present, colorize (validateStream:false skips this).

        const formatArray = Array.isArray(format) ? format : [format];
        const codeList = [];
        for (const key of formatArray) {
          if (key === "none") continue;
          const formatCodes = colors[key];
          if (formatCodes == null) {
            throw new codes.ERR_INVALID_ARG_VALUE(
              "format",
              key,
              `must be one of: ${Object.keys(colors)
                .map((k) => `'${k}'`)
                .join(", ")}`,
            );
          }
          codeList.push(formatCodes);
        }

        let openCodes = "";
        for (let i = 0; i < codeList.length; i++) {
          openCodes += escapeStyleCode(codeList[i][0]);
        }

        let processedText = text;
        if (codeList.length > 0) {
          processedText = codeList.reduce(
            (acc, code) =>
              acc.replace(new RegExp(`\\u001b\\[${code[1]}m`, "g"), (match, offset) => {
                if (offset + match.length < acc.length) {
                  if (code[0] === colors.dim[0] || code[0] === colors.bold[0]) {
                    return `${match}${escapeStyleCode(code[0])}`;
                  }
                  return escapeStyleCode(code[0]);
                }
                return match;
              }),
            text,
          );
        }

        let closeCodes = "";
        for (let i = codeList.length - 1; i >= 0; i--) {
          closeCodes += escapeStyleCode(codeList[i][1]);
        }
        return `${openCodes}${processedText}${closeCodes}`;
      },
      log: function utilLog() {
        var d = new Date();
        var ts = d.getUTCDate() + " " + ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"][d.getUTCMonth()] + " " + ("0" + d.getUTCHours()).slice(-2) + ":" + ("0" + d.getUTCMinutes()).slice(-2) + ":" + ("0" + d.getUTCSeconds()).slice(-2);
        globalThis.process.stdout.write(ts + " - " + format.apply(null, arguments) + "\n");
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
        // These are V8-level checks in node and are documented to work across
        // realms, so they probe internal slots rather than compare prototypes.
        isDate: (v) => isRealDate(v),
        isRegExp: (v) => isRealRegExp(v),
        isNativeError: (v) => isNativeErrorValue(v),
        isPromise: (v) => natives.v8Is(v, "promise"),
        isMap: (v) => isRealMap(v),
        isSet: (v) => isRealSet(v),
        isWeakMap: (v) => isRealWeakMap(v),
        isWeakSet: (v) => isRealWeakSet(v),
        isArrayBuffer: (v) => isAnyArrayBuffer(v) && !isSharedArrayBufferValue(v),
        isSharedArrayBuffer: (v) => isSharedArrayBufferValue(v),
        isAnyArrayBuffer: (v) => isAnyArrayBuffer(v),
        isTypedArray: (v) => typedArrayName(v) !== undefined,
        isUint8Array: (v) => typedArrayName(v) === "Uint8Array",
        isDataView: (v) => isRealDataView(v),
        // Asked of V8 rather than inferred: `.constructor` is writable, so a
        // plain function decorated with AsyncFunction's constructor used to
        // read as async here, and isProxy could not be answered at all.
        isAsyncFunction: (v) => natives.v8Is(v, "asyncFunction"),
        isGeneratorFunction: (v) => natives.v8Is(v, "generatorFunction"),
        isProxy: (v) => natives.v8Is(v, "proxy"),
        isArrayBufferView: (v) => ArrayBuffer.isView(v),
        isUint8ClampedArray: (v) => typedArrayName(v) === "Uint8ClampedArray",
        isUint16Array: (v) => typedArrayName(v) === "Uint16Array",
        isUint32Array: (v) => typedArrayName(v) === "Uint32Array",
        isInt8Array: (v) => typedArrayName(v) === "Int8Array",
        isInt16Array: (v) => typedArrayName(v) === "Int16Array",
        isInt32Array: (v) => typedArrayName(v) === "Int32Array",
        isFloat32Array: (v) => typedArrayName(v) === "Float32Array",
        isFloat64Array: (v) => typedArrayName(v) === "Float64Array",
        isBigInt64Array: (v) => typedArrayName(v) === "BigInt64Array",
        isBigUint64Array: (v) => typedArrayName(v) === "BigUint64Array",
        isMapIterator: (v) => natives.v8Is(v, "mapIterator"),
        isSetIterator: (v) => natives.v8Is(v, "setIterator"),
        isGeneratorObject: (v) => natives.v8Is(v, "generatorObject"),
        isWeakRef: (v) => v instanceof WeakRef,
        // Was hardcoded false, so a real module namespace -- which
        // vm.SourceTextModule now hands out -- answered no.
        isModuleNamespaceObject: (v) => natives.v8Is(v, "moduleNamespaceObject"),
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

    // ---- colors (port of node internal/util/colors.js) ----------------------
    // Re-read from process.env on every refresh() so a test that flips
    // NO_COLOR / NODE_DISABLE_COLORS mid-run is honored.
    function colorDepthFor(stream) {
      const env = process.env;
      // Node checks FORCE_COLOR first -- it wins over NO_COLOR.
      if (env.FORCE_COLOR !== undefined) {
        switch (env.FORCE_COLOR) {
          case "":
          case "1":
          case "true":
            return 4;
          case "2":
            return 8;
          case "3":
            return 24;
          default:
            return 1;
        }
      }
      if (env.NODE_DISABLE_COLORS !== undefined || env.NO_COLOR !== undefined || env.TERM === "dumb") {
        return 1;
      }
      if (!stream || !stream.isTTY) return 1;
      if (typeof stream.getColorDepth === "function") return stream.getColorDepth();
      return 4;
    }
    const colors = {
      blue: "", green: "", white: "", red: "", gray: "", yellow: "", clear: "", reset: "",
      hasColors: false,
      shouldColorize(stream) {
        if (process.env.FORCE_COLOR !== undefined) return colorDepthFor(stream) > 2;
        return !!(stream && stream.isTTY) && colorDepthFor(stream) > 2;
      },
      refresh() {
        let hasColors = false;
        try {
          hasColors = colors.shouldColorize(process.stderr);
        } catch {
          hasColors = false;
        }
        colors.blue = hasColors ? "\u001b[34m" : "";
        colors.green = hasColors ? "\u001b[32m" : "";
        colors.white = hasColors ? "\u001b[39m" : "";
        colors.yellow = hasColors ? "\u001b[33m" : "";
        colors.red = hasColors ? "\u001b[31m" : "";
        colors.gray = hasColors ? "\u001b[90m" : "";
        colors.clear = hasColors ? "\u001bc" : "";
        colors.reset = hasColors ? "\u001b[0m" : "";
        colors.hasColors = hasColors;
      },
    };
    colors.refresh();

    // ---- Myers diff (port of node internal/assert/myers_diff.js) -----------
    const kNopLinesToCollapse = 5;
    const OP_DELETE = -1;
    const OP_NOP = 0;
    const OP_INSERT = 1;

    function areLinesEqual(actual, expected, checkCommaDisparity) {
      if (actual === expected) return true;
      if (checkCommaDisparity) {
        return `${actual},` === expected || actual === `${expected},`;
      }
      return false;
    }

    function myersDiff(actual, expected, checkCommaDisparity = false) {
      const actualLength = actual.length;
      const expectedLength = expected.length;
      const max = actualLength + expectedLength;
      if (max > 2 ** 31 - 1) {
        throw new codes.ERR_OUT_OF_RANGE("myersDiff input size", "< 2^31", max);
      }
      const v = new Int32Array(2 * max + 1);
      const trace = [];
      for (let diffLevel = 0; diffLevel <= max; diffLevel++) {
        trace.push(new Int32Array(v)); // snapshot of `v` at this level
        for (let diagonalIndex = -diffLevel; diagonalIndex <= diffLevel; diagonalIndex += 2) {
          const offset = diagonalIndex + max;
          const previousOffset = v[offset - 1];
          const nextOffset = v[offset + 1];
          let x =
            diagonalIndex === -diffLevel || (diagonalIndex !== diffLevel && previousOffset < nextOffset)
              ? nextOffset
              : previousOffset + 1;
          let y = x - diagonalIndex;
          while (x < actualLength && y < expectedLength && areLinesEqual(actual[x], expected[y], checkCommaDisparity)) {
            x++;
            y++;
          }
          v[offset] = x;
          if (x >= actualLength && y >= expectedLength) {
            return myersBacktrack(trace, actual, expected, checkCommaDisparity);
          }
        }
      }
      return undefined;
    }

    function myersBacktrack(trace, actual, expected, checkCommaDisparity) {
      const actualLength = actual.length;
      const expectedLength = expected.length;
      const max = actualLength + expectedLength;
      let x = actualLength;
      let y = expectedLength;
      const result = [];
      for (let diffLevel = trace.length - 1; diffLevel >= 0; diffLevel--) {
        const v = trace[diffLevel];
        const diagonalIndex = x - y;
        const offset = diagonalIndex + max;
        let prevDiagonalIndex;
        if (diagonalIndex === -diffLevel || (diagonalIndex !== diffLevel && v[offset - 1] < v[offset + 1])) {
          prevDiagonalIndex = diagonalIndex + 1;
        } else {
          prevDiagonalIndex = diagonalIndex - 1;
        }
        const prevX = v[prevDiagonalIndex + max];
        const prevY = prevX - prevDiagonalIndex;
        while (x > prevX && y > prevY) {
          const actualItem = actual[x - 1];
          const value = checkCommaDisparity && !actualItem.endsWith(",") ? expected[y - 1] : actualItem;
          result.push([OP_NOP, value]);
          x--;
          y--;
        }
        if (diffLevel > 0) {
          if (x > prevX) {
            result.push([OP_INSERT, actual[--x]]);
          } else {
            result.push([OP_DELETE, expected[--y]]);
          }
        }
      }
      return result;
    }

    function printSimpleMyersDiff(diff) {
      let message = "";
      for (let diffIdx = diff.length - 1; diffIdx >= 0; diffIdx--) {
        const [operation, value] = diff[diffIdx];
        let color = colors.white;
        if (operation === OP_INSERT) color = colors.green;
        else if (operation === OP_DELETE) color = colors.red;
        message += `${color}${value}${colors.white}`;
      }
      return `\n${message}`;
    }

    function printMyersDiff(diff, operator) {
      let message = "";
      let skipped = false;
      let nopCount = 0;
      for (let diffIdx = diff.length - 1; diffIdx >= 0; diffIdx--) {
        const [operation, value] = diff[diffIdx];
        const previousOperation = diffIdx < diff.length - 1 ? diff[diffIdx + 1][0] : null;
        // Avoid grouping if only one line would have been grouped otherwise.
        if (previousOperation === OP_NOP && operation !== previousOperation) {
          if (nopCount === kNopLinesToCollapse + 1) {
            message += `${colors.white}  ${diff[diffIdx + 1][1]}\n`;
          } else if (nopCount === kNopLinesToCollapse + 2) {
            message += `${colors.white}  ${diff[diffIdx + 2][1]}\n`;
            message += `${colors.white}  ${diff[diffIdx + 1][1]}\n`;
          } else if (nopCount >= kNopLinesToCollapse + 3) {
            message += `${colors.blue}...${colors.white}\n`;
            message += `${colors.white}  ${diff[diffIdx + 1][1]}\n`;
            skipped = true;
          }
          nopCount = 0;
        }
        if (operation === OP_INSERT) {
          if (operator === "partialDeepStrictEqual") {
            message += `${colors.gray}${colors.hasColors ? " " : "+"} ${value}${colors.white}\n`;
          } else {
            message += `${colors.green}+${colors.white} ${value}\n`;
          }
        } else if (operation === OP_DELETE) {
          message += `${colors.red}-${colors.white} ${value}\n`;
        } else if (operation === OP_NOP) {
          if (nopCount < kNopLinesToCollapse) {
            message += `${colors.white}  ${value}\n`;
          }
          nopCount++;
        }
      }
      message = message.trimEnd();
      return { message: `\n${message}`, skipped };
    }

    // ---- assertion messages (port of internal/assert/assertion_error.js) ---
    const kReadableOperator = {
      deepStrictEqual: "Expected values to be strictly deep-equal:",
      partialDeepStrictEqual: "Expected values to be partially and strictly deep-equal:",
      strictEqual: "Expected values to be strictly equal:",
      strictEqualObject: 'Expected "actual" to be reference-equal to "expected":',
      deepEqual: "Expected values to be loosely deep-equal:",
      notDeepStrictEqual: 'Expected "actual" not to be strictly deep-equal to:',
      notStrictEqual: 'Expected "actual" to be strictly unequal to:',
      notStrictEqualObject: 'Expected "actual" not to be reference-equal to "expected":',
      notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
      notIdentical: "Values have same structure but are not reference-equal:",
      notDeepEqualUnequal: "Expected values not to be loosely deep-equal:",
    };
    const kMaxShortStringLength = 12;
    const kMaxLongStringLength = 512;
    const kMethodsWithCustomMessageDiff = new Set(["deepStrictEqual", "strictEqual", "partialDeepStrictEqual"]);

    function isErrorLike(v) {
      return v instanceof Error || Object.prototype.toString.call(v) === "[object Error]";
    }

    function copyError(source) {
      const target = Object.assign({ __proto__: Object.getPrototypeOf(source) }, source);
      Object.defineProperty(target, "message", { __proto__: null, value: source.message });
      if (Object.prototype.hasOwnProperty.call(source, "cause")) {
        let { cause } = source;
        if (isErrorLike(cause)) cause = copyError(cause);
        Object.defineProperty(target, "cause", { __proto__: null, value: cause });
      }
      return target;
    }

    function inspectValue(val) {
      // The util.inspect default values could be changed. This makes sure the
      // error messages contain the necessary information nevertheless.
      return util.inspect(val, {
        compact: false,
        customInspect: false,
        depth: 1000,
        maxArrayLength: Infinity,
        // Assert compares only enumerable properties (with a few exceptions).
        showHidden: false,
        // Assert does not detect proxies currently.
        showProxy: false,
        sorted: true,
        // Inspect getters as we also check them when comparing entries.
        getters: true,
      });
    }

    function getErrorMessage(operator, message) {
      return message || kReadableOperator[operator];
    }

    function checkOperator(actual, expected, operator) {
      // In case both values are objects or functions explicitly mark them as
      // not reference equal for the `strictEqual` operator.
      if (
        operator === "strictEqual" &&
        ((typeof actual === "object" && actual !== null && typeof expected === "object" && expected !== null) ||
          (typeof actual === "function" && typeof expected === "function"))
      ) {
        operator = "strictEqualObject";
      }
      return operator;
    }

    function getColoredMyersDiff(actual, expected) {
      const header = `${colors.green}actual${colors.white} ${colors.red}expected${colors.white}`;
      const skipped = false;
      const diff = myersDiff(actual.split(""), expected.split(""));
      const message = printSimpleMyersDiff(diff);
      return { message, header, skipped };
    }

    function getStackedDiff(actual, expected) {
      const isStringComparison = typeof actual === "string" && typeof expected === "string";
      let message = `\n${colors.green}+${colors.white} ${actual}\n${colors.red}- ${colors.white}${expected}`;
      const stringsLen = actual.length + expected.length;
      let maxTerminalLength = 80;
      try {
        if (process.stderr.isTTY) maxTerminalLength = process.stderr.columns;
      } catch {
        maxTerminalLength = 80;
      }
      const showIndicator = isStringComparison && stringsLen <= maxTerminalLength;
      if (showIndicator) {
        let indicatorIdx = -1;
        for (let i = 0; i < actual.length; i++) {
          if (actual[i] !== expected[i]) {
            // Skip the indicator for the first 2 characters because the diff is
            // immediately apparent. It is 3 instead of 2 to account for quotes.
            if (i >= 3) indicatorIdx = i;
            break;
          }
        }
        if (indicatorIdx !== -1) {
          message += `\n${" ".repeat(indicatorIdx + 2)}^`;
        }
      }
      return { message };
    }

    function getSimpleDiff(originalActual, actual, originalExpected, expected) {
      let stringsLen = actual.length + expected.length;
      // Accounting for the quotes wrapping strings.
      if (typeof originalActual === "string") stringsLen -= 2;
      if (typeof originalExpected === "string") stringsLen -= 2;
      if (stringsLen <= kMaxShortStringLength && (originalActual !== 0 || originalExpected !== 0)) {
        return { message: `${actual} !== ${expected}`, header: "" };
      }
      const isStringComparison = typeof originalActual === "string" && typeof originalExpected === "string";
      if (isStringComparison && colors.hasColors) {
        return getColoredMyersDiff(actual, expected);
      }
      return getStackedDiff(actual, expected);
    }

    function isSimpleDiff(actual, inspectedActual, expected, inspectedExpected) {
      if (inspectedActual.length > 1 || inspectedExpected.length > 1) return false;
      return typeof actual !== "object" || actual === null || typeof expected !== "object" || expected === null;
    }

    function createErrDiff(actual, expected, operator, customMessage, diffType = "simple") {
      operator = checkOperator(actual, expected, operator);
      let skipped = false;
      let message = "";
      const inspectedActual = inspectValue(actual);
      const inspectedExpected = inspectValue(expected);
      const inspectedSplitActual = inspectedActual.split("\n");
      const inspectedSplitExpected = inspectedExpected.split("\n");
      const showSimpleDiff = isSimpleDiff(actual, inspectedSplitActual, expected, inspectedSplitExpected);
      let header = `${colors.green}+ actual${colors.white} ${colors.red}- expected${colors.white}`;

      if (showSimpleDiff) {
        const simpleDiff = getSimpleDiff(actual, inspectedSplitActual[0], expected, inspectedSplitExpected[0]);
        message = simpleDiff.message;
        if (typeof simpleDiff.header !== "undefined") header = simpleDiff.header;
        if (simpleDiff.skipped) skipped = true;
      } else if (inspectedActual === inspectedExpected) {
        // Structurally the same but different references.
        operator = "notIdentical";
        if (inspectedSplitActual.length > 50 && diffType !== "full") {
          message = `${inspectedSplitActual.slice(0, 50).join("\n")}\n...}`;
          skipped = true;
        } else {
          message = inspectedSplitActual.join("\n");
        }
        header = "";
      } else {
        const checkCommaDisparity = actual != null && typeof actual === "object";
        const diff = myersDiff(inspectedSplitActual, inspectedSplitExpected, checkCommaDisparity);
        const myersDiffMessage = printMyersDiff(diff, operator);
        message = myersDiffMessage.message;
        if (operator === "partialDeepStrictEqual") {
          header = `${colors.gray}${colors.hasColors ? "" : "+ "}actual${colors.white} ${colors.red}- expected${colors.white}`;
        }
        if (myersDiffMessage.skipped) skipped = true;
      }

      const headerMessage = `${getErrorMessage(operator, customMessage)}\n${header}`;
      const skippedMessage = skipped ? "\n... Skipped lines" : "";
      return `${headerMessage}${skippedMessage}\n${message}\n`;
    }

    function addEllipsis(string) {
      const lines = string.split("\n", 11);
      if (lines.length > 10) {
        lines.length = 10;
        return `${lines.join("\n")}\n...`;
      } else if (string.length > kMaxLongStringLength) {
        // NOTE: node slices FROM kMaxLongStringLength here (shipped behavior).
        return `${string.slice(kMaxLongStringLength)}...`;
      }
      return string;
    }

    // The `diff` option is per-Assert-instance in Node (`this?.[kOptions]?.diff`).
    // oam's assert methods are receiver-less arrows, so the active instance's
    // value is carried in this dynamically-scoped slot for the synchronous
    // extent of the call -- a destructured (receiver-less) call correctly falls
    // back to 'simple', exactly like Node.
    let currentDiff = "simple";
    // Monotonic id stamped on every AssertionError, so an assert-method wrapper
    // can tell an error IT caused from one merely passing through.
    let assertionErrorSerial = 0;
    const kAssertSerial = Symbol("assertSerial");

    // Re-anchor an AssertionError's stack at `stackStartFn`, dropping the
    // wrapper frame an Assert-instance method would otherwise leave behind.
    // The bracketed name is restored during capture so `.stack` keeps its
    // "AssertionError [ERR_ASSERTION]: ..." header.
    function recaptureAssertStack(err, stackStartFn) {
      if (typeof Error.captureStackTrace !== "function") return;
      const savedName = err.name;
      try {
        err.name = "AssertionError [ERR_ASSERTION]";
        Error.captureStackTrace(err, stackStartFn);
        err.stack; // eslint-disable-line no-unused-expressions
      } finally {
        err.name = savedName;
      }
    }

    class AssertionError extends Error {
      constructor(options) {
        if (typeof options !== "object" || options === null) {
          throw new codes.ERR_INVALID_ARG_TYPE("options", "Object", options);
        }
        const {
          message,
          operator,
          stackStartFn,
          details,
          stackStartFunction,
          diff = currentDiff,
        } = options;
        let { actual, expected } = options;

        const limit = Error.stackTraceLimit;
        Error.stackTraceLimit = 0;

        if (message != null) {
          if (kMethodsWithCustomMessageDiff.has(operator)) {
            // A custom message replaces only the HEADER line -- the diff body
            // is still produced for these three operators.
            super(createErrDiff(actual, expected, operator, message, diff));
          } else {
            super(String(message));
          }
        } else {
          // Reset colors on each call so a dynamically-set env var is honored.
          colors.refresh();
          // Prevent the error stack from being visible by duplicating the error
          // in a very close way to the original in case both sides are Errors.
          if (
            typeof actual === "object" && actual !== null &&
            typeof expected === "object" && expected !== null &&
            "stack" in actual && actual instanceof Error &&
            "stack" in expected && expected instanceof Error
          ) {
            actual = copyError(actual);
            expected = copyError(expected);
          }

          if (kMethodsWithCustomMessageDiff.has(operator)) {
            super(createErrDiff(actual, expected, operator, message, diff));
          } else if (operator === "notDeepStrictEqual" || operator === "notStrictEqual") {
            // The objects are equal but the operator requires unequal: show the
            // first object and say A equals B.
            let base = kReadableOperator[operator];
            const res = inspectValue(actual).split("\n");
            if (
              operator === "notStrictEqual" &&
              ((typeof actual === "object" && actual !== null) || typeof actual === "function")
            ) {
              base = kReadableOperator.notStrictEqualObject;
            }
            // Only remove lines in case it makes sense to collapse those.
            if (res.length > 50 && diff !== "full") {
              res[46] = `${colors.blue}...${colors.white}`;
              while (res.length > 47) res.pop();
            }
            if (res.length === 1) {
              super(`${base}${res[0].length > 5 ? "\n\n" : " "}${res[0]}`);
            } else {
              super(`${base}\n\n${res.join("\n")}\n`);
            }
          } else {
            let res = inspectValue(actual);
            let other = inspectValue(expected);
            const knownOperator = kReadableOperator[operator];
            if (operator === "notDeepEqual" && res === other) {
              res = `${knownOperator}\n\n${res}`;
              if (res.length > 1024 && diff !== "full") res = `${res.slice(0, 1021)}...`;
              super(res);
            } else {
              if (res.length > kMaxLongStringLength && diff !== "full") res = `${res.slice(0, 509)}...`;
              if (other.length > kMaxLongStringLength && diff !== "full") other = `${other.slice(0, 509)}...`;
              if (operator === "deepEqual") {
                res = `${knownOperator}\n\n${res}\n\nshould loosely deep-equal\n\n`;
              } else {
                const newOp = kReadableOperator[`${operator}Unequal`];
                if (newOp) {
                  res = `${newOp}\n\n${res}\n\nshould not loosely deep-equal\n\n`;
                } else {
                  other = ` ${operator} ${other}`;
                }
              }
              super(`${res}${other}`);
            }
          }
        }

        Error.stackTraceLimit = limit;

        // assert.ok builds its own message text (it needs the call-site source)
        // but node still reports generatedMessage=true for it.
        this.generatedMessage = !message || options.forceGeneratedMessage === true;
        Object.defineProperty(this, "name", {
          __proto__: null,
          value: "AssertionError [ERR_ASSERTION]",
          enumerable: false,
          writable: true,
          configurable: true,
        });
        this.code = "ERR_ASSERTION";
        if (details) {
          this.actual = undefined;
          this.expected = undefined;
          this.operator = undefined;
          for (let i = 0; i < details.length; i++) {
            this["message " + i] = details[i].message;
            this["actual " + i] = details[i].actual;
            this["expected " + i] = details[i].expected;
            this["operator " + i] = details[i].operator;
            this["stack trace " + i] = details[i].stack;
          }
        } else {
          this.actual = actual;
          this.expected = expected;
          this.operator = operator;
        }
        Object.defineProperty(this, kAssertSerial, {
          __proto__: null,
          value: ++assertionErrorSerial,
          enumerable: false,
          writable: true,
          configurable: true,
        });
        if (typeof Error.captureStackTrace === "function") {
          Error.captureStackTrace(this, stackStartFn || stackStartFunction);
        }
        // Materialize the stack while `name` still carries the code, then
        // reset the name -- this is what puts "[ERR_ASSERTION]" in `.stack`.
        this.stack; // eslint-disable-line no-unused-expressions
        this.name = "AssertionError";
        this.diff = diff;
      }

      toString() {
        return `${this.name} [${this.code}]: ${this.message}`;
      }

      [util.inspect.custom](recurseTimes, ctx) {
        // Long strings should not be fully inspected.
        const tmpActual = this.actual;
        const tmpExpected = this.expected;
        if (typeof this.actual === "string") this.actual = addEllipsis(this.actual);
        if (typeof this.expected === "string") this.expected = addEllipsis(this.expected);
        // Limit `actual`/`expected` inspection to the minimum depth; otherwise
        // they would be far more verbose than the combined message above.
        const result = util.inspect(this, { ...ctx, customInspect: false, depth: 0 });
        this.actual = tmpActual;
        this.expected = tmpExpected;
        return result;
      }
    }

    function innerFail(actual, expected, message, operator, stackStartFn) {
      // Node's innerFail THROWS an Error passed as the message.
      if (message instanceof Error) throw message;
      throw new AssertionError({ actual, expected, message, operator, stackStartFn });
    }

    // ---- assert.ok source extraction (node internal/assert/utils.js) ----
    // Node quotes the FAILING EXPRESSION in the message. It does not parse the
    // file: it takes the call site's ONE source line off the V8 stack and
    // tokenizes it, walking back from the call column to the start of the
    // member chain and forward to the matching ')'. Verified byte-identical
    // to node v22.22.2 on 16 shapes (computed access, ')' inside a string,
    // unicode identifiers, tagged templates, a preceding statement).
    const ASSERT_MEMBER_PUNCT = new Set([".", "?.", "[", "]"]);
    const assertSourceCache = new Map();

    function assertTokenizeLine(code) {
      const tokens = [];
      let i = 0;
      const n = code.length;
      const isIdStart = (c) => /[\p{ID_Start}$_]/u.test(c);
      const isIdPart = (c) => /[\p{ID_Continue}$]/u.test(c);
      while (i < n) {
        const c = code[i];
        if (c === " " || c === "\t" || c === "\r" || c === "\v" || c === "\f") { i++; continue; }
        if (c === "/" && code[i + 1] === "/") break;
        if (c === "/" && code[i + 1] === "*") {
          const end = code.indexOf("*/", i + 2);
          i = end === -1 ? n : end + 2;
          continue;
        }
        const start = i;
        if (c === '"' || c === "'" || c === "`") {
          const quote = c;
          i++;
          while (i < n) {
            if (code[i] === "\\") { i += 2; continue; }
            if (code[i] === quote) { i++; break; }
            i++;
          }
          tokens.push({ type: "string", start, end: i });
          continue;
        }
        if (/[0-9]/.test(c) || (c === "." && /[0-9]/.test(code[i + 1] || ""))) {
          i++;
          while (i < n && /[0-9a-fA-FxXoObBeE._n]/.test(code[i])) i++;
          tokens.push({ type: "num", start, end: i });
          continue;
        }
        if (isIdStart(c)) {
          i++;
          while (i < n && isIdPart(code[i])) i++;
          tokens.push({ type: "name", start, end: i });
          continue;
        }
        const three = code.slice(i, i + 3);
        const two = code.slice(i, i + 2);
        let punct;
        if (three === "...") punct = three;
        else if (["?.", "=>", "&&", "||", "??", "==", "!=", "<=", ">=", "++", "--", "**"].includes(two)) punct = two;
        else punct = c;
        i += punct.length;
        tokens.push({ type: "punct", value: punct, start, end: i });
      }
      return tokens;
    }

    function assertFirstExpression(code, startColumn) {
      const tokens = assertTokenizeLine(code);
      let chainStart = -1;
      let idx = 0;
      for (; idx < tokens.length; idx++) {
        const t = tokens[idx];
        if (t.start >= startColumn) break;
        if (t.type === "name") {
          if (chainStart === -1) chainStart = t.start;
        } else if (t.type === "punct" && ASSERT_MEMBER_PUNCT.has(t.value)) {
          // still inside the member chain
        } else if (t.type === "string" || t.type === "num") {
          // computed member content
        } else {
          chainStart = -1;
        }
        if (t.type === "punct" && t.value === ";") chainStart = -1;
      }
      if (chainStart === -1) chainStart = startColumn;
      let depth = 0;
      for (; idx < tokens.length; idx++) {
        const t = tokens[idx];
        if (t.type !== "punct") continue;
        if (t.value === "(") depth++;
        else if (t.value === ")") {
          depth--;
          if (depth <= 0) return code.slice(chainStart, t.end);
        } else if (t.value === ";" && depth === 0) {
          return code.slice(chainStart, t.start);
        }
      }
      return code.slice(chainStart);
    }

    // Returns the source text of the assert call that is `stackStartFn`'s
    // caller, or null when it cannot be recovered (eval, missing file, a
    // read error). Never throws -- a failed extraction must not replace the
    // user's assertion failure.
    function assertCallSource(stackStartFn) {
      const target = {};
      const originalPrepare = Error.prepareStackTrace;
      const originalLimit = Error.stackTraceLimit;
      try {
        Error.prepareStackTrace = (_e, frames) => frames;
        Error.stackTraceLimit = 10;
        Error.captureStackTrace(target, stackStartFn);
        const frames = Array.isArray(target.stack) ? target.stack : [];
        // assert.ok is exposed through an anonymous wrapper, so the frame
        // right after stackStartFn is still oam-internal. node_compat.js is
        // snapshot-embedded and its frames report no real filename, so the
        // first frame with an absolute path IS the user's call site.
        let file = null;
        let lineNo = 0;
        let colNo = 0;
        for (const f of frames) {
          const name = typeof f.getFileName === "function" ? f.getFileName() : null;
          if (!name || !/^[a-zA-Z]:[\\/]|^\//.test(name)) continue; // eval / virtual / internal
          file = name;
          lineNo = typeof f.getLineNumber === "function" ? f.getLineNumber() : 0;
          colNo = typeof f.getColumnNumber === "function" ? f.getColumnNumber() : 0;
          break;
        }
        if (!file || !lineNo || !colNo) return null;
        let lines = assertSourceCache.get(file);
        if (lines === undefined) {
          try {
            // `natives` is not in this factory's closure; read through the
            // same global the runtime exposes.
            const bytes = globalThis.__oam.node.fsReadFileSync(file);
            lines = (typeof bytes === "string" ? bytes : new TextDecoder().decode(bytes)).split("\n");
          } catch {
            lines = null;
          }
          assertSourceCache.set(file, lines);
        }
        if (!lines) return null;
        const line = lines[lineNo - 1];
        if (typeof line !== "string") return null;
        const expr = assertFirstExpression(line.replace(/\r$/, ""), colNo - 1);
        return expr ? expr.trim() : null;
      } catch {
        return null;
      } finally {
        Error.prepareStackTrace = originalPrepare;
        Error.stackTraceLimit = originalLimit;
      }
    }

    function ok(value, message) {
      if (arguments.length === 0) {
        throw new AssertionError({
          actual: undefined,
          expected: true,
          message: "No value argument passed to `assert.ok()`",
          operator: "==",
          stackStartFn: ok,
        });
      }
      if (!value) {
        if (message instanceof Error) throw message;
        let generated = message;
        if (generated === undefined) {
          const source = assertCallSource(ok);
          generated = source
            ? `The expression evaluated to a falsy value:\n\n  ${source}\n`
            : "The expression evaluated to a falsy value";
        }
        throw new AssertionError({
          actual: value,
          expected: true,
          message: generated,
          operator: "==",
          stackStartFn: ok,
          // node reports generatedMessage=true whenever IT built the text.
          forceGeneratedMessage: message === undefined,
        });
      }
    }

    // Placeholder object pair that makes a validation-object mismatch render
    // as a clean key-by-key diff (Node's `Comparison`).
    class Comparison {
      constructor(obj, keys, actual) {
        for (const key of keys) {
          if (key in obj) {
            if (
              actual !== undefined &&
              typeof actual[key] === "string" &&
              obj[key] instanceof RegExp &&
              obj[key].test(actual[key])
            ) {
              this[key] = actual[key];
            } else {
              this[key] = obj[key];
            }
          }
        }
      }
    }

    // `operator` / `stackStartFn` shape the AssertionError raised when a
    // validation function misbehaves. `soft` makes a non-`true` return a plain
    // false (Node's hasMatchingError, used by the doesNot* family) instead.
    // `message` is the caller's custom message: when present, a mismatch
    // returns false so the caller can build the message-carrying error.
    function checkExpected(err, expected, operator = "throws", stackStartFn = undefined, soft = false, message = undefined) {
      if (expected instanceof RegExp) {
        // Node tests the regexp against String(err) -- i.e. err.toString(),
        // which for coded errors renders "TypeError [ERR_X]: msg" via
        // applyNodeErrorShape. A regex like /ERR_OUT_OF_RANGE/ matches that
        // rendered form even though it isn't present in the bare .message.
        const str = String(err);
        if (expected.test(str)) return true;
        if (soft || message !== undefined) return false;
        const rerr = new AssertionError({
          actual: err,
          expected,
          message:
            `The input did not match the regular expression ${util.inspect(expected)}. ` +
            `Input:\n\n${util.inspect(str)}\n`,
          operator,
          stackStartFn,
        });
        rerr.generatedMessage = true;
        throw rerr;
      }
      if (typeof expected === "function") {
        if (expected.prototype !== undefined && err instanceof expected) return true;
        // An Error constructor at ANY depth (B extends A extends Error) is a
        // class check, not a validation function: instanceof already failed
        // above, so report the mismatch. Calling it as expected(err) would throw
        // "Class constructor cannot be invoked without 'new'". Error.isPrototypeOf
        // catches the whole chain; `=== Error` covers Error itself.
        if (expected === Error || Error.isPrototypeOf(expected)) {
          if (soft || message !== undefined) return false;
          let msg = `The error is expected to be an instance of "${expected.name}". Received `;
          if (isErrorLike(err)) {
            const name = (err.constructor && err.constructor.name) || err.name;
            if (expected.name === name) {
              msg += "an error with identical name but a different prototype.";
            } else {
              msg += `"${name}"`;
            }
            if (err.message) msg += `\n\nError message:\n\n${err.message}`;
          } else {
            msg += `"${util.inspect(err, { depth: -1 })}"`;
          }
          const cerr = new AssertionError({ actual: err, expected, message: msg, operator, stackStartFn });
          cerr.generatedMessage = true;
          throw cerr;
        }
        // Validation function: Node requires it to return EXACTLY `true`. Any
        // other return value is its own failure mode -- a specific AssertionError
        // embedding the inspected return + the caught error -- NOT the generic
        // "does not match the expected pattern". A throw inside propagates.
        const ret = expected(err);
        if (ret === true) return true;
        if (soft) return false;
        // Node names the function in the message and only appends the caught
        // error when the thrown value actually is one.
        const fnName = expected.name ? `"${expected.name}" ` : "";
        let msg =
          `The ${fnName}validation function is expected to return "true". Received ` + util.inspect(ret);
        if (isErrorLike(err)) msg += `\n\nCaught error:\n\n${err}`;
        const aerr = new AssertionError({
          actual: err,
          expected,
          message: msg,
          operator,
          stackStartFn,
        });
        aerr.generatedMessage = true;
        throw aerr;
      }
      if (expected && typeof expected === "object") {
        // A validation object cannot be compared against a primitive: Node
        // reports the whole value with a deepStrictEqual-shaped diff.
        if (typeof err !== "object" || err === null) {
          if (soft) return false;
          const perr = new AssertionError({
            actual: err,
            expected,
            message,
            operator: "deepStrictEqual",
            stackStartFn,
          });
          perr.operator = operator;
          throw perr;
        }
        // Validation object (Node's compareExceptionKey): for every own
        // enumerable key of `expected`, compare against the error. A RegExp
        // value is `.test()`ed against the error's STRING property; any other
        // value (name, code, arbitrary prop) is deepStrictEqual'd. String
        // `message` therefore compares by EQUALITY, RegExp `message` by
        // `.test`, exactly matching Node.
        const keys = Object.keys(expected);
        // Errors also compare name + message even when not listed.
        if (isErrorLike(expected)) {
          keys.push("name", "message");
        } else if (keys.length === 0) {
          throw new codes.ERR_INVALID_ARG_VALUE("error", expected, "may not be an empty object");
        }
        for (const key of keys) {
          const want = expected[key];
          const got = err[key];
          if (typeof got === "string" && want instanceof RegExp && want.test(got)) continue;
          if (!(key in err) || !deepEqual(got, want, true)) {
            if (soft) return false;
            if (message !== undefined) return false; // caller attaches the custom message
            // Node builds paired placeholder objects so the diff shows only
            // the compared keys, then re-points actual/expected at the reals.
            const a = new Comparison(err, keys);
            const b = new Comparison(expected, keys, err);
            const cerr = new AssertionError({
              actual: a,
              expected: b,
              operator: "deepStrictEqual",
              stackStartFn,
            });
            cerr.actual = err;
            cerr.expected = expected;
            cerr.operator = operator;
            throw cerr;
          }
        }
        return true;
      }
      return true;
    }

    function throws(fn, ...args) {
      let actual = NO_EXCEPTION_SENTINEL;
      try {
        fn();
      } catch (e) {
        actual = e;
      }
      return expectsError("throws", "exception", actual, args[0], args[1], throws, args.length + 1);
    }

    // ---- async throws/does-not-reject plumbing (port of node assert.js) ----
    // A unique sentinel meaning "the callee did not throw / reject".
    const NO_EXCEPTION_SENTINEL = {};

    // Accept native promises and promise-likes, but NOT a thenable that is a
    // function or that lacks `catch` (Node checkIsPromise).
    function checkIsPromise(obj) {
      return (
        obj instanceof Promise ||
        (obj !== null &&
          typeof obj === "object" &&
          typeof obj.then === "function" &&
          typeof obj.catch === "function")
      );
    }

    async function waitForActual(promiseFn) {
      let resultPromise;
      if (typeof promiseFn === "function") {
        // A synchronous throw from promiseFn propagates out as a rejection.
        resultPromise = promiseFn();
        if (!checkIsPromise(resultPromise)) {
          throw new codes.ERR_INVALID_RETURN_VALUE("instance of Promise", "promiseFn", resultPromise);
        }
      } else if (checkIsPromise(promiseFn)) {
        resultPromise = promiseFn;
      } else {
        throw new codes.ERR_INVALID_ARG_TYPE("promiseFn", ["Function", "Promise"], promiseFn);
      }
      try {
        await resultPromise;
      } catch (e) {
        return e;
      }
      return NO_EXCEPTION_SENTINEL;
    }

    // Port of node expectsError's tail, shared by throws/rejects.
    function expectsError(operator, fnType, actual, expected, message, stackStartFn, argCount) {
      if (typeof expected === "string") {
        if (argCount >= 3) {
          throw new codes.ERR_INVALID_ARG_TYPE("error", ["Object", "Error", "Function", "RegExp"], expected);
        }
        message = expected;
        expected = undefined;
      } else if (expected != null && typeof expected !== "object" && typeof expected !== "function") {
        throw new codes.ERR_INVALID_ARG_TYPE("error", ["Object", "Error", "Function", "RegExp"], expected);
      }
      if (actual === NO_EXCEPTION_SENTINEL) {
        let details = "";
        if (expected && expected.name) details += ` (${expected.name})`;
        details += message ? `: ${message}` : ".";
        throw new AssertionError({
          actual: undefined,
          expected,
          operator,
          message: `Missing expected ${fnType}${details}`,
          stackStartFn,
        });
      }
      if (!expected) return actual;
      if (!checkExpected(actual, expected, operator, stackStartFn, false, message)) {
        if (actual instanceof AssertionError) throw actual;
        const err = new AssertionError({
          actual,
          expected,
          message: message ?? "The error does not match the expected pattern",
          operator,
          stackStartFn,
        });
        // Node reports generatedMessage=true when IT produced the text.
        if (message === undefined) err.generatedMessage = true;
        throw err;
      }
      return actual;
    }

    // Port of node expectsNoError: rethrow a NON-matching error untouched;
    // only a matching (or unconstrained) one becomes "Got unwanted ...".
    function expectsNoError(operator, fnType, actual, expected, message) {
      if (actual === NO_EXCEPTION_SENTINEL) return;
      if (typeof expected === "string") {
        message = expected;
        expected = undefined;
      }
      if (!expected || checkExpected(actual, expected, operator, undefined, true)) {
        const details = message ? `: ${message}` : ".";
        throw new AssertionError({
          actual,
          expected,
          operator,
          message: `Got unwanted ${fnType}${details}\nActual message: "${actual === null || actual === undefined ? undefined : actual.message}"`,
          stackStartFn: operator === "doesNotReject" ? doesNotReject : doesNotThrowImpl,
        });
      }
      throw actual;
    }

    // Named so it can serve as its own `stackStartFn` (the assert frames must
    // not appear in the reported stack).
    function doesNotThrowImpl(fn, ...args) {
      let actual = NO_EXCEPTION_SENTINEL;
      try {
        fn();
      } catch (e) {
        actual = e;
      }
      return expectsNoError("doesNotThrow", "exception", actual, args[0], args[1]);
    }

    async function rejects(promiseFn, ...args) {
      return expectsError("rejects", "rejection", await waitForActual(promiseFn), args[0], args[1], rejects, args.length + 1);
    }

    async function doesNotReject(promiseFn, ...args) {
      return expectsNoError("doesNotReject", "rejection", await waitForActual(promiseFn), args[0], args[1]);
    }

    // DEP0094 fires at most once per process, like Node's `warned` flag.
    let assertFailWarned = false;
    const assert = Object.assign(ok, {
      AssertionError,
      ok,
      fail: (...failArgs) => {
        const argsLen = failArgs.length;
        const [actual, expected, message, operator, stackStartFn] = failArgs;
        // assert.fail() / assert.fail(null) -- Node's `internalMessage` path:
        // the literal message "Failed" but still generatedMessage === true.
        if (actual == null && argsLen <= 1) {
          throw new AssertionError({
            actual: undefined,
            expected: undefined,
            message: "Failed",
            operator: "fail",
            forceGeneratedMessage: true,
          });
        }
        // assert.fail(message) -- 1 arg: an Error rethrows; otherwise the arg
        // is the user-supplied message.
        if (argsLen === 1) {
          if (actual instanceof Error) throw actual;
          throw new AssertionError({
            actual: undefined,
            expected: undefined,
            message: actual,
            operator: "fail",
          });
        }
        if (expected instanceof Error) throw expected;
        if (message instanceof Error) throw message;
        // DEP0094, emitted ONCE: the multi-argument form reads like an
        // equality assertion but never compares anything, so Node steers
        // callers to strictEqual.
        if (!assertFailWarned) {
          assertFailWarned = true;
          process.emitWarning(
            "assert.fail() with more than one argument is deprecated. " +
              "Please use assert.strictEqual() instead or only pass a message.",
            "DeprecationWarning",
            "DEP0094",
          );
        }
        throw new AssertionError({
          actual,
          expected,
          message,
          // EXACTLY two arguments means "these differ", so the operator is
          // '!=' and the message reads "'a' != 'b'". Defaulting to 'fail'
          // produced the nonsense "'first' fail 'second'".
          operator: operator || (argsLen === 2 ? "!=" : "fail"),
          stackStartFn: stackStartFn || undefined,
        });
      },
      // The whole equality family validates arity: Node throws ERR_MISSING_ARGS
      // when called with fewer than two arguments.
      equal: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        // eslint-disable-next-line eqeqeq
        if (!(actual == expected || (Number.isNaN(actual) && Number.isNaN(expected)))) {
          innerFail(actual, expected, message, "==");
        }
      },
      notEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        // eslint-disable-next-line eqeqeq
        if (actual == expected) innerFail(actual, expected, message, "!=");
      },
      strictEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        if (!Object.is(actual, expected)) {
          innerFail(actual, expected, message, "strictEqual");
        }
      },
      notStrictEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        if (Object.is(actual, expected)) {
          innerFail(actual, expected, message, "notStrictEqual");
        }
      },
      deepEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        if (!deepEqual(actual, expected, false)) {
          innerFail(actual, expected, message, "deepEqual");
        }
      },
      notDeepEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        if (deepEqual(actual, expected, false)) {
          innerFail(actual, expected, message, "notDeepEqual");
        }
      },
      deepStrictEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        if (!deepEqual(actual, expected, true)) {
          innerFail(actual, expected, message, "deepStrictEqual");
        }
      },
      notDeepStrictEqual: function (actual, expected, message) {
        if (arguments.length < 2) throw new codes.ERR_MISSING_ARGS("actual", "expected");
        if (deepEqual(actual, expected, true)) {
          innerFail(actual, expected, message, "notDeepStrictEqual");
        }
      },
      throws,
      // (fn, error, message) -- an error that does NOT match `error` is
      // rethrown untouched rather than reported as unwanted.
      doesNotThrow: doesNotThrowImpl,
      rejects,
      doesNotReject,
      match: (string, regexp, message) => {
        if (!regexp.test(string)) {
          innerFail(string, regexp, message ?? `The input did not match the regular expression`, "match");
        }
      },
      ifError: (err) => {
        if (err !== null && err !== undefined) {
          // Node renders the offending value via util.inspect (so a string is
          // quoted); an Error contributes its message (or constructor name when
          // empty).
          let message = "ifError got unwanted exception: ";
          if (typeof err === "object" && typeof err.message === "string") {
            if (err.message.length === 0 && err.constructor) {
              message += err.constructor.name;
            } else {
              message += err.message;
            }
          } else {
            message += util.inspect(err);
          }
          const newErr = new AssertionError({
            actual: err,
            expected: null,
            operator: "ifError",
            message,
          });

          // Merge the original error's frames into the new stack the way Node
          // does (so the unwanted exception's origin is visible) -- but only
          // when the original stack actually has "\n    at" frames.
          const origStack = err && err.stack;
          if (typeof origStack === "string") {
            const origStackStart = origStack.indexOf("\n    at");
            if (origStackStart !== -1) {
              const originalFrames = origStack
                .slice(origStackStart + 1)
                .split("\n");
              let newFrames = String(newErr.stack).split("\n");
              for (const errFrame of originalFrames) {
                const pos = newFrames.indexOf(errFrame);
                if (pos !== -1) {
                  newFrames = newFrames.slice(0, pos);
                  break;
                }
              }
              newErr.stack = `${newFrames.join("\n")}\n${originalFrames.join("\n")}`;
            }
          }
          throw newErr;
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
    // ---- regexp validation for match / doesNotMatch ----
    function validateRegExp(regexp, name) {
      if (!(regexp instanceof RegExp)) {
        throw new codes.ERR_INVALID_ARG_TYPE(name, "RegExp", regexp);
      }
    }
    const origMatch = assert.match;
    assert.match = (string, regexp, message) => {
      validateRegExp(regexp, "regexp");
      return origMatch(string, regexp, message);
    };
    const origDoesNotMatch = assert.doesNotMatch;
    assert.doesNotMatch = (string, regexp, message) => {
      validateRegExp(regexp, "regexp");
      return origDoesNotMatch(string, regexp, message);
    };

    // ---- partialDeepStrictEqual ----
    // A structural "expected is a partial subset of actual" check. Full
    // colored Myers-diff message output is out of scope; the function exists,
    // validates arity, and throws an ERR_ASSERTION AssertionError on mismatch.
    function partialDeepStrictEqual(actual, expected, message) {
      if (arguments.length < 2) {
        throw new codes.ERR_MISSING_ARGS("actual", "expected");
      }
      if (!util._partialDeepEqual(actual, expected)) {
        innerFail(actual, expected, message, "partialDeepStrictEqual");
      }
    }
    assert.partialDeepStrictEqual = partialDeepStrictEqual;

    // ---- Assert class (constructable; bare call requires `new`) ----
    // new Assert({ strict, diff }) yields an assert-like instance. Errors it
    // throws carry `.diff` from the instance option (default 'simple'). When a
    // method is destructured off the instance it loses `this`, so `.diff`
    // falls back to 'simple' -- matching Node's observable behavior.
    function bindAssertMethod(fn) {
      const wrapper = function (...args) {
        // The error's `diff` comes from the receiver instance when the method
        // is called as a method; a destructured (receiver-less) call loses
        // `this`, so it falls back to the default 'simple'.
        const prev = currentDiff;
        const serialBefore = assertionErrorSerial;
        currentDiff = this && this.diff !== undefined ? this.diff : "simple";
        try {
          return fn.apply(this, args);
        } catch (e) {
          // Only re-anchor errors THIS call produced -- one merely passing
          // through (a user's own AssertionError) keeps its original stack.
          if (e instanceof AssertionError && e[kAssertSerial] > serialBefore) {
            recaptureAssertStack(e, wrapper);
          }
          throw e;
        } finally {
          currentDiff = prev;
        }
      };
      return wrapper;
    }
    function Assert(options) {
      if (!new.target) {
        throw new codes.ERR_CONSTRUCT_CALL_REQUIRED("Assert");
      }
      // Node defaults `strict` to TRUE and validates `diff` against the
      // allowed set.
      const strict = options && options.strict !== undefined ? !!options.strict : true;
      const diff = options && options.diff !== undefined ? options.diff : "simple";
      if (options && options.diff !== undefined && diff !== "simple" && diff !== "full") {
        // Node's validateOneOf phrasing (a dotted name reads as "property").
        throw codedError(
          TypeError,
          "ERR_INVALID_ARG_VALUE",
          `The property 'options.diff' must be one of: 'simple', 'full'. Received ${util.inspect(options.diff)}`,
        );
      }
      const self = this;
      self.diff = diff;
      self.strict = strict;
      self.AssertionError = AssertionError;
      const methods = [
        "ok", "equal", "notEqual", "strictEqual", "notStrictEqual",
        "deepEqual", "notDeepEqual", "deepStrictEqual", "notDeepStrictEqual",
        "throws", "doesNotThrow", "rejects", "doesNotReject", "match",
        "doesNotMatch", "ifError", "fail", "partialDeepStrictEqual",
      ];
      // Always bind the loose family, then -- exactly like Node -- ALIAS the
      // loose names onto the already-bound strict ones when strict. Aliasing
      // (rather than binding twice) is what makes
      // `instance.equal === instance.strictEqual` hold.
      for (const m of methods) {
        if (typeof assert[m] === "function") {
          self[m] = bindAssertMethod(assert[m]);
        }
      }
      if (strict) {
        self.equal = self.strictEqual;
        self.deepEqual = self.deepStrictEqual;
        self.notEqual = self.notStrictEqual;
        self.notDeepEqual = self.notDeepStrictEqual;
      }
      return self;
    }
    assert.Assert = Assert;

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
  // POSIX file-type bits of `st_mode`. Identical on every platform oam
  // targets, and synthesized to match on Windows (see stat_to_json).
  const S_IFMT = 0o170000;
  const S_IFIFO = 0o010000;
  const S_IFCHR = 0o020000;
  const S_IFDIR = 0o040000;
  const S_IFBLK = 0o060000;
  const S_IFREG = 0o100000;
  const S_IFLNK = 0o120000;
  const S_IFSOCK = 0o140000;

  // node's fs.Stats carries its predicates on the PROTOTYPE. That is what lets
  // two stats of the same file compare equal: as own properties they are
  // distinct function objects, so `assert.deepStrictEqual(statSync(f),
  // statSync(f))` failed on every pair, and inspect printed seven [Function]
  // members node does not show.
  class Stats {
    constructor(raw) {
      // node's own field order, so inspect output lines up with it.
      // Conditionally defined: if a platform could not supply one, it stays
      // ABSENT rather than becoming an own property holding undefined, which
      // deepStrictEqual and JSON.stringify would both treat as an answer.
      for (const key of ["dev", "mode", "nlink", "uid", "gid", "rdev", "blksize"]) {
        if (raw[key] !== undefined) this[key] = raw[key];
      }
      if (raw.ino !== undefined) this.ino = raw.ino;
      this.size = raw.size;
      if (raw.blocks !== undefined) this.blocks = raw.blocks;
      this.atimeMs = raw.atimeMs;
      this.mtimeMs = raw.mtimeMs;
      this.ctimeMs = raw.ctimeMs;
      this.birthtimeMs = raw.birthtimeMs;
    }
    // node exposes the Date views as lazy getters on the PROTOTYPE, not as own
    // properties. As own properties they showed up in Object.keys, JSON and
    // inspect where node shows none of them, and they cost four Date
    // allocations on every stat for callers that only read the *Ms numbers.
    // Cached in PRIVATE fields so the memo stays invisible to
    // getOwnPropertySymbols and cannot make two stats of one file unequal.
    #atime;
    #mtime;
    #ctime;
    #birthtime;
    get atime() { return (this.#atime ??= new Date(this.atimeMs)); }
    get mtime() { return (this.#mtime ??= new Date(this.mtimeMs)); }
    get ctime() { return (this.#ctime ??= new Date(this.ctimeMs)); }
    get birthtime() { return (this.#birthtime ??= new Date(this.birthtimeMs)); }
    // Derived from the S_IFMT bits of `mode`, as node does. While `mode` was
    // hardcoded to 0 these had to read an oam-only `kind` string, which could
    // only ever answer file/dir/symlink -- the other four were hardcoded false,
    // so a POSIX character device, block device, FIFO or socket all reported
    // themselves as none of those.
    _checkModeProperty(bits) { return (this.mode & S_IFMT) === bits; }
    isDirectory() { return this._checkModeProperty(S_IFDIR); }
    isFile() { return this._checkModeProperty(S_IFREG); }
    isBlockDevice() { return this._checkModeProperty(S_IFBLK); }
    isCharacterDevice() { return this._checkModeProperty(S_IFCHR); }
    isSymbolicLink() { return this._checkModeProperty(S_IFLNK); }
    isFIFO() { return this._checkModeProperty(S_IFIFO); }
    isSocket() { return this._checkModeProperty(S_IFSOCK); }
  }

  // Class members are non-enumerable; node builds Stats.prototype by
  // assignment, so ITS members are enumerable -- observable through `for...in`
  // over a stat object, which must list the same names in the same order.
  for (const name of [
    "atime", "mtime", "ctime", "birthtime",
    "_checkModeProperty", "isDirectory", "isFile", "isBlockDevice",
    "isCharacterDevice", "isSymbolicLink", "isFIFO", "isSocket",
  ]) {
    const descriptor = Object.getOwnPropertyDescriptor(Stats.prototype, name);
    descriptor.enumerable = true;
    Object.defineProperty(Stats.prototype, name, descriptor);
  }

  function wrapStat(raw) {
    return new Stats(raw);
  }

  // node's fs.StatFs. Seven own fields in node's order and NOTHING on the
  // prototype but the constructor, which is the whole observable surface --
  // node builds it the same way, so inspect/JSON/deepStrictEqual line up.
  // Deliberately NOT exported from node:fs: node keeps the class internal and
  // only ever hands back instances (verified against v22.22.2).
  class StatFs {
    constructor(raw, bigint) {
      // The native sends every field as a decimal STRING so bigint mode is
      // exact (see statfs_fields_json in oam_core). Number() reproduces
      // node's double for the default form; BigInt() the exact u64.
      const cast = bigint ? BigInt : Number;
      this.type = cast(raw.type);
      this.bsize = cast(raw.bsize);
      this.blocks = cast(raw.blocks);
      this.bfree = cast(raw.bfree);
      this.bavail = cast(raw.bavail);
      this.files = cast(raw.files);
      this.ffree = cast(raw.ffree);
    }
  }

  function wrapStatFs(raw, options) {
    return new StatFs(raw, readOptions(options).bigint === true);
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

  // node's fs.Dir. Was two ad-hoc object literals (one per opendir form),
  // which left `fs.Dir` unexported and gave each form only half the method
  // set -- an opendirSync() handle had no read()/close(), an opendir() handle
  // no readSync()/closeSync(), where node's single class carries all four.
  // Entries are snapshotted at open, as both literals already did.
  class Dir {
    #entries;
    #index = 0;
    #closed = false;
    constructor(path, entries) {
      this.path = path;
      this.#entries = entries;
    }
    #next() {
      // node tracks the handle's closed state and throws on ANY later
      // operation -- including a second close() -- rather than reporting
      // end-of-directory. Returning null here instead made a use-after-close
      // bug look like an empty directory.
      if (this.#closed) {
        throw makeNodeError("ERR_DIR_CLOSED", "Directory handle was closed");
      }
      if (this.#index >= this.#entries.length) return null;
      return makeDirent(this.path, this.#entries[this.#index++]);
    }
    readSync() { return this.#next(); }
    closeSync() {
      if (this.#closed) {
        throw makeNodeError("ERR_DIR_CLOSED", "Directory handle was closed");
      }
      this.#closed = true;
    }
    // Both forms accept node's optional callback as well as returning a
    // promise (node's Dir#read/#close are dual-shaped).
    //
    // How a CLOSED handle is reported differs between the two, and it differs
    // between read and close -- measured against v22.22.2, not assumed:
    //   read(cb)  -> throws SYNCHRONOUSLY   read()  -> rejects
    //   close(cb) -> calls back with err    close() -> rejects
    // node's #readImpl runs inline when a callback is supplied, so its guard
    // throws out of the call itself; close routes its guard through the
    // callback instead.
    read(callback) {
      if (typeof callback === "function") {
        // Deliberately UNGUARDED: a closed handle must throw out of this call.
        const value = this.#next();
        callback(null, value);
        return undefined;
      }
      try { return Promise.resolve(this.#next()); } catch (e) { return Promise.reject(e); }
    }
    close(callback) {
      try { this.closeSync(); } catch (e) {
        if (typeof callback === "function") { callback(e); return undefined; }
        return Promise.reject(e);
      }
      if (typeof callback === "function") { callback(null); return undefined; }
      return Promise.resolve();
    }
    // Iterating to the end CLOSES the handle, and so does abandoning the loop
    // early (`break` / `throw` runs the iterator's return()). That is node's
    // behavior -- `await dir.close()` after a for-await rejects there with
    // ERR_DIR_CLOSED -- and without it a loop leaves the handle open, so the
    // close() that node rejects would quietly succeed here.
    #finish() {
      if (!this.#closed) this.#closed = true;
      return { done: true, value: undefined };
    }
    [Symbol.asyncIterator]() {
      return {
        next: async () => {
          const value = this.#next();
          return value === null ? this.#finish() : { done: false, value };
        },
        return: async () => this.#finish(),
      };
    }
    [Symbol.iterator]() {
      return {
        next: () => {
          const value = this.#next();
          return value === null ? this.#finish() : { done: false, value };
        },
        return: () => this.#finish(),
      };
    }
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

  // -- glob support (Node v22 surface) --
  //
  // The runtime never reaches the filesystem directly for glob: it builds a
  // pattern-to-regex compiler, then walks readdir() results. Both pieces
  // live here so the three factories (fs.sync, fs/promises, callback) and
  // path.matchesGlob all share them.

  // Hard cap on deduped glob matches. Anything past this is almost always a
  // runaway pattern (`**/*` against a tree with millions of files inside
  // node_modules / target) that the caller did not intend. Surfacing it as
  // an error beats the alternative: the walker's `out.push()` loop has no
  // yield points, so a 100k+ match glob against a deep tree can grow the V8
  // heap past 1 GiB and trip "Ineffective mark-compacts near heap limit",
  // aborting the whole process (no graceful exit). The dedup pass below
  // also early-aborts once the unique count clears the cap, bounding the
  // worst-case heap cost to roughly:
  //   `out` array   ~ 24 MB / 1M entries (pointers)
  //   `emitted` Set ~ 80 MB / 1M entries (string hash + value)
  //   result strings ~ 300 MB / 1M entries @ ~300-byte avg paths
  //                          (a 5M-match glob aborts at the cap with ~400 MB
  //                           total rather than ~1.5 GB for the full walk)
  // 1M matches fits comfortably in the 4 GiB V8 heap default. A normal
  // `src/**/*.{ts,tsx}` against a real workspace lands in the low
  // thousands. Set `options.maxResults` to override per-call; passing
  // `Infinity` disables it.
  const DEFAULT_MAX_RESULTS = 1_000_000;

  // Pattern -> { regex, hasMagic }. Splits on `/`, normalizes path separators
  // internally. `*` is any-non-slash, `**` is any (incl. slash), `?` one-
  // non-slash, `[abc]` / `[!abc]` char classes, `{a,b}` brace expansion (one
  // level deep; backslash escapes `,` `}` `{`). `**` followed by a literal
  // segment in the same pattern (e.g. `a/**/b`) lets the `**` match zero or
  // more directories by absorbing the trailing `/` so the regex doesn't pin
  // a literal slash at that point.
  function globToRegex(pattern, opts) {
    var nocase = !!(opts && opts.nocase);
    var segs = pattern.split("/");
    var re = "^";
    var hasMagic = false;
    for (var si = 0; si < segs.length; si++) {
      var seg = segs[si];
      var segRe = "";
      var segMagic = false;
      for (var i = 0; i < seg.length; i++) {
        var c = seg[i];
        if (c === "*") {
          if (seg[i + 1] === "*") {
            segMagic = true;
            segRe += ".*";
            while (i + 1 < seg.length && seg[i + 1] === "*") i++;
            if (i + 1 < seg.length && seg[i + 1] === "/") i++;
          } else {
            segMagic = true;
            segRe += "[^/]*";
          }
        } else if (c === "?") {
          segMagic = true;
          segRe += "[^/]";
        } else if (c === "[") {
          segMagic = true;
          var cls = "";
          i++;
          if (i < seg.length && (seg[i] === "!" || seg[i] === "^")) { cls += "^"; i++; }
          if (i < seg.length && seg[i] === "]") { cls += "\\]"; i++; }
          while (i < seg.length && seg[i] !== "]") {
            if (seg[i] === "\\" && i + 1 < seg.length) { cls += seg[i] + seg[i + 1]; i += 2; continue; }
            cls += seg[i];
            i++;
          }
          segRe += "[" + cls + "]";
        } else if (c === "{") {
          segMagic = true;
          var depth = 1;
          var body = "";
          i++;
          while (i < seg.length && depth > 0) {
            if (seg[i] === "\\" && i + 1 < seg.length) { body += seg[i] + seg[i + 1]; i += 2; continue; }
            if (seg[i] === "{") depth++;
            else if (seg[i] === "}") { depth--; if (depth === 0) break; }
            body += seg[i];
            i++;
          }
          var alts = [];
          var cur = "";
          var d2 = 0;
          for (var j = 0; j < body.length; j++) {
            var bj = body[j];
            if (bj === "\\" && j + 1 < body.length) { cur += bj + body[j + 1]; j++; continue; }
            if (bj === "{") d2++;
            else if (bj === "}") d2--;
            if (bj === "," && d2 === 0) { alts.push(cur); cur = ""; }
            else cur += bj;
          }
          alts.push(cur);
          var inner = "(?:";
          for (var k = 0; k < alts.length; k++) {
            if (k > 0) inner += "|";
            inner += globToRegex(alts[k], opts).source.slice(1, -1);
          }
          segRe += inner + ")";
        } else if (c === "\\") {
          if (i + 1 < seg.length) { segRe += "\\" + seg[++i]; }
          else segRe += "\\\\";
        } else if (/[.+^$()|]/.test(c)) {
          segRe += "\\" + c;
        } else {
          segRe += c;
        }
      }
      hasMagic = hasMagic || segMagic;
      re += segRe;
      if (si < segs.length - 1) re += "/";
    }
    return { regex: new RegExp(re + "$", nocase ? "i" : ""), source: re + "$", hasMagic: hasMagic };
  }

  // -- traversal --
  //
  // `cwd` is the absolute directory the pattern is resolved against. `segs`
  // are the slash-split pattern; `pos` is the segment index we're at. Each
  // step reads the directory, picks entries that match `segs[pos]`, then
  // either descends (more segments to consume) or records a match (this is
  // the final segment).
  //
  // `**` is the twist: it can match zero or more directories. The walker
  // therefore treats `**` as "consume the segment AND try to skip it (try the
  // empty match first); if a directory entry fits, also descend through it."
  // The cycle check on `follow:true` records every ancestor inode we've
  // recursed into; revisiting one would loop forever, so we throw ELOOP.
  function globWalk(opts, natives, cwd, segs, pos, base, visited, out, sep, absolutePattern) {
    var seg = segs[pos];
    var entries;
    try {
      entries = natives.fsReaddirSync(base);
    } catch (e) {
      // Missing directories (ENOENT), non-directory paths (ENOTDIR), and any
      // other readdir failure are silently treated as zero matches -- matches
      // node v22 behavior, which never throws from a glob for any of these.
      // Callers can detect "no cwd" via the empty result.
      return;
    }
    var include = opts.include;
    var includeRe = null;
    if (typeof include === "string") {
      var inc = globToRegex(include, opts);
      includeRe = inc.regex;
    }
    var segCompiled = globToRegex(seg, opts);
    var segIsMagic = segCompiled.hasMagic;
    var isGlobStar = seg === "**";
    var isLast = pos === segs.length - 1;
    // `include` is a directory pre-filter that ONLY applies to non-`**`
    // intermediate segments. `**` is a permission to traverse anything, so
    // gating it on `include` would silently exclude the whole subtree.
    var includeApplies = includeRe && !isGlobStar && !isLast;
    for (var k = 0; k < entries.length; k++) {
      var entry = entries[k];
      var name = entry.name;
      // The leaf-name filter: for non-`**`, the entry's name must match the
      // segment pattern. For `**`, every name passes (the segment is just a
      // permission to traverse).
      if (!isGlobStar) {
        if (!matchSegment(seg, segCompiled, segIsMagic, name, opts)) continue;
      }
      if (includeApplies && !includeRe.test(name)) continue;
      var childPath = base + sep + name;
      // `**` is a wildcard for any number of path segments, INCLUDING zero.
      // Three actions:
      //  1. Skip `**` and try the rest of the pattern at the SAME depth
      //     (covers `a/**/b.js` matching `a/b.js` with ** absorbing zero dirs).
      //  2. When `**` is the FINAL pattern segment, record the entry.
      //  3. Descend with `**` still active -- the next pattern segment(s)
      //     will eventually match against some descendant.
      // The order matters: action 2 only fires for terminal `**`, otherwise
      // emitting the directory itself would shadow the per-segment match in
      // action 1 (a/ would show up in **/*.js, which isn't what callers want).
      if (isGlobStar) {
        if (isLast) {
          // node v22 emits directories from a terminal `**` regardless of
          // nodir:true (verified empirically against v22.22.2). The
          // non-globstar terminal branch honors nodir because literal pattern
          // segments are explicit user intent.
          var relGS = childPath.slice(cwd.length + 1);
          if (!(opts.exclude && matchExclude(opts.exclude, relGS))) {
            emitMatch(opts, natives, base, relGS, childPath, entry.kind, out, sep, absolutePattern);
          }
        } else {
          // Empty match: skip **, run the next pattern segment at this depth.
          globWalk(opts, natives, cwd, segs, pos + 1, base, visited, out, sep, absolutePattern);
        }
        if (entry.kind === "dir" || (opts.follow === true && entry.kind === "symlink")) {
          if (opts.follow === true && entry.kind === "symlink") {
            try { natives.fsReaddirSync(childPath); } catch (e) { continue; }
            if (visited.has(childPath)) {
              throw makeSystemError("ELOOP", "glob", childPath);
            }
            visited.add(childPath);
          }
          globWalk(opts, natives, cwd, segs, pos, childPath, visited, out, sep, absolutePattern);
          if (opts.follow === true && entry.kind === "symlink") visited.delete(childPath);
        }
      } else if (isLast) {
        if (opts.nodir === true && entry.kind === "dir") continue;
        var rel = childPath.slice(cwd.length + 1);
        if (opts.exclude && matchExclude(opts.exclude, rel)) continue;
        emitMatch(opts, natives, base, rel, childPath, entry.kind, out, sep, absolutePattern);
      } else if (entry.kind === "dir" || (opts.follow === true && entry.kind === "symlink")) {
        if (opts.follow === true && entry.kind === "symlink") {
          try { natives.fsReaddirSync(childPath); } catch (e) { continue; }
          if (visited.has(childPath)) {
            throw makeSystemError("ELOOP", "glob", childPath);
          }
          visited.add(childPath);
        }
        globWalk(opts, natives, cwd, segs, pos + 1, childPath, visited, out, sep, absolutePattern);
        if (opts.follow === true && entry.kind === "symlink") visited.delete(childPath);
      }
    }
  }

  function matchSegment(seg, compiled, isMagic, name, opts) {
    if (!isMagic) return seg === name;
    return compiled.regex.test(name);
  }

  // node accepts `function` or `string[]` for `exclude` (a bare string is
  // rejected with ERR_INVALID_ARG_TYPE in the validator above). The string[]
  // form treats each element as a glob matched against the relative path.
  function matchExclude(exclude, relPath) {
    if (typeof exclude === "function") return exclude(relPath);
    if (Array.isArray(exclude)) {
      for (var i = 0; i < exclude.length; i++) {
        if (globToRegex(exclude[i], {}).regex.test(relPath)) return true;
      }
    }
    return false;
  }

  function emitMatch(opts, natives, parentPath, rel, childPath, kind, out, sep, absolutePattern) {
    if (opts.withFileTypes) {
      // Dirent's `name` is the leaf, `parentPath` is the containing directory.
      // The walker threads `parentPath` (the readdir base) through, not the
      // glob root -- otherwise entries inside a/b/ would all claim the same
      // parent as the cwd.
      out.push(new Dirent(rel.split(sep).pop(), parentPath, kind));
      return;
    }
    // `absolute` is a no-op in node v22 unless the pattern itself is absolute:
    // a relative pattern always yields relative paths even with absolute:true,
    // an absolute pattern always yields absolute paths even without the flag.
    out.push(absolutePattern ? childPath : rel);
  }

  // Public entry used by the three factories. Validates args; everything
  // else (path normalization, traversal, option plumbing) is `globWalk`'s
  // job.
  function globSyncRaw(pattern, options, natives) {
    if (typeof pattern !== "string") {
      throw codes.ERR_INVALID_ARG_TYPE("pattern", "string", pattern);
    }
    var opts = options || {};
    if (opts.cwd !== undefined && typeof opts.cwd !== "string") {
      throw codes.ERR_INVALID_ARG_TYPE("options.cwd", "string", opts.cwd);
    }
    if (opts.exclude !== undefined && opts.exclude !== null) {
      var validExclude =
        typeof opts.exclude === "function" ||
        (Array.isArray(opts.exclude) && opts.exclude.every(function (e) { return typeof e === "string"; }));
      if (!validExclude) {
        throw codes.ERR_INVALID_ARG_TYPE("options.exclude", ["function", "string[]"], opts.exclude);
      }
    }
    // Resolve the result-count cap. `maxResults` is an oam extension (node's
    // fs.glob has no such option today); a non-finite or non-positive number
    // throws rather than silently falling back to the default, since a
    // caller passing `0` expects "zero matches" or "fail fast", not 1M
    // results they didn't ask for. `Infinity` disables the cap. The
    // rationale lives on `DEFAULT_MAX_RESULTS` above.
    var maxResults;
    if (typeof opts.maxResults === "number") {
      if (Number.isFinite(opts.maxResults)) {
        if (opts.maxResults > 0) {
          maxResults = Math.floor(opts.maxResults);
        } else {
          // Finite but non-positive: zero or negative. Reject loudly so the
          // caller doesn't quietly get 1M matches. Floats get floored first
          // so 1.5 -> 1 (valid) lands in the cap branch, and 0.5 -> 0 hits
          // here as "user asked for zero".
          if (Math.floor(opts.maxResults) > 0) {
            maxResults = Math.floor(opts.maxResults);
          } else {
            throw codes.ERR_OUT_OF_RANGE(
              "options.maxResults",
              "a positive integer or Infinity",
              opts.maxResults,
            );
          }
        }
      } else if (opts.maxResults === Infinity) {
        maxResults = Infinity;
      } else {
        // NaN / -Infinity. The latter is a finite-style request for "no cap"
        // -- honor it. NaN gets the same ERR_OUT_OF_RANGE treatment.
        if (opts.maxResults === -Infinity) {
          maxResults = Infinity;
        } else {
          throw codes.ERR_OUT_OF_RANGE(
            "options.maxResults",
            "a positive integer or Infinity",
            opts.maxResults,
          );
        }
      }
    } else if (opts.maxResults === undefined) {
      maxResults = DEFAULT_MAX_RESULTS;
    } else {
      throw codes.ERR_INVALID_ARG_TYPE(
        "options.maxResults",
        ["number", "undefined"],
        opts.maxResults,
      );
    }
    var normalized = pattern.replace(/\\/g, "/");
    if (normalized.startsWith("./")) normalized = normalized.slice(2);
    var segs = normalized.split("/");
    var cwd = opts.cwd ? String(opts.cwd) : ".";
    var isAbsolute = normalized.startsWith("/") || /^[A-Za-z]:\//.test(normalized);
    // For absolute patterns, the walker should start at the directory portion
    // of the path (everything except the last segment). The pattern's
    // trailing component is then matched against the listing in that dir.
    // Relative patterns start at cwd.
    var root, startSeg;
    if (isAbsolute) {
      // pattern[0] is "/" (POSIX) or the drive letter (Windows). Walk starts
      // at the directory containing the final segment. The segments were
      // normalized to '/', so rejoin with the platform's native separator --
      // the original pattern uses '\\' on Windows and node emits '\\' in the
      // result.
      startSeg = segs.length - 1;
      root = segs.slice(0, startSeg).join(pattern.indexOf("\\") !== -1 ? "\\" : "/");
    } else {
      startSeg = 0;
      root = cwd;
    }
    // Path separator for joining the root with descendant names during
    // traversal. For relative cwd, derive from the cwd itself (Windows cwd
    // uses '\\', POSIX uses '/'). For absolute patterns the root above
    // already carries the platform separator.
    var sep = root.indexOf("\\") !== -1 ? "\\" : "/";
    var out = [];
    var seen = new Set();
    // `**` matches the cwd itself (`.` on a relative cwd) in addition to
    // everything below it -- this is the documented node behavior and the
    // shape most tooling (lint, prettier ignore globs) relies on. The `.`
    // is emitted in string form even with nodir:true; for withFileTypes the
    // walker handles the cwd-self via the normal descent path (the cwd's
    // own children get emitted, not a synthetic "." Dirent).
    if (segs.length === 1 && segs[0] === "**" && !opts.withFileTypes) {
      out.push(isAbsolute ? root : ".");
    }
    globWalk(opts, natives, root, segs, startSeg, root, seen, out, sep, isAbsolute);
    out.sort();
    // The walker explores each match multiple times when `**` is involved
    // (zero-segment match + every-depth descent). Real Node deduplicates via
    // a realpath cache; a path-set is enough for our purposes and avoids
    // the extra stat.
    var deduped = [];
    var emitted = new Set();
    var overCap = false;
    for (var i = 0; i < out.length; i++) {
      var item = out[i];
      var key;
      if (opts.withFileTypes) {
        key = item.name + "|" + item.parentPath + "|" + item._kind;
      } else if (isAbsolute) {
        key = item;
      } else {
        key = item;
      }
      if (emitted.has(key)) continue;
      emitted.add(key);
      deduped.push(item);
      // Early-abort during dedup once the unique count clears the cap. The
      // post-loop check below is still required for the accurate count in
      // the error message, but stopping here means we don't keep allocating
      // `emitted` Set entries past `maxResults` -- the OOM path is bounded
      // to roughly `maxResults * (1 + dedup_multiplier)` entries rather than
      // the full match count. `out` itself is freed with the call frame.
      if (!overCap && maxResults !== Infinity && deduped.length > maxResults) {
        overCap = true;
      }
      if (overCap) break;
    }
    // Cap check: the dedup loop above breaks early once `deduped.length`
    // exceeds `maxResults`, so by the time we reach this line `deduped` is
    // the minimum unique count (the true count is at least this large). If
    // the early-abort didn't fire the dedup finished in full and the count
    // is exact. Either way the error message tells the caller how many
    // unique matches were found before the cap tripped, which is the number
    // they care about when deciding whether to widen the cap or narrow the
    // pattern. Uses `codes.ERR_OUT_OF_RANGE` so conformance harnesses see
    // the standard `.code` field.
    if (overCap || deduped.length > maxResults) {
      throw codes.ERR_OUT_OF_RANGE(
        "results",
        "<= " + maxResults + " (set options.maxResults higher or narrow the pattern)",
        deduped.length,
      );
    }
    return deduped;
  }

  // node v22's fs.promises.glob returns an AsyncIterable<string|Dirent>, not a
  // Promise<string[]>. Build the iterable from the materialized array so
  // Array.fromAsync works identically on both runtimes. (Native async
  // streaming would be a wider change -- the walker is sync and the dedup
  // pass needs the full set anyway.)
  function globAsyncIterable(items) {
    return {
      [Symbol.asyncIterator]() {
        var i = 0;
        return {
          next() {
            if (i < items.length) return Promise.resolve({ value: items[i++], done: false });
            return Promise.resolve({ value: undefined, done: true });
          },
        };
      },
    };
  }

  // path.matchesGlob shares the regex compiler but treats its input as a
  // single-name match (no slash splitting). path/posix and path/win32 both
  // forward to it (the underlying match is separator-agnostic on this side;
  // the path module picks the separator when splitting).
  function pathMatchesGlob(p, pattern) {
    if (typeof p !== "string") throw codes.ERR_INVALID_ARG_TYPE("path", "string", p);
    if (typeof pattern !== "string") throw codes.ERR_INVALID_ARG_TYPE("pattern", "string", pattern);
    var compiled = globToRegex(pattern, {});
    if (!compiled.hasMagic) return p === pattern;
    return compiled.regex.test(p);
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
    const buf = BufferCtor.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
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

  // node's internal time coercion, shared by the fs and fs/promises factories
  // (fs re-exports it as `_toUnixTimestamp`, which wrappers in the wild call).
  // Yields SECONDS, as node's does; the natives take milliseconds, so every
  // caller multiplies. Defined out here rather than duplicated because the two
  // factories cannot see each other's locals and `fs/promises` is BUILT FIRST
  // (the fs factory opens by calling registry.get("fs/promises")), so reaching
  // across at construction time is not an option.
  function toUnixSeconds(time, name) {
    if (typeof time === "string" && Number(time) === Number(time)) return Number(time);
    if (typeof time === "number" && Number.isFinite(time)) {
      // A negative time means "now" in node, not a pre-epoch date.
      return time < 0 ? Date.now() / 1000 : time;
    }
    if (time instanceof Date) return time.getTime() / 1000;
    // node's wording verbatim, odd article and all ("or an Time in seconds"),
    // because assert.throws({ message }) compares it exactly.
    throw nodeTypeError(
      `The "${name ?? "time"}" argument must be an instance of Date or an Time in seconds. ` +
        `Received ${describeArg(time)}`,
    );
  }
  const toUnixMs = (time, name) => toUnixSeconds(time, name) * 1000;

  // A read/write POSITION argument, normalised for the natives: a non-negative
  // number is a pread/pwrite, anything else (null, undefined, a negative) means
  // "from the current cursor". Shared by the fs and fs/promises factories so
  // the two cannot drift -- FileHandle.read/write silently DROPPED their
  // position for as long as the natives had nowhere to put it.
  const fsPositionArg = (p) => (typeof p === "number" && p >= 0 ? p : null);

  // `Object.keys(err)` order, for the ONE fd call where node's differs.
  //
  // Every fd error node raises enumerates errno, code, syscall -- except
  // fs.writeSync, which builds its error in JS from a libuv ctx object
  // (errno and syscall copied off the ctx, code assigned last) instead of
  // throwing from C++. That lands as errno, syscall, code. fs.write and
  // fs.writevSync both use the common order, so this is a property of that one
  // call site, not of the `write` syscall, and it cannot live in the native
  // (all three share it). Own-key order is observable to anything that
  // snapshots or diffs an error.
  function ctxOrderError(e) {
    if (!e || e.code === undefined || e.syscall === undefined) return e;
    const out = new Error(e.message);
    out.errno = e.errno;
    out.syscall = e.syscall;
    out.code = e.code;
    if (e.path !== undefined) out.path = e.path;
    try { out.stack = e.stack; } catch { /* frozen stack: keep our own */ }
    return out;
  }

  // node's ERR_OUT_OF_RANGE guard on a read into a caller-supplied buffer.
  // Without it an over-long `length` reaches the native, which then allocates
  // it -- `fs.read(fd, Buffer.alloc(4), 0, 1e9)` is a gigabyte on our side and
  // a synchronous throw on node's.
  function validateReadLength(buffer, offset, length) {
    if (!buffer || typeof length !== "number") return;
    const room = buffer.byteLength - (offset || 0);
    if (length > room) {
      throw codes.ERR_OUT_OF_RANGE("length", "<= " + room, length);
    }
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
      statfs: async (path, options) => wrapStatFs(await natives.fsStatfs(String(path)), options),
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
        // depend on the throw); kind-check first. The probe is an internal
        // detail -- node reports `rmdir` as the failing syscall, so relabel
        // rather than leaking `lstat` (same as the sync twin).
        let raw;
        try {
          raw = await natives.fsStat(String(path), true);
        } catch (e) {
          if (e && e.syscall === "lstat") e.syscall = "rmdir";
          throw e;
        }
        if (raw.kind !== "dir") {
          // As in the sync twin: node's full system-error shape.
          throw makeSystemError(isWin ? "ENOENT" : "ENOTDIR", "rmdir", path);
        }
        await natives.fsRm(String(path), false, false);
      },
      unlink: (path) => natives.fsUnlink(String(path)),
      rename: (from, to) => natives.fsRename(String(from), String(to)),
      copyFile: (from, to) => natives.fsCopyFile(String(from), String(to)),
      // node v22's fs.promises.glob returns an AsyncIterable, not a Promise.
      // Wrap the materialized array so Array.fromAsync() works on both sides.
      glob: (pattern, options) => globAsyncIterable(globSyncRaw(pattern, options, natives)),
      // callback fs.glob needs a Promise-returning function (callbackify1
      // calls .then on the result). Hide this from users -- the public
      // promises API is the AsyncIterable form above.
      _globAsPromise: (pattern, options) => Promise.resolve().then(() => globSyncRaw(pattern, options, natives)),
      access: (path, mode) => natives.fsAccess(String(path), mode ?? 0),
      realpath: (path) => natives.fsRealpath(String(path)),
      mkdtemp: (prefix) => natives.fsMkdtemp(String(prefix)),
      symlink: (target, path) => natives.fsSymlink(String(target), String(path)),
      readlink: (path) => natives.fsReadlink(String(path)),
      link: (existing, newPath) => natives.fsLink(String(existing), String(newPath)),
      chmod: (path, mode) => natives.fsChmod(String(path), mode),
      truncate: (path, len) => natives.fsTruncate(String(path), len ?? 0),
      chown: (path, uid, gid) => natives.fsChown(String(path), uid, gid),
      lchown: (path, uid, gid) => natives.fsLchown(String(path), uid, gid),
      utimes: (path, atime, mtime) =>
        natives.fsUtimes(String(path), toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime")),
      lutimes: (path, atime, mtime) =>
        natives.fsLutimes(String(path), toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime")),
      // lchmod diverges between the two modules, which is easy to get wrong.
      // In `node:fs` the name is bound to UNDEFINED off macOS. Here in
      // `fs/promises` it is ALWAYS a function, and off macOS it REJECTS.
      // Measured on win32 node v22.22.2: `typeof fsp.lchmod === "function"`,
      // and calling it rejects with a plain Error carrying only a `code` own
      // property -- name "Error", not a subclass.
      lchmod: (path, mode) => {
        if (natives.platform === "darwin") return natives.fsLchmod(String(path), mode);
        const err = new Error("The lchmod() method is not implemented");
        err.code = "ERR_METHOD_NOT_IMPLEMENTED";
        return Promise.reject(err);
      },
      opendir: async function (path) {
        var dirPath = String(path);
        return new Dir(dirPath, await natives.fsReaddir(dirPath));
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
          // `position` is a pwrite/pread offset: it writes/reads THERE and
          // leaves the cursor alone. Both used to accept the argument and throw
          // it away, because the natives had no position parameter to pass it
          // to -- fh.read(buf, 0, 3, 10) returned the bytes at the cursor.
          write: async function (buffer, offset, length, position) {
            if (typeof buffer === "string") buffer = globalThis.Buffer.from(buffer);
            var slice = (offset != null || length != null) ? buffer.subarray(offset || 0, length != null ? (offset || 0) + length : undefined) : buffer;
            await natives.fsWriteChunk(h, slice, fsPositionArg(position));
            return { bytesWritten: slice.length, buffer: buffer };
          },
          read: async function (buffer, offset, length, position) {
            validateReadLength(buffer, offset, length);
            var want = length != null ? length : (buffer ? buffer.byteLength - (offset || 0) : 65536);
            var chunk = await natives.fsReadChunk(h, want, fsPositionArg(position));
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

    // Active-request tracking for process._getActiveRequests() /
    // process.getActiveResourcesInfo(). Node's callback-form fs ops each hold a
    // live FSReqCallback from the call until the callback is dispatched;
    // test-process-getactiverequests fires 12 fs.open()s and asserts the count
    // SYNCHRONOUSLY, so the token has to be added on the call, not on settle.
    // Only this CALLBACK layer is instrumented -- the promise forms it
    // delegates to are node's separately-tagged FSReqPromise, and counting
    // both would report 24 where the tests expect 12.
    const fsReqStart = () => {
      const token = { type: "FSReqCallback" };
      registry._activeRequests.add(token);
      return token;
    };
    const fsReqEnd = (token) => registry._activeRequests.delete(token);

    // Callback forms delegate to the promise forms (Node-style (err, value)).
    function callbackify1(promiseFn) {
      return (...args) => {
        const cb = args.pop();
        if (typeof cb !== "function") {
          throw new TypeError("Callback must be a function");
        }
        const token = fsReqStart();
        // Several promise forms are plain (non-async) arrows, so argument
        // validation throws SYNCHRONOUSLY -- the token must drop before the
        // throw escapes or it is stranded in the Set forever (the Set is
        // strong and this is the only other removal path).
        let p;
        try {
          p = promiseFn(...args);
        } catch (e) {
          fsReqEnd(token);
          throw e;
        }
        p.then(
          (value) => { fsReqEnd(token); queueMicrotask(() => cb(null, value)); },
          (err) => { fsReqEnd(token); queueMicrotask(() => cb(err)); },
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

    // filename -> [{ listener, stop }] for every live watchFile poller, so
    // unwatchFile can find and stop them. Without this registry watchFile
    // handed back a watcher and there was no way to cancel it by path, which
    // is the only way node's unwatchFile addresses it.
    const watchFilePollers = new Map();

    // `match` decides which entries go: a function matches by listener
    // identity, undefined means "every poller on this path" (node's
    // unwatchFile with no listener). Callers that own ONE entry pass a
    // predicate instead, so they cannot take their siblings down with them.
    function stopWatchEntries(filePath, match) {
      const entries = watchFilePollers.get(filePath);
      if (!entries) return;
      const remaining = entries.filter((entry) => {
        if (!match(entry)) return true;
        entry.stop();
        return false;
      });
      if (remaining.length) watchFilePollers.set(filePath, remaining);
      else watchFilePollers.delete(filePath);
    }

    function fsUnwatchFile(filename, listener) {
      stopWatchEntries(
        String(filename),
        // No listener means "stop watching this path entirely" (node).
        listener === undefined ? () => true : (entry) => entry.listener === listener,
      );
    }

    function fsWatchFile(filename, options, listener) {
      if (typeof options === "function") {
        listener = options;
        options = {};
      }
      // node REQUIRES the listener and rejects anything else; accepting a
      // missing one handed back a poller that could never report a change.
      if (typeof listener !== "function") {
        throw nodeTypeError(
          `The "listener" argument must be of type function. Received ${describeArg(listener)}`,
        );
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
      var stop = function () { clearInterval(poll); };
      var self = { listener: listener, stop: stop };
      var entries = watchFilePollers.get(filePath);
      if (entries) entries.push(self);
      else watchFilePollers.set(filePath, [self]);
      return {
        // Drop THIS entry only, matched by identity. Routing through
        // fsUnwatchFile(path, listener) stopped every sibling poller whenever
        // the listener was not unique to this watcher.
        close: function () { stopWatchEntries(filePath, function (entry) { return entry === self; }); },
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
      const { Readable, Writable, finished } = registry.get("stream");
      class ReadStream extends Readable {
        // Node v22 ReadStream.prototype.close (lib/internal/fs/streams.js --
        // outside the vendored internal/streams tree, so supplied here).
        close(cb) {
          if (typeof cb === "function") finished(this, cb);
          this.destroy();
        }
        constructor(path, options) {
          const opts = readOptions(options);
          const highWaterMark = opts.highWaterMark ?? 65536;
          const endByte = typeof opts.end === "number" ? opts.end : Infinity;
          const startByte = typeof opts.start === "number" ? opts.start : 0;
          const maxBytes = endByte === Infinity ? Infinity : endByte - startByte + 1;
          let handle = null;
          let totalRead = 0;
          super({
            // Node's fs.ReadStream flow: EOF -> push(null) -> 'end' ->
            // autoDestroy -> destroy() -> _destroy -> 'close'. The stream
            // machine owns 'close' (single emission); the read path only
            // closes the fd early so EOF never holds it open.
            // autoClose:false (Node option) = keep the stream alive past
            // EOF/error, mapped straight onto the machine's autoDestroy.
            autoDestroy: opts.autoClose !== false,
            highWaterMark,
            encoding: opts.encoding ?? null,
            // Eager open (Node's _construct phase): a bad path surfaces as
            // 'error' + 'close' BEFORE any read -- finished()/pipeline() on
            // an unopenable stream reject with the ENOENT instead of
            // hanging on a lazily-failing first read.
            construct(callback) {
              Promise.resolve(natives.fsOpen(String(path), "r")).then(
                (r) => {
                  handle = r.handle;
                  this.emit("open", handle);
                  this.emit("ready");
                  callback();
                },
                (e) => callback(e),
              );
            },
            async read(size) {
              try {
                if (handle === null) {
                  // construct() always ran first (vendored Readable contract),
                  // so a null handle means the EOF path already closed it:
                  // never re-open a finished file.
                  return;
                }
                const remaining = maxBytes - totalRead;
                if (remaining <= 0) {
                  await Promise.resolve(natives.fsClose(handle)).catch(() => {});
                  handle = null;
                  this.push(null);
                  return;
                }
                const want = Math.min(size || highWaterMark, remaining);
                const chunk = await natives.fsReadChunk(handle, want);
                if (chunk === undefined) {
                  await Promise.resolve(natives.fsClose(handle)).catch(() => {});
                  handle = null;
                  this.push(null);
                } else {
                  const buf = globalThis.Buffer.from(chunk.buffer, chunk.byteOffset, chunk.length);
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
        // Node v22 WriteStream.prototype.close === end-then-wait-for-close.
        close(cb) {
          if (typeof cb === "function") {
            if (this.closed) process.nextTick(cb);
            else this.on("close", cb);
          }
          this.end();
        }
        constructor(path, options) {
          const opts = readOptions(options);
          const flags = opts.flags === "a" ? "a" : "w";
          let handle = null;
          let totalWritten = 0;
          super({
            // Node's fs.WriteStream flow: end() -> final() -> 'finish' ->
            // autoDestroy -> destroy() -> 'close'. The machine owns 'close'.
            autoDestroy: opts.autoClose !== false,
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
      // node re-exports the four access-mode constants at the TOP level of
      // node:fs as well as under fs.constants, and code destructures them
      // from the module (`const { R_OK } = require("fs")`) -- as named ESM
      // imports those were link-time failures.
      F_OK: 0, X_OK: 1, W_OK: 2, R_OK: 4,
      Dir,

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
      statfsSync: (path, options) => wrapStatFs(natives.fsStatfsSync(String(path)), options),
      readdirSync: (path, options) => {
        const { withFileTypes } = readOptions(options);
        return wrapDirents(
          String(path),
          natives.fsReaddirSync(String(path)),
          withFileTypes === true,
        );
      },
      globSync: (pattern, options) => globSyncRaw(pattern, options, natives),
      mkdirSync: (path, options) => {
        natives.fsMkdirSync(String(path), readOptions(options).recursive === true);
      },
      rmSync: (path, options = {}) => {
        natives.fsRmSync(String(path), options.recursive === true, options.force === true);
      },
      rmdirSync: (path) => {
        // The kind probe is an implementation detail: node reports `rmdir` as
        // the failing syscall, so an ENOENT from this internal lstat must not
        // surface as `syscall: "lstat"`.
        let raw;
        try {
          raw = natives.fsStatSync(String(path), true);
        } catch (e) {
          if (e && e.syscall === "lstat") e.syscall = "rmdir";
          throw e;
        }
        if (raw.kind !== "dir") {
          // Full system-error shape, not just a code: node sets syscall/path/
          // errno here as it does on any other rmdir failure.
          throw makeSystemError(
            natives.platform === "win32" ? "ENOENT" : "ENOTDIR",
            "rmdir",
            path,
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
        const len = length ?? (buffer ? buffer.length - (offset ?? 0) : 0);
        validateReadLength(buffer, offset ?? 0, len);
        return natives.fsReadSync(fd, buffer, offset ?? 0, len, position ?? null);
      },
      writeSync: (fd, data, offsetOrPosition, length, position) => {
        // Buffer form: (fd, buffer, offset, length, position).
        // String form: (fd, string, position, encoding).
        let buf, pos;
        if (typeof data === "string") {
          const enc = typeof length === "string" ? length : "utf8";
          buf = globalThis.Buffer.from(data, enc);
          pos = typeof offsetOrPosition === "number" ? offsetOrPosition : null;
        } else {
          const offset = typeof offsetOrPosition === "number" ? offsetOrPosition : 0;
          const len = typeof length === "number" ? length : (data.length - offset);
          buf = (offset !== 0 || len !== data.length) ? data.subarray(offset, offset + len) : data;
          pos = typeof position === "number" ? position : null;
        }
        // fd 1/2 (stdout/stderr) have no native fd-table entry -- route them to
        // the process stdout/stderr sinks so fs.writeSync(1|2, ...) matches Node
        // instead of throwing EBADF (pino/sonic-boom sync mode writes here).
        if (fd === 1) { natives.stdoutWrite(buf); return buf.length; }
        if (fd === 2) { natives.stderrWrite(buf); return buf.length; }
        // ctxOrderError only here: fs.write and fs.writevSync route through the
        // same native but keep node's common errno/code/syscall order.
        try {
          return natives.fsWriteSync(fd, buf, pos);
        } catch (e) {
          throw ctxOrderError(e);
        }
      },
      opendirSync: function (path) {
        var dirPath = String(path);
        return new Dir(dirPath, natives.fsReaddirSync(dirPath));
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
      statfs: callbackify1(promises.statfs),
      readdir: callbackify1(promises.readdir),
      glob: callbackify1(promises._globAsPromise),
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
      // native open handle (a number) IS the integer fd. fsReadChunk/
      // fsWriteChunk take a position, so the positional (pread/pwrite) forms
      // are honoured here as well as in the sync family.
      open: function (path, flags, mode, cb) {
        if (typeof flags === "function") { cb = flags; flags = "r"; }
        else if (typeof mode === "function") { cb = mode; }
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        var flagStr = typeof flags === "number" ? numericOpenFlags(flags) : (flags || "r");
        var token = fsReqStart();
        // String(path) can throw (a poisoned toString) -- drop the token
        // before the throw escapes, or it is stranded forever.
        var p;
        try {
          p = Promise.resolve(natives.fsOpen(String(path), String(flagStr)));
        } catch (e) {
          fsReqEnd(token);
          throw e;
        }
        p.then(
          function (info) { fsReqEnd(token); queueMicrotask(function () { cb(null, info.handle); }); },
          function (err) { fsReqEnd(token); queueMicrotask(function () { cb(err); }); },
        );
      },
      close: function (fd, cb) {
        var err = null;
        try { natives.fsClose(fd); } catch (e) { err = e; }
        if (typeof cb === "function") queueMicrotask(function () { cb(err); });
      },
      // Async fd-based write. Node overloads:
      //   fs.write(fd, buffer[, offset[, length[, position]]], cb)
      //   fs.write(fd, buffer, options, cb)
      //   fs.write(fd, string[, position[, encoding]], cb)
      // fd 1/2 route to the stdout/stderr sinks (pino/sonic-boom's default
      // async destination writes here); other fds use the sync native op
      // dispatched on a microtask to preserve the async callback contract.
      write: function (fd, data) {
        var rest = Array.prototype.slice.call(arguments, 2);
        var cb = rest.length ? rest[rest.length - 1] : undefined;
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        var mid = rest.slice(0, rest.length - 1);
        var buf, pos;
        try {
          if (typeof data === "string") {
            pos = typeof mid[0] === "number" ? mid[0] : null;
            var enc = typeof mid[1] === "string" ? mid[1] : (typeof mid[0] === "string" ? mid[0] : "utf8");
            buf = globalThis.Buffer.from(data, enc);
          } else {
            var offset = 0, length, position = null;
            if (mid[0] !== null && typeof mid[0] === "object" && !ArrayBuffer.isView(mid[0])) {
              offset = mid[0].offset ?? 0;
              length = mid[0].length ?? (data.length - offset);
              position = typeof mid[0].position === "number" ? mid[0].position : null;
            } else {
              offset = typeof mid[0] === "number" ? mid[0] : 0;
              length = typeof mid[1] === "number" ? mid[1] : (data.length - offset);
              position = typeof mid[2] === "number" ? mid[2] : null;
            }
            buf = (offset !== 0 || length !== data.length) ? data.subarray(offset, offset + length) : data;
            pos = position;
          }
        } catch (e) {
          queueMicrotask(function () { cb(e); });
          return;
        }
        var n;
        try {
          if (fd === 1) { natives.stdoutWrite(buf); n = buf.length; }
          else if (fd === 2) { natives.stderrWrite(buf); n = buf.length; }
          else { n = natives.fsWriteSync(fd, buf, typeof pos === "number" ? pos : null); }
        } catch (e) {
          queueMicrotask(function () { cb(e); });
          return;
        }
        queueMicrotask(function () { cb(null, n, data); });
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
          // o.position was the one field this form never read, so the options
          // overload kept reading from the cursor after the positional overload
          // below was fixed. readSync's object form has always honoured it.
          position = o.position ?? null;
        }
        if (typeof offset === "function") { cb = offset; offset = 0; length = buffer ? buffer.length : 16384; }
        if (typeof length === "function") { cb = length; length = buffer ? buffer.length - (offset || 0) : 16384; }
        if (typeof position === "function") { cb = position; }
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        var want = length != null ? length : (buffer ? buffer.length - (offset || 0) : 16384);
        // Bounded by the destination, exactly as node bounds it -- and thrown
        // SYNCHRONOUSLY even from this callback form, which is what node does.
        validateReadLength(buffer, offset, want);
        // The position was parsed above and then DROPPED -- fsReadChunk had no
        // position parameter, so `fs.read(fd, buf, 0, 3, 10, cb)` read from the
        // cursor and handed back the wrong bytes with no error. The native now
        // takes one; null still means "from the cursor".
        var readPos = fsPositionArg(position);
        Promise.resolve(natives.fsReadChunk(fd, want, readPos)).then(
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
      unwatchFile: fsUnwatchFile,
      // fd-based stat, callback form. fstatSync already existed; only the
      // async spelling was missing, and it is the one promisify() reaches for.
      fstat: function (fd, options, cb) {
        if (typeof options === "function") { cb = options; options = undefined; }
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        var token = fsReqStart();
        // The ASYNC native, not fsFstatSync: reading metadata inline and then
        // deferring only the callback still blocked the loop for the whole
        // stat, which is the one thing the callback form exists to avoid.
        var p;
        try {
          p = natives.fsFstat(fd);
        } catch (e) {
          // A bad fd throws out of the op before a promise exists; node
          // reports it through the callback, never synchronously.
          fsReqEnd(token);
          queueMicrotask(function () { cb(e); });
          return;
        }
        p.then(
          function (raw) { fsReqEnd(token); queueMicrotask(function () { cb(null, wrapStat(raw)); }); },
          function (err) { fsReqEnd(token); queueMicrotask(function () { cb(err); }); },
        );
      },
      // node's internal time coercion, exported (unprefixed by convention but
      // public enough that utimes wrappers in the wild call it).
      // Shared with the fs/promises factory rather than defined twice --
      // see toUnixSeconds above.
      _toUnixTimestamp: toUnixSeconds,
    };
    // ---- fd-based ops: fsync / fdatasync / ftruncate / fchmod / fchown /
    // futimes, callback and sync forms.
    //
    // Assigned after the literal so they can reuse fs._toUnixTimestamp rather
    // than duplicating node's coercion. These are module-level exports in node
    // (unlike the FileHandle methods of the same name in fs/promises), and a
    // builtin's ESM named exports are its module object's own enumerable keys,
    // so a missing one is a LINK-time SyntaxError for anyone importing it --
    // which is how a single unused name takes down a whole bundled CLI.
    //
    // Deliberately NOT routed through callbackify1: that helper re-throws a
    // synchronous error, and these natives throw EBADF synchronously out of the
    // op. Node reports a bad descriptor through the callback, never at the call
    // site, so the throw has to be caught and re-delivered -- the same shape
    // `fstat` uses above.
    const voidCallbackOp = (run) =>
      function (...args) {
        const cb = args.pop();
        if (typeof cb !== "function") throw new TypeError("Callback must be a function");
        const token = fsReqStart();
        let p;
        try {
          p = run(...args);
        } catch (e) {
          fsReqEnd(token);
          queueMicrotask(() => cb(e));
          return;
        }
        p.then(
          () => { fsReqEnd(token); queueMicrotask(() => cb(null)); },
          (err) => { fsReqEnd(token); queueMicrotask(() => cb(err)); },
        );
      };


    fs.fsync = voidCallbackOp((fd) => natives.fsFsync(fd));
    fs.fdatasync = voidCallbackOp((fd) => natives.fsFdatasync(fd));
    // node permits `ftruncate(fd, cb)` with the length omitted, which lands
    // here as undefined once the callback is popped.
    fs.ftruncate = voidCallbackOp((fd, len) => natives.fsFtruncate(fd, len ?? 0));
    fs.fchmod = voidCallbackOp((fd, mode) => natives.fsFchmod(fd, mode));
    fs.fchown = voidCallbackOp((fd, uid, gid) => natives.fsFchown(fd, uid, gid));
    fs.futimes = voidCallbackOp((fd, atime, mtime) =>
      natives.fsFutimes(fd, toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime")),
    );

    fs.fsyncSync = (fd) => { natives.fsFsyncSync(fd); };
    fs.fdatasyncSync = (fd) => { natives.fsFdatasyncSync(fd); };
    fs.ftruncateSync = (fd, len) => { natives.fsFtruncateSync(fd, len ?? 0); };
    fs.fchmodSync = (fd, mode) => { natives.fsFchmodSync(fd, mode); };
    fs.fchownSync = (fd, uid, gid) => { natives.fsFchownSync(fd, uid, gid); };
    fs.futimesSync = (fd, atime, mtime) => {
      natives.fsFutimesSync(fd, toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime"));
    };

    // ---- path-based ownership / time: chown, lchown, utimes, lutimes, lchmod.
    //
    // The `l` forms act on a symlink itself rather than its target, which is
    // the only reason they exist.
    fs.chown = voidCallbackOp((p, uid, gid) => natives.fsChown(String(p), uid, gid));
    fs.lchown = voidCallbackOp((p, uid, gid) => natives.fsLchown(String(p), uid, gid));
    fs.utimes = voidCallbackOp((p, atime, mtime) =>
      natives.fsUtimes(String(p), toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime")),
    );
    fs.lutimes = voidCallbackOp((p, atime, mtime) =>
      natives.fsLutimes(String(p), toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime")),
    );

    fs.chownSync = (p, uid, gid) => { natives.fsChownSync(String(p), uid, gid); };
    fs.lchownSync = (p, uid, gid) => { natives.fsLchownSync(String(p), uid, gid); };
    fs.utimesSync = (p, atime, mtime) => {
      natives.fsUtimesSync(String(p), toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime"));
    };
    fs.lutimesSync = (p, atime, mtime) => {
      natives.fsLutimesSync(String(p), toUnixMs(atime, "atime"), toUnixMs(mtime, "mtime"));
    };

    // lchmod is macOS-only. node gates its own on O_SYMLINK -- which the BSD
    // family has and Linux does not -- and OFF macOS it publishes the NAME with
    // the value left undefined. Measured on win32 node v22.22.2: `lchmod` is an
    // own enumerable key, `typeof fs.lchmod === "undefined"`, and
    // `import { lchmod } from "node:fs"` links fine and binds undefined.
    //
    // So the correct shape off macOS is a present key holding undefined -- NOT
    // a stub that throws. Assigning undefined creates the own enumerable key,
    // which is exactly what the ESM named export is derived from, so the import
    // links; and calling it gives "fs.lchmod is not a function", byte-for-byte
    // what node gives. A throwing stub would link the same and then diverge on
    // the message.
    if (process.platform === "darwin") {
      fs.lchmod = voidCallbackOp((p, mode) => natives.fsLchmod(String(p), mode));
      fs.lchmodSync = (p, mode) => { natives.fsLchmodSync(String(p), mode); };
    } else {
      fs.lchmod = undefined;
      fs.lchmodSync = undefined;
    }

    // ---- vectored IO: readv / writev, callback and sync forms.
    //
    // Built ON TOP of the read/write paths rather than as new natives: writev
    // concatenates and issues one write, readv reads once and scatters. For a
    // regular file that is indistinguishable from a real iovec call, and it
    // inherits the positional (pread/pwrite) semantics those paths already have
    // instead of growing a second copy of them.
    //
    // node exports these on node:fs ONLY. fs/promises has no top-level
    // readv/writev -- there they are FileHandle methods.
    //
    // Behaviours measured against node v22.22.2, several of which are not what
    // you would guess:
    //   - the sync forms return a plain NUMBER, not {bytesRead, buffers}. Only
    //     the FileHandle methods return objects.
    //   - the callback is (err, bytes, buffers) and `buffers` is the SAME ARRAY
    //     IDENTITY that went in, not a copy.
    //   - an EMPTY ARRAY is asymmetric: writev returns 0, readv throws EINVAL
    //     -- and the EINVAL is raised BEFORE the fd is looked at, so it wins
    //     even on a closed descriptor.
    //   - an array of only ZERO-LENGTH views is NOT the same case: readv
    //     returns 0 rather than throwing. The rule keys on the array being
    //     empty, not on the byte total.
    //   - a partial final buffer keeps the rest of its bytes UNTOUCHED.
    const asViewArray = (buffers) => {
      // Index loop, NOT Array.prototype.every: every() skips holes, so a sparse
      // array would slip through where node throws.
      let ok = Array.isArray(buffers);
      if (ok) {
        for (let i = 0; i < buffers.length; i++) {
          if (!ArrayBuffer.isView(buffers[i])) { ok = false; break; }
        }
      }
      if (!ok) {
        throw nodeTypeError(
          `The "buffers" argument must be an ArrayBufferView[]. Received ${describeArg(buffers)}`,
        );
      }
      let total = 0;
      for (let i = 0; i < buffers.length; i++) total += buffers[i].byteLength;
      return total;
    };

    // node's EINVAL for readv with an empty list. Own props in node's order.
    const einvalRead = () => {
      const err = new Error("EINVAL: invalid argument, read");
      err.errno = natives.platform === "win32" ? -4071 : -22;
      err.code = "EINVAL";
      err.syscall = "read";
      return err;
    };

    const flattenViews = (buffers, total) => {
      const combined = globalThis.Buffer.allocUnsafe(total);
      let off = 0;
      for (let i = 0; i < buffers.length; i++) {
        const v = buffers[i];
        combined.set(new Uint8Array(v.buffer, v.byteOffset, v.byteLength), off);
        off += v.byteLength;
      }
      return combined;
    };

    // Scatter `n` bytes of `src` across the views, filling each in turn. A view
    // that only partially fills keeps its remaining bytes as they were, because
    // TypedArray.set writes exactly as many bytes as the source holds.
    const scatterViews = (buffers, src, n) => {
      let off = 0;
      for (let i = 0; i < buffers.length && off < n; i++) {
        const v = buffers[i];
        const take = Math.min(v.byteLength, n - off);
        if (take > 0) {
          new Uint8Array(v.buffer, v.byteOffset, v.byteLength).set(src.subarray(off, off + take));
        }
        off += take;
      }
    };

    // ONLY an empty array skips the descriptor. An array of zero-length views
    // is a different case: it still issues the syscall, so it still reports
    // EBADF on a closed or wrong-mode fd. Returning 0 early for both conflated
    // "no iovecs" with "no bytes" and made readvSync(closedFd, [Buffer.alloc(0)])
    // succeed where node throws.
    const emptyList = (buffers) => buffers.length === 0;

    fs.writevSync = (fd, buffers, position) => {
      const total = asViewArray(buffers);
      // node returns 0 without touching the descriptor -- measured: writev([])
      // on a CLOSED fd does not throw.
      if (emptyList(buffers)) return 0;
      return natives.fsWriteSync(fd, flattenViews(buffers, total), fsPositionArg(position));
    };

    fs.readvSync = (fd, buffers, position) => {
      const total = asViewArray(buffers);
      // Before the fd check, deliberately: node raises this even for a closed
      // descriptor.
      if (emptyList(buffers)) throw einvalRead();
      const tmp = globalThis.Buffer.allocUnsafe(total);
      const n = natives.fsReadSync(fd, tmp, 0, total, fsPositionArg(position));
      scatterViews(buffers, tmp, n);
      return n;
    };

    const vectoredCallback = (cb) => {
      if (typeof cb !== "function") {
        throw nodeTypeError(
          `The "cb" argument must be of type function. Received ${describeArg(cb)}`,
        );
      }
      return cb;
    };

    // The two validation failures land DIFFERENTLY, which is easy to get
    // backwards: node's validateBufferArray runs at the call site and THROWS
    // synchronously (node:fs:758), while the empty-list EINVAL is delivered to
    // the callback. Routing both through the callback meant a try/catch around
    // fs.readv silently stopped firing.
    fs.writev = function (fd, buffers, position, cb) {
      if (typeof position === "function") { cb = position; position = null; }
      cb = vectoredCallback(cb);
      const total = asViewArray(buffers);
      if (emptyList(buffers)) { queueMicrotask(() => cb(null, 0, buffers)); return; }
      // Reuses fs.write, so the position handling lives in exactly one place.
      fs.write(fd, flattenViews(buffers, total), 0, total, fsPositionArg(position), (err, written) =>
        err ? cb(err, 0, buffers) : cb(null, written, buffers),
      );
    };

    fs.readv = function (fd, buffers, position, cb) {
      if (typeof position === "function") { cb = position; position = null; }
      cb = vectoredCallback(cb);
      const total = asViewArray(buffers);
      if (emptyList(buffers)) {
        // This one IS deferred, and beats the fd: node reports EINVAL through
        // the callback even for a closed descriptor.
        queueMicrotask(() => cb(einvalRead(), 0, buffers));
        return;
      }
      const tmp = globalThis.Buffer.allocUnsafe(total);
      fs.read(fd, tmp, 0, total, fsPositionArg(position), (err, n) => {
        if (err) { cb(err, 0, buffers); return; }
        scatterViews(buffers, tmp, n);
        // The SAME array instance goes back, which callers compare by identity.
        cb(null, n, buffers);
      });
    };

    // ---- openAsBlob
    //
    // Shapes measured against node v22.22.2 rather than inferred:
    //   - `options.type` is coerced with `|| ''` BEFORE the string check, so
    //     {type: 0 | false | null | NaN | undefined} all yield "" and only a
    //     truthy non-string (e.g. 123) throws ERR_INVALID_ARG_TYPE. node's own
    //     source is `const type = options.type || ''; validateString(...)`.
    //   - an unreadable path does NOT surface the fs error. node throws a
    //     TypeError with code ERR_INVALID_ARG_VALUE, message "Unable to open
    //     file as blob", and `code` as its ONLY own property -- no errno, no
    //     syscall, no path.
    fs.openAsBlob = async (p, options) => {
      const type = (options && options.type) || "";
      if (typeof type !== "string") {
        throw nodeTypeError(
          `The "options.type" argument must be of type string. Received ${describeArg(type)}`,
        );
      }
      let bytes;
      try {
        bytes = await natives.fsReadFile(String(p));
      } catch {
        // Deliberately swallowing the underlying error: node reports none of
        // it, and leaking ENOENT here would be a divergence, not a courtesy.
        const err = new TypeError("Unable to open file as blob");
        err.code = "ERR_INVALID_ARG_VALUE";
        throw err;
      }
      const blob = new Blob([bytes], { type });
      // node returns an instance of an INTERNAL TransferableBlob subclass and
      // plants an own `constructor` back-pointing at Blob so the subclass does
      // not leak through `b.constructor.name`. Measured descriptor:
      // {value: Blob, writable: true, enumerable: true, configurable: true} --
      // enumerable, so `Object.keys(blob)` is ["constructor"], which a
      // structural comparison would otherwise see as a difference. A plain
      // assignment produces exactly that descriptor.
      blob.constructor = Blob;
      // Marks it un-structuredClone-able, which node's file-backed blob is.
      // A registered symbol so bootstrap.js's cloner can see it across scopes,
      // and a symbol key so it stays out of Object.keys.
      blob[Symbol.for("oam.blob.fileBacked")] = true;
      return blob;
    };

    fs.realpathSync.native = fs.realpathSync;
    fs.Dirent = Dirent;
    // The real class, so `stat instanceof fs.Stats` holds -- it was a bare
    // placeholder no stat object was ever an instance of.
    fs.Stats = Stats;
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
    // One-shot handoff: the module defines itself on globalThis so it can be a
    // separate script (and so carry its own origin), and is cleared right away
    // rather than left as an oam-only global on the runtime context.
    const perThread = globalThis.__oamPerThread(natives, codes, applyNodeErrorShape);
    delete globalThis.__oamPerThread;
    const EventEmitter = registry.get("events");
    // Streams are consumed LAZILY (stdio getters below), never at factory
    // time: this factory runs inside installRuntimeGlobals BEFORE
    // globalThis.process is assigned, and the vendored stream port's module
    // bodies read process.platform at load time (state.js HWM default) --
    // requiring the port here would see process === undefined and crash the
    // runtime at startup. Node lazy-initializes stdio the same way.
    const { Buffer } = registry.get("buffer");

    // ---- POSIX credential helpers (see the identity block far below) ----
    // Node validates every id argument before it reaches the syscall, and
    // the conformance suite asserts the exact message text, so these build
    // Node's shapes rather than approximating them.
    const credentialKindWord = (kind) => (kind === "uid" ? "User" : "Group");
    /// TYPE validation only -- Node checks the shape of EVERY argument before
    /// resolving ANY of them, so `initgroups('nonexistent', undefined)`
    /// reports the bad extraGroup rather than failing the name lookup first.
    const validateCredentialType = (value, argName) => {
      if (typeof value === "number") {
        // Node rejects non-integers and anything outside uid_t range here;
        // -0 and 2**32-1 are explicitly exercised and must not crash.
        if (!Number.isInteger(value) || value < 0 || value > 4294967295) {
          throw new codes.ERR_OUT_OF_RANGE(
            argName,
            ">= 0 && < 4294967296",
            value,
          );
        }
        return;
      }
      if (typeof value !== "string") {
        throw new codes.ERR_INVALID_ARG_TYPE(argName, ["number", "string"], value);
      }
    };
    /// number | string -> numeric id. ERR_UNKNOWN_CREDENTIAL for a name with
    /// no passwd/group entry.
    const resolveCredential = (value, argName, kind) => {
      validateCredentialType(value, argName);
      if (typeof value === "number") return value;
      const id = natives.posixLookupId(kind === "uid" ? 0 : 1, value);
      if (id === null || id === undefined) {
        throw makeNodeError(
          "ERR_UNKNOWN_CREDENTIAL",
          `${credentialKindWord(kind)} identifier does not exist: ${value}`,
        );
      }
      return id;
    };
    /// An errno name from a credential syscall -> a Node-shaped Error.
    const credentialSyscallError = (errnoName, syscall) => {
      const err = makeNodeError(errnoName, `${errnoName}, ${syscall}`);
      err.errno = errnoName;
      err.syscall = syscall;
      return err;
    };
    const posixSetId = (which, id, kind) => {
      const resolved = resolveCredential(id, "id", kind);
      const errnoName = natives.posixSetId(which, resolved);
      if (errnoName) {
        throw credentialSyscallError(
          errnoName,
          ["setuid", "setgid", "seteuid", "setegid"][which],
        );
      }
    };

    // process.nextTick: a real FIFO tick queue (streams-port slice 0), now
    // HOST-DRAINED (docs/design/nexttick-engine.md): the engine loops
    // drain -> microtask checkpoint at every tick point, Node's
    // processTicksAndRejections shape. The drain runs the queue to
    // exhaustion, including entries appended mid-drain. Each callback is
    // bound to its scheduling ALS frame individually (captured as DATA at
    // schedule time and restored by the drain) -- binding only the drain
    // would leak the first scheduler's frame into every other callback in
    // the batch.
    let tickQueue = [];
    let tickIndex = 0;
    // Canonical uncaught dispatcher -- Node's onGlobalUncaughtException
    // ladder with the origin second argument: monitor (always, informational),
    // then the capture callback (REPLACES emission, never fatal), then
    // 'uncaughtException' listeners. Returns true when something consumed the
    // error (the run survives), false when it is fatal. Shared by the tick
    // drain, the queueMicrotask wrapper, and the ENGINE's uncaught paths
    // (via the __oamDispatchUncaught global).
    const dispatchUncaught = (err, origin) => {
      origin = origin || "uncaughtException";
      // A throw that already escaped a handler INSIDE this ladder (marked
      // below) must not re-enter it: Node treats a throwing
      // uncaughtException/monitor handler as immediately fatal, exit 7 --
      // never re-emitting the monitor or re-invoking listeners.
      if (
        err !== null && (typeof err === "object" || typeof err === "function") &&
        err.__oamHandlerThrow
      ) {
        process.__oamFatalCode = 7;
        return false;
      }
      // Node's TriggerUncaughtException refuses to run when
      // process._fatalException was monkeypatched to a non-function:
      // exit code 6 (kInvalidFatalExceptionMonkeyPatching). oam's ladder
      // replaces _fatalException's BODY, but the patch check is API surface
      // (a patched FUNCTION is not invoked -- accepted divergence, no
      // corpus coverage).
      if (typeof process._fatalException !== "function") {
        process.__oamFatalCode = 6;
        return false;
      }
      try {
        // Monitor first, ALWAYS -- including for domain-handled errors
        // (probe-verified: node emits uncaughtExceptionMonitor before the
        // domain machinery runs).
        process.emit("uncaughtExceptionMonitor", err, origin);
        // Active domain next. _errorHandler (a) returns true when a domain
        // 'error' listener consumed it, (b) rethrows THE SAME err when the
        // domain has no listener (fall through to the ladder, with the
        // process-level domain clear node performs there), (c) lets a
        // throwing 'error' LISTENER's own exception propagate -- e2 !== err,
        // escalate: node dies exit 7 on a throwing domain handler.
        const dom = process.domain;
        if (dom && typeof dom._errorHandler === "function") {
          try {
            dom._errorHandler(err);
            // Domain consumed it: node's domainUncaughtExceptionClear runs
            // on this process-level path (later callbacks see null).
            if (typeof registry._domainUncaughtClear === "function") {
              registry._domainUncaughtClear();
            }
            return true;
          } catch (e2) {
            if (e2 !== err) throw e2; // domain 'error' listener threw: fatal
            if (typeof registry._domainUncaughtClear === "function") {
              registry._domainUncaughtClear();
            }
            // fall through to the ladder with the original error, untagged
          }
        }
        // With the domain module in use, node's uncaught path always runs
        // domainUncaughtExceptionClear before the listeners: they observe
        // process.domain === null (not the unwound undefined) even when the
        // throwing domain was already exited by sync containment.
        if (typeof registry._domainUncaughtClear === "function") {
          registry._domainUncaughtClear();
        }
        if (process._uncaughtCaptureCb) {
          process._uncaughtCaptureCb(err);
          return true;
        }
        if (process.listenerCount("uncaughtException") > 0) {
          process.emit("uncaughtException", err, origin);
          return true;
        }
        return false;
      } catch (e2) {
        // A handler inside the ladder threw. Mark the escaping error so a
        // ledger re-dispatch recognizes it as handler-throw (fatal 7)
        // instead of re-running the ladder. Primitive throws can't carry
        // the mark -- they take the legacy re-dispatch path.
        if (e2 !== null && (typeof e2 === "object" || typeof e2 === "function")) {
          try {
            Object.defineProperty(e2, "__oamHandlerThrow", {
              value: true, writable: false, configurable: false, enumerable: false,
            });
          } catch { /* frozen -- best effort */ }
        }
        throw e2;
      }
    };
    Object.defineProperty(globalThis, "__oamDispatchUncaught", {
      value: dispatchUncaught,
      writable: false,
      configurable: false,
      enumerable: false,
    });

    // Named to match the frame Node renders in every tick-drain stack:
    // "at process.processTicksAndRejections". Installed as a real process
    // method below and invoked through it, so V8 renders the receiver too.
    const drainTickQueue = function processTicksAndRejections() {
      // The finally is load-bearing: EVERY exit -- normal completion, the
      // fatal no-listener rethrow, or a throw from a user's uncaughtException/
      // monitor handler itself -- must reset the queue state so a later
      // host drain starts from a consistent queue while the process keeps
      // running.
      try {
        while (tickIndex < tickQueue.length) {
          const entry = tickQueue[tickIndex];
          // Null the consumed slot so the closure + args are collectible
          // mid-batch, and compact periodically so array length tracks the
          // PENDING entries: a self-perpetuating tick chain must run at O(1)
          // memory like Node's FixedQueue, not retain the batch's history.
          tickQueue[tickIndex] = undefined;
          tickIndex += 1;
          if (tickIndex >= 1024) {
            tickQueue = tickQueue.slice(tickIndex);
            tickIndex = 0;
          }
          try {
            // Domain wrap, Node shape: enter before, exit ONLY on success --
            // a throw unwinds with the domain entered so dispatchUncaught
            // (in the catch below) routes it to the domain's error handler.
            const tickDom = entry[2];
            if (tickDom) tickDom.enter();
            // Local, not entry[0](...): calling a function off an array makes
            // V8 name the callee's frame "Array.<anonymous>".
            const tickFn = entry[0];
            // Only pay the continuation get/set when a frame was actually
            // captured -- the overwhelmingly common case is no ALS in play,
            // and this is the hottest loop in the runtime.
            const frame = entry[3];
            if (frame === undefined) {
              tickFn(...entry[1]);
            } else {
              const prevFrame = registry._setContinuationFrame(frame);
              try {
                tickFn(...entry[1]);
              } finally {
                registry._setContinuationFrame(prevFrame);
              }
            }
            if (tickDom) tickDom.exit();
          } catch (e) {
            // Node's onGlobalUncaughtException order: monitor first; then the
            // capture callback, which REPLACES 'uncaughtException' emission
            // and is never fatal; then listeners (handled -> this same drain
            // keeps going). Emitting inline here (rather than rethrowing to
            // the engine ledger, which only delivers after the whole
            // microtask checkpoint) is what keeps the handler AHEAD of later
            // ticks. No consumer at all = fatal: rethrow, and Node-faithfully
            // drop the remaining ticks (via the finally) rather than racing
            // their side effects against process death. A throw from the
            // handler itself also escapes here -- fatal in Node; the finally
            // keeps nextTick functional if the engine survives the run.
            if (dispatchUncaught(e, "uncaughtException")) continue;
            throw e;
          }
        }
      } finally {
        tickQueue = [];
        tickIndex = 0;
      }
    };
    // Host-driven tick points (docs/design/nexttick-engine.md): the engine
    // calls __oamDrainTicks BEFORE each microtask checkpoint and loops
    // drain -> checkpoint while __oamHasTicks reports work, reproducing
    // Node's processTicksAndRejections. Nothing schedules the drain as a
    // microtask anymore -- ticks run ahead of already-queued promise jobs,
    // and ticks scheduled FROM promise jobs run after full microtask
    // exhaustion, both Node-identical.
    // Non-enumerable: Node's test/common leak check walks enumerable
    // globals, and these are host plumbing, not API surface. Non-writable +
    // non-configurable: a global-scrubbing test harness deleting them would
    // otherwise strand queued ticks (the host also guards: no drain fn =
    // stop looping, never spin).
    // V8 renders a frame from the function's own name, and the host invokes
    // this through a global rather than off `process`, so name it what Node
    // renders: "at process.processTicksAndRejections". It is not added to
    // `process` -- Node does not expose it there either.
    Object.defineProperty(drainTickQueue, "name", {
      value: "process.processTicksAndRejections",
      configurable: true,
    });
    Object.defineProperty(globalThis, "__oamDrainTicks", {
      value: drainTickQueue,
      writable: false,
      configurable: false,
      enumerable: false,
    });
    Object.defineProperty(globalThis, "__oamHasTicks", {
      value: () => tickIndex < tickQueue.length,
      writable: false,
      configurable: false,
      enumerable: false,
    });
    // Node's process is an instance of a `process`-named EventEmitter subclass
    // (not a bare EventEmitter) -- gives it its own prototype layer, makes
    // process.constructor.name === "process" (so a failed `delete process.x`
    // says "#<process>"), and satisfies test-process-prototype's chain checks.
    const process = new (class process extends EventEmitter {})();

    // NODE_REDIRECT_WARNINGS destination, resolved lazily on the first warning
    // and cached: undefined = not looked up yet, null = unset/empty (write to
    // stderr), string = append warnings to that path. See emitWarning.
    let redirectWarningsPath;

    // Lazy env: natives.env() crosses the FFI boundary and copies every
    // environment variable into a JS object (50-200 vars on a typical dev
    // machine). For programs that never touch process.env, this is pure
    // waste. A Proxy defers the copy until first property access.
    let envCache = null;
    const ensureEnv = () => (envCache ??= natives.env());
    // process.env is a string-coercing view: assigning a non-string coerces
    // via String() (NOT delete -- `process.env.X = undefined` reads back as
    // the literal string 'undefined'); symbol keys are rejected the way Node
    // does (get -> undefined, set -> TypeError, `in` -> false, delete ->
    // true); defineProperty only accepts a configurable+writable+enumerable
    // data descriptor.
    // Windows environment variables are CASE-INSENSITIVE (probe-verified:
    // node reads process.env.CASETEST after setting process.env.CaseTest).
    // The stored key keeps the ORIGINAL casing -- only lookups fold -- so
    // Object.keys() still reports the name as it was written.
    const envCaseFold = natives.platform === "win32";
    const envResolveKey = (store, prop) => {
      if (!envCaseFold || prop in store) return prop;
      const wanted = String(prop).toLowerCase();
      for (const k of Object.keys(store)) {
        if (k.toLowerCase() === wanted) return k;
      }
      return prop;
    };
    const env = new Proxy(Object.create(null), {
      get(_, prop) {
        if (typeof prop === "symbol") return undefined;
        const store = ensureEnv();
        return store[envResolveKey(store, prop)];
      },
      set(_, prop, value) {
        if (typeof prop === "symbol") {
          throw new TypeError("Cannot convert a Symbol value to a string");
        }
        if (typeof value === "symbol") {
          throw new TypeError("Cannot convert a Symbol value to a string");
        }
        // An empty name is not a valid environment variable: node accepts the
        // assignment expression but stores nothing (probe: reads back
        // undefined). Silently ignore rather than throw.
        if (prop === "") return true;
        // DEP0104, under --pending-deprecation only: anything other than a
        // string/number/boolean is String()-coerced on the way in, and the
        // coercion is rarely what the caller meant (an object becomes
        // "[object Object]"). Node warns rather than change the behavior.
        if (
          globalThis.__oamPendingDeprecation &&
          typeof value !== "string" &&
          typeof value !== "number" &&
          typeof value !== "boolean"
        ) {
          process.emitWarning(
            "Assigning any value other than a string, number, or boolean to a " +
              "process.env property is deprecated. Please make sure to convert the " +
              "value to a string before setting process.env with it.",
            "DeprecationWarning",
            "DEP0104",
          );
        }
        const store = ensureEnv();
        const key = envResolveKey(store, prop);
        store[key] = String(value);
        // TZ is not just a string: Node re-reads the zone on assignment so
        // subsequent Dates render in it. Without this the variable changed
        // and every Date kept the zone the process started in.
        if (key === "TZ") natives.setTimeZone(String(value));
        return true;
      },
      has(_, prop) {
        if (typeof prop === "symbol") return false;
        const store = ensureEnv();
        // `in` on process.env reports only real variables (node: an
        // inherited name like 'hasOwnProperty' is NOT in process.env).
        return Object.prototype.hasOwnProperty.call(store, envResolveKey(store, prop));
      },
      deleteProperty(_, prop) {
        if (typeof prop === "symbol") return true;
        const store = ensureEnv();
        const key = envResolveKey(store, prop);
        delete store[key];
        // Deleting TZ returns to the host zone -- same refresh as setting it.
        if (key === "TZ") natives.setTimeZone(null);
        return true;
      },
      defineProperty(_, prop, desc) {
        // Node only accepts a data descriptor that is explicitly
        // configurable + writable + enumerable; an accessor descriptor, or
        // any of the three flags absent/false, is rejected.
        if ("get" in desc || "set" in desc) {
          throw applyNodeErrorShape(
            new TypeError(
              "'process.env' does not accept an accessor(getter/setter) descriptor",
            ),
            "ERR_INVALID_OBJECT_DEFINE_PROPERTY",
          );
        }
        if (
          desc.configurable !== true ||
          desc.writable !== true ||
          desc.enumerable !== true
        ) {
          throw applyNodeErrorShape(
            new TypeError(
              "'process.env' only accepts a configurable, writable," +
                " and enumerable data descriptor",
            ),
            "ERR_INVALID_OBJECT_DEFINE_PROPERTY",
          );
        }
        if (typeof prop === "symbol") {
          throw new TypeError("Cannot convert a Symbol value to a string");
        }
        ensureEnv()[prop] = String(desc.value);
        return true;
      },
      ownKeys() { return Reflect.ownKeys(ensureEnv()); },
      getOwnPropertyDescriptor(_, prop) {
        const obj = ensureEnv();
        if (typeof prop === "symbol") return undefined;
        // OWN properties only: `prop in obj` walks the prototype chain, so an
        // inherited name reported as own -- node asserts
        // Object.hasOwn(process.env, 'hasOwnProperty') === false while
        // process.env.hasOwnProperty still resolves via the prototype.
        const key = envResolveKey(obj, prop);
        if (!Object.prototype.hasOwnProperty.call(obj, key)) return undefined;
        return { value: obj[key], writable: true, enumerable: true, configurable: true };
      },
    });
    const stdoutIsTTY = natives.isTTY(1);
    const stderrIsTTY = natives.isTTY(2);
    const stdinIsTTY = natives.isTTY(0);

    // --- TTY surface (tty.WriteStream / tty.ReadStream) -----------------------
    // columns/rows are LIVE getters over the native ttyGetWinSize op
    // (GetConsoleScreenBufferInfo on Windows, TIOCGWINSZ ioctl on Unix) so they
    // always reflect the real terminal. 'resize' fires via a lazy, UNREF'd poll
    // that runs only while a 'resize' listener is attached -- mirrors Node
    // starting/stopping its SIGWINCH watch on add/removeListener('resize'). The
    // interval is unref'd so it never keeps the event loop alive (an unref'd
    // interval still fires while the loop is otherwise alive but does not block
    // exit -- both verified against node).
    function decorateTtyWriteStream(stream, fd, isTTY) {
      stream.fd = fd;
      if (!isTTY) return stream; // non-TTY: plain Writable, no isTTY/columns (node parity)
      stream.isTTY = true;
      stream.hasColors = () => true;
      const readSize = () => natives.ttyGetWinSize(fd);
      Object.defineProperties(stream, {
        columns: { configurable: true, enumerable: true, get() { const s = readSize(); return s ? s[0] : undefined; } },
        rows: { configurable: true, enumerable: true, get() { const s = readSize(); return s ? s[1] : undefined; } },
      });
      stream.getWindowSize = () => { const s = readSize(); return s ? [s[0], s[1]] : [80, 24]; };
      let poll = null, lastCols = -1, lastRows = -1;
      const startPoll = () => {
        if (poll) return;
        const s = readSize();
        if (s) { lastCols = s[0]; lastRows = s[1]; }
        poll = setInterval(() => {
          const cur = readSize();
          if (!cur) return;
          if (cur[0] !== lastCols || cur[1] !== lastRows) {
            lastCols = cur[0]; lastRows = cur[1];
            stream.emit("resize");
          }
        }, 250);
        if (poll && typeof poll.unref === "function") poll.unref();
      };
      const stopPoll = () => {
        if (poll && stream.listenerCount("resize") === 0) { clearInterval(poll); poll = null; }
      };
      stream.on("newListener", (ev) => { if (ev === "resize") startPoll(); });
      stream.on("removeListener", (ev) => { if (ev === "resize") stopPoll(); });
      return stream;
    }

    function decorateTtyReadStream(stream, fd, isTTY) {
      stream.fd = fd;
      if (!isTTY) return stream; // non-TTY stdin: no isTTY/setRawMode (node parity)
      stream.isTTY = true;
      stream.isRaw = false;
      let exitHooked = false;
      stream.setRawMode = function setRawMode(mode) {
        const enable = !!mode;
        // Native flips the console mode / termios. In raw mode line-buffering,
        // echo and PROCESSED_INPUT/ISIG are OFF, so each keypress arrives as a
        // stdin 'data' byte and Ctrl-C is delivered as 0x03 (no SIGINT) -- the
        // signals-in item's console-ctrl handler naturally won't fire.
        const okRaw = natives.ttySetRawMode(fd, enable);
        if (okRaw) stream.isRaw = enable;
        // Restore cooked mode on graceful exit so the shell isn't left raw
        // (Node restores internally). Hard kills can't be covered.
        if (enable && !exitHooked) {
          exitHooked = true;
          process.on("exit", () => { if (stream.isRaw) natives.ttySetRawMode(fd, false); });
        }
        return stream;
      };
      return stream;
    }

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
      // The RESOLVED binary path, not argv[0]: invoked through a symlink,
      // Node reports the symlink's target here and keeps the invoked name
      // in process.argv0. Falling back to argv[0] keeps embedders that
      // never set the native value working.
      get: () => natives.execPath || argv()[0],
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

    // 'beforeExit': emitted by the ENGINE at natural event-loop drain (main
    // entry paths only -- not REPL/test-runner/worker pumps) with the
    // prospective exit code. Never after explicit process.exit() (the
    // _exiting guard). The engine re-enters the loop when the handler
    // scheduled new ref'd work, re-emitting on each subsequent drain --
    // Node's EmitBeforeExit shape. Locked-down global like __oamDrainTicks.
    Object.defineProperty(globalThis, "__oamEmitBeforeExit", {
      value: function emitBeforeExit() {
        if (process._exiting) return false;
        const code = typeof process.exitCode === "number" ? process.exitCode : 0;
        process.emit("beforeExit", code);
        return true;
      },
      writable: false,
      configurable: false,
      enumerable: false,
    });

    // Inbound OS signals. process.on('SIGTERM'/'SIGINT'/...) must fire the
    // listener on delivery and suppress the default terminate while a listener
    // is attached (Node semantics; the k8s graceful-shutdown case). We arm the
    // native watcher lazily on the FIRST listener and disarm on the LAST, via
    // newListener/removeListener -- composing with events rather than
    // overriding process.on (the worker branch installs its own on() override).
    // newListener fires BEFORE the add (count 0 => first); removeListener fires
    // AFTER the removal (count 0 => last). Windows accepts SIGTERM in the set
    // but the OS never delivers it -- matches Node (listener allowed, no-op);
    // only SIGINT/SIGBREAK/SIGHUP actually fire there.
    const SIGNAL_NAMES = new Set(
      natives.platform === "win32"
        ? ["SIGINT", "SIGBREAK", "SIGHUP", "SIGTERM"]
        : ["SIGINT", "SIGTERM", "SIGHUP", "SIGUSR1", "SIGUSR2", "SIGWINCH", "SIGBREAK", "SIGQUIT", "SIGCONT", "SIGTSTP"],
    );
    process.on("newListener", (type) => {
      if (
        SIGNAL_NAMES.has(type) &&
        typeof natives.startSignal === "function" &&
        process.listenerCount(type) === 0
      ) {
        natives.startSignal(type);
      }
    });
    process.on("removeListener", (type) => {
      if (
        SIGNAL_NAMES.has(type) &&
        typeof natives.stopSignal === "function" &&
        process.listenerCount(type) === 0
      ) {
        natives.stopSignal(type);
      }
    });

    // Node's internal/process/finalization, ported. Pure JS there too: it
    // needs only WeakRef, FinalizationRegistry and the exit/beforeExit
    // events. The WeakRef deref is what makes `register` NOT resurrect a
    // collected object -- the callback is skipped, not called with a
    // dangling handle.
    function createFinalization() {
      let registry = null;
      const refs = { exit: [], beforeExit: [] };
      const listeners = { exit: null, beforeExit: null };

      function callRefsToFree(event) {
        const pending = refs[event];
        refs[event] = [];
        for (const ref of pending) {
          const obj = ref.deref();
          if (obj !== undefined) ref.fn(obj, event);
        }
      }

      function detachIfEmpty(event) {
        if (refs[event].length === 0 && listeners[event]) {
          process.removeListener(event, listeners[event]);
          listeners[event] = null;
        }
      }

      // FinalizationRegistry callback: the object is gone, so drop our
      // WeakRef too rather than keeping a dead entry until exit.
      function clear(ref) {
        for (const event of ["exit", "beforeExit"]) {
          const i = refs[event].indexOf(ref);
          if (i !== -1) {
            refs[event].splice(i, 1);
            detachIfEmpty(event);
          }
        }
      }

      function _register(event, obj, fn) {
        if (obj === null || (typeof obj !== "object" && typeof obj !== "function")) {
          throw new codes.ERR_INVALID_ARG_TYPE("obj", "object", obj);
        }
        if (typeof fn !== "function") {
          throw new codes.ERR_INVALID_ARG_TYPE("fn", "function", fn);
        }
        if (!listeners[event]) {
          listeners[event] = () => callRefsToFree(event);
          process.on(event, listeners[event]);
        }
        const ref = new WeakRef(obj);
        ref.fn = fn;
        refs[event].push(ref);
        if (!registry) registry = new FinalizationRegistry(clear);
        registry.register(obj, ref);
      }

      return {
        register(obj, fn) {
          _register("exit", obj, fn);
        },
        registerBeforeExit(obj, fn) {
          _register("beforeExit", obj, fn);
        },
        unregister(obj) {
          if (!registry) return;
          registry.unregister(obj);
          for (const event of ["exit", "beforeExit"]) {
            refs[event] = refs[event].filter((ref) => {
              const held = ref.deref();
              return held !== undefined && held !== obj;
            });
            detachIfEmpty(event);
          }
        },
      };
    }

    Object.assign(process, {
      finalization: createFinalization(),
      // Node's process.dlopen. oam cannot load a native addon through this
      // entry point, but the ERROR SHAPE is observable and was wrong: a
      // missing process.dlopen threw a bare TypeError where Node throws a
      // plain Error with code ERR_DLOPEN_FAILED. The filename is
      // CONCATENATED into the message, never used as a format string --
      // that is what test-process-dlopen-error-message-crash guards.
      dlopen() {
        if (arguments.length < 2) {
          const err = new TypeError("process.dlopen needs at least 2 arguments");
          err.code = "ERR_MISSING_ARGS";
          throw err;
        }
        const filename = String(arguments[1]);
        const exists = natives.fsExistsSync ? natives.fsExistsSync(filename) : false;
        const err = new Error(
          exists
            ? "Module did not self-register: '" + filename + "'."
            : natives.platform === "win32"
              ? "The specified module could not be found.\r\n" + filename
              : filename + ": cannot open shared object file: No such file or directory\n" + filename,
        );
        err.code = "ERR_DLOPEN_FAILED";
        throw err;
      },
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
        // Real shipped implementations under their REAL names, versions
        // read at build time by oam_engine/build.rs -- crate versions
        // from Cargo.lock, icu/unicode from the pinned v8 crate's own
        // version headers -- so they can never drift from what is
        // linked. Node's other dependency keys (uv, openssl, llhttp,
        // ares, napi, modules, cldr, tz, ...) stay ABSENT on purpose:
        // packages branch on those claims -- addon loaders load .node
        // binaries when `modules` looks right, crypto detection takes
        // OpenSSL paths -- and a missing key routes them to their
        // supported fallbacks. docs/node-divergences.md #1.
        ...JSON.parse(natives.depVersions),
      },
      pid: natives.pid,
      ppid: natives.ppid,
      title: "oam",
      exit(code) {
        // Route through the validating exitCode setter (coerces '2'->2, throws
        // on invalid), then exit with the COERCED value -- reading c directly
        // would send the raw string/undefined.
        if (code !== undefined && code !== null) process.exitCode = code;
        const numeric = typeof process.exitCode === "number" ? process.exitCode : 0;
        emitProcessExit(numeric);
        // Node RE-READS process.exitCode after the 'exit' emission: an exit
        // handler mutating it wins over the exit(code) argument.
        const finalCode = typeof process.exitCode === "number" ? process.exitCode : numeric;
        // Route through the writable reallyExit hook (Node per_thread.js):
        // a monkey-patched reallyExit returning makes exit() a no-op and the
        // run continues (test-process-really-exit). Read at CALL time.
        process.reallyExit(finalCode);
        // Survived (patched reallyExit returned): node keeps emitting
        // beforeExit/'exit' on later drains -- re-arm the once-guard.
        process._exiting = false;
      },
      // Default hard-exit hook behind process.exit -- plain writable
      // property, patchable like Node's.
      reallyExit(code) {
        natives.exit(code | 0);
      },
      // Node bootstrap installs _fatalException; the canonical dispatcher
      // checks it is still a function before running the uncaught ladder
      // (undefined -> exit 6, matching TriggerUncaughtException).
      _fatalException(err) {
        const dispatch = globalThis.__oamDispatchUncaught;
        return typeof dispatch === "function" ? dispatch(err, "uncaughtException") : false;
      },
      cwd: () => natives.cwd(),
      chdir: (dir) => {
        if (typeof dir !== "string") {
          throw new codes.ERR_INVALID_ARG_TYPE("directory", "string", dir);
        }
        const oldCwd = natives.cwd();
        try {
          return natives.chdir(dir);
        } catch (e) {
          // Node shapes a failed chdir as "<code>: <desc>, chdir '<oldcwd>' -> '<dest>'"
          // with err.path = the cwd at call time and err.dest = the target. oam's
          // native error omits the old-cwd path and the dest arrow, so rebuild it.
          const code = e.code || "ENOENT";
          const desc = String(e.message || "")
            .replace(/^[A-Z][A-Z0-9_]*:\s*/, "")
            .replace(/,\s*chdir\b.*$/, "");
          const err = new Error(`${code}: ${desc}, chdir '${oldCwd}' -> '${dir}'`);
          err.code = code;
          if (e.errno !== undefined) err.errno = e.errno;
          err.syscall = "chdir";
          err.path = oldCwd;
          err.dest = dir;
          throw err;
        }
      },
      nextTick(fn, ...args) {
        if (typeof fn !== "function") {
          throw new codes.ERR_INVALID_ARG_TYPE("callback", "Function", fn);
        }
        // Capture the ALS frame as DATA. Wrapping the callback in a binder
        // closure (the old shape) put an extra "at Array.<anonymous>" frame
        // between the user's frame and the drain -- Node has no such frame,
        // because its continuation propagation is native, not a JS closure.
        const captureFrame = registry._captureContinuationFrame;
        // Just enqueue: the HOST drains at every tick point (see
        // __oamDrainTicks above) -- no microtask trampoline.
        // entry[2]: domain captured at schedule time (undefined until
        // require('domain') installs process.domain) -- the drain enters it
        // around the callback so throws route to the domain, like timers.
        // async_hooks init for the TickObject, before the enqueue (node
        // order). The helper is installed by the async_hooks factory and
        // returns immediately when no hook is enabled, so the hot path is
        // unchanged for the overwhelmingly common no-hooks case.
        if (registry._emitAsyncInit) registry._emitAsyncInit("TickObject", null);
        tickQueue.push([
          fn,
          args,
          process.domain || null,
          captureFrame ? captureFrame() : undefined,
        ]);
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
      // stdout/stderr/stdin are installed as lazy memoized getters after the
      // object literal (see defineProperties below) -- constructing them here
      // would require('stream') at factory time, re-entering the init cycle
      // documented at the factory top.
      // Legacy process.binding: node v22 still ships it (deprecated but
      // present) and real packages feature-detect through it -- older
      // graceful-fs read process.binding('constants'), older supports-color
      // read process.binding('tty_wrap').isTTY.
      //
      // Only bindings oam can back with the REAL thing are modeled, and every
      // member is taken BY IDENTITY off the public surface that already
      // implements it, so the binding cannot drift from the module. Members
      // oam does not have are omitted, and the other 20 names on node's
      // allowlist keep throwing.
      //
      // That last part is deliberate, and it is why
      // test-process-binding-internalbinding-allowlist does not pass: it
      // asserts all 24 names are truthy, and most of them are libuv handle
      // classes (TCP, UDP, Pipe, JSStream, FSEvent) that oam has no
      // implementation behind. Returning `{}` would claim them. It is also
      // worse for the caller than throwing -- a package that feature-detects
      // process.binding('tcp_wrap'), gets an empty object, and proceeds hits
      // a TypeError deep in its own code, where the throw sends it straight
      // to the fallback branch that is correct for oam.
      //
      // Resolved lazily to avoid the process<->util factory cycle.
      binding(module) {
        // Node contract (probe-verified against v22.22.2): String()-coerce
        // the argument FIRST (a Symbol therefore throws the coercion
        // TypeError, and node's own message interpolates it the same way);
        // an unknown module is a plain Error with code UNDEFINED (node
        // reserves ERR_UNKNOWN_BUILTIN_MODULE for the require() side); and
        // the returned object is FRESH per call, not memoized -- no cache
        // property is stored on process.
        const name = String(module);
        if (name === "util") {
          const types = registry.get("util").types;
          const out = {};
          for (const k of [
            "isAnyArrayBuffer", "isArrayBuffer", "isArrayBufferView", "isAsyncFunction",
            "isDataView", "isDate", "isExternal", "isMap", "isMapIterator",
            "isNativeError", "isPromise", "isRegExp", "isSet", "isSetIterator",
            "isTypedArray", "isUint8Array",
          ]) {
            if (typeof types[k] === "function") out[k] = types[k];
          }
          return out;
        }
        if (name === "constants") {
          // Data only. node also publishes os.dlopen (RTLD_*),
          // os.UV_UDP_REUSEADDR, and the `trace` and `internal` namespaces;
          // oam has no dlopen, no UDP handle and no tracing subsystem, so
          // those are left out rather than given invented values.
          const os = registry.get("os").constants;
          return {
            os: { errno: os.errno, signals: os.signals, priority: os.priority },
            fs: registry.get("fs").constants,
            crypto: registry.get("crypto").constants,
            zlib: registry.get("zlib").constants,
          };
        }
        if (name === "tty_wrap") {
          // node also exposes the TTY handle class, which is a libuv stream
          // wrapper oam has nothing behind. isTTY is the half that is real.
          return { isTTY: registry.get("tty").isatty };
        }
        throw new Error(`No such module: ${name}`);
      },
      getBuiltinModule(id) {
        // Node contract (probe-verified against v22.22.2): non-string ids
        // are ERR_INVALID_ARG_TYPE (no String() coercion); 'internal/*' is
        // never exposed with or without the prefix; unknown ids return
        // undefined; sub-path builtins ('timers/promises') resolve.
        if (typeof id !== "string") {
          throw new codes.ERR_INVALID_ARG_TYPE("id", "string", id);
        }
        var prefixed = id.startsWith("node:");
        var bare = prefixed ? id.slice(5) : id;
        if (bare.startsWith("internal/")) return undefined;
        // Prefix-only builtins: node exposes these ONLY as 'node:x' -- bare
        // 'test' / 'sea' / 'sqlite' / 'test/reporters' are undefined
        // (probe-verified against v22.22.2).
        if (!prefixed && PREFIX_ONLY_BUILTINS.has(bare)) return undefined;
        if (registry.factories[bare]) return registry.get(bare);
        return undefined;
      },
      _rawDebug(...args) {
        // Synchronous low-level stderr write, bypassing the writable stream
        // (Node's process._rawDebug); util.format the args + trailing newline.
        let text = registry.get("util").format(...args) + "\n";
        // Node's RawDebug fprintf()s to a CRT stream that is in TEXT mode on
        // Windows, so EVERY LF in the payload becomes CRLF -- including ones
        // inside the formatted message, not just the trailing one.
        // (Probe-verified: node's _rawDebug lines are CRLF while its
        // console.error lines stay LF, so the translation is scoped HERE.)
        if (natives.platform === "win32") text = text.replace(/\n/g, "\r\n");
        natives.stderrWrite(text);
      },
      setSourceMapsEnabled(val) {
        // oam has no source-map translation toggle yet; validate + no-op so the
        // surface matches Node (the test only asserts the ERR_INVALID_ARG_TYPE throw).
        if (typeof val !== "boolean") {
          throw new codes.ERR_INVALID_ARG_TYPE("val", "boolean", val);
        }
      },
      assert(condition, message) {
        // Deprecated (DEP0100): emit the warning once, then assert.
        if (!process._assertDep0100Warned) {
          process._assertDep0100Warned = true;
          process.emitWarning(
            "process.assert() is deprecated. Please use the `assert` module instead.",
            { type: "DeprecationWarning", code: "DEP0100" },
          );
        }
        if (!condition) throw new codes.ERR_ASSERTION(message);
      },
      emitWarning(warning, typeOrOptions, codeArg, ctorArg) {
        // Normalize the (warning, type, code, ctor) / (warning, options)
        // overloads the way Node does.
        let type = "Warning";
        let code;
        let ctor;
        let detail;
        if (typeOrOptions !== null && typeof typeOrOptions === "object" && !Array.isArray(typeOrOptions)) {
          // options form
          if (typeOrOptions.type !== undefined) {
            if (typeof typeOrOptions.type !== "string") {
              throw new codes.ERR_INVALID_ARG_TYPE("options.type", "string", typeOrOptions.type);
            }
            type = typeOrOptions.type;
          }
          if (typeOrOptions.code !== undefined) {
            if (typeof typeOrOptions.code !== "string") {
              throw new codes.ERR_INVALID_ARG_TYPE("options.code", "string", typeOrOptions.code);
            }
            code = typeOrOptions.code;
          }
          if (typeof typeOrOptions.ctor === "function") ctor = typeOrOptions.ctor;
          if (typeof typeOrOptions.detail === "string") detail = typeOrOptions.detail;
        } else if (typeof typeOrOptions === "function") {
          // emitWarning(msg, ctor): a FUNCTION in the type position is the
          // constructor used to trim the stack, not a type name. Node
          // shifts it and leaves type at 'Warning'; oam used to throw
          // ERR_INVALID_ARG_TYPE and reject a documented call form.
          ctor = typeOrOptions;
        } else {
          if (typeOrOptions !== undefined) {
            if (typeof typeOrOptions !== "string") {
              throw new codes.ERR_INVALID_ARG_TYPE("type", "string", typeOrOptions);
            }
            type = typeOrOptions;
          }
          if (typeof codeArg === "function") {
            ctor = codeArg;
          } else if (codeArg !== undefined) {
            if (typeof codeArg !== "string") {
              throw new codes.ERR_INVALID_ARG_TYPE("code", "string", codeArg);
            }
            code = codeArg;
          }
          if (typeof ctorArg === "function") ctor = ctorArg;
        }

        let warningObj;
        if (warning instanceof Error) {
          warningObj = warning;
        } else if (typeof warning === "string") {
          warningObj = new Error(warning);
          warningObj.name = String(type);
          if (code !== undefined) warningObj.code = code;
          if (detail !== undefined) warningObj.detail = detail;
          if (typeof Error.captureStackTrace === "function") {
            Error.captureStackTrace(warningObj, ctor || process.emitWarning);
          }
        } else {
          throw new codes.ERR_INVALID_ARG_TYPE("warning", ["Error", "string"], warning);
        }

        // throwDeprecation: a DeprecationWarning surfaces as an async throw
        // (uncaughtException), NOT a synchronous throw from emitWarning --
        // test-process-warning asserts the call site does not throw.
        if (warningObj.name === "DeprecationWarning") {
          if (process.throwDeprecation) {
            process.nextTick(() => { throw warningObj; });
            return;
          }
          if (process.noDeprecation) return;
        }

        // Defer so listeners attached after the emitWarning call (Node defers
        // to nextTick) still fire.
        process.nextTick(() => {
          process.emit("warning", warningObj);
          // Node's built-in default 'warning' handler always writes the
          // formatted warning to stderr (it fires even when a user listener is
          // also present). emitWarning always builds an Error, so this path is
          // Error-shaped; a raw-string warning emitted via process.emit(...)
          // bypasses emitWarning and is correctly NOT printed (test-process-
          // warning). Use name+message rather than toString() so a warning with
          // a non-function toString does not crash. The (node:PID) prefix
          // matches Node's format so tooling that greps warnings still works.
          const code = warningObj.code ? `[${warningObj.code}] ` : "";
          let line = `(node:${process.pid}) ${code}${warningObj.name}: ${warningObj.message}\n`;
          if (typeof warningObj.detail === "string") line += `${warningObj.detail}\n`;
          // NODE_REDIRECT_WARNINGS: append the formatted warning to the named
          // file instead of stderr. Read from natives.env() (the real OS
          // environment) and cached, NOT process.env: node snapshots the
          // variable at startup, so a runtime `process.env.NODE_REDIRECT_
          // WARNINGS = ...` is ignored (probe-verified -- the warning still
          // goes to stderr). Relative paths resolve against cwd; 'a' appends
          // across runs. Node silently falls back to stderr on ANY write
          // failure (probe: a path under a missing directory prints to stderr
          // and still exits 0), so every throw here routes to stderrWrite.
          // Suppression flags (--no-warnings / --no-deprecation /
          // --disable-warning=<name|code>, installed by the CLI as
          // non-enumerable globals). Node still EMITS the 'warning' event
          // under all of them -- only this default stderr writer is gated.
          if (globalThis.__oamNoWarnings) return;
          if (globalThis.__oamNoDeprecation && warningObj.name === "DeprecationWarning") return;
          const disabled = globalThis.__oamDisabledWarnings;
          if (
            Array.isArray(disabled) &&
            (disabled.includes(warningObj.name) ||
              (warningObj.code && disabled.includes(warningObj.code)))
          ) {
            return;
          }
          if (redirectWarningsPath === undefined) {
            // --redirect-warnings=<path> takes precedence over the env var
            // (probe-verified); both are snapshotted, not re-read.
            let v = globalThis.__oamRedirectWarnings;
            if (typeof v !== "string" || v === "") {
              try { v = natives.env().NODE_REDIRECT_WARNINGS; } catch (_) { v = undefined; }
            }
            redirectWarningsPath = typeof v === "string" && v !== "" ? v : null;
          }
          if (redirectWarningsPath !== null) {
            try {
              registry.get("fs").appendFileSync(redirectWarningsPath, line, "utf8");
              return;
            } catch (_) {
              // fall through to stderr, exactly as node does
            }
          }
          natives.stderrWrite(line);
        });
      },
      cpuUsage: (prev) => {
        if (prev) {
          const valid = (n) => typeof n === "number" && n >= 0 && n <= Number.MAX_SAFE_INTEGER;
          if (typeof prev !== "object" || prev === null) {
            throw new codes.ERR_INVALID_ARG_TYPE("prevValue", "object", prev);
          }
          if (!valid(prev.user)) {
            if (typeof prev.user !== "number") {
              throw new codes.ERR_INVALID_ARG_TYPE("prevValue.user", "number", prev.user);
            }
            throw applyNodeErrorShape(
              new RangeError("The property 'prevValue.user' is invalid. Received " + String(prev.user)),
              "ERR_INVALID_ARG_VALUE",
            );
          }
          if (!valid(prev.system)) {
            if (typeof prev.system !== "number") {
              throw new codes.ERR_INVALID_ARG_TYPE("prevValue.system", "number", prev.system);
            }
            throw applyNodeErrorShape(
              new RangeError("The property 'prevValue.system' is invalid. Received " + String(prev.system)),
              "ERR_INVALID_ARG_VALUE",
            );
          }
        }
        var usage = natives.processCpuUsage();
        if (prev) return { user: usage.user - prev.user, system: usage.system - prev.system };
        return usage;
      },
      threadCpuUsage: (prev) => {
        if (prev !== undefined) {
          if (typeof prev !== "object" || prev === null || Array.isArray(prev)) {
            throw new codes.ERR_INVALID_ARG_TYPE("prevValue", "object", prev);
          }
          const valid = (n) => typeof n === "number" && n >= 0 && n <= Number.MAX_SAFE_INTEGER;
          if (!valid(prev.user)) {
            if (typeof prev.user !== "number") {
              throw new codes.ERR_INVALID_ARG_TYPE("prevValue.user", "number", prev.user);
            }
            throw applyNodeErrorShape(
              new RangeError("The property 'prevValue.user' is invalid. Received " + String(prev.user)),
              "ERR_INVALID_ARG_VALUE",
            );
          }
          if (!valid(prev.system)) {
            if (typeof prev.system !== "number") {
              throw new codes.ERR_INVALID_ARG_TYPE("prevValue.system", "number", prev.system);
            }
            throw applyNodeErrorShape(
              new RangeError("The property 'prevValue.system' is invalid. Received " + String(prev.system)),
              "ERR_INVALID_ARG_VALUE",
            );
          }
        }
        // Main-thread approximation: thread CPU time ~= process CPU time. Enough
        // for the finite/non-negative/monotonic asserts; a true per-thread clock
        // needs a native binding.
        const usage = natives.processCpuUsage();
        if (prev) return { user: usage.user - prev.user, system: usage.system - prev.system };
        return usage;
      },
      kill: (pid, signal) => {
        if (pid != (pid | 0)) {
          throw new codes.ERR_INVALID_ARG_TYPE("pid", "number", pid);
        }
        const SIGNALS = { SIGHUP: 1, SIGINT: 2, SIGQUIT: 3, SIGILL: 4, SIGTRAP: 5, SIGABRT: 6, SIGBUS: 7, SIGFPE: 8, SIGKILL: 9, SIGUSR1: 10, SIGSEGV: 11, SIGUSR2: 12, SIGPIPE: 13, SIGALRM: 14, SIGTERM: 15 };
        let err;
        if (signal === (signal | 0)) {
          err = process._kill(pid, signal);
        } else {
          const name = signal || "SIGTERM";
          const n = SIGNALS[name];
          if (n === undefined) throw new codes.ERR_UNKNOWN_SIGNAL(name);
          err = process._kill(pid, n);
        }
        if (err) {
          const NAME = { "-22": "EINVAL", "-1": "EPERM", "-3": "ESRCH" };
          const nm = NAME[String(err)] || "UNKNOWN";
          const e = new Error("kill " + nm);
          e.code = nm;
          e.errno = err;
          e.syscall = "kill";
          throw e;
        }
        return true;
      },
      // Node-binding shape: 0 on success, negative libuv errno on failure.
      // process.kill routes through this (monkeypatchable -- test-process-kill-pid
      // replaces it to intercept pid/sig without actually signalling).
      _kill: (pid, sig) => {
        const VALID = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        if (sig !== 0 && !VALID.includes(sig)) return -22; // EINVAL before touching the sandbox
        try { natives.processKill(pid, sig); return 0; } catch { return -1; }
      },
      // Lives in js/internal/process/per_thread.js so its stack frame names
      // the module node core names. Bound once here; see that file.
      execve: perThread.execve,
      umask(mask) {
        const prev = process._umask ?? 0;
        if (mask === undefined) return prev;
        if (typeof mask === "number") {
          process._umask = mask & 0o777;
          return prev;
        }
        if (typeof mask === "string") {
          // Octal string ("0664"). Reject non-octal.
          if (!/^[0-7]+$/.test(mask)) {
            throw new codes.ERR_INVALID_ARG_VALUE("mask", mask, "must be a 32-bit unsigned integer or an octal string");
          }
          const parsed = parseInt(mask, 8);
          if (!Number.isFinite(parsed) || parsed > 0xffffffff) {
            throw new codes.ERR_INVALID_ARG_VALUE("mask", mask, "must be a 32-bit unsigned integer or an octal string");
          }
          // Node keeps only the low permission bits (mode > 0o777 still works).
          process._umask = parsed & 0o777;
          return prev;
        }
        throw new codes.ERR_INVALID_ARG_TYPE("mask", ["number", "string"], mask);
      },
      // Active-resource introspection, backed by three JS-side tables:
      // registry._activeRequests (fs callback ops), registry._activeHandles
      // (net sockets/servers) and registry._activeTimers (the timer registry
      // installed in installRuntimeGlobals).
      //
      // Ordering matches node (probed on v22.22.2): requests, then handles,
      // then timers -- e.g. a pending connect + listening server + timer
      // yields ["GetAddrInfoReqWrap","TCPServerWrap","TCPSocketWrap","Timeout"].
      getActiveResourcesInfo() {
        const out = [];
        const reqs = registry._activeRequests;
        if (reqs) for (const r of reqs) out.push(r.type);
        const handles = registry._activeHandles;
        if (handles) {
          for (const [h, type] of handles) {
            if (h._handleRefed === false) continue; // unref'd: node omits it
            out.push(type);
          }
        }
        const reg = registry._activeTimers;
        if (reg) {
          for (const t of reg.values()) {
            out.push(t._kind === "Immediate" ? "Immediate" : "Timeout");
          }
        }
        return out;
      },
      // Node's _getActiveHandles() returns libuv HANDLES only -- timers are
      // requests-of-a-different-kind and are deliberately absent (probe: with
      // a pending setTimeout, node returns []).
      _getActiveHandles() {
        const out = [];
        const handles = registry._activeHandles;
        if (handles) {
          for (const h of handles.keys()) {
            if (h._handleRefed === false) continue; // unref'd: node omits it
            out.push(h);
          }
        }
        return out;
      },
      _getActiveRequests() {
        const out = [];
        const reqs = registry._activeRequests;
        if (reqs) for (const r of reqs) out.push(r);
        return out;
      },
      ref(handle) {
        if (handle == null) return;
        if (typeof handle.ref === "function") { handle.ref(); return; }
        const sym = Symbol.for("nodejs.ref");
        if (typeof handle[sym] === "function") handle[sym]();
      },
      unref(handle) {
        if (handle == null) return;
        if (typeof handle.unref === "function") { handle.unref(); return; }
        const sym = Symbol.for("nodejs.unref");
        if (typeof handle[sym] === "function") handle[sym]();
      },
      // POSIX identity API. Node defines these only on POSIX hosts -- on
      // Windows every one of them is undefined (test-process-uid-gid and
      // friends assert exactly that), so the keys must be absent, not stubbed.
      //
      // These were stubs returning 0 until 2026-08-04. `getuid: () => 0`
      // claimed the process was ROOT, which is both false and load-bearing:
      // real code (and Node's own suite) branches on `getuid() === 0`. They
      // are real syscalls now, and the setters validate their arguments
      // exactly as Node does instead of silently accepting anything.
      ...(natives.platform === "win32" ? {} : {
        getuid: () => natives.posixGetId(0),
        getgid: () => natives.posixGetId(1),
        geteuid: () => natives.posixGetId(2),
        getegid: () => natives.posixGetId(3),
        setuid: (id) => posixSetId(0, id, "uid"),
        setgid: (id) => posixSetId(1, id, "gid"),
        seteuid: (id) => posixSetId(2, id, "uid"),
        setegid: (id) => posixSetId(3, id, "gid"),
        getgroups: () => natives.posixGetGroups() ?? [],
        setgroups: (groups) => {
          if (!Array.isArray(groups)) {
            throw new codes.ERR_INVALID_ARG_TYPE("groups", "Array", groups);
          }
          // Node names the INDEX in element errors ("groups[0]"), which is
          // the difference between a usable message and a guess when one
          // entry of a long list is wrong.
          const resolved = groups.map((g, i) =>
            resolveCredential(g, `groups[${i}]`, "gid"),
          );
          const err = natives.posixSetGroups(resolved);
          if (err) throw credentialSyscallError(err, "setgroups");
        },
        initgroups: (user, extraGroup) => {
          validateCredentialType(user, "user");
          validateCredentialType(extraGroup, "extraGroup");
          const gid = resolveCredential(extraGroup, "extraGroup", "gid");
          // initgroups(3) takes the user by NAME, so a numeric uid is mapped
          // back through getpwuid rather than rejected.
          let name = user;
          if (typeof user === "number") {
            name = natives.posixLookupId(2, user);
            if (name === null || name === undefined) {
              throw makeNodeError(
                "ERR_UNKNOWN_CREDENTIAL",
                `User identifier does not exist: ${user}`,
              );
            }
          } else if (natives.posixLookupId(0, user) === null) {
            throw makeNodeError(
              "ERR_UNKNOWN_CREDENTIAL",
              `User identifier does not exist: ${user}`,
            );
          }
          const err = natives.posixInitGroups(name, gid);
          if (err) throw credentialSyscallError(err, "initgroups");
        },
      }),
      setUncaughtExceptionCaptureCallback(fn) {
        if (fn === null) { process._uncaughtCaptureCb = null; return; }
        if (typeof fn !== "function") {
          throw new codes.ERR_INVALID_ARG_TYPE("fn", ["Function", "null"], fn);
        }
        // Node forbids the capture callback once the domain module is in
        // use (they own the same fatal seam).
        if (registry._domainLoaded) {
          throw applyNodeErrorShape(
            new Error("The `domain` module is in use, which is mutually exclusive with calling process.setUncaughtExceptionCaptureCallback()"),
            "ERR_DOMAIN_CANNOT_SET_UNCAUGHT_EXCEPTION_CAPTURE",
          );
        }
        if (process._uncaughtCaptureCb) {
          throw applyNodeErrorShape(
            new Error("`setupUncaughtExceptionCapture()` was called while a capture callback was already active"),
            "ERR_UNCAUGHT_EXCEPTION_CAPTURE_ALREADY_SET",
          );
        }
        process._uncaughtCaptureCb = fn;
      },
      hasUncaughtExceptionCaptureCallback() {
        return !!process._uncaughtCaptureCb;
      },
      resourceUsage: () => ({
        userCPUTime: 0, systemCPUTime: 0, maxRSS: 0,
        sharedMemorySize: 0, unsharedDataSize: 0, unsharedStackSize: 0,
        minorPageFault: 0, majorPageFault: 0, swappedOut: 0,
        fsRead: 0, fsWrite: 0, ipcSent: 0, ipcReceived: 0,
        signalsCount: 0, voluntaryContextSwitches: 0, involuntaryContextSwitches: 0,
      }),
      release: { name: "node", lts: "Jod" },
      config: Object.freeze({
        variables: Object.freeze({
          // Which of Node's shareable builtin JS deps are compiled into
          // this binary: none -- oam ships its own builtin set, so the
          // honest value is the empty list (it also answers probes like
          // test-process-versions' hasUndici/hasAmaro with false, which
          // is the truth).
          node_builtin_shareable_builtins: Object.freeze([]),
        }),
      }),
      features: {
        inspector: false,
        debug: false,
        uv: true,
        ipv6: true,
        openssl_is_boringssl: false,
        tls_alpn: true,
        tls_sni: true,
        tls_ocsp: true,
        tls: true,
        cached_builtins: true,
        require_module: true,
        typescript: "strip",
      },
      allowedNodeEnvironmentFlags: (() => {
        // Node returns a frozen, immutable Set whose mutators no-op, whose
        // size/iteration derive from an internal array (defeating raw
        // Set.prototype.add/clear/delete probes), and whose has() normalises
        // underscores->dashes and matches both dashed and un-dashed forms.
        const flagsArray = [
          "--allow-child-process", "--allow-fs-read", "--allow-fs-write", "--allow-worker",
          "--conditions", "--cpu-prof", "--cpu-prof-dir", "--cpu-prof-interval", "--cpu-prof-name",
          "--diagnostic-dir", "--disable-proto", "--dns-result-order", "--enable-source-maps",
          "--experimental-vm-modules", "--force-context-aware", "--frozen-intrinsics",
          "--heap-prof", "--heap-prof-dir", "--heap-prof-interval", "--heap-prof-name",
          "--heapsnapshot-near-heap-limit", "--heapsnapshot-signal", "--icu-data-dir",
          "--import", "--input-type", "--insecure-http-parser", "--loader", "--max-http-header-size",
          "--no-deprecation", "--no-experimental-fetch", "--no-warnings", "--openssl-config",
          "--perf-basic-prof", "--perf-basic-prof-only-functions", "--perf-prof",
          "--preserve-symlinks", "--preserve-symlinks-main", "--prof-process", "--redirect-warnings",
          "--report-dir", "--report-filename", "--report-on-fatalerror", "--report-on-signal",
          "--report-signal", "--report-uncaught-exception", "--require", "--secure-heap",
          "--stack-trace-limit", "--throw-deprecation", "--title", "--tls-cipher-list",
          "--tls-keylog", "--tls-max-v1.2", "--tls-max-v1.3", "--tls-min-v1.0", "--tls-min-v1.1",
          "--tls-min-v1.2", "--tls-min-v1.3", "--trace-deprecation", "--trace-event-categories",
          "--trace-event-file-pattern", "--trace-exit", "--trace-sigint", "--trace-sync-io",
          "--trace-tls", "--trace-uncaught", "--trace-warnings", "--track-heap-objects",
          "--unhandled-rejections", "--use-bundled-ca", "--use-openssl-ca", "--v8-pool-size",
          "--zero-fill-buffers", "-r", "-C",
        ];
        const nodeFlags = flagsArray.map((f) => f.replace(/^--?/, ""));
        class NodeEnvironmentFlagsSet extends Set {
          constructor(arr) {
            super();
            Object.defineProperty(this, "_array", { value: arr.slice() });
            // Populate the backing set via the NATIVE add -- super(arr) would
            // route through the overridden (no-op) add() below and leave it empty.
            for (const x of arr) Set.prototype.add.call(this, x);
          }
          add() { return this; }
          delete() { return false; }
          clear() {}
          get size() { return this._array.length; }
          has(key) {
            if (typeof key !== "string") return false;
            key = key.replace(/_/g, "-");
            if (/^--?/.test(key)) {
              key = key.replace(/=.*$/, "");
              return Set.prototype.has.call(this, key);
            }
            return nodeFlags.includes(key);
          }
          forEach(cb, thisArg) { this._array.forEach((v) => cb.call(thisArg, v, v, this)); }
          entries() { return this._array.map((v) => [v, v])[Symbol.iterator](); }
          values() { return this._array[Symbol.iterator](); }
          keys() { return this._array[Symbol.iterator](); }
          [Symbol.iterator]() { return this._array[Symbol.iterator](); }
        }
        return Object.freeze(new NodeEnvironmentFlagsSet(flagsArray));
      })(),
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
        // A missing file throws the plain fs ENOENT system error, not a
        // wrapped ERR_ENV_FILE_NOT_FOUND (probe-verified against node v22).
        var text = fs.readFileSync(envPath, "utf8");
        // Same dotenv parser as util.parseEnv (Node routes both through
        // node_dotenv.cc). Real env vars win: node v22 loadEnvFile does NOT
        // overwrite keys already present (probe-verified). Presence check
        // reads the value ('in' would consult the env proxy target's
        // prototype chain, silently dropping keys like 'toString').
        var parsed = registry.get("util").parseEnv(text);
        var keys = Object.keys(parsed);
        for (var ki = 0; ki < keys.length; ki++) {
          var key = keys[ki];
          if (process.env[key] === undefined) process.env[key] = parsed[key];
        }
      },
    });

    // Lazy stdio (Node does the same): memoized getters so the FIRST
    // require('stream') happens on first stdio ACCESS, after
    // installRuntimeGlobals has assigned globalThis.process -- never inside
    // this factory (init-cycle documented at the factory top). The getters
    // are replaceable data-property-style (set overrides), matching Node's
    // configurable stdio accessors.
    {
      const lazyStdio = (name, build) => {
        let cached;
        Object.defineProperty(process, name, {
          configurable: true,
          enumerable: true,
          get() {
            if (cached === undefined) cached = build();
            return cached;
          },
          set(v) {
            cached = v;
          },
        });
      };
      lazyStdio("stdout", () => {
        const { Writable } = registry.get("stream");
        return decorateTtyWriteStream(new Writable({
          write(chunk, _enc, cb) { natives.stdoutWrite(chunk); cb(); },
          decodeStrings: false,
        }), 1, stdoutIsTTY);
      });
      lazyStdio("stderr", () => {
        const { Writable } = registry.get("stream");
        return decorateTtyWriteStream(new Writable({
          write(chunk, _enc, cb) { natives.stderrWrite(chunk); cb(); },
          decodeStrings: false,
        }), 2, stderrIsTTY);
      });
      lazyStdio("stdin", () => {
        const { Readable } = registry.get("stream");
        return decorateTtyReadStream(new Readable({
          autoDestroy: false,
          read() {
            natives.stdinRead().then(
              (chunk) => {
                if (chunk === undefined || chunk === null || chunk.length === 0) this.push(null);
                else this.push(Buffer.from(chunk));
              },
              () => this.push(null),
            );
          },
        }), 0, stdinIsTTY);
      });
    }

    // process.exitCode: a validating, non-configurable accessor (Node). Accepts
    // an integer, an integer-valued string ('2' -> 2), or undefined/null
    // (-> undefined, exits 0). A non-numeric string / non-number throws
    // ERR_INVALID_ARG_TYPE; a non-integer number throws ERR_OUT_OF_RANGE.
    // Non-configurable so `delete process.exitCode` throws (test-process-exit-
    // code-validation).
    let _exitCode = undefined;
    Object.defineProperty(process, "exitCode", {
      enumerable: true,
      configurable: false,
      get() {
        return _exitCode;
      },
      set(value) {
        if (value === undefined || value === null) {
          _exitCode = undefined;
          return;
        }
        if (typeof value === "string") {
          const n = Number(value);
          if (value.trim() !== "" && Number.isInteger(n)) {
            _exitCode = n;
            return;
          }
          throw new codes.ERR_INVALID_ARG_TYPE("code", "number", value);
        }
        if (typeof value !== "number") {
          throw new codes.ERR_INVALID_ARG_TYPE("code", "number", value);
        }
        if (!Number.isInteger(value)) {
          throw new codes.ERR_OUT_OF_RANGE("code", "an integer", value);
        }
        _exitCode = value;
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
      // NOTE: 'internal/errors' is deliberately NOT listed. node's
      // builtinModules contains zero 'internal/*' entries, and consumers --
      // including node's own test-process-get-builtin -- require() every
      // listed name. oam advertised it here while `require('internal/
      // errors')` has always thrown (verified pre-change), so listing it
      // was purely a false advertisement. The vendored-streams loader
      // reaches its own internal shim directly, not through this list.
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
      "_stream_readable",
      "_stream_writable",
      "_stream_duplex",
      "_stream_transform",
      "_stream_passthrough",
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

    // createHook: a REAL registry, but a deliberately NARROW one. oam emits
    // init for the resource kinds it actually models (currently TickObject);
    // executionAsyncId/triggerAsyncId stay 0 because oam has no async-id
    // plumbing, and before/after/destroy never fire. This is NOT full
    // async_hooks -- it is an honest init-observer instead of a silent no-op.
    // Full support would need a PromiseHook binding plus hook dispatch around
    // every native op; AsyncLocalStorage (fully supported here) is what real
    // code overwhelmingly uses.
    const activeHooks = new Set();
    let asyncIdSeq = 1;
    registry._emitAsyncInit = (type, resource) => {
      if (activeHooks.size === 0) return;
      const id = ++asyncIdSeq;
      for (const h of activeHooks) {
        if (typeof h.init === "function") {
          try { h.init(id, type, 0, resource); } catch { /* node swallows */ }
        }
      }
    };
    function createHook(callbacks) {
      return {
        enable() { activeHooks.add(callbacks); return this; },
        disable() { activeHooks.delete(callbacks); return this; },
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
  // Legacy internal stream module aliases: Node exposes require()able builtins
  // that point at the public stream constructors (test-stream-aliases-legacy).
  registry.factories["_stream_readable"] = () => registry.get("stream").Readable;
  registry.factories["_stream_writable"] = () => registry.get("stream").Writable;
  registry.factories["_stream_duplex"] = () => registry.get("stream").Duplex;
  registry.factories["_stream_transform"] = () => registry.get("stream").Transform;
  registry.factories["_stream_passthrough"] = () => registry.get("stream").PassThrough;
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
        // Permissive on purpose: blob/arrayBuffer/buffer route through
        // Blob in Node, which STRINGIFIES a non-buffer chunk (an
        // object-mode stream really does yield "[object Object]" bytes).
        // Only text/json validate -- see textOf.
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
    /// text() and json() are STRICT where the byte collectors are not: a
    /// chunk that is neither a string nor a view has no sane text reading,
    /// and silently yielding "[object Object]" hides a mis-shaped stream
    /// from the caller. An incomplete multi-byte tail still flushes to
    /// U+FFFD, because the whole byte run is decoded in one pass.
    async function textOf(streamLike) {
      const parts = [];
      let total = 0;
      for await (const chunk of streamLike) {
        let bytes;
        if (typeof chunk === "string") {
          bytes = globalThis.Buffer.from(chunk, "utf8");
        } else if (ArrayBuffer.isView(chunk)) {
          bytes = new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
        } else {
          throw new codes.ERR_INVALID_ARG_TYPE(
            "chunk",
            ["string", "ArrayBufferView"],
            chunk,
          );
        }
        parts.push(bytes);
        total += bytes.length;
      }
      const all = new Uint8Array(total);
      let at = 0;
      for (const p of parts) {
        all.set(p, at);
        at += p.length;
      }
      return new TextDecoder().decode(all);
    }
    return {
      arrayBuffer: async (s) => (await bytesOf(s)).buffer,
      buffer: async (s) => {
        const bytes = await bytesOf(s);
        return globalThis.Buffer.from(bytes.buffer, bytes.byteOffset, bytes.length);
      },
      text: (s) => textOf(s),
      json: async (s) => JSON.parse(await textOf(s)),
      // Node exports blob() alongside the rest; it was simply missing here,
      // so `import { blob } from 'node:stream/consumers'` yielded undefined
      // and failed at the call site rather than at import.
      blob: async (s) => new globalThis.Blob([await bytesOf(s)]),
    };
  };

  // -------------------------------------------------------- string_decoder
  // StringDecoder = boundary-safe Buffer -> string decoding. The utf8
  // family rides TextDecoder's stream:true buffering (multi-byte chars
  // split across chunks reassemble exactly); single-byte encodings decode
  // per chunk. base64/hex per-chunk decoding can tear at chunk boundaries
  // (punch-listed; rare in stream position).
  registry.factories.string_decoder = () => {
    const Buffer = globalThis.Buffer;

    // Faithful port of Node's pure-JS StringDecoder reference (the algorithm
    // behind lib/string_decoder.js): it buffers an incomplete multi-byte
    // sequence (utf8), an incomplete 2-byte code unit (utf16le/ucs2), or an
    // incomplete 3-byte group (base64/base64url) across write() calls and
    // flushes it on end() exactly the way Node does (lone bytes -> U+FFFD).

    function normalizeEncoding(enc) {
      const e = String(enc || "utf8").toLowerCase();
      switch (e) {
        case "utf8":
        case "utf-8":
          return "utf8";
        case "ucs2":
        case "ucs-2":
        case "utf16le":
        case "utf-16le":
          return "utf16le";
        case "latin1":
        case "binary":
          return "latin1";
        case "base64":
          return "base64";
        case "base64url":
          return "base64url";
        case "ascii":
        case "hex":
          return e;
        default:
          throw new codes.ERR_UNKNOWN_ENCODING(enc);
      }
    }

    // How many continuation bytes a UTF-8 lead byte still needs, or -1/-2 for
    // invalid leads (Node's utf8CheckByte semantics).
    function utf8CheckByte(byte) {
      if (byte <= 0x7f) return 0;
      // C0/C1 are overlong-only and F5-FF are beyond U+10FFFF: never legal
      // leads. The pure-JS reference accepts them (bit-pattern match alone),
      // so an invalid lead was treated as the start of an incomplete
      // sequence and swallowed the byte after it.
      if (byte >> 5 === 0x06) return byte >= 0xc2 ? 2 : -2;
      if (byte >> 4 === 0x0e) return 3;
      if (byte >> 3 === 0x1e) return byte <= 0xf4 ? 4 : -2;
      return byte >> 6 === 0x02 ? -1 : -2;
    }

    class StringDecoder {
      constructor(encoding) {
        this.encoding = normalizeEncoding(encoding);
        let nb;
        switch (this.encoding) {
          case "utf16le":
            this.text = utf16Text;
            this.end = utf16End;
            this.fillLast = simpleFillLast;
            nb = 4;
            break;
          case "utf8":
            this.fillLast = utf8FillLast;
            nb = 4;
            break;
          case "base64":
          case "base64url":
            this.text = base64Text;
            this.end = base64End;
            this.fillLast = simpleFillLast;
            nb = 3;
            break;
          default:
            this.write = simpleWrite;
            this.end = simpleEnd;
            return;
        }
        this.lastNeed = 0;
        this.lastTotal = 0;
        this.lastChar = Buffer.allocUnsafe(nb);
      }

      write(buf) {
        if (buf.length === 0) return "";
        let r;
        let i;
        if (this.lastNeed) {
          r = this.fillLast(buf);
          if (r === undefined) return "";
          i = this.lastNeed;
          this.lastNeed = 0;
        } else {
          i = 0;
        }
        if (i < buf.length) {
          return r ? r + this.text(buf, i) : this.text(buf, i);
        }
        return r || "";
      }

      end(buf) {
        const r = buf && buf.length ? this.write(buf) : "";
        if (this.lastNeed) {
          // Reset so the SAME decoder can be reused after a flush (Node parity:
          // the corpus reuses one StringDecoder across write/end pairs).
          this.lastNeed = 0;
          this.lastTotal = 0;
          return r + "�";
        }
        return r;
      }

      // utf8 text + utf8 end (overridable below for other encodings).
      text(buf, i) {
        const total = utf8CheckIncomplete(this, buf, i);
        if (!this.lastNeed) return buf.toString("utf8", i);
        this.lastTotal = total;
        const end = buf.length - (total - this.lastNeed);
        buf.copy(this.lastChar, 0, end);
        return buf.toString("utf8", i, end);
      }

      fillLast(buf) {
        return utf8FillLast.call(this, buf);
      }
    }

    // ---- simple fillLast (utf16le / base64): accumulate raw bytes ----
    function simpleFillLast(buf) {
      if (this.lastNeed <= buf.length) {
        buf.copy(this.lastChar, this.lastTotal - this.lastNeed, 0, this.lastNeed);
        return this.lastChar.toString(this.encoding, 0, this.lastTotal);
      }
      buf.copy(this.lastChar, this.lastTotal - this.lastNeed, 0, buf.length);
      this.lastNeed -= buf.length;
      return undefined;
    }

    // ---- utf8 helpers ----
    function utf8FillLast(buf) {
      const p = this.lastTotal - this.lastNeed;
      const r = utf8CheckExtraBytes(this, buf);
      if (r !== undefined) return r;
      if (this.lastNeed <= buf.length) {
        buf.copy(this.lastChar, p, 0, this.lastNeed);
        return this.lastChar.toString(this.encoding, 0, this.lastTotal);
      }
      buf.copy(this.lastChar, p, 0, buf.length);
      this.lastNeed -= buf.length;
      return undefined;
    }

    function utf8CheckExtraBytes(self, buf) {
      // Bytes of the sequence already buffered; 1 means only the lead, so
      // the incoming byte is the SECOND and the lead constrains its range.
      const have = self.lastTotal - self.lastNeed;
      if (
        (buf[0] & 0xc0) !== 0x80 ||
        (have === 1 && !utf8SecondByteOk(self.lastChar[0], buf[0]))
      ) {
        self.lastNeed = 0;
        return "�";
      }
      if (self.lastNeed > 1 && buf.length > 1) {
        if ((buf[1] & 0xc0) !== 0x80) {
          self.lastNeed = 1;
          return "�";
        }
        if (self.lastNeed > 2 && buf.length > 2) {
          if ((buf[2] & 0xc0) !== 0x80) {
            self.lastNeed = 2;
            return "�";
          }
        }
      }
      return undefined;
    }

    // Is `b` a legal SECOND byte for this lead? The lead constrains it
    // (E0:A0-BF, ED:80-9F, F0:90-BF, F4:80-8F, otherwise 80-BF). Node's
    // shipped StringDecoder is the C++ one and enforces this; the pure-JS
    // reference this file ports does not, which made "E0 80" look like an
    // INCOMPLETE 3-byte sequence (one U+FFFD) instead of two errors.
    function utf8SecondByteOk(lead, b) {
      if (b === undefined) return true;
      const lower = lead === 0xe0 ? 0xa0 : lead === 0xf0 ? 0x90 : 0x80;
      const upper = lead === 0xed ? 0x9f : lead === 0xf4 ? 0x8f : 0xbf;
      return b >= lower && b <= upper;
    }

    function utf8CheckIncomplete(self, buf, i) {
      let j = buf.length - 1;
      if (j < i) return 0;
      let nb = utf8CheckByte(buf[j]);
      if (nb >= 0) {
        if (nb > 0) self.lastNeed = nb - 1;
        return nb;
      }
      if (--j < i || nb === -2) return 0;
      nb = utf8CheckByte(buf[j]);
      if (nb >= 0) {
        if (nb > 0) {
          if (!utf8SecondByteOk(buf[j], buf[j + 1])) return 0;
          self.lastNeed = nb - 2;
        }
        return nb;
      }
      if (--j < i || nb === -2) return 0;
      nb = utf8CheckByte(buf[j]);
      if (nb >= 0) {
        if (nb > 0) {
          if (!utf8SecondByteOk(buf[j], buf[j + 1])) return 0;
          if (nb === 2) nb = 0;
          else self.lastNeed = nb - 3;
        }
        return nb;
      }
      return 0;
    }

    // ---- utf16le helpers ----
    function utf16Text(buf, i) {
      if ((buf.length - i) % 2 === 0) {
        const r = buf.toString("utf16le", i);
        if (r) {
          const c = r.charCodeAt(r.length - 1);
          if (c >= 0xd800 && c <= 0xdbff) {
            this.lastNeed = 2;
            this.lastTotal = 4;
            this.lastChar[0] = buf[buf.length - 2];
            this.lastChar[1] = buf[buf.length - 1];
            return r.slice(0, -1);
          }
        }
        return r;
      }
      this.lastNeed = 1;
      this.lastTotal = 2;
      this.lastChar[0] = buf[buf.length - 1];
      return buf.toString("utf16le", i, buf.length - 1);
    }

    function utf16End(buf) {
      const r = buf && buf.length ? this.write(buf) : "";
      if (this.lastNeed) {
        const end = this.lastTotal - this.lastNeed;
        const out = this.lastChar.toString("utf16le", 0, end);
        this.lastNeed = 0;
        this.lastTotal = 0;
        return r + out;
      }
      return r;
    }

    // ---- base64 helpers ----
    function base64Text(buf, i) {
      const n = (buf.length - i) % 3;
      if (n === 0) return buf.toString(this.encoding, i);
      this.lastNeed = 3 - n;
      this.lastTotal = 3;
      if (n === 1) {
        this.lastChar[0] = buf[buf.length - 1];
      } else {
        this.lastChar[0] = buf[buf.length - 2];
        this.lastChar[1] = buf[buf.length - 1];
      }
      return buf.toString(this.encoding, i, buf.length - n);
    }

    function base64End(buf) {
      const r = buf && buf.length ? this.write(buf) : "";
      if (this.lastNeed) {
        const out = this.lastChar.toString(this.encoding, 0, 3 - this.lastNeed);
        this.lastNeed = 0;
        this.lastTotal = 0;
        return r + out;
      }
      return r;
    }

    // ---- simple (latin1/ascii/hex) ----
    function simpleWrite(buf) {
      return buf.toString(this.encoding);
    }
    function simpleEnd(buf) {
      return buf && buf.length ? this.write(buf) : "";
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
      // Un-forgeable brand. `_href` is an ordinary property, so nothing about
      // a URL was distinguishable from a duck-typed copy -- and the legacy
      // url.parse() result carries href/protocol/path, so every href-shaped
      // object read as a URL. A private field can only exist on an object this
      // constructor produced, and `#brand in value` asks that without throwing.
      #brand = true;
      static [Symbol.for("oam.isURL")](value) {
        return value !== null && typeof value === "object" && #brand in value;
      }
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

    URL.revokeObjectURL = function revokeObjectURL() {
      if (arguments.length === 0) {
        throw new codes.ERR_MISSING_ARGS("id");
      }
    };

    globalThis.URL = URL;
    globalThis.URLSearchParams = URLSearchParams;
  }

  // ------------------------------------------------------------- node:url
  registry.factories.url = (natives) => {
    const isWin = natives.platform === "win32";

    function fileURLToPath(input, options) {
      if (typeof input !== "string" && !(input instanceof globalThis.URL)) {
        throw new codes.ERR_INVALID_ARG_TYPE("path", ["string", "URL"], input);
      }
      const url = typeof input === "string" ? new globalThis.URL(input) : input;
      if (url.protocol !== "file:") {
        throw makeNodeError(
          "ERR_INVALID_URL_SCHEME",
          "The URL must be of scheme file",
        );
      }
      // options.windows forces win32/posix semantics regardless of host
      // (Node v22: fileURLToPath(path, { windows }), mirroring
      // pathToFileURL). `null` is explicitly allowed and means host default.
      const win =
        options != null && typeof options === "object" && options.windows !== undefined
          ? !!options.windows
          : isWin;

      if (win) {
        // Encoded separators would let a URL smuggle path segments past
        // consumers. Windows rejects BOTH, since '\' is a separator there.
        if (/%2f|%5c/i.test(url.pathname)) {
          const e = makeNodeError(
            "ERR_INVALID_FILE_URL_PATH",
            "File URL path must not include encoded \\ or / characters",
          );
          e.input = url;
          throw e;
        }
        let pathname = decodeURIComponent(url.pathname).replaceAll("/", "\\");
        if (url.hostname) {
          // file://server/share -> \\server\share
          return `\\\\${url.hostname}${pathname}`;
        }
        if (!/^\\[A-Za-z]:/.test(pathname)) {
          // A drive-less path would silently resolve against the cwd's
          // drive â€” fail loud like Node.
          const e = makeNodeError(
            "ERR_INVALID_FILE_URL_PATH",
            "File URL path must be absolute",
          );
          e.input = url;
          throw e;
        }
        return pathname.slice(1); // strip the slash before the drive letter
      }

      // POSIX: only an encoded '/' is rejected. A BACKSLASH IS A LEGAL
      // FILENAME CHARACTER here, so rejecting %5C (the Windows rule)
      // made 'file:///foo%5Cbar' -- a real, addressable file -- unopenable.
      if (/%2f/i.test(url.pathname)) {
        const e = makeNodeError(
          "ERR_INVALID_FILE_URL_PATH",
          "File URL path must not include encoded / characters",
        );
        e.input = url;
        throw e;
      }
      // A host is meaningless for a POSIX file path (no UNC), so Node
      // refuses rather than silently dropping it and returning a path
      // that points somewhere else entirely.
      if (url.hostname) {
        const e = makeNodeError(
          "ERR_INVALID_FILE_URL_HOST",
          `File URL host must be "localhost" or empty on ${
            natives && natives.platform ? natives.platform : "posix"
          }`,
        );
        e.input = url;
        throw e;
      }
      return decodeURIComponent(url.pathname);
    }

    function pathToFileURL(path, options) {
      if (typeof path !== "string") {
        throw new codes.ERR_INVALID_ARG_TYPE("path", "string", path);
      }
      // options.windows forces win32/posix path semantics regardless of host
      // (Node v22: pathToFileURL(path, { windows })). Default = host platform.
      const win = options != null && typeof options === "object" && options.windows !== undefined
        ? !!options.windows
        : isWin;
      const pathModule = registry.get("path");
      const pm = win ? pathModule.win32 : pathModule.posix;
      let p = path;
      if (win) {
        // Reject a malformed UNC prefix (Node throws ERR_INVALID_ARG_VALUE for
        // a leading '\\' run with no server, or a missing share).
        if (/^\\\\\\/.test(p) || /^\\\\[^\\]+$/.test(p)) {
          throw new codes.ERR_INVALID_ARG_VALUE(
            "path",
            path,
            "Missing UNC resource path",
          );
        }
        // \\?\ device paths: resolve the prefix away (Node parity);
        // \\?\UNC\server\share is the long form of \\server\share.
        if (p.startsWith("\\\\?\\UNC\\")) p = "\\\\" + p.slice(8);
        else if (p.startsWith("\\\\?\\")) p = p.slice(4);
      }
      const trailingSep = (win ? /[\\/]$/ : /\/$/).test(p) && p.length > 1;
      p = pm.resolve(p); // relative paths anchor at cwd (Node parity)
      if (win) p = p.replaceAll("\\", "/"); // win: '\' is a separator
      if (trailingSep && !p.endsWith("/")) p += "/";
      // Percent-encode the URL-special characters paths may carry ('%'
      // FIRST â€” later substitutions insert literal % sequences).
      let encoded = p
        .replaceAll("%", "%25")
        // C0 control chars (incl \b \t \n \r): the URL constructor STRIPS raw
        // tab/newline, so pre-encode every 0x00-0x1F (Node's encodePathChars).
        .replace(/[\x00-\x1f]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase().padStart(2, "0"))
        .replaceAll("#", "%23")
        .replaceAll("?", "%3F")
        .replaceAll(" ", "%20")
        .replaceAll("~", "%7E")
        .replaceAll("^", "%5E")
        // [ ] | are left raw by the URL path parser; Node pre-encodes them.
        .replaceAll("[", "%5B")
        .replaceAll("]", "%5D")
        .replaceAll("|", "%7C");
      // posix: '\' is a literal path character, not a separator -> encode it.
      if (!win) encoded = encoded.replaceAll("\\", "%5C");
      if (win && encoded.startsWith("//")) {
        // UNC: \\server\share -> file://server/share
        return new globalThis.URL("file:" + encoded);
      }
      return new globalThis.URL("file://" + (encoded.startsWith("/") ? "" : "/") + encoded);
    }

    // Legacy url.format: a WHATWG URL stringifies to .href; a plain object is
    // assembled from its components; a string is reparsed via the WHATWG URL.
    // Non-string/non-object input throws ERR_INVALID_ARG_TYPE.
    function legacyFormat(urlObject, options) {
      if (typeof urlObject === "string") {
        // Node's legacy url.format(string) parses with the LEGACY parser and
        // formats with the LEGACY Url.format -- this round-trips slash-exactly
        // (e.g. 'fred:///s' stays 'fred:///s'), unlike the WHATWG URL which
        // normalizes. (The WHATWG-URL-instance path below is unchanged.)
        if (urlObject === "") return "";
        return urlParse(urlObject, false, false).format();
      } else if (urlObject instanceof Url) {
        // A legacy Url instance (e.g. a resolveObject result) formats via its
        // own faithful method, not the plain-object hand-assembly below.
        return urlObject.format();
      } else if (urlObject instanceof globalThis.URL) {
        // fall through to WHATWG serializer below
      } else if (urlObject === null || typeof urlObject !== "object") {
        throw new codes.ERR_INVALID_ARG_TYPE(
          "urlObject",
          ["object", "string"],
          urlObject,
        );
      }
      if (urlObject instanceof globalThis.URL) {
        const o = options || {};
        const auth = o.auth !== false;
        const fragment = o.fragment !== false;
        const search = o.search !== false;
        let ret = "";
        ret += urlObject.protocol;
        if (urlObject.host) {
          ret += "//";
          if (auth && (urlObject.username || urlObject.password)) {
            ret += urlObject.username;
            if (urlObject.password) ret += `:${urlObject.password}`;
            ret += "@";
          }
          ret += urlObject.host;
        } else if (urlObject.protocol === "file:") {
          ret += "//";
        }
        ret += urlObject.pathname;
        if (search) ret += urlObject.search;
        if (fragment) ret += urlObject.hash;
        return ret;
      }
      // Legacy object form: assemble from components (subset Node supports).
      let result = "";
      let protocol = urlObject.protocol || "";
      if (protocol && !protocol.endsWith(":")) protocol += ":";
      result += protocol;
      let host = "";
      if (urlObject.host) host = urlObject.host;
      else if (urlObject.hostname) {
        host =
          urlObject.hostname.includes(":") && urlObject.hostname[0] !== "["
            ? `[${urlObject.hostname}]`
            : urlObject.hostname;
        if (urlObject.port) host += `:${urlObject.port}`;
      }
      let auth = "";
      if (urlObject.auth) auth = urlObject.auth;
      if (host || (protocol && protocol !== "" && urlObject.slashes !== false && host)) {
        result += "//";
      }
      if (host) {
        if (auth) result += `${auth}@`;
        result += host;
      }
      let pathname = urlObject.pathname || "";
      if (pathname && pathname[0] !== "/" && host) pathname = `/${pathname}`;
      result += pathname;
      let query = "";
      if (urlObject.search) query = urlObject.search;
      else if (urlObject.query && typeof urlObject.query === "object") {
        const sp = new globalThis.URLSearchParams(urlObject.query);
        const s = sp.toString();
        if (s) query = `?${s}`;
      } else if (typeof urlObject.query === "string" && urlObject.query) {
        query = `?${urlObject.query}`;
      }
      if (query) result += query[0] === "?" ? query : `?${query}`;
      let hash = urlObject.hash || "";
      if (hash && hash[0] !== "#") hash = `#${hash}`;
      result += hash;
      return result;
    }

    function urlToHttpOptions(url) {
      if (url === null || typeof url !== "object") {
        throw new codes.ERR_INVALID_ARG_TYPE("url", "object", url);
      }
      const { hostname, pathname, port, username, password, search } = url;
      const options = {
        __proto__: null,
        ...url, // preserve any user-added properties on the url object
        protocol: url.protocol,
        hostname:
          hostname && hostname[0] === "["
            ? hostname.slice(1, -1) // IPv6: net/dns want it bracket-free
            : hostname,
        hash: url.hash,
        search,
        pathname,
        path: `${pathname || ""}${search || ""}`,
        href: url.href,
      };
      if (port !== "") options.port = Number(port);
      if (username || password) {
        options.auth = `${decodeURIComponent(username)}:${decodeURIComponent(password)}`;
      }
      return options;
    }

    // ===== Legacy (pre-WHATWG) URL API: faithful port of Node v22 lib/url.js =====
    const querystring = registry.get("querystring");
    const protocolPattern = /^[a-z0-9.+-]+:/i;
    const portPattern = /:[0-9]*$/;
    const hostPattern = /^\/\/[^@/]+@[^@/]+/;
    const simplePathPattern = /^(\/\/?(?!\/)[^?\s]*)(\?[^\s]*)?$/;
    const hostnameMaxLen = 255;
    const unsafeProtocol = new Set(["javascript", "javascript:"]);
    const hostlessProtocol = new Set(["javascript", "javascript:"]);
    const slashedProtocol = new Set([
      "http", "http:", "https", "https:", "ftp", "ftp:", "gopher", "gopher:",
      "file", "file:", "ws", "ws:", "wss", "wss:",
    ]);
    // chars
    const C_TAB = 9, C_LF = 10, C_FF = 12, C_CR = 13, C_SPACE = 32, C_DQUOTE = 34,
      C_HASH = 35, C_PERCENT = 37, C_SQUOTE = 39, C_FSLASH = 47, C_QUESTION = 63,
      C_AT = 64, C_BSLASH = 92, C_CARET = 94, C_GRAVE = 96, C_LCURLY = 123,
      C_PIPE = 124, C_RCURLY = 125, C_SEMI = 59, C_LT = 60, C_GT = 62,
      C_NBSP = 160, C_ZWNBSP = 65279, C_COLON = 58;
    // autoEscape set: chars in the path that must be percent-encoded.
    const autoEscapeMap = {
      "\t": "%09", "\n": "%0A", "\r": "%0D", " ": "%20", '"': "%22",
      "'": "%27", "<": "%3C", ">": "%3E", "`": "%60",
    };
    function autoEscapeStr(rest) {
      let out = "";
      for (let i = 0; i < rest.length; i++) {
        const c = rest[i];
        const esc = autoEscapeMap[c];
        out += esc !== undefined ? esc : c;
      }
      return out;
    }
    function isIpv6Hostname(hostname) {
      return hostname.charCodeAt(0) === 91 /* [ */ &&
        hostname.charCodeAt(hostname.length - 1) === 93 /* ] */;
    }

    function Url() {
      this.protocol = null;
      this.slashes = null;
      this.auth = null;
      this.host = null;
      this.port = null;
      this.hostname = null;
      this.hash = null;
      this.search = null;
      this.query = null;
      this.pathname = null;
      this.path = null;
      this.href = null;
    }

    Url.prototype.parse = function (url, parseQueryString, slashesDenoteHost) {
      if (typeof url !== "string") {
        throw new codes.ERR_INVALID_ARG_TYPE("url", "string", url);
      }
      let hasHash = false;
      let start = -1;
      let end = -1;
      let rest = "";
      let lastPos = 0;
      for (let i = 0, inWs = false, split = false; i < url.length; ++i) {
        const code = url.charCodeAt(i);
        const isWs = code === C_SPACE || code === C_TAB || code === C_CR ||
          code === C_LF || code === C_FF || code === C_NBSP || code === C_ZWNBSP;
        if (start === -1) {
          if (isWs) continue;
          lastPos = start = i;
        } else if (inWs) {
          if (!isWs) { end = -1; inWs = false; }
        } else if (isWs) {
          end = i;
          inWs = true;
        }
        if (!split) {
          switch (code) {
            case C_HASH:
              hasHash = true;
            // falls through
            case C_QUESTION:
              split = true;
              break;
            case C_BSLASH:
              if (i - lastPos > 0) rest += url.slice(lastPos, i);
              rest += "/";
              lastPos = i + 1;
              break;
          }
        } else if (!hasHash && code === C_HASH) {
          hasHash = true;
        }
      }
      if (start !== -1) {
        if (lastPos === start) {
          if (end === -1) {
            rest = start === 0 ? url : url.slice(start);
          } else {
            rest = url.slice(start, end);
          }
        } else if (end === -1 && lastPos < url.length) {
          rest += url.slice(lastPos);
        } else if (end !== -1 && lastPos < end) {
          rest += url.slice(lastPos, end);
        }
      }

      if (!slashesDenoteHost && !hasHash) {
        const simplePath = simplePathPattern.exec(rest);
        if (simplePath) {
          this.path = rest;
          this.href = rest;
          this.pathname = simplePath[1];
          if (simplePath[2]) {
            this.search = simplePath[2];
            this.query = parseQueryString
              ? querystring.parse(this.search.slice(1))
              : this.search.slice(1);
          } else if (parseQueryString) {
            this.search = null;
            this.query = { __proto__: null };
          }
          return this;
        }
      }

      let proto = protocolPattern.exec(rest);
      let lowerProto;
      if (proto) {
        proto = proto[0];
        lowerProto = proto.toLowerCase();
        this.protocol = lowerProto;
        rest = rest.slice(proto.length);
      }

      let slashes;
      if (slashesDenoteHost || proto || hostPattern.test(rest)) {
        slashes = rest.charCodeAt(0) === C_FSLASH && rest.charCodeAt(1) === C_FSLASH;
        if (slashes && !(proto && hostlessProtocol.has(lowerProto))) {
          rest = rest.slice(2);
          this.slashes = true;
        }
      }

      if (!hostlessProtocol.has(lowerProto) &&
        (slashes || (proto && !slashedProtocol.has(proto)))) {
        let hostEnd = -1;
        let atSign = -1;
        let nonHost = -1;
        for (let i = 0; i < rest.length; ++i) {
          switch (rest.charCodeAt(i)) {
            case C_TAB: case C_LF: case C_CR: case C_SPACE: case C_DQUOTE:
            case C_PERCENT: case C_SQUOTE: case C_SEMI: case C_LT: case C_GT:
            case C_BSLASH: case C_CARET: case C_GRAVE: case C_LCURLY:
            case C_PIPE: case C_RCURLY:
              if (nonHost === -1) nonHost = i;
              break;
            case C_HASH: case C_FSLASH: case C_QUESTION:
              if (hostEnd === -1) hostEnd = i;
              break;
            case C_AT:
              atSign = i;
              nonHost = -1;
              break;
          }
          if (hostEnd !== -1) break;
        }
        start = 0;
        if (atSign !== -1) {
          this.auth = decodeURIComponent(rest.slice(0, atSign));
          start = atSign + 1;
        }
        if (nonHost === -1) {
          // No forbidden char in the host region: the host ends at the first
          // host-ending char (hostEnd = first / ? #), or extends to the end of
          // `rest` if there is none. (The loop broke at hostEnd, so anything
          // past it is the path/query/hash and must stay in `rest`.)
          if (hostEnd === -1) {
            this.host = rest.slice(start);
            rest = "";
          } else {
            this.host = rest.slice(start, hostEnd);
            rest = rest.slice(hostEnd);
          }
        } else {
          this.host = rest.slice(start, nonHost);
          rest = rest.slice(nonHost);
        }
        this.parseHost();
        if (typeof this.hostname !== "string") this.hostname = "";
        const hostname = this.hostname;
        const ipv6Hostname = isIpv6Hostname(hostname);
        // Host validation, matched to the LIVE node v22.22.2 binary
        // (battery-probed; the published lib/url.js diverges, same story as
        // node_dotenv.cc). Rules:
        // 1. A bracket in a NON-ipv6 hostname is a hard ERR_INVALID_URL
        //    (spoofing: could make a non-ipv6 host read as ipv6) -- checked
        //    BEFORE truncation, or the evidence is gone ('a[b].com' throws).
        // 2. A well-formed ipv6 [..] hostname throws if forbidden chars sit
        //    inside the brackets ('[127.0.0.1 c8763]' throws, '[]' is fine).
        // 3. Everything else truncates LENIENTLY at the first invalid char,
        //    remainder to path ('evil.com:.example.com' -> path
        //    '/:.example.com'), with a strict post-truncation backstop.
        const throwInvalidUrl = () => {
          const e = new codes.ERR_INVALID_URL();
          e.input = url;
          throw e;
        };
        if (!ipv6Hostname) {
          if (/[[\]]/.test(this.hostname)) throwInvalidUrl();
          let cut = -1;
          for (let i = 0; i < this.hostname.length; i++) {
            const c = this.hostname.charCodeAt(i);
            const valid = (c >= 97 && c <= 122) || (c >= 65 && c <= 90) ||
              (c >= 48 && c <= 57) || c === 46 || c === 45 || c === 43 ||
              c === 95 || c > 127;
            if (!valid) { cut = i; break; }
          }
          if (cut !== -1) {
            rest = "/" + this.hostname.slice(cut) + rest;
            this.hostname = this.hostname.slice(0, cut);
          }
        }
        if (this.hostname.length > hostnameMaxLen) {
          this.hostname = "";
        } else {
          this.hostname = this.hostname.toLowerCase();
        }
        if (this.hostname !== "") {
          if (ipv6Hostname) {
            if (/[\0\t\n\r #%/<>?@\\^|]/.test(this.hostname)) throwInvalidUrl();
          } else if (/[\0\t\n\r #%/:<>?@[\\\]^|]/.test(this.hostname)) {
            throwInvalidUrl();
          }
        }
        const pp = this.port ? ":" + this.port : "";
        const h = this.hostname || "";
        this.host = h + pp;
        if (ipv6Hostname) {
          this.hostname = this.hostname.slice(1, -1);
          if (rest[0] !== "/") rest = "/" + rest;
        }
      }

      if (!unsafeProtocol.has(lowerProto)) {
        rest = autoEscapeStr(rest);
      }

      let questionIdx = -1;
      let hashIdx = -1;
      for (let i = 0; i < rest.length; ++i) {
        const code = rest.charCodeAt(i);
        if (code === C_HASH) {
          this.hash = rest.slice(i);
          hashIdx = i;
          break;
        } else if (code === C_QUESTION && questionIdx === -1) {
          questionIdx = i;
        }
      }

      if (questionIdx !== -1) {
        if (hashIdx === -1) {
          this.search = rest.slice(questionIdx);
          this.query = rest.slice(questionIdx + 1);
        } else {
          this.search = rest.slice(questionIdx, hashIdx);
          this.query = rest.slice(questionIdx + 1, hashIdx);
        }
        if (parseQueryString) this.query = querystring.parse(this.query);
      } else if (parseQueryString) {
        this.search = null;
        this.query = { __proto__: null };
      }

      const useQuestionIdx = questionIdx !== -1 && (hashIdx === -1 || questionIdx < hashIdx);
      const firstIdx = useQuestionIdx ? questionIdx : hashIdx;
      if (firstIdx === -1) {
        if (rest.length > 0) this.pathname = rest;
      } else if (firstIdx > 0) {
        this.pathname = rest.slice(0, firstIdx);
      }
      if (slashedProtocol.has(lowerProto) && this.hostname && !this.pathname) {
        this.pathname = "/";
      }
      if (this.pathname || this.search) {
        this.path = (this.pathname || "") + (this.search || "");
      }
      this.href = this.format();
      return this;
    };

    Url.prototype.parseHost = function () {
      let host = this.host;
      let port = portPattern.exec(host);
      if (port) {
        port = port[0];
        if (port !== ":") this.port = port.slice(1);
        host = host.slice(0, host.length - port.length);
      }
      if (host) this.hostname = host;
    };

    Url.prototype.format = function () {
      let auth = this.auth || "";
      if (auth) {
        auth = encodeURIComponent(auth).replace(/%3A/gi, ":");
        auth += "@";
      }
      let protocol = this.protocol || "";
      let pathname = this.pathname || "";
      let hash = this.hash || "";
      let host = "";
      let query = "";
      if (this.host) {
        host = auth + this.host;
      } else if (this.hostname) {
        host = auth + (this.hostname.includes(":") && !isIpv6Hostname(this.hostname)
          ? "[" + this.hostname + "]"
          : this.hostname);
        if (this.port) host += ":" + this.port;
      }
      if (this.query !== null && typeof this.query === "object") {
        query = querystring.stringify(this.query);
      }
      let search = this.search || (query && "?" + query) || "";
      if (protocol && protocol.charCodeAt(protocol.length - 1) !== C_COLON) protocol += ":";
      let newPathname = "";
      let lastPos = 0;
      for (let i = 0; i < pathname.length; ++i) {
        switch (pathname.charCodeAt(i)) {
          case C_HASH:
            if (i - lastPos > 0) newPathname += pathname.slice(lastPos, i);
            newPathname += "%23";
            lastPos = i + 1;
            break;
          case C_QUESTION:
            if (i - lastPos > 0) newPathname += pathname.slice(lastPos, i);
            newPathname += "%3F";
            lastPos = i + 1;
            break;
        }
      }
      if (lastPos > 0) {
        pathname = lastPos !== pathname.length
          ? newPathname + pathname.slice(lastPos)
          : newPathname;
      }
      if (this.slashes || slashedProtocol.has(protocol)) {
        if (this.slashes || host) {
          if (pathname && pathname.charCodeAt(0) !== C_FSLASH) pathname = "/" + pathname;
          host = "//" + host;
        } else if (protocol === "file:") {
          host = "//";
        }
      }
      search = search.replace(/#/g, "%23");
      if (hash && hash.charCodeAt(0) !== C_HASH) hash = "#" + hash;
      if (search && search.charCodeAt(0) !== C_QUESTION) search = "?" + search;
      return protocol + host + pathname + search + hash;
    };

    Url.prototype.resolve = function (relative) {
      return this.resolveObject(urlParse(relative, false, true)).format();
    };

    Url.prototype.resolveObject = function (relative) {
      if (typeof relative === "string") {
        const rel = new Url();
        rel.parse(relative, false, true);
        relative = rel;
      }
      const result = new Url();
      const tkeys = Object.keys(this);
      for (let tk = 0; tk < tkeys.length; tk++) {
        const tkey = tkeys[tk];
        result[tkey] = this[tkey];
      }
      result.hash = relative.hash;
      if (relative.href === "") {
        result.href = result.format();
        return result;
      }
      if (relative.slashes && !relative.protocol) {
        const rkeys = Object.keys(relative);
        for (let rk = 0; rk < rkeys.length; rk++) {
          const rkey = rkeys[rk];
          if (rkey !== "protocol") result[rkey] = relative[rkey];
        }
        if (slashedProtocol.has(result.protocol) && result.hostname && !result.pathname) {
          result.path = result.pathname = "/";
        }
        result.href = result.format();
        return result;
      }

      if (relative.protocol && relative.protocol !== result.protocol) {
        if (!slashedProtocol.has(relative.protocol)) {
          const keys = Object.keys(relative);
          for (let v = 0; v < keys.length; v++) {
            const k = keys[v];
            result[k] = relative[k];
          }
          result.href = result.format();
          return result;
        }
        result.protocol = relative.protocol;
        if (!relative.host && !/^file:?$/.test(relative.protocol) && !hostlessProtocol.has(relative.protocol)) {
          const relPath = (relative.pathname || "").split("/");
          while (relPath.length && !(relative.host = relPath.shift()));
          if (!relative.host) relative.host = "";
          if (!relative.hostname) relative.hostname = "";
          if (relPath[0] !== "") relPath.unshift("");
          if (relPath.length < 2) relPath.unshift("");
          result.pathname = relPath.join("/");
        } else {
          result.pathname = relative.pathname;
        }
        result.search = relative.search;
        result.query = relative.query;
        result.host = relative.host || "";
        result.auth = relative.auth;
        result.hostname = relative.hostname || relative.host;
        result.port = relative.port;
        if (result.pathname || result.search) {
          result.path = (result.pathname || "") + (result.search || "");
        }
        result.slashes = result.slashes || relative.slashes;
        result.href = result.format();
        return result;
      }

      const isSourceAbs = result.pathname && result.pathname.charAt(0) === "/";
      const isRelAbs = relative.host || (relative.pathname && relative.pathname.charAt(0) === "/");
      let mustEndAbs = isRelAbs || isSourceAbs || (result.host && relative.pathname);
      const removeAllDots = mustEndAbs;
      let srcPath = (result.pathname && result.pathname.split("/")) || [];
      const relPath = (relative.pathname && relative.pathname.split("/")) || [];
      const noLeadingSlashes = result.protocol && !slashedProtocol.has(result.protocol);

      if (noLeadingSlashes) {
        result.hostname = "";
        result.port = null;
        if (result.host) {
          if (srcPath[0] === "") srcPath[0] = result.host;
          else srcPath.unshift(result.host);
        }
        result.host = "";
        if (relative.protocol) {
          relative.hostname = null;
          relative.port = null;
          result.auth = null;
          if (relative.host) {
            if (relPath[0] === "") relPath[0] = relative.host;
            else relPath.unshift(relative.host);
          }
          relative.host = null;
        }
        mustEndAbs = mustEndAbs && (relPath[0] === "" || srcPath[0] === "");
      }

      if (isRelAbs) {
        // Take host/auth/hostname/port from the relative ONLY when it actually
        // carries a host; otherwise the source's host+hostname are kept (a
        // relative absolute PATH like '/p/a/t/h' must not null out the host).
        if (relative.host || relative.host === "") {
          if (result.host !== relative.host) result.auth = null;
          result.host = relative.host;
          result.port = relative.port;
          result.hostname = relative.hostname;
        }
        result.search = relative.search;
        result.query = relative.query;
        srcPath = relPath;
      } else if (relPath.length) {
        if (!srcPath) srcPath = [];
        srcPath.pop();
        srcPath = srcPath.concat(relPath);
        result.search = relative.search;
        result.query = relative.query;
      } else if (relative.search !== null && relative.search !== undefined) {
        if (noLeadingSlashes) {
          result.hostname = result.host = srcPath.shift();
          const authInHost = result.host && result.host.indexOf("@") > 0 && result.host.split("@");
          if (authInHost) {
            result.auth = authInHost.shift();
            result.host = result.hostname = authInHost.shift();
          }
        }
        result.search = relative.search;
        result.query = relative.query;
        if (result.pathname !== null || result.search !== null) {
          result.path = (result.pathname ? result.pathname : "") + (result.search ? result.search : "");
        }
        result.href = result.format();
        return result;
      }

      if (!srcPath.length) {
        result.pathname = null;
        result.path = result.search ? "/" + result.search : null;
        result.href = result.format();
        return result;
      }

      let last = srcPath.slice(-1)[0];
      const hasTrailingSlash =
        ((result.host || relative.host || srcPath.length > 1) &&
          (last === "." || last === "..")) || last === "";

      let up = 0;
      for (let i = srcPath.length - 1; i >= 0; i--) {
        last = srcPath[i];
        if (last === ".") {
          srcPath.splice(i, 1);
        } else if (last === "..") {
          srcPath.splice(i, 1);
          up++;
        } else if (up) {
          srcPath.splice(i, 1);
          up--;
        }
      }

      if (!mustEndAbs && !removeAllDots) {
        while (up--) srcPath.unshift("..");
      }

      if (mustEndAbs && srcPath[0] !== "" && (!srcPath[0] || srcPath[0].charAt(0) !== "/")) {
        srcPath.unshift("");
      }

      if (hasTrailingSlash && srcPath.join("/").slice(-1) !== "/") {
        srcPath.push("");
      }

      const isAbsolute = srcPath[0] === "" || (srcPath[0] && srcPath[0].charAt(0) === "/");

      if (noLeadingSlashes) {
        result.hostname = result.host = isAbsolute ? "" : srcPath.length ? srcPath.shift() : "";
        const authInHost = result.host && result.host.indexOf("@") > 0 && result.host.split("@");
        if (authInHost) {
          result.auth = authInHost.shift();
          result.host = result.hostname = authInHost.shift();
        }
      }

      mustEndAbs = mustEndAbs || (result.host && srcPath.length);

      if (mustEndAbs && !isAbsolute) {
        srcPath.unshift("");
      }

      if (!srcPath.length) {
        result.pathname = null;
        result.path = null;
      } else {
        result.pathname = srcPath.join("/");
      }

      if (result.pathname !== null || result.search !== null) {
        result.path = (result.pathname ? result.pathname : "") + (result.search ? result.search : "");
      }
      result.auth = relative.auth || result.auth;
      result.slashes = result.slashes || relative.slashes;
      result.href = result.format();
      return result;
    };

    let dep0170Warned = false;
    function urlParse(url, parseQueryString, slashesDenoteHost) {
      if (url instanceof Url) return url;
      const u = new Url();
      u.parse(url, parseQueryString, slashesDenoteHost);
      // DEP0170: the legacy parser accepted a string WHATWG rejects.
      // Warn once per process (node dedupes deprecation warnings by code).
      if (
        !dep0170Warned &&
        typeof url === "string" &&
        typeof globalThis.URL?.canParse === "function" &&
        !globalThis.URL.canParse(url)
      ) {
        dep0170Warned = true;
        process.emitWarning(
          `The URL ${url} is invalid. Future versions of Node.js will throw an error.`,
          "DeprecationWarning",
          "DEP0170",
        );
      }
      return u;
    }
    function urlResolve(source, relative) {
      return urlParse(source, false, true).resolve(relative);
    }
    function urlResolveObject(source, relative) {
      if (!source) return relative;
      return urlParse(source, false, true).resolveObject(relative);
    }

    return {
      URL: globalThis.URL,
      URLSearchParams: globalThis.URLSearchParams,
      fileURLToPath,
      pathToFileURL,
      urlToHttpOptions,
      format: legacyFormat,
      Url,
      parse: urlParse,
      resolve: urlResolve,
      resolveObject: urlResolveObject,
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
          if (input.format === "der") {
            // DER bytes: wrap as PEM so the native layer (which parses PEM) and
            // detectKeyType see real ASN.1, not the bytes mis-decoded as UTF-8 text.
            var privLabel = input.type === "sec1" ? "EC PRIVATE KEY"
              : input.type === "pkcs1" ? "RSA PRIVATE KEY"
              : "PRIVATE KEY";
            pem = derToPem(input.key, privLabel);
          } else {
            pem = new TextDecoder().decode(input.key);
          }
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
        if (options && options.format === "der") return BufferCtor.from(pemToDer(pem));
        return pem;
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
          if (input.format === "der") {
            var pubLabel = input.type === "pkcs1" ? "RSA PUBLIC KEY" : "PUBLIC KEY";
            pem = derToPem(input.key, pubLabel);
          } else {
            pem = new TextDecoder().decode(input.key);
          }
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
        if (options && options.format === "der") return BufferCtor.from(pemToDer(pem));
        return pem;
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
          sig = asBuffer(natives.cryptoSign(this._algorithm == null ? "" : this._algorithm, merged, pem, keyType));
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
        return natives.cryptoVerify(this._algorithm == null ? "" : this._algorithm, merged, pem, sigBuf, keyType);
      }
      _transform(chunk, encoding, callback) {
        this.update(chunk, encoding);
        callback();
      }
    }

    function createSign(algorithm) { return new Sign(normalizeAlgo(algorithm)); }
    function createVerify(algorithm) { return new Verify(normalizeAlgo(algorithm)); }

    function normalizeAlgo(raw) {
      // Ed25519/Ed448 pass a null algorithm to sign/verify (the digest is implied
      // by the key). Node accepts crypto.sign(null, data, ed25519Key); guard so we
      // never call .toUpperCase() on null.
      if (raw === null || raw === undefined) return null;
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
      BufferCtor.from(bytes.buffer, bytes.byteOffset, bytes.length);

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
      // Ed25519 with no encoding options -> KeyObjects (Node parity). The relay
      // token flow calls generateKeyPairSync('ed25519').privateKey.export({...}),
      // which requires KeyObjects, not PEM strings. Scoped to ed25519 so the
      // established RSA/EC string returns (and their e2e tests) stay unchanged.
      if (type === 'ed25519') {
        if (!(options && options.publicKeyEncoding)) pubOut = createPublicKey(result.publicKey);
        if (!(options && options.privateKeyEncoding)) privOut = createPrivateKey(result.privateKey);
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
    const asBuffer = (bytes) => BufferCtor.from(bytes.buffer, bytes.byteOffset, bytes.length);
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
    // Node's querystring.escape: a custom UTF-8 percent-encoder (NOT
    // encodeURIComponent) -- unreserved set is alphanum + !'()*-._~, and a
    // high surrogate pairs with the FOLLOWING code unit (lone trailing
    // surrogate -> ERR_INVALID_URI). Coercion is `+ ''` for non-objects so a
    // Symbol throws TypeError (unlike String(symbol)).
    const qsHexTable = [];
    for (let i = 0; i < 256; i++) {
      qsHexTable[i] = "%" + (i < 16 ? "0" : "") + i.toString(16).toUpperCase();
    }
    // 1 = leave unescaped. alphanum + ! ' ( ) * - . _ ~
    const qsNoEscape = new Int8Array([
      0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0-15
      0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 16-31
      0, 1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 1, 1, 0, // 32-47 (! ' ( ) * - .)
      1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, // 48-63 (0-9)
      0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 64-79 (A-O)
      1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 1, // 80-95 (P-Z _)
      0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 96-111 (a-o)
      1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, // 112-127 (p-z ~)
    ]);
    const qsInvalidUri = () => {
      const e = new URIError("URI malformed");
      e.code = "ERR_INVALID_URI";
      return e;
    };
    const escape = (str) => {
      if (typeof str !== "string") {
        if (typeof str === "object" && str !== null) str = String(str);
        else str += ""; // Symbol -> TypeError (matches Node), number -> "5"
      }
      const len = str.length;
      if (len === 0) return "";
      let out = "";
      let lastPos = 0;
      for (let i = 0; i < len; i++) {
        let c = str.charCodeAt(i);
        if (c < 0x80) {
          if (qsNoEscape[c] === 1) continue;
          if (lastPos < i) out += str.slice(lastPos, i);
          lastPos = i + 1;
          out += qsHexTable[c];
          continue;
        }
        if (lastPos < i) out += str.slice(lastPos, i);
        if (c < 0x800) {
          lastPos = i + 1;
          out += qsHexTable[0xc0 | (c >> 6)] + qsHexTable[0x80 | (c & 0x3f)];
          continue;
        }
        if (c < 0xd800 || c >= 0xe000) {
          lastPos = i + 1;
          out += qsHexTable[0xe0 | (c >> 12)] +
            qsHexTable[0x80 | ((c >> 6) & 0x3f)] +
            qsHexTable[0x80 | (c & 0x3f)];
          continue;
        }
        // surrogate: pair with the next code unit
        ++i;
        if (i >= len) throw qsInvalidUri();
        const c2 = str.charCodeAt(i) & 0x3ff;
        lastPos = i + 1;
        c = 0x10000 + (((c & 0x3ff) << 10) | c2);
        out += qsHexTable[0xf0 | (c >> 18)] +
          qsHexTable[0x80 | ((c >> 12) & 0x3f)] +
          qsHexTable[0x80 | ((c >> 6) & 0x3f)] +
          qsHexTable[0x80 | (c & 0x3f)];
      }
      if (lastPos === 0) return str;
      if (lastPos < len) return out + str.slice(lastPos);
      return out;
    };

    function parse(input, sep = "&", eq = "=", options = {}) {
      const out = Object.create(null);
      if (typeof input !== "string" || input.length === 0) return out;
      // Node honors maxKeys only when it is a NUMBER (Infinity/NaN included);
      // a non-number (e.g. the string 'Infinity') falls back to the 1000
      // default. A number > 0 caps; <=0 / NaN means unlimited.
      const maxKeys = typeof options.maxKeys === "number" ? options.maxKeys : 1000;
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
    // Node's internal AbortError shape: a real Error SUBCLASS (constructor
    // name 'AbortError') with code ABORT_ERR, signal.reason carried as
    // cause. timers/promises rejects with THIS, never with signal.reason.
    class AbortError extends Error {
      constructor(message = "The operation was aborted", options = undefined) {
        super(message, options);
        this.code = "ABORT_ERR";
        this.name = "AbortError";
      }
    }
    function makeAbortError(cause) {
      return cause !== undefined ? new AbortError(undefined, { cause }) : new AbortError();
    }
    // Node validates every argument BEFORE scheduling and surfaces the
    // failure as a REJECTION (these are async functions there), so a typo
    // like `{ signl: sig }` or a string delay fails loudly instead of
    // scheduling a timer nobody can cancel. Returns an Error to reject
    // with, or null when the arguments are fine.
    function validateTimerArgs(delay, options) {
      if (delay !== undefined && typeof delay !== "number") {
        return new codes.ERR_INVALID_ARG_TYPE("delay", "number", delay);
      }
      if (options === null || typeof options !== "object") {
        return new codes.ERR_INVALID_ARG_TYPE("options", "Object", options);
      }
      const { signal, ref } = options;
      if (
        signal !== undefined &&
        (signal === null || typeof signal !== "object" || !("aborted" in signal))
      ) {
        return new codes.ERR_INVALID_ARG_TYPE("options.signal", "AbortSignal", signal);
      }
      if (ref !== undefined && typeof ref !== "boolean") {
        return new codes.ERR_INVALID_ARG_TYPE("options.ref", "boolean", ref);
      }
      return null;
    }
    function promisedTimeout(delay, value, options = {}) {
      const invalid = validateTimerArgs(delay, options);
      if (invalid) return Promise.reject(invalid);
      const signal = options.signal;
      // Already-aborted: reject immediately, never schedule (Node).
      if (signal?.aborted) {
        return Promise.reject(makeAbortError(signal.reason));
      }
      return new Promise((resolve, reject) => {
        // The abort listener has to come OFF when the timer fires normally.
        // Leaving it attached meant a long-lived AbortSignal reused across
        // many waits accumulated one listener per call, each retaining this
        // promise's closure -- a leak that grows with the loop around it.
        let onAbort;
        const done = (fn, arg) => {
          if (onAbort) signal?.removeEventListener?.("abort", onAbort);
          fn(arg);
        };
        const id = globalThis.setTimeout(() => done(resolve, value), delay ?? 1);
        // ref:false means "do not hold the process open for this". Ignoring
        // it kept the event loop alive for the full delay, so a program
        // that asked for a non-blocking timer waited it out anyway.
        if (options.ref === false && typeof id?.unref === "function") id.unref();
        if (signal?.addEventListener) {
          onAbort = () => {
            globalThis.clearTimeout(id);
            done(reject, makeAbortError(signal.reason));
          };
          signal.addEventListener("abort", onAbort, { once: true });
        }
      });
    }
    function promisedImmediate(value, options = {}) {
      // setImmediate(value, { signal, ref }) is abortable in Node. oam
      // ignored options entirely, so an abort never fired and the caller's
      // signal did nothing at all.
      const invalid = validateTimerArgs(undefined, options);
      if (invalid) return Promise.reject(invalid);
      const signal = options.signal;
      if (signal?.aborted) {
        return Promise.reject(makeAbortError(signal.reason));
      }
      return new Promise((resolve, reject) => {
        let onAbort;
        const done = (fn, arg) => {
          if (onAbort) signal?.removeEventListener?.("abort", onAbort);
          fn(arg);
        };
        const id = globalThis.setImmediate(() => done(resolve, value));
        if (options.ref === false && typeof id?.unref === "function") id.unref();
        if (signal?.addEventListener) {
          onAbort = () => {
            globalThis.clearImmediate(id);
            done(reject, makeAbortError(signal.reason));
          };
          signal.addEventListener("abort", onAbort, { once: true });
        }
      });
    }
    async function* intervalIterator(delay, value, options = {}) {
      const invalid = validateTimerArgs(delay, options);
      if (invalid) throw invalid;
      const signal = options?.signal;
      if (signal?.aborted) {
        // makeAbortError carries the signal's REASON through as `cause`;
        // the bare Error here dropped it, so a caller that aborted with a
        // specific reason could not tell why it was aborted.
        throw makeAbortError(signal.reason);
      }
      // A REPEATING timer, counting ticks independently of how fast the
      // consumer drains them. Restarting a one-shot after each `yield`
      // silently drops every tick that lands while the consumer is working,
      // so a 10ms interval feeding a consumer that takes 40ms yielded once
      // per 50ms where node yields four -- the interval degraded into
      // "delay between iterations" instead of a rate.
      let ticks = 0;
      // Resolver for the pass that is waiting on the next tick, or null when
      // the consumer is busy and nothing is awaiting. ONE abort listener for
      // the whole iterator, not one per tick: the per-tick add/remove churned
      // the signal's listener list and left nothing attached between ticks.
      let notify = null;
      let timer = null;
      let abortListener = null;
      const wake = () => {
        if (notify) {
          const resolve = notify;
          notify = null;
          resolve();
        }
      };
      try {
        timer = globalThis.setInterval(() => {
          ticks++;
          wake();
        }, delay);
        // `ref: false` has to reach the real handle: the whole point is a
        // process that exits with the interval still pending.
        if (options?.ref === false) timer?.unref?.();
        if (signal?.addEventListener) {
          abortListener = () => {
            globalThis.clearInterval(timer);
            timer = null;
            // Ends the WAIT; the loop below re-reads the signal and stops.
            wake();
          };
          signal.addEventListener("abort", abortListener, { once: true });
        }
        while (!signal?.aborted) {
          if (ticks === 0) {
            await new Promise((resolve) => {
              notify = resolve;
            });
          }
          // Ticks already counted are still delivered after an abort -- the
          // abort stops the waiting, not the backlog. A consumer that aborts
          // from inside its own callback still receives the iterations that
          // had already come due, which is what node does and what callers
          // depend on to finish in-flight work.
          for (; ticks > 0; ticks--) {
            yield value;
          }
        }
        throw makeAbortError(signal.reason);
      } finally {
        if (timer !== null) {
          globalThis.clearInterval(timer);
        }
        // Optional call: validation accepts any duck-typed signal (anything
        // carrying `aborted`), which need not have the removal half.
        if (abortListener) {
          signal?.removeEventListener?.("abort", abortListener);
        }
      }
    }
    // Web-standard Scheduler: an instance of an un-constructible class
    // (new scheduler.constructor() must throw ERR_ILLEGAL_CONSTRUCTOR).
    // wait/yield live on the PROTOTYPE like node's class methods.
    class Scheduler {
      constructor() {
        throw applyNodeErrorShape(new TypeError("Illegal constructor"), "ERR_ILLEGAL_CONSTRUCTOR");
      }
      wait(delay, options) {
        return promisedTimeout(delay, undefined, options);
      }
      yield() {
        return promisedImmediate(undefined);
      }
    }
    const scheduler = Object.create(Scheduler.prototype);

    // Node's timers/promises functions carry the PUBLIC names, and that is
    // observable: promisify(setImmediate).name must read 'setImmediate',
    // not whatever the internal function happened to be called.
    Object.defineProperty(promisedTimeout, "name", { value: "setTimeout" });
    Object.defineProperty(promisedImmediate, "name", { value: "setImmediate" });
    Object.defineProperty(intervalIterator, "name", { value: "setInterval" });
    return {
      setTimeout: promisedTimeout,
      setImmediate: promisedImmediate,
      setInterval: intervalIterator,
      scheduler,
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
        // Node's kIgnoreErrors shape: attach a no-op 'error' listener once
        // so a failing stream cannot throw out of console.log, and pass an
        // error handler as write()'s CALLBACK -- consumers (and node's own
        // samecb-singletick test) rely on that callback being invoked.
        const ignoreErr = () => {};
        const writeTo = (stream) => {
          let armed = false;
          return (...args) => {
            if (!armed) {
              armed = true;
              if (typeof stream.once === "function") stream.once("error", ignoreErr);
            }
            stream.write(`${util.format(...args)}\n`, ignoreErr);
          };
        };
        this.log = writeTo(this._out);
        this.info = this.log;
        this.debug = this.log;
        this.warn = writeTo(this._err);
        this.error = this.warn;
      }
    }
    // node's `require("node:console")` IS globalThis.console (identity holds),
    // with Console hung off it as an own property. This used to be
    // Object.create(globalThis.console), which put every method on the
    // PROTOTYPE -- so the module's own enumerable keys were just ["Console"]
    // and the ESM facade (own keys -> named exports) could export nothing
    // else. `import { log } from "node:console"` was a link-time SyntaxError.
    const mod = globalThis.console;
    mod.Console = Console;
    // node's console.context(name) hands back a console-shaped object; the
    // name only tags output for an attached inspector, so with none the
    // methods are the global console's own.
    mod.context = (_name) => {
      const ctx = {};
      for (const key of Object.keys(mod)) {
        if (typeof mod[key] === "function" && key !== "Console" && key !== "context") {
          ctx[key] = mod[key];
        }
      }
      return ctx;
    };
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
        // The http layer manages this stream's close lifecycle; opt out of
        // Readable autoDestroy to keep the prior single-close behavior.
        super({ autoDestroy: false });
        // `meta` is the native accept record on the server path. Tolerate a
        // missing/socket-shaped arg so `new http.IncomingMessage()` and the
        // old-style `IncomingMessage.call(this, socket)` inheritance pattern
        // do not throw (Node's IncomingMessage is constructible with no args).
        meta = meta || {};
        this.method = meta.method;
        this.url = meta.uri;
        this.httpVersion = "1.1";
        this.headers = {};
        this.rawHeaders = [];
        for (const [name, value] of meta.headers || []) {
          const key = name.toLowerCase();
          this.headers[key] = key in this.headers ? `${this.headers[key]}, ${value}` : value;
          this.rawHeaders.push(name, value);
        }
        // An EventEmitter, not a bare object: handlers register error/close
        // listeners on req.socket (test-stream-pipeline does), and Node's
        // socket is an emitter even when oam never surfaces events on it.
        this.socket = Object.assign(new EventEmitter(), {
          remoteAddress: "127.0.0.1",
          encrypted: false,
        });
        this._requestId = meta.requestId;
        this._bodyPushed = false;
        this._bodyDone = false;
        this._reading = false;
        this.complete = false;
        this.aborted = false;
        // Server-request duck props: the vendored destroyer keys
        // isServerRequest() off these to DETACH req.socket instead of
        // tearing the transport down -- stream.destroy(req) must leave the
        // paired response answerable (test-stream-destroy).
        this._consuming = false;
        this._dumped = false;
      }
      _read() {
        // Pull one chunk per _read. The op serves a streamed body chunk by
        // chunk and a buffered one as a single chunk, so this is the same
        // code path whether or not the server opted into streaming.
        // push() returning false is backpressure -- Readable calls _read
        // again when drained, which is what paces the socket.
        // Node sets _consuming on first _read; resOnFinish (_dumpReq here)
        // dumps only NON-consumed requests, so a handler that responds
        // early and keeps draining the body must not have it cancelled.
        this._consuming = true;
        if (this._bodyDone || this._reading) return;
        this._reading = true;
        natives.httpRequestBodyRead(this._requestId).then(
          (chunk) => {
            this._reading = false;
            if (chunk === undefined || chunk === null || chunk.length === 0) {
              this._bodyDone = true;
              this.complete = true;
              this.push(null);
              return;
            }
            this.push(globalThis.Buffer.from(chunk.buffer, chunk.byteOffset, chunk.length));
          },
          (err) => {
            // Body too large, or the transport failed mid-body. The handler
            // may already have responded, so this errors the request stream
            // rather than producing a status.
            this._reading = false;
            this._bodyDone = true;
            this.destroy(err instanceof Error ? err : new Error(String(err)));
          },
        );
      }
      // Node's server dumps an unconsumed request when its response
      // finishes: mark complete + drain so a later destroy() is routine
      // teardown, not an abort.
      _dump() {
        if (this._dumped) return;
        this._dumped = true;
        this.complete = true;
        // Drop any un-read body so the pump stops reading the socket
        // instead of buffering for a consumer that will never arrive.
        if (!this._bodyDone) natives.httpRequestBodyCancel(this._requestId);
        this.resume();
      }
      _destroy(err, callback) {
        if ((!this.readableEnded || !this.complete) && !this._dumped) {
          this.aborted = true;
          this.emit("aborted");
        }
        // A request destroyed mid-body must stop the pump: once a response
        // is in flight the engine no longer reaps the body entry (a live
        // full-duplex read has to survive responding), so an unconsumed
        // remainder would pin the channel and its pump task forever.
        if (!this._bodyDone && typeof this._requestId === "number") {
          this._bodyDone = true;
          natives.httpRequestBodyCancel(this._requestId);
        }
        // Node tears the transport down only when the socket is still
        // ATTACHED and the message aborted mid-stream (req.destroy());
        // the module-level destroyer nulls req.socket first precisely so
        // the paired response can still answer. The abort makes an
        // unanswered exchange surface a connection error client-side.
        if (this.socket && this.aborted && typeof this._requestId === "number") {
          natives.httpAbort(this._requestId);
        }
        callback(err);
      }
    }

    class ServerResponse extends EventEmitter {
      constructor(requestId) {
        super();
        this._requestId = requestId;
        // Mock/inject mode: light-my-request et al construct this via
        // `ServerResponse.call(this, req)` passing a request OBJECT, not the
        // integer request id the native hyper layer hands us. In that mode
        // write()/end()/flushHeaders() must NOT touch the native responder.
        this._mock = typeof requestId !== "number";
        this._headers = new Map();
        this._streamId = null;
        this._ended = false;
        this._finished = false;
        this._chain = Promise.resolve(); // serializes streaming writes
        this.statusCode = 200;
        this.statusMessage = "";
        this.headersSent = false;
        // Vendored end-of-stream duck-reads lifecycle flags off the response:
        // isClosed() requires a boolean `closed`, and its onclose path treats
        // closed-with-writableFinished=false as ERR_STREAM_PREMATURE_CLOSE.
        this.closed = false;
      }
      get writableEnded() {
        return this._ended;
      }
      get writableFinished() {
        // Node's computed getter is already true in the synchronous window
        // after end() returns (bytes handed off), BEFORE 'finish' emits --
        // except when the response died unflushed (premature close beat the
        // finish emission), where Node reports false.
        return this._ended && !(this.closed && !this._finished);
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
        if (this._mock) {
          // Absorb into the assigned (usually null) socket; the mock consumer
          // captures the real payload through its own write() override.
          if (this._ended) return false;
          this.headersSent = true;
          if (this.socket && typeof this.socket.write === "function") {
            try {
              this.socket.write(this._toBytes(chunk, encoding));
            } catch {
              /* null socket swallows */
            }
          }
          if (cb) queueMicrotask(cb);
          return true;
        }
        if (this._ended) return false;
        const bytes = this._toBytes(chunk, encoding);
        if (this._streamId === null) {
          this.headersSent = true;
          this._streamId = natives.httpRespondStream(
            this._requestId,
            this.statusCode,
            this._headerPairsJson(),
          ) ?? null;
          if (this._streamId === null) {
            // Exchange already gone (req.destroy() aborted it, or the
            // request was answered elsewhere): Node's post-abort write is
            // a soft failure, never a synchronous throw. Surface the
            // premature close once and error the callback async. Reap the
            // request body too -- the engine keeps a dispatched streamed
            // body alive for JS, and this branch is the only notification
            // JS gets that the exchange is dead.
            if (!this.closed) {
              this.closed = true;
              this._dumpReq();
              queueMicrotask(() => this.emit("close"));
            }
            if (cb) {
              const err = Object.assign(
                new Error("Cannot call write after a stream was destroyed"),
                { code: "ERR_STREAM_DESTROYED" },
              );
              queueMicrotask(() => cb(err));
            }
            return false;
          }
          // Watch for hyper dropping the response body: on the client
          // tearing the connection down mid-stream, an unfinished response
          // surfaces Node's 'close'-without-'finish' shape (eos/pipeline
          // map it to ERR_STREAM_PREMATURE_CLOSE). Normal completion
          // resolves the watcher too -- the _finished guard no-ops it.
          const watchedId = this._streamId;
          natives.httpStreamClosed(watchedId).then(() => {
            if (this._finished || this.closed) return;
            this.closed = true;
            natives.httpBodyEnd(watchedId);
            // The connection died mid-response: reap an unconsumed request
            // body too, or its pump outlives the exchange (the engine keeps
            // streamed bodies alive once a response is in flight).
            this._dumpReq();
            this.emit("close");
          }, () => {});
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
        if (this._mock) {
          if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
          if (!this._ended) {
            this._ended = true;
            this.headersSent = true;
            // Node with assignSocket emits 'finish' then runs the end
            // callback and NEVER emits 'close' here -- close belongs to the
            // assigned socket's teardown, which mock consumers own.
            queueMicrotask(() => {
              this._finished = true;
              this.emit("finish");
              cb?.();
            });
          } else if (cb) {
            queueMicrotask(cb);
          }
          return this;
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
            if (this.closed) {
              cb?.();
              return;
            }
            this._finished = true;
            this._dumpReq();
            this.emit("finish");
            cb?.();
            this.closed = true;
            this.emit("close");
          });
        } else {
          // A trailing chunk joins the same serialized chain; the stream
          // closes only AFTER every queued push has flushed, in order.
          if (chunk !== undefined && chunk !== null) this.write(chunk, encoding);
          this._ended = true;
          const streamId = this._streamId;
          this._chain = this._chain.then(() => {
            if (this.closed) {
              // The httpStreamClosed watcher already surfaced a premature
              // 'close' (client abort mid-stream): never follow it with a
              // spurious 'finish' or a second 'close'.
              cb?.();
              return;
            }
            natives.httpBodyEnd(streamId);
            this._finished = true;
            this._dumpReq();
            this.emit("finish");
            cb?.();
            this.closed = true;
            this.emit("close");
          });
        }
        return this;
      }
      // Node's resOnFinish dumps an unconsumed request when the response
      // finishes, so a later req.destroy() reads as routine teardown, not a
      // client abort (aborted stays false, no 'aborted' event). A CONSUMED
      // request is never dumped (Node gates on _consuming): the handler may
      // respond early and keep draining, and on a client abort the pump's
      // queued error must reach the consumer instead of being cancelled
      // into a clean truncated 'end'.
      _dumpReq() {
        const req = this.req;
        if (!req || typeof req._dump !== "function") return;
        // Node's resOnFinish gate is _consuming OR resumeScheduled: a
        // handler that attached 'data' in the same tick as res.end() has
        // only SCHEDULED its first read (resume runs on nextTick, the
        // finish path on a microtask), so _consuming alone dumps a body
        // the handler is about to drain.
        const rs = req._readableState;
        if (req._consuming || (rs && rs.resumeScheduled)) return;
        req._dump();
      }
      flushHeaders() {
        if (this._mock) return;
        if (this._streamId === null && !this._ended) this.write(new Uint8Array(0));
      }
      assignSocket(socket) {
        // Mock/inject consumers (light-my-request) hand us a throwaway Writable
        // to absorb the base-class output; the real payload is captured by the
        // consumer's own write() override. Flag mock mode so write()/end() skip
        // the native hyper responder, and expose it as socket + connection.
        this._mock = true;
        this.socket = socket;
        this.connection = socket;
        if (socket) socket._httpMessage = this;
      }
      detachSocket() {
        this.socket = null;
        this.connection = null;
      }
      getHeaders() {
        // ServerResponse stores headers in a Map; the inherited
        // OutgoingMessage.getHeaders reads a plain object, so override to
        // reflect what setHeader()/writeHead() actually recorded.
        const out = {};
        for (const [key, value] of this._headers) out[key] = value;
        return out;
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
        // Node http.Server timeout knobs. The native hyper substrate owns the
        // real per-connection lifecycle, so these are plain properties that
        // libraries (e.g. fastify) read/write at boot; oam stores them but does
        // not arm real per-socket timers off them (see setTimeout below).
        this.timeout = 0;
        this.keepAliveTimeout = 5000;
        this.requestTimeout = 300000;
        this.headersTimeout = 60000;
        this.maxRequestsPerSocket = 0;
      }
      listen(port, host, callback) {
        if (typeof port === "function") {
          // listen(cb) -- ephemeral port, Node accepts callback-first.
          callback = port;
          port = undefined;
        }
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
        // Stream request bodies: the handler is dispatched on headers and
        // req delivers chunks as they arrive, instead of waiting for the
        // last byte (docs/design/streaming-bodies.md).
        natives.httpServe(hostname, port ?? 0, true).then(
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
                  // Node's server keeps a request-stream error from becoming
                  // an unhandled 'error' that kills the process: a client that
                  // hangs up mid-upload, or a body the server sheds under
                  // load, destroys the request stream, and with no listener
                  // that would be fatal. Node delivers such an error to a
                  // user listener and to an in-flight `for await`, but a
                  // server with neither stays up (verified against Node
                  // v22: process exits 0 on a mid-upload RST). This default
                  // listener is additive -- req.on('error') still fires.
                  req.on("error", () => {});
                  const res = new ServerResponse(meta.requestId);
                  // Node exposes the pair on each other; res end() uses
                  // req via _dumpReq() (resOnFinish parity).
                  req.res = res;
                  res.req = req;
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
          if (callback) this.once("close", callback);
        } else if (callback) {
          // Node fires close()'s callback with ERR_SERVER_NOT_RUNNING when the
          // server was never listening. Without this, a fastify onClose hook
          // (which always calls server.close) on a non-listening instance hangs
          // because the "close" event is never emitted.
          process.nextTick(() => {
            callback(Object.assign(new Error("Server is not running."), {
              code: "ERR_SERVER_NOT_RUNNING",
            }));
          });
        }
        return this;
      }
      // Node http.Server.setTimeout(msecs[, callback]): store the socket
      // inactivity timeout and, if given, register callback as a "timeout"
      // listener; return the server. Divergence: the native hyper layer owns
      // connection sockets and never surfaces idle sockets to JS, so oam stores
      // the value and wires the listener but never arms a real per-socket timer
      // nor emits "timeout". Sufficient for fastify boot, which unconditionally
      // calls server.setTimeout(connectionTimeout).
      setTimeout(msecs, callback) {
        this.timeout = msecs;
        if (callback) this.on("timeout", callback);
        return this;
      }
      // Node closeIdleConnections()/closeAllConnections() destroy idle / all
      // open connections. oam's graceful server.close() already drains idle
      // keep-alive connections in the native substrate (http_server.rs) and
      // per-connection sockets are not reachable from JS, so these are no-ops
      // that satisfy fastify's shutdown path (it feature-detects and calls them
      // before server.close()).
      closeIdleConnections() {}
      closeAllConnections() {}
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
        // Node's ClientRequest is an OutgoingMessage: a LEGACY writable
        // stream -- no _writableState, lifecycle duck-read off plain
        // properties. The vendored end-of-stream keys on exactly these, so
        // without them `pipeline(src, req)` / `stream.finished(req)` wait
        // forever on a 'finish' that never comes (test-stream-pipeline).
        this.writable = true;
        this.finished = false;
        this.destroyed = false;
        this.closed = false;
        this.aborted = false;
        this.res = null;
        this.errored = null;
        this._bodyLength = 0;
        this._bodyStream = null;
        this._streamArmed = false;
        this._sent = false;
        this._droppedWrites = false;
        this._finished = false;
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
      // Node OutgoingMessage writable-shape getters. Computed, not stored:
      // end-of-stream reads writableFinished/writableEnded directly.
      get writableEnded() { return this.finished; }
      get writableFinished() { return this._finished; }
      get writableLength() { return this._bodyLength; }
      get writableHighWaterMark() { return 16384; }
      get writableObjectMode() { return false; }
      get writableCorked() { return 0; }
      setHeader(name, value) { this._headers[name.toLowerCase()] = value; return this; }
      getHeader(name) { return this._headers[name.toLowerCase()]; }
      removeHeader(name) { delete this._headers[name.toLowerCase()]; }
      getHeaders() { return Object.assign({}, this._headers); }
      hasHeader(name) { return name.toLowerCase() in this._headers; }
      flushHeaders() { /* fetch sends headers with the body */ }
      write(chunk, encoding, callback) {
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        var bytes;
        if (typeof chunk === "string") {
          bytes = globalThis.Buffer.from(chunk, encoding || "utf8");
        } else if (chunk instanceof Uint8Array) {
          bytes = chunk;
        } else {
          bytes = globalThis.Buffer.from(chunk);
        }
        this._bodyLength += bytes.length;
        if (this._bodyStream !== null) {
          // Already streaming: hand the chunk to the transport. The op
          // resolves once the socket accepts it, so write() backpressure
          // follows the wire rather than buffering.
          natives.fetchBodyChannelWrite(this._bodyStream, bytes).then(
            () => { if (callback) callback(); },
            () => { if (callback) callback(); },
          );
          return true;
        }
        this._body.push(bytes);
        if (callback) queueMicrotask(callback);
        // A body still open on the next tick is INCREMENTAL and has to
        // stream, or the request cannot go out until the producer finishes
        // (the pipeline-into-a-request stall). The common write()+end() in
        // one tick never reaches here, so it keeps the materialized body and
        // its Content-Length instead of falling back to chunked.
        if (!this._streamArmed) {
          this._streamArmed = true;
          queueMicrotask(() => this._startBodyStreamIfOpen());
        }
        return true;
      }

      _startBodyStreamIfOpen() {
        if (this.finished || this._bodyStream !== null || this._aborted) return;
        if (this.method === "GET" || this.method === "HEAD") {
          // Upgrade handshakes (websocket preamble written before end())
          // must keep waiting for end() -> _doUpgradeRequest; an early
          // fetch here would consume the exchange and 'upgrade' would
          // never fire.
          var conn = (this._headers["connection"] || "").toLowerCase();
          if (conn.indexOf("upgrade") !== -1) return;
          // Node never chunk-frames GET/HEAD writes: the request goes out on
          // the first write and the body bytes follow UNFRAMED, so a server
          // parses a bodyless request (the stray bytes then poison the
          // keep-alive connection). Match the observable shape without the
          // wire poisoning: dispatch the request bodyless now; the buffered
          // writes never go out. The response completing then closes a
          // request that never end()ed, which surfaces the same premature
          // close Node's dead connection produces (test-stream-pipeline
          // pipes a Readable into a GET and waits on exactly that).
          if (this._sent) return;
          this.headersSent = true;
          this._droppedWrites = true;
          this._doFetchRequest(null);
          return;
        }
        this._bodyStream = natives.fetchBodyChannelNew();
        const pending = this._body;
        this._body = [];
        this.headersSent = true;
        // Send now; the body follows over the channel.
        this._doFetchRequest(null);
        for (const chunk of pending) {
          natives.fetchBodyChannelWrite(this._bodyStream, chunk).then(
            () => {},
            () => {},
          );
        }
      }
      end(data, encoding, callback) {
        if (typeof data === "function") { callback = data; data = undefined; }
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        // Node's end() is idempotent. Without the guard a second end() --
        // pipeline ends the destination, then the caller ends it too --
        // fires a SECOND request over the wire.
        if (this._ended) {
          // The response may already have arrived (a GET dispatched on its
          // first write, or a re-end after completion): once('response')
          // would never fire again, silently dropping the callback.
          if (callback) {
            if (this.res) queueMicrotask(callback);
            else this.once("response", callback);
          }
          return this;
        }
        if (data != null) this.write(data, encoding);
        this._ended = true;
        this.finished = true;
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
        if (self._bodyStream !== null) {
          // Already in flight: flush the tail and close the channel, which
          // ends the body. Must NOT send a second request.
          if (bodyData) {
            natives.fetchBodyChannelWrite(self._bodyStream, bodyData).then(
              () => natives.fetchBodyChannelEnd(self._bodyStream),
              () => natives.fetchBodyChannelEnd(self._bodyStream),
            );
          } else {
            natives.fetchBodyChannelEnd(self._bodyStream);
          }
        } else if (self._sent) {
          // Dispatched bodyless on first write (GET/HEAD): nothing further
          // goes on the wire, and re-sending would fire a second request.
        } else if (connHdr.indexOf("upgrade") !== -1) {
          self._doUpgradeRequest(bodyData);
        } else {
          self._doFetchRequest(bodyData);
        }
        // Node emits 'finish' once the message has been handed to the
        // socket -- here, once the body has been handed to the transport.
        // The tick keeps the ctor's 'socket' first, matching Node's
        // socket -> finish -> response -> close order.
        process.nextTick(function () {
          if (self._finished) return;
          self._finished = true;
          self._bodyLength = 0;
          self.emit("finish");
        });
        if (callback) {
          // A GET dispatched on its first write may have its response
          // before end() is ever called; once('response') would then
          // never fire.
          if (self.res) queueMicrotask(callback);
          else self.once("response", callback);
        }
        return this;
      }
      _doFetchRequest(bodyData) {
        var self = this;
        self._sent = true;
        var fetchOpts = {
          method: self.method,
          headers: self._headers,
        };
        if (self._bodyStream !== null) {
          fetchOpts.__oamBodyStream = self._bodyStream;
        } else if (bodyData && self.method !== "GET" && self.method !== "HEAD") {
          fetchOpts.body = bodyData;
        }
        globalThis.fetch(self._url, fetchOpts).then(function (resp) {
          if (self._aborted) return;
          // Stream the body through a reader instead of draining
          // arrayBuffer(): chunks surface as the server flushes them, and
          // res.destroy() cancels the native body handle so an endless
          // response cannot pin the event loop (Node socket-destroy
          // semantics; test-stream-pipeline destroys mid-stream).
          var reader = resp.body.getReader();
          var res = new Readable({
            read: function () {
              reader.read().then(function (r) {
                if (r.done) {
                  res.push(null);
                  self.destroyed = true;
                  self._emitClose();
                } else {
                  res.push(globalThis.Buffer.from(r.value));
                }
              }, function (err) {
                res.destroy(err);
              });
            },
            destroy: function (err, cb) {
              reader.cancel().then(function () { cb(err); }, function () { cb(err); });
            },
          });
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
          // Handle for req.destroy(): node destroys the socket, which
          // surfaces as ECONNRESET on an in-flight response stream.
          self.res = res;
          self._res = res;
          self.emit("response", res);
          if (self._droppedWrites && !self._ended) {
            // Node's unframed GET/HEAD writes poison the connection, which
            // dies around response time and closes the never-finished
            // request whether or not anyone reads the response. Emulate the
            // socket death: close the request now (the response stays
            // readable). Waiting for the response reader to hit EOF instead
            // would hang a caller that never consumes the response.
            process.nextTick(function () {
              if (self.destroyed) return;
              self.destroyed = true;
              self._emitClose();
            });
          }
        }, function (err) {
          // A torn-down request swallows the transport failure it caused:
          // Node's abort()/destroy() destroys the socket, and the resulting
          // ECONNRESET is never re-emitted on the destroyed request.
          if (self._aborted) return;
          // Map transport failures to Node-shaped codes: retry logic keys
          // on err.code, and reqwest's strings carry none.
          var msg = typeof err === "string" ? err : (err && err.message) || String(err);
          var mapped;
          if (/connection refused|ECONNREFUSED/i.test(msg)) {
            mapped = Object.assign(new Error("connect ECONNREFUSED"), {
              code: "ECONNREFUSED",
              syscall: "connect",
            });
          } else if (/error sending request|connection reset|connection closed|IncompleteMessage/i.test(msg)) {
            mapped = Object.assign(new Error("socket hang up"), { code: "ECONNRESET" });
          } else {
            mapped = err instanceof Error ? err : new Error(msg);
          }
          self.errored = mapped;
          self.destroyed = true;
          self._emitClose();
          self.emit("error", mapped);
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
                  // Handle for req.destroy(): node destroys the socket, which
                  // surfaces as ECONNRESET on an in-flight response stream.
                  self.res = res;
                  self._res = res;
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
        if (this.aborted) return;
        this.aborted = true;
        this._aborted = true;
        this.emit("abort");
        this._tearDown();
      }
      destroy(err) {
        // Node's ClientRequest.destroy() returns early on an already-
        // destroyed request and does NOT re-emit -- a second destroy(err)
        // (or one after abort()) is silent.
        if (this._aborted) return this;
        this._aborted = true;
        this._tearDown();
        if (err) {
          this.errored = err;
          this.emit("error", err);
        }
        return this;
      }
      // Node semantics: aborting or destroying the request destroys the
      // underlying socket, so an in-flight response stream fails with
      // ECONNRESET -- otherwise `for await (const c of res)` never
      // terminates and the program hangs. 'close' follows, once.
      _cancelBodyStream() {
        if (this._bodyStream !== null) {
          natives.fetchBodyChannelCancel(this._bodyStream);
          this._bodyStream = null;
        }
      }

      _tearDown() {
        if (this.destroyed) return;
        this.destroyed = true;
        // Abort an in-flight upload so the transport tears the request down
        // instead of completing it with a truncated body.
        this._cancelBodyStream();
        const res = this.res;
        if (res && !res.destroyed) {
          const reset = new Error("aborted");
          reset.code = "ECONNRESET";
          res.destroy(reset);
        }
        this._emitClose();
      }
      _emitClose() {
        if (this.closed) return;
        this.closed = true;
        var self = this;
        process.nextTick(function () { self.emit("close"); });
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

    // Node's http constructors are old-style functions callable via
    // `Super.call(this, ...)` (util.inherits). ES6 classes reject that, so wrap
    // the public exports in a Proxy whose `apply` trap initializes the caller's
    // `this` in place for the inheritance form (light-my-request's Response does
    // `http.ServerResponse.call(this, req)`), while `construct` normalises
    // new.target for a direct `new http.X()`. Internal server code references
    // the raw lexical classes, so the native request/response path is unaffected.
    const callableHttp = (Cls) => {
      const proxy = new Proxy(Cls, {
        apply: (_t, thisArg, args) => {
          if (thisArg != null && thisArg !== globalThis && thisArg instanceof Cls) {
            const tmp = Reflect.construct(Cls, args, Cls);
            for (const key of Reflect.ownKeys(tmp)) {
              Object.defineProperty(thisArg, key, Object.getOwnPropertyDescriptor(tmp, key));
            }
            return thisArg;
          }
          return Reflect.construct(Cls, args, Cls);
        },
        construct: (_t, args, newTarget) =>
          Reflect.construct(Cls, args, newTarget === proxy ? Cls : newTarget),
      });
      return proxy;
    };
    return {
      createServer: (options, handler) =>
        new Server(typeof options === "function" ? options : handler),
      Server: callableHttp(Server),
      IncomingMessage: callableHttp(IncomingMessage),
      ServerResponse: callableHttp(ServerResponse),
      ClientRequest: callableHttp(ClientRequest),
      OutgoingMessage: callableHttp(OutgoingMessage),
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

    // Reshape oam's native connect failure into Node's exact error form. The
    // native op rejects with an Error carrying only .code (ECONNREFUSED /
    // ENOTFOUND / ETIMEDOUT / ...) and a raw OS message; Node additionally sets
    // .errno (the negative libuv error number, platform-specific), .syscall,
    // and either .address/.port (connect) or .hostname (getaddrinfo), with a
    // "connect <CODE> <addr>:<port>" / "getaddrinfo <CODE> <host>" message.
    function _netErrno(code) {
      const plat = globalThis.process.platform;
      const table =
        plat === "win32"
          ? { ECONNREFUSED: -4078, ECONNRESET: -4077, ECONNABORTED: -4079, ETIMEDOUT: -4039, EHOSTUNREACH: -4073, ENETUNREACH: -4062, EADDRINUSE: -4091, EADDRNOTAVAIL: -4090, ENOTCONN: -4053, EPIPE: -4047, EACCES: -4092, ENOTFOUND: -3008, EAI_AGAIN: -3001 }
          : plat === "darwin"
            ? { ECONNREFUSED: -61, ECONNRESET: -54, ECONNABORTED: -53, ETIMEDOUT: -60, EHOSTUNREACH: -65, ENETUNREACH: -51, EADDRINUSE: -48, EADDRNOTAVAIL: -49, ENOTCONN: -57, EPIPE: -32, EACCES: -13, ENOTFOUND: -3008, EAI_AGAIN: -3001 }
            : { ECONNREFUSED: -111, ECONNRESET: -104, ECONNABORTED: -103, ETIMEDOUT: -110, EHOSTUNREACH: -113, ENETUNREACH: -101, EADDRINUSE: -98, EADDRNOTAVAIL: -99, ENOTCONN: -107, EPIPE: -32, EACCES: -13, ENOTFOUND: -3008, EAI_AGAIN: -3001 };
      return table[code];
    }
    function _shapeConnectError(err, host, port) {
      const code = err && err.code;
      if (!code) return err;
      const errno = _netErrno(code);
      let e;
      if (code === "ENOTFOUND" || code === "EAI_AGAIN") {
        // DNS resolution failure: syscall=getaddrinfo, hostname (no addr/port).
        e = new Error("getaddrinfo " + code + " " + host);
        e.syscall = "getaddrinfo";
        e.hostname = host;
      } else {
        // Connection failure: "connect <CODE> <address>:<port>".
        e = new Error("connect " + code + " " + host + ":" + port);
        e.syscall = "connect";
        e.address = host;
        e.port = port;
      }
      e.code = code;
      if (errno !== undefined) e.errno = errno;
      return e;
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
        // Node's net.Socket is a Duplex stream, so libraries read its stream
        // state objects directly. ws's socketOnClose reads socket._readableState
        // (.endEmitted / .length) and its bufferedAmount getter reads
        // socket._writableState.length. Without these, socketOnClose throws a
        // TypeError inside emit("close") BEFORE it reaches websocket.emitClose(),
        // so the ws-level "close" never fires and peers leak. oam's socket is
        // always in flowing mode (readLoop emits "data"), so the readable buffer
        // length is always 0.
        // Field set covers everything the VENDORED stream predicates read
        // (utils/destroy/end-of-stream/pipeline: closed, closeEmitted,
        // constructed, destroyed, ended, ending, errored, errorEmitted,
        // finalCalled, finished, pendingcb, prefinished, readable/writable,
        // reading, length) so pipeline()/finished() on a socket see a
        // consistent not-yet-finished stream; lifecycle sites (destroy,
        // _doClose, _readLoop EOF, end) mutate the load-bearing ones.
        this._readableState = {
          endEmitted: false, length: 0, closed: false, closeEmitted: false,
          constructed: true, destroyed: false, ended: false, errored: null,
          errorEmitted: false, readable: true, reading: false,
        };
        this._writableState = {
          length: 0, finished: false, errorEmitted: false, needDrain: false,
          closed: false, closeEmitted: false, constructed: true,
          destroyed: false, ended: false, ending: false, errored: null,
          finalCalled: false, pendingcb: 0, prefinished: false, writable: true,
        };
        this._paused = false;
        this._readLoopActive = false;
        this._pipeHandler = null;
        this._timeoutMs = 0;
        this._timeoutId = null;
        // Distinguishes ERR_SOCKET_CLOSED vs ERR_SOCKET_CLOSED_BEFORE_
        // CONNECTION for callbacks queued on a dead socket (Node parity).
        this._everConnected = false;
        if (options && options._handle !== undefined) {
          this._handle = options._handle;
          this.connecting = false;
          this._everConnected = true;
          // Pre-connected socket (server accept, HTTP/WS upgrade): node has a
          // live TCPSocketWrap the moment the wrapper exists, so it shows up in
          // _getActiveHandles()/getActiveResourcesInfo() immediately.
          registry._activeHandles.set(this, "TCPSocketWrap");
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
        // Node registers the TCPSocketWrap synchronously inside connect(), well
        // before the connection is established (probe: _getActiveHandles()
        // contains the Socket on the line after net.connect() returns).
        // Guard: a socket that is ALREADY destroyed must not be re-registered
        // -- oam's connect() does not reset `destroyed`, so destroy() and
        // _doClose() both early-return and the entry would be pinned in this
        // strong Map for the process lifetime (one per socket).
        if (!this.destroyed) registry._activeHandles.set(this, "TCPSocketWrap");
        const pending = natives.tcpConnect(host, port).then(
          (result) => {
            if (this.destroyed) {
              // destroy() raced the connect: close the just-established
              // native handle instead of reviving a dead socket (no
              // 'connect' after 'close', no leaked TCP connection).
              try { natives.tcpClose(result.handle); } catch (_) { /* noop */ }
              return;
            }
            this._handle = result.handle;
            this.connecting = false;
            this._everConnected = true;
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
            this.destroy(_shapeConnectError(err, host, port));
          },
        );
        // Node queues writes (and the end() FIN) issued before the
        // connection exists; gate the write chain on the pending connect so
        // an immediate end() shuts the socket down instead of silently
        // skipping tcpShutdown on a null handle.
        this._chain = this._chain.then(() => pending);
        return this;
      }

      write(data, encoding, cb) {
        if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        // Node's C++ StreamBase typecheck: a string with the pseudo-encoding
        // 'buffer' is rejected synchronously (test-stream-base-typechecking).
        if (encoding === "buffer" && typeof data === "string") {
          const err = new TypeError("Second argument must be a buffer");
          err.code = "ERR_INVALID_ARG_TYPE";
          throw err;
        }
        if (this.destroyed || !this.writable) {
          const err = new Error("This socket has been ended");
          if (cb) cb(err);
          else this.emit("error", err);
          return false;
        }
        if (this._timeoutMs > 0) this._resetTimeout();
        const bytes = toBytes(data, encoding);
        this.bytesWritten += bytes.length;
        // Writes queue on _chain, so bytes handed over faster than the socket
        // drains stay alive in that chain. Without accounting, write() always
        // returned true, 'drain' never fired, and a producer that respects
        // backpressure (the documented contract) had nothing to respect: a
        // 960MB send buffered ~1.2GB in-process, where Node held 52MB.
        // Count the outstanding bytes and report them the way Node does.
        this._writableState.length += bytes.length;
        this.bufferSize = this._writableState.length;
        const settle = () => {
          this._writableState.length -= bytes.length;
          this.bufferSize = this._writableState.length;
          // Node emits 'drain' only when the buffer fully empties AND a write
          // previously returned false -- not on every partial flush.
          if (this._writableState.needDrain && this._writableState.length === 0) {
            this._writableState.needDrain = false;
            if (!this.destroyed) this.emit("drain");
          }
        };
        this._chain = this._chain.then(() => {
          if (this.destroyed) {
            // Node invokes queued write callbacks with the socket-closed
            // error (NOT the connect error -- that went to 'error') --
            // silently dropping them hangs promisified writes forever.
            if (cb) {
              const err = Object.assign(
                new Error(this._everConnected
                  ? "Socket is closed"
                  : "Socket closed before the connection was established"),
                { code: this._everConnected
                  ? "ERR_SOCKET_CLOSED"
                  : "ERR_SOCKET_CLOSED_BEFORE_CONNECTION" },
              );
              process.nextTick(() => cb(err));
            }
            settle();
            return;
          }
          return natives.tcpWrite(this._handle, bytes).then(
            () => { settle(); if (cb) cb(); },
            (err) => { settle(); if (cb) cb(err); else this.emit("error", err); },
          );
        });
        // Node: false once the queue is at or past the high-water mark. The
        // write is still accepted -- false is advisory, asking the producer to
        // wait for 'drain'.
        if (this._writableState.length >= this.writableHighWaterMark) {
          this._writableState.needDrain = true;
          return false;
        }
        return true;
      }

      // Node's default for a net.Socket. Settable, as Node allows via options.
      get writableHighWaterMark() {
        return this._writableHighWaterMark ?? 16384;
      }
      set writableHighWaterMark(v) {
        this._writableHighWaterMark = v;
      }

      /** Node parity: bytes accepted but not yet flushed to the socket. */
      get writableLength() {
        return this._writableState.length;
      }

      end(data, encoding, cb) {
        if (typeof data === "function") { cb = data; data = undefined; encoding = undefined; }
        else if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        // Reentry guard (EOF auto-end + a user 'end' listener calling end()
        // again would otherwise chain a SECOND 'finish' after 'close'). Node:
        // end() after end() is a no-op that still fires the callback.
        if (this._writableState.ended) {
          if (cb) this.once("finish", cb);
          return this;
        }
        if (data !== undefined && data !== null) this.write(data, encoding);
        this.writable = false;
        // state.writable stays untouched (side-existence marker; see destroy).
        this._writableState.ending = true;
        this._writableState.ended = true;
        this._chain = this._chain.then(() => {
          if (this._handle !== null) return natives.tcpShutdown(this._handle);
        }).then(() => {
          if (this.destroyed || this._writableState.errored) {
            // Never report success on a socket that died first: Node skips
            // 'finish' entirely and hands the end callback the error.
            if (cb) {
              cb(this._writableState.errored ?? Object.assign(
                new Error(this._everConnected
                  ? "Socket is closed"
                  : "Socket closed before the connection was established"),
                { code: this._everConnected
                  ? "ERR_SOCKET_CLOSED"
                  : "ERR_SOCKET_CLOSED_BEFORE_CONNECTION" },
              ));
            }
            return;
          }
          this._writableState.finished = true;
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
        registry._activeHandles.delete(this);
        const rs = this._readableState;
        const ws = this._writableState;
        rs.destroyed = ws.destroyed = true;
        // Deliberately NOT flipping rs.readable / ws.writable: Node never
        // mutates those state fields post-construction (they are
        // side-existence markers isReadableNodeStream/isWritableNodeStream
        // key off), and flipping them before emit('close') makes the
        // vendored end-of-stream skip its premature-close detection --
        // pipeline() would report success on a silently truncated transfer.
        if (err) rs.errored = ws.errored = err;
        if (this._timeoutId !== null) {
          globalThis.clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
        if (this._handle !== null) {
          try { natives.tcpClose(this._handle); } catch (_) { /* noop */ }
          this._handle = null;
        }
        if (err) {
          this.emit("error", err);
          rs.errorEmitted = ws.errorEmitted = true;
        }
        rs.closed = ws.closed = true;
        this.emit("close", !!err);
        rs.closeEmitted = ws.closeEmitted = true;
        return this;
      }

      async _readLoop() {
        // One pump per handle. pause() only takes effect at the top of the
        // next iteration, so a pause/resume while a read is in flight left the
        // original loop awaiting tcpRead and started a second one -- two
        // concurrent reads of the same handle, and the loser rejects with
        // "read handle is gone". The in-flight loop sees _paused cleared and
        // simply carries on, which is what resume() wants anyway.
        if (this._readLoopActive) return;
        this._readLoopActive = true;
        try {
          await this._readLoopBody();
        } finally {
          this._readLoopActive = false;
        }
      }

      async _readLoopBody() {
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
            // state.readable stays untouched (side-existence marker; see destroy).
            this._readableState.ended = true;
            this._readableState.endEmitted = true;
            this.emit("end");
            if (!this.allowHalfOpen) {
              if (this._writableState.ended) {
                // end() already ran (the common write->end->echo->EOF shape);
                // the reentry guard means calling it again would no-op, so
                // route the close through the write chain -- it sequences
                // AFTER the in-flight shutdown + 'finish'. Before the guard,
                // this path re-ran end() and closed via its duplicate chain
                // (which also double-emitted 'finish').
                this._chain = this._chain.then(() => this._doClose());
              } else {
                this.end();
              }
            } else if (!this.writable) {
              this._doClose();
            }
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
        registry._activeHandles.delete(this);
        if (this._handle !== null) {
          try { natives.tcpClose(this._handle); } catch (_) { /* noop */ }
          this._handle = null;
        }
        if (!this.destroyed) {
          this.destroyed = true;
          const rs = this._readableState;
          const ws = this._writableState;
          rs.destroyed = ws.destroyed = true;
          rs.closed = ws.closed = true;
          this.emit("close", false);
          rs.closeEmitted = ws.closeEmitted = true;
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
      // Node excludes UNREF'd handles from _getActiveHandles() and
      // getActiveResourcesInfo() (probe-verified: server.unref() removes it
      // from both). The flag is read by those views; it does not yet affect
      // oam's loop-liveness, which is native-op driven.
      ref() { this._handleRefed = true; return this; }
      unref() { this._handleRefed = false; return this; }
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
        // Node binds (and so creates the TCPServerWrap) synchronously inside
        // listen(); createServer() alone registers nothing. Probed: after
        // createServer() _getActiveHandles() is [], on the line after
        // listen(0) it is [Server] / ["TCPServerWrap"].
        registry._activeHandles.set(this, "TCPServerWrap");
        // Supersede any in-flight accept loop from a previous listen() so
        // its tail cannot unregister this fresh registration.
        this._listenGeneration = (this._listenGeneration || 0) + 1;
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
          (err) => {
            // Bind failed -- there is no live handle to introspect.
            registry._activeHandles.delete(this);
            this.emit("error", err);
          },
        );
        return this;
      }

      async _acceptLoop() {
        // Generation token: close()+listen() in the same tick leaves this
        // (stale) loop still awaiting. Without the check its tail would
        // delete the registration the FRESH listen() just made, hiding a
        // live server from _getActiveHandles()/getActiveResourcesInfo().
        // listen() owns the counter; the loop only snapshots it.
        const generation = this._listenGeneration;
        for (;;) {
          let accepted;
          try {
            accepted = await natives.tcpAccept(this._serverId);
          } catch (err) {
            if (this.listening) this.emit("error", err);
            break;
          }
          if (accepted === undefined) break;
          if (generation !== this._listenGeneration) return; // superseded
          const socket = new Socket({
            _handle: accepted.handle,
            _remoteAddr: accepted.remoteAddr,
          });
          socket._readLoop();
          this.emit("connection", socket);
        }
        if (generation !== this._listenGeneration) return; // superseded
        registry._activeHandles.delete(this);
        this.emit("close");
      }

      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }

      close(cb) {
        registry._activeHandles.delete(this);
        if (this._serverId !== null) {
          natives.tcpServerClose(this._serverId);
          this.listening = false;
        }
        if (cb) this.once("close", cb);
        return this;
      }

      getConnections(cb) { if (cb) cb(null, 0); return this; }
      // Same unref semantics as Socket (see the note there).
      ref() { this._handleRefed = true; return this; }
      unref() { this._handleRefed = false; return this; }
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
      // Node throws ERR_INVALID_STATE only for a detached ArrayBuffer passed
      // DIRECTLY (probe-verified shape: plain Error, no "Invalid state:"
      // prefix). A view over a detached buffer reads as zero-length and
      // validates vacuously true -- do not guard it.
      if (input instanceof ArrayBuffer && input.detached) {
        const e = new Error("Cannot validate on a detached buffer");
        e.code = "ERR_INVALID_STATE";
        throw e;
      }
      // View over a detached buffer: zero-length, vacuously valid (node
      // returns true). Short-circuit -- constructing a Uint8Array over a
      // detached ArrayBuffer would throw.
      if (ArrayBuffer.isView(input)) {
        const b = input.buffer;
        if (b instanceof ArrayBuffer && b.detached) return new Uint8Array(0);
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
      // NOT allocUnsafe: "slow" means unpooled, i.e. its own exact-size
      // ArrayBuffer. Routing it through the pool made sb.buffer.byteLength
      // the pool size.
      SlowBuffer: function SlowBuffer(size) { return globalThis.Buffer.allocUnsafeSlow(size); },
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
  registry.factories.timers = () => {
    // Capture the live globals at factory-build time so the module keeps
    // working after a test deletes globalThis.setTimeout et al
    // (test-timers-api-refs). installRuntimeGlobals has already upgraded
    // these to the Timeout-returning wrappers by the time any user module
    // requires 'timers'.
    const _setTimeout = globalThis.setTimeout;
    const _clearTimeout = globalThis.clearTimeout;
    const _setInterval = globalThis.setInterval;
    const _clearInterval = globalThis.clearInterval;
    const _setImmediate = globalThis.setImmediate;
    const _clearImmediate = globalThis.clearImmediate;

    // Legacy linked-list timer API (deprecated in Node, still exercised by
    // the corpus). enroll/unenroll mutate the object's _idle* fields;
    // active()/_unrefActive() (re)schedule based on _idleTimeout. Faithful
    // enough for the validation + mutation assertions; the actual timer is
    // a plain setTimeout calling the object's _onTimeout.
    function validateMsecs(msecs, name) {
      if (typeof msecs !== "number") {
        throw new codes.ERR_INVALID_ARG_TYPE(name, "number", msecs);
      }
      if (msecs < 0 || !Number.isFinite(msecs)) {
        throw new codes.ERR_OUT_OF_RANGE(name, "a non-negative finite number", msecs);
      }
    }
    function enroll(item, msecs) {
      validateMsecs(msecs, "msecs");
      // TIMEOUT_MAX overflow on the enroll path: node v22 TRUNCATES to
      // 2147483647 (unlike the Timeout ctor's set-to-1) with its own text.
      if (msecs > 2147483647) {
        process.emitWarning(
          msecs + " does not fit into a 32-bit signed integer." +
            "\nTimer duration was truncated to 2147483647.",
          "TimeoutOverflowWarning",
        );
        msecs = 2147483647;
      }
      if (item._idleNext) unenroll(item);
      item._idleTimeout = msecs;
      item._idleNext = null;
      item._idlePrev = null;
    }
    function unenroll(item) {
      if (item._idleTimerId !== undefined && item._idleTimerId !== null) {
        _clearTimeout(item._idleTimerId);
        item._idleTimerId = null;
      }
      item._idleTimeout = -1;
      item._idleNext = null;
      item._idlePrev = null;
    }
    function active(item) {
      _insert(item, false);
    }
    function _unrefActive(item) {
      _insert(item, true);
    }
    function _insert(item, unrefed) {
      // A real Timeout/Immediate handle re-arms through its own machinery --
      // scheduling a SECOND plain setTimeout here would double-fire the
      // callback (the handle's _onTimeout field is now populated).
      if (item && item._kind && typeof item.refresh === "function") {
        item.refresh();
        return;
      }
      const msecs = item._idleTimeout;
      if (typeof msecs !== "number" || msecs < 0) return;
      item._idleStart = Math.trunc(globalThis.performance?.now?.() ?? Date.now());
      // _idleNext/_idlePrev are sentinels in Node's linked list; the corpus
      // only asserts they are truthy after active(), so a self-link suffices.
      item._idleNext = item._idleNext || item;
      item._idlePrev = item._idlePrev || item;
      if (item._idleTimerId !== undefined && item._idleTimerId !== null) {
        _clearTimeout(item._idleTimerId);
      }
      const t = _setTimeout(() => {
        item._idleTimerId = null;
        if (typeof item._onTimeout === "function") item._onTimeout();
      }, msecs);
      if (unrefed && typeof t.unref === "function") t.unref();
      item._idleTimerId = t;
    }

    // Node ships these behind util.deprecate: one DeprecationWarning per
    // function per process, BEFORE the call proceeds (the max-duration
    // warning test counts them via process.on('warning')).
    function deprecatedOnce(fn, msg, code) {
      let warned = false;
      return function (...a) {
        if (!warned) {
          warned = true;
          process.emitWarning(msg, "DeprecationWarning", code);
        }
        return fn.apply(this, a);
      };
    }

    // Node tags the callback-style timers with promisify.custom so
    // `promisify(setTimeout)` yields the timers/promises version -- which
    // resolves the VALUE and accepts an AbortSignal -- instead of a generic
    // wrapper that would treat the timer id as an error argument.
    const promisifyCustom = Symbol.for("nodejs.util.promisify.custom");
    const tagPromises = (fn, name) => {
      Object.defineProperty(fn, promisifyCustom, {
        get() {
          return registry.get("timers/promises")[name];
        },
        enumerable: false,
        configurable: true,
      });
      return fn;
    };
    return {
      setTimeout: tagPromises(
        (fn, ms, ...args) => _setTimeout(fn, ms, ...args),
        "setTimeout",
      ),
      clearTimeout: (id) => _clearTimeout(id),
      setInterval: tagPromises(
        (fn, ms, ...args) => _setInterval(fn, ms, ...args),
        "setInterval",
      ),
      clearInterval: (id) => _clearInterval(id),
      setImmediate: tagPromises(
        (fn, ...args) => _setImmediate(fn, ...args),
        "setImmediate",
      ),
      clearImmediate: (id) => _clearImmediate(id),
      enroll: deprecatedOnce(enroll, "timers.enroll() is deprecated. Please use setTimeout instead.", "DEP0095"),
      unenroll: deprecatedOnce(unenroll, "timers.unenroll() is deprecated. Please use clearTimeout instead.", "DEP0096"),
      active: deprecatedOnce(active, "timers.active() is deprecated. Please use timeout.refresh() instead.", "DEP0126"),
      _unrefActive: deprecatedOnce(_unrefActive, "timers._unrefActive() is deprecated. Please use timeout.refresh() instead.", "DEP0127"),
      get promises() {
        return registry.get("timers/promises");
      },
    };
  };

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
  registry.factories.vm = (natives) => {
    // Every entry point here runs on a real v8::Context (see
    // crates/oam_engine/src/vm_context.rs). The script therefore gets its own
    // intrinsics and its own global, which is what separates `vm` from the
    // `with (sandbox) { ... }` closure this module used to be: patching
    // `Array.prototype` inside a context no longer reaches the host, and a
    // write to `globalThis` no longer lands on the runtime's real global.

    // node's validateInt32(timeout, 'options.timeout', 1): a non-number is a
    // type error, and 0 / negative / fractional are out of range. Returns 0
    // for "no timeout", which is what the native side treats as unbounded.
    function timeoutOf(options) {
      if (options == null || typeof options !== "object") return 0;
      const timeout = options.timeout;
      if (timeout === undefined) return 0;
      if (typeof timeout !== "number") {
        throw new codes.ERR_INVALID_ARG_TYPE("options.timeout", "number", timeout);
      }
      if (!Number.isInteger(timeout) || timeout < 1) {
        throw applyNodeErrorShape(
          new RangeError(
            `The value of "options.timeout" is out of range. It must be a positive integer. Received ${timeout}`,
          ),
          "ERR_OUT_OF_RANGE",
        );
      }
      return timeout;
    }

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
        // Node surfaces a syntax error from the constructor, not from the first
        // run, so compile here and let it throw. V8's compilation cache makes
        // the recompile at run time cheap.
        natives.vmCompile(this._code, this._filename, this._lineOffset, this._columnOffset);
      }
      runInThisContext(options) {
        return natives.vmRunInThisContext(
          this._code,
          this._filename,
          this._lineOffset,
          this._columnOffset,
          timeoutOf(options),
        );
      }
      runInContext(contextifiedObject, options) {
        if (!isContext(contextifiedObject)) {
          throw new codes.ERR_INVALID_ARG_TYPE(
            "contextifiedObject",
            "vm.Context",
            contextifiedObject,
          );
        }
        return natives.vmRunInContext(
          contextifiedObject,
          this._code,
          this._filename,
          this._lineOffset,
          this._columnOffset,
          timeoutOf(options),
        );
      }
      runInNewContext(sandbox, options) {
        return this.runInContext(createContext(sandbox), options);
      }
      createCachedData() {
        return new Uint8Array(0);
      }
    }

    // ------------------------------------------------ vm.SourceTextModule
    // Gated on --experimental-vm-modules, as in node. The flag is read lazily
    // because this factory runs inside the snapshot, long before argv exists.
    let vmModulesEnabled;
    function requireVmModulesFlag() {
      vmModulesEnabled ??= natives.env().OAM_EXPERIMENTAL_VM_MODULES === "1";
      if (!vmModulesEnabled) {
        throw applyNodeErrorShape(
          new Error(
            "Experimental vm.SourceTextModule is not enabled. Run with --experimental-vm-modules",
          ),
          "ERR_VM_MODULE_NOT_ENABLED",
        );
      }
    }

    // A v8::Module is not a JS value, so it cannot be hung off the wrapper and
    // left to the GC the way a vm context is. The wrapper's collection is what
    // releases the native entry; without this every module ever compiled would
    // be pinned for the life of the isolate.
    const releaseModule =
      typeof FinalizationRegistry === "function"
        ? new FinalizationRegistry((id) => natives.vmModuleRelease(id))
        : null;

    const kModuleId = Symbol("oam.vmModuleId");
    let moduleSequence = 0;

    class SourceTextModule {
      constructor(source, options) {
        requireVmModulesFlag();
        if (typeof source !== "string") {
          throw new codes.ERR_INVALID_ARG_TYPE("code", "string", source);
        }
        const opts = options != null && typeof options === "object" ? options : {};
        this.identifier = opts.identifier ?? `vm:module(${moduleSequence++})`;
        this.context = opts.context;
        // Compiles now, so a syntax error throws from the constructor.
        // The context travels to the native side: a module compiled outside
        // the context it claims to belong to would write to the HOST global.
        this[kModuleId] = natives.vmModuleCompile(source, this.identifier, this.context);
        this._linkedOnce = false;
        if (releaseModule) releaseModule.register(this, this[kModuleId]);
      }
      get status() {
        return natives.vmModuleStatus(this[kModuleId]);
      }
      get dependencySpecifiers() {
        return natives.vmModuleRequests(this[kModuleId]);
      }
      get namespace() {
        if (this.status === "unlinked") {
          throw applyNodeErrorShape(
            new Error("Module status must not be unlinked"),
            "ERR_VM_MODULE_STATUS",
          );
        }
        return natives.vmModuleNamespace(this[kModuleId]);
      }
      get error() {
        if (this.status !== "errored") {
          throw applyNodeErrorShape(
            new Error("Module status must be errored"),
            "ERR_VM_MODULE_STATUS",
          );
        }
        return natives.vmModuleError(this[kModuleId]);
      }
      /// Resolves the whole graph in JS BEFORE instantiating: V8's resolve
      /// callback is synchronous and cannot await, but a linker may return a
      /// promise. Node splits it the same way.
      async link(linker) {
        if (typeof linker !== "function") {
          throw new codes.ERR_INVALID_ARG_TYPE("linker", "function", linker);
        }
        if (this._linkedOnce || this.status !== "unlinked") {
          throw applyNodeErrorShape(
            new Error("Module status must be unlinked"),
            "ERR_VM_MODULE_STATUS",
          );
        }
        this._linkedOnce = true;
        const specifiers = this.dependencySpecifiers;
        const resolvedIds = [];
        for (const specifier of specifiers) {
          const dependency = await linker(specifier, this, { attributes: {} });
          if (!(dependency instanceof SourceTextModule)) {
            throw applyNodeErrorShape(
              new Error("Linker must return a Module object"),
              "ERR_VM_MODULE_NOT_MODULE",
            );
          }
          // Depth-first, and only once per module: a cycle would otherwise
          // recurse forever, and diamond imports would link a shared
          // dependency twice.
          if (dependency.status === "unlinked" && !dependency._linkedOnce) {
            await dependency.link(linker);
          }
          resolvedIds.push(dependency[kModuleId]);
        }
        natives.vmModuleLink(this[kModuleId], specifiers, resolvedIds);
        natives.vmModuleInstantiate(this[kModuleId]);
      }
      async evaluate(options) {
        const timeout = timeoutOf(options);
        const status = this.status;
        if (status !== "linked" && status !== "evaluated" && status !== "errored") {
          throw applyNodeErrorShape(
            new Error(`Module status must be one of linked, evaluated, or errored`),
            "ERR_VM_MODULE_STATUS",
          );
        }
        await natives.vmModuleEvaluate(this[kModuleId], timeout);
        return undefined;
      }
    }
    Object.defineProperty(SourceTextModule.prototype, Symbol.toStringTag, {
      value: "SourceTextModule",
      configurable: true,
    });

    function createContext(sandbox, _options) {
      if (sandbox !== undefined && (sandbox === null || typeof sandbox !== "object")) {
        throw new codes.ERR_INVALID_ARG_TYPE("contextObject", "object", sandbox);
      }
      const obj = sandbox != null ? sandbox : {};
      // Idempotent on the native side: a second call hands back the context the
      // sandbox already carries.
      natives.vmCreateContext(obj);
      return obj;
    }

    function isContext(value) {
      // Node rejects a non-object outright rather than answering false, and a
      // function counts as a non-object here.
      if (value === null || typeof value !== "object") {
        throw new codes.ERR_INVALID_ARG_TYPE("object", "Object", value);
      }
      return natives.vmIsContext(value);
    }

    function runInThisContext(code, options) {
      return new Script(code, options).runInThisContext(options);
    }
    function runInNewContext(code, sandbox, options) {
      return new Script(code, options).runInNewContext(sandbox, options);
    }
    function runInContext(code, contextifiedObject, options) {
      return new Script(code, options).runInContext(contextifiedObject, options);
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
      Script, createContext, isContext, createScript, SourceTextModule,
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
              // Handle for req.destroy(): node destroys the socket, which
              // surfaces as ECONNRESET on an in-flight response stream.
              self._res = res;
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
    // Node domain state (differentially probed against real v22.22.2):
    // process.domain starts null on require('domain'); an EMPTIED stack via
    // exit() leaves it undefined (stack[len-1] of []); after a domain
    // CATCHES an error, domainUncaughtExceptionClear resets it to null --
    // so error handlers observe undefined and later timers observe null.
    const stack = [];
    const _domain = [null];
    Object.defineProperty(process, "domain", {
      get: () => _domain[0],
      set: (v) => { _domain[0] = v; },
      enumerable: true,
      configurable: true,
    });
    function tagError(er, d) {
      if (er && (typeof er === "object" || typeof er === "function")) {
        // Best-effort: a frozen error passes through untagged rather than
        // being replaced by a defineProperty TypeError.
        try {
          Object.defineProperty(er, "domain", {
            value: d, writable: true, configurable: true, enumerable: false,
          });
          er.domainThrown = true;
        } catch { /* frozen/sealed error */ }
      }
      return er;
    }
    // Process-level uncaught path clear (node's domainUncaughtExceptionClear):
    // consumed by __oamDispatchUncaught ONLY -- sync bind/intercept handling
    // must NOT clear unrelated domains (they keep executing).
    registry._domainUncaughtClear = () => {
      stack.length = 0;
      _domain[0] = null;
    };
    registry._domainLoaded = true;
    class Domain extends EventEmitter {
      constructor() {
        super();
        this.members = [];
      }
      enter() {
        stack.push(this);
        _domain[0] = this;
      }
      exit() {
        const idx = stack.lastIndexOf(this);
        if (idx === -1) return;
        stack.splice(idx);
        _domain[0] = stack.length ? stack[stack.length - 1] : undefined;
      }
      add(obj) { this.members.push(obj); return this; }
      remove(obj) {
        const i = this.members.indexOf(obj);
        if (i >= 0) this.members.splice(i, 1);
        return this;
      }
      _errorHandler(er, viaThrow) {
        // Handlers run OUTSIDE the domain's context: unwind this domain
        // fully first (empties to undefined when it was the only one).
        while (_domain[0] === this) this.exit();
        if (this.listenerCount("error") > 0) {
          // Tag only when a listener will observe it -- an error that falls
          // through to the regular uncaught ladder stays untagged (node).
          tagError(er, this);
          // A throwing 'error' listener propagates from emit -- the process-
          // level dispatcher escalates that to fatal exit 7 (node parity).
          this.emit("error", er);
          // THROW-driven handling maps to node's fatal path, which runs
          // domainUncaughtExceptionClear on a caught error (later callbacks
          // see process.domain === null). Callback-DELIVERED errors
          // (intercept's err argument) never reach the fatal path in node
          // and must not evict unrelated domains.
          if (viaThrow) registry._domainUncaughtClear();
          return true;
        }
        // No listener: rethrow the ORIGINAL, untagged. The global clear on
        // this path is the process-level dispatcher's job.
        throw er;
      }
      bind(fn) {
        const d = this;
        return function (...args) {
          d.enter();
          try { const r = fn.apply(this, args); d.exit(); return r; }
          catch (e) { d._errorHandler(e, true); }
        };
      }
      intercept(fn) {
        const d = this;
        return function (err, ...args) {
          // Callback-delivered error: no fatal path, no global clear.
          if (err) { d._errorHandler(err, false); return; }
          d.enter();
          try { const r = fn.apply(this, args); d.exit(); return r; }
          catch (e) { d._errorHandler(e, true); }
        };
      }
      run(fn, ...args) {
        this.enter();
        try { const r = fn.apply(this, args); this.exit(); return r; }
        catch (e) { this._errorHandler(e, true); }
      }
    }
    function create() { return new Domain(); }
    return {
      Domain,
      create,
      get active() { return _domain[0]; },
    };
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

    // oam subcommands -- if a self-spawn's first arg is one of these (or a
    // flag), it's already an oam-shaped invocation and must NOT be rewritten.
    const OAM_SUBCOMMANDS = new Set([
      "run", "test", "repl", "check", "daemon", "mcp", "serve",
      "install", "trust", "compile", "self-update", "help",
    ]);

    function normalizeArgs(command, args, options) {
      if (args != null && typeof args === "object" && !Array.isArray(args)) {
        options = args;
        args = [];
      }
      let cmd = String(command);
      let argv = (args || []).map(String);
      const opts = options || {};
      // Self-spawn parity: Node runs `node <script> [args]`, but oam's CLI needs
      // the `run` subcommand. When the command resolves to oam's own execPath
      // (not a shell invocation) and the first arg is a SCRIPT (not a flag, not
      // an existing oam subcommand), rewrite `<execPath> <script> [args]` ->
      // `<execPath> run <script> --no-check -- [args]`, matching fork()'s
      // injection. Leaves `node --flag script` (leading flag) and already-
      // `run`-shaped invocations untouched.
      if (!opts.shell && argv.length > 0) {
        const execPath = globalThis.process.execPath;
        const sameBin =
          cmd === execPath ||
          cmd.replace(/\\/g, "/").split("/").pop() === execPath.replace(/\\/g, "/").split("/").pop();
        const first = argv[0];
        if (sameBin && !first.startsWith("-") && !OAM_SUBCOMMANDS.has(first)) {
          argv = ["run", first, "--no-check", "--", ...argv.slice(1)];
        }
      }
      return {
        command: cmd,
        args: argv,
        options: opts,
      };
    }

    function decodeOutput(buf, encoding) {
      if (!encoding || encoding === "buffer") return Buffer.from(buf);
      return Buffer.from(buf).toString(encoding);
    }

    // The native spawn ops throw a JSON body {code,message} on failure
    // (child.rs / child_win.rs / child_unix.rs). Lift it into the error shape
    // node produces -- `spawn <cmd> ENOENT` with .code/.syscall/.path set --
    // because `err.code === 'ENOENT'` is what execa, cross-spawn and every
    // which-style resolver branch on. Left opaque, callers see an unparsed
    // blob as the message and `undefined` as the code.
    function spawnFailureError(err, command, args) {
      const e = typeof err === "string" ? new Error(err) : err;
      const raw = typeof err === "string" ? err : (e && e.message) || "";
      let parsed;
      try {
        parsed = JSON.parse(raw);
      } catch {
        return e; // message was not the JSON body: pass it through unchanged
      }
      if (!parsed || !parsed.code) return e;
      const shaped = new Error(`spawn ${command} ${parsed.code}`);
      // node orders errno first on this error; the key order shows up in
      // util.inspect output, which people paste into bug reports.
      if (typeof parsed.errno === "number") shaped.errno = parsed.errno;
      shaped.code = parsed.code;
      shaped.syscall = `spawn ${command}`;
      shaped.path = command;
      shaped.spawnargs = args ? args.slice() : [];
      return shaped;
    }

    // Native per-fd stdio direction codes, shared with spawnExtra's stdioCode.
    const STDIO_IGNORE = 0;
    const STDIO_INHERIT = 1;
    const STDIO_PIPE = 2;
    // spawnExtra's codes run 0-3 (ignore/inherit/child-read/child-write), so a
    // descriptor is offset past them rather than encoded as a bare number --
    // otherwise fd 2 and "child-read" would be the same code. Must match
    // DESCRIPTOR_CODE_BASE in node_ops.rs.
    const STDIO_EXTRA_DESCRIPTOR_BASE = 1000;

    // One entry of node's `stdio` option -> a direction code. 'inherit', a
    // numeric fd and a stream all mean "hand the child the parent's fd" for
    // slots 0/1/2, which is inherit; a null/undefined slot takes the default
    // for its fd ('pipe' for 0/1/2, 'ignore' beyond -- node's documented rule).
    function stdioEntryCode(entry, fd) {
      if (entry === "pipe" || entry === "overlapped") return STDIO_PIPE;
      if (entry === "inherit") return STDIO_INHERIT;
      if (entry === "ignore") return STDIO_IGNORE;
      if (entry === null || entry === undefined) {
        return fd <= 2 ? STDIO_PIPE : STDIO_IGNORE;
      }
      // A DESCRIPTOR -- named by number, or by a stream that owns one
      // (fs.createWriteStream exposes `.fd`). Any code >= 3 IS the descriptor
      // number; the native side resolves it against oam's registry and hands
      // the child a dup. Everything used to collapse to 'inherit' here, so the
      // daemonize shape `stdio: ['ignore', logFd, logFd]` wrote to the parent's
      // console instead of the log file the caller opened.
      //
      // 0/1/2 stay 'inherit': naming this process's own std fds is exactly what
      // inherit means, and it keeps the common `stdio: [0, 1, 2]` on the cheap
      // path rather than dup'ing three descriptors to say the same thing.
      const descriptor =
        typeof entry === "number"
          ? entry
          : entry && typeof entry.fd === "number"
            ? entry.fd
            : null;
      if (descriptor !== null && Number.isInteger(descriptor) && descriptor >= 0) {
        return descriptor <= 2 ? STDIO_INHERIT : descriptor;
      }
      // A stream with no descriptor behind it (a PassThrough, a socket) cannot
      // be handed to a child as an fd; inherit remains the closest thing.
      return STDIO_INHERIT;
    }

    // node's `stdio` option -> the three direction codes spawn_child maps to
    // Stdio::null()/inherit()/piped(). Accepts the string shorthands ('pipe',
    // 'inherit', 'ignore', 'overlapped'), the array form, and absence (node's
    // default: all piped).
    //
    // Honouring 'inherit' is what lets a launcher script hand its OWN stdio to
    // a grandchild -- the npm `bin` shim shape -- instead of the child writing
    // into pipes the launcher never forwards.
    /** Shared by spawn() and fork(): both splice the 'ipc' entry out, so both
     *  have to refuse the positions where that silently renumbers fds. One
     *  implementation because two copies drift -- fork carried the bug for a
     *  while after spawn was fixed. */
    function assertIpcSlotSupported(stdioArr, ipcIndex) {
      if (ipcIndex >= 3 && ipcIndex === stdioArr.length - 1) return;
      const err = new Error(
        "stdio: 'ipc' must be the LAST entry, at index 3 or above -- got " +
          `index ${ipcIndex} of ${stdioArr.length} entries. oam carries the ` +
          "IPC channel on a loopback socket rather than an fd, so an 'ipc' " +
          "slot anywhere else renumbers the fds after it (Node instead makes " +
          "that slot itself the channel). Use stdio: ['pipe','pipe','pipe','ipc']",
      );
      err.code = "ERR_INVALID_ARG_VALUE";
      throw err;
    }

    function stdioModes(stdio) {
      if (typeof stdio === "string") {
        const code = stdioEntryCode(stdio, 0);
        return [code, code, code];
      }
      if (Array.isArray(stdio)) {
        return [
          stdioEntryCode(stdio[0], 0),
          stdioEntryCode(stdio[1], 1),
          stdioEntryCode(stdio[2], 2),
        ];
      }
      return [STDIO_PIPE, STDIO_PIPE, STDIO_PIPE];
    }

    function spawnSync(command, args, options) {
      const norm = normalizeArgs(command, args, options);
      const opts = norm.options;
      const nativeOpts = {
        cwd: opts.cwd || undefined,
        // No explicit env: hand the child the LIVE process.env view, not the
        // pristine OS environment. node's process.env writes through to the
        // real environment, so a runtime `process.env.X = v` is inherited by
        // children; oam's proxy mutates a JS-side cache, so pass that.
        env: opts.env || globalThis.process.env,
        shell: !!opts.shell,
        clearEnv: false,
        timeout: opts.timeout || 0,
        maxBuffer: opts.maxBuffer || 50 * 1024 * 1024,
        input: opts.input != null
          ? (typeof opts.input === "string" ? Buffer.from(opts.input, opts.encoding || "utf8") : opts.input)
          : undefined,
      };
      const modes = stdioModes(opts.stdio);
      nativeOpts.stdio = modes;
      const result = natives.spawnSync(norm.command, norm.args, nativeOpts);
      const encoding = opts.encoding || "buffer";
      // node reports null -- not an empty buffer -- for a slot it never piped,
      // so `spawnSync(..., {stdio: 'inherit'}).stdout === null`.
      const slot = (buf, fd) =>
        modes[fd] === STDIO_PIPE ? decodeOutput(buf, encoding) : null;
      return {
        pid: result.pid,
        output: [null, slot(result.stdout, 1), slot(result.stderr, 2)],
        stdout: slot(result.stdout, 1),
        stderr: slot(result.stderr, 2),
        status: result.status,
        signal: result.signal,
        error: result.error
          ? Object.assign(new Error(result.error.message), { code: result.error.code })
          : undefined,
      };
    }

    /** node's checkExecSyncError: the message appends stderr only when there IS
     *  stderr, and the WHOLE spawnSync result is assigned onto the error -- so
     *  `err.output` is the 3-slot [null, stdout, stderr] array that test
     *  harnesses and CI wrappers read to get both streams from one throw. */
    function execSyncError(result, command) {
      // Unconditionally appending left a trailing newline node never emits on
      // every quiet-stderr failure -- and the literal text "null" whenever the
      // caller inherited or ignored the stream.
      let message = `Command failed: ${command}`;
      if (result.stderr && result.stderr.length > 0) {
        message += `\n${result.stderr}`;
      }
      const err = new Error(message);
      return Object.assign(err, result);
    }

    function execSync(command, options) {
      const opts = Object.assign({ shell: true }, options);
      const result = spawnSync(command, [], opts);
      if (result.error) throw result.error;
      if (result.status !== 0) throw execSyncError(result, command);
      return result.stdout;
    }

    function execFileSync(file, args, options) {
      const norm = normalizeArgs(file, args, options);
      const result = spawnSync(norm.command, norm.args, norm.options);
      if (result.error) throw result.error;
      // Same error shape as execSync -- including `output` -- since callers
      // branch on it identically.
      if (result.status !== 0) throw execSyncError(result, norm.command);
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
        // A kill() issued before the native handle resolves parks here.
        this._pendingKill = null;
      }
      kill(signal) {
        if (this._handle == null) {
          // node's spawn is synchronous, so this window does not exist there.
          // It does here for an ipc-bound child, whose channel must bind before
          // exec. Dropping the request left kill() returning true, `killed`
          // false, and the child running -- with nothing the caller could
          // observe. Hold it and deliver the moment the handle lands.
          this._pendingKill = signal || "SIGTERM";
          return true;
        }
        if (this._extra) natives.spawnExtraKill(this._handle, signal);
        else natives.spawnKill(this._handle, signal);
        this.killed = true;
        return true;
      }
      /** Deliver a kill that arrived before the handle did. */
      _flushPendingKill() {
        if (this._pendingKill == null || this._handle == null) return;
        const signal = this._pendingKill;
        this._pendingKill = null;
        this.kill(signal);
      }
      ref() { return this; }
      unref() { return this; }
    }

    // Run `onReady` once the child has spawned, or invoke `callback(err)` if
    // the spawn fails -- whichever happens first. Prevents deferred stdin
    // write/final callbacks from dangling forever on a spawn error.
    function deferUntilSpawn(cp, onReady, callback) {
      // 'spawnfail' is ONE-SHOT, so a waiter registered after the failure has
      // already fired -- a write issued from inside the child's own 'error'
      // handler, the natural place to do cleanup -- would park on an event that
      // can never fire again and its callback would dangle forever. The latch
      // is what makes the late registration settle.
      if (cp._spawnFailure) {
        callback(cp._spawnFailure);
        return;
      }
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
    // (The ordinary 0/1/2-only path uses `stdioModes` above; these codes are a
    // different, richer encoding read by a different native op.)
    function stdioCode(entry, fd) {
      if (entry === "ignore") return 0;
      if (entry === null || entry === undefined) {
        // Default by fd, node's rule: 'pipe' for 0/1/2, 'ignore' beyond.
        if (fd > 2) return 0;
        entry = "pipe";
      }
      if (entry === "pipe" || entry === "overlapped") {
        if (fd === 0) return 2;
        if (fd === 1 || fd === 2) return 3;
        return fd === 3 ? 2 : 3;
      }
      // A DESCRIPTOR the caller named, by number or via a stream that owns one.
      // Encoded as base+fd because the low codes are already dispositions; the
      // native side resolves it and hands the child a dup. Left as plain
      // 'inherit', an extra slot got whatever the parent held at that index --
      // on Windows literally STD_ERROR_HANDLE for every fd above 2, so
      // `stdio: [..., logFd]` silently gave the child stderr.
      //
      // 0/1/2 still mean inherit: naming our own std fds is what that is.
      const descriptor =
        typeof entry === "number"
          ? entry
          : entry && typeof entry.fd === "number"
            ? entry.fd
            : null;
      if (descriptor !== null && Number.isInteger(descriptor) && descriptor > 2) {
        return STDIO_EXTRA_DESCRIPTOR_BASE + descriptor;
      }
      // 'inherit', fd 0/1/2, or a stream with nothing behind it.
      return 1;
    }

    // Extra-fd spawn: a child with numbered fds beyond 0/1/2 (Chromium
    // --remote-debugging-pipe needs fds 3/4 for CDP). Synchronous like Node's
    // spawn(); exposes cp.stdio[n] as Readable/Writable streams. The native
    // backend is Windows (CreateProcessW) or Unix (Command+pre_exec dup2).
    function spawnExtra(norm, stdioArr, wantsIpc, ipcIndex) {
      const opts = norm.options;
      const cp = new ChildProcess();
      cp._extra = true;
      const codes = stdioArr.map((e, i) => stdioCode(e, i));
      const nativeOpts = {
        cwd: opts.cwd || undefined,
        // No explicit env: hand the child the LIVE process.env view, not the
        // pristine OS environment. node's process.env writes through to the
        // real environment, so a runtime `process.env.X = v` is inherited by
        // children; oam's proxy mutates a JS-side cache, so pass that.
        env: opts.env || globalThis.process.env,
        clearEnv: false,
      };

      cp.stdio = new Array(stdioArr.length).fill(null);

      // Deferred, matching spawn()'s shape: an 'ipc' slot has to bind its
      // loopback channel and inject the port BEFORE exec. With no ipc slot
      // this runs synchronously, so pid stays readable the instant spawn()
      // returns (node parity, and what the CDP callers rely on).
      const launch = (extraEnv) => {
      if (extraEnv) {
        nativeOpts.env = Object.assign(
          {},
          opts.env || globalThis.process.env,
          extraEnv,
        );
      }
      let info;
      try {
        info = JSON.parse(natives.spawnExtra(norm.command, norm.args, nativeOpts, codes));
      } catch (err) {
        const e = spawnFailureError(err, norm.command, norm.args);
        // An ipc channel bound for this child would otherwise keep listening
        // forever: 'exit' never fires for a child that never started.
        if (cp._ipcTeardown) cp._ipcTeardown();
        queueMicrotask(() => cp.emit("error", e));
        return;
      }
      cp._handle = info.handle;
      cp._flushPendingKill();
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
            // NOT done at push(null) -- see readableFinished. Node fires the
            // ChildProcess 'close' only AFTER each stdio stream emits 'end';
            // settling here would let 'close' (gated on allSettled(reads))
            // race ahead of the buffered-data drain and drop 'end' on a
            // child that writes, closes, and exits within one peek cycle.
            await readableFinished(r);
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
      };

      if (wantsIpc) attachIpcChannel(cp, launch, ipcIndex);
      else launch(null);

      return cp;
    }

    // The IPC half of fork(), reusable by spawn(stdio:[...,'ipc']). Binds a
    // loopback channel, hands the port to `launch` as an env override, and
    // wires send/disconnect/'message' onto `cp`. The bind is async, so a
    // child started this way resolves its pid one tick later than a plain
    // spawn -- the same property fork() already has.
    function attachIpcChannel(cp, launch, ipcIndex) {
      let ipcSocket = null;
      const pendingSends = [];
      cp.connected = true;

      cp.send = function send(message, _sendHandle, _options, callback) {
        if (typeof _sendHandle === "function") callback = _sendHandle;
        else if (typeof _options === "function") callback = _options;
        if (!cp.connected) {
          const err = new Error("channel closed");
          err.code = "ERR_IPC_CHANNEL_CLOSED";
          if (callback) callback(err);
          return false;
        }
        const line = JSON.stringify(message) + "\n";
        if (ipcSocket) ipcSocket.write(line, "utf8", callback);
        else pendingSends.push({ line, callback });
        return true;
      };

      cp.disconnect = function disconnect() {
        cp.connected = false;
        if (ipcSocket) {
          const sock = ipcSocket;
          ipcSocket = null;
          sock.destroy();
        }
        cp.emit("disconnect");
      };

      const net = registry.get("net");
      const ipcServer = net.createServer();
      // Node reports the ipc slot as null in child.stdio, at the index the
      // caller put 'ipc' at -- only 3 in the common case, so hardcoding 3
      // clobbered a REAL extra fd on the numbered-fd path.
      //
      // Guarded to indices at or above 3: below that the splice has already
      // renumbered the surviving fds, so slot 0/1/2 now holds a live, actively
      // pumped stream and nulling it would leave child.stdio[n] disagreeing
      // with child.stdout/stderr about the same fd.
      const slot = ipcIndex ?? 3;
      if (Array.isArray(cp.stdio) && slot >= 3) cp.stdio[slot] = null;

      ipcServer.on("connection", (socket) => {
        ipcSocket = socket;
        // One channel per child; stop accepting so the listener does not pin
        // the parent's event loop open.
        ipcServer.close();
        for (const p of pendingSends) socket.write(p.line, "utf8", p.callback);
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
              cp.emit("message", JSON.parse(line));
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

      ipcServer.listen(0, "127.0.0.1", () => {
        launch({ OAM_FORK_IPC_PORT: String(ipcServer.address().port) });
      });

      // Never leave the listener or socket holding the loop open. Exposed on
      // `cp` because a spawn that FAILS emits 'error' and never 'exit', so the
      // launch-failure paths have to tear this down explicitly -- and they
      // cannot do it by listening for 'error' here, since an internal listener
      // would swallow node's throw-on-unhandled-'error'.
      cp._ipcTeardown = () => {
        try { ipcServer.close(); } catch (_) { /* already closed */ }
        // Unconditional: on the FAILED-spawn path -- the case this was added
        // for -- no socket ever connected, so gating this on `ipcSocket` left
        // `connected` true. cp.send() then kept returning true and queueing
        // into pendingSends forever, with every callback unsettled.
        cp.connected = false;
        if (ipcSocket) {
          const sock = ipcSocket;
          ipcSocket = null;
          try { sock.destroy(); } catch (_) { /* already gone */ }
        }
        // Settle anything queued before the channel died; node delivers EPIPE
        // to a send callback on a closed channel rather than dropping it.
        if (pendingSends.length > 0) {
          const queued = pendingSends.splice(0, pendingSends.length);
          for (const p of queued) {
            if (p.callback) {
              const err = new Error("channel closed");
              err.code = "EPIPE";
              p.callback(err);
            }
          }
        }
      };
      cp.on("exit", () => {
        cp._ipcTeardown();
      });
    }

    // Resolve once a child's stdio Readable has actually FINISHED, for the
    // 'close' ordering below.
    //
    // `push(null)` marks EOF; it is not the point the stream is done. The
    // Readable emits 'end' from a process.nextTick, while the pump's promise
    // continues on the microtask queue -- so a pump that resolves at push(null)
    // leaves "does 'end' beat 'close'?" up to nextTick-vs-microtask draining.
    // That held on Windows and macOS and inverted on a loaded Linux box, where
    // the child's 'close' fired first and `stdout.on('end')` had not run.
    // node's order is invariant when stdout is consumed (measured, 12B and
    // 2MB): stdout 'end' -> stdout 'close' -> child 'exit' -> child 'close'.
    //
    // Waiting unconditionally would HANG the common ignore-the-output spawn: a
    // Readable nobody reads never emits 'end', it just sits paused holding its
    // buffer, and node still closes that child (measured: unconsumed 12B gives
    // `exit close`). So wait only when something is really consuming -- a
    // 'data' listener or a pipe (readableFlowing === true), or a 'readable'
    // consumer / async iterator (which leaves flowing false, hence the
    // listener check). 'close'/'error' are accepted as terminal too, so a
    // destroyed stream cannot strand the child.
    const readableFinished = (stream) =>
      new Promise((resolve) => {
        if (!stream || stream.readableEnded || stream.destroyed) {
          resolve();
          return;
        }
        const consuming =
          stream.readableFlowing === true || stream.listenerCount("readable") > 0;
        if (!consuming) {
          resolve();
          return;
        }
        stream.once("end", resolve);
        stream.once("close", resolve);
        stream.once("error", resolve);
      });

    function spawn(command, args, options) {
      const norm = normalizeArgs(command, args, options);
      const opts = norm.options;
      // stdio: [..., 'ipc'] asks for a message channel, the same one fork()
      // sets up. Splice the slot out FIRST so a 4-entry array does not get
      // misrouted to the extra-fd path below; Node reports stdio[3] as null.
      // Derived into a LOCAL, never written back onto `opts`: that object is the
      // caller's, and reusing one options literal to spawn several children --
      // the worker-pool shape -- meant the first spawn spliced 'ipc' out of it
      // and every later child silently came up with no channel at all
      // (cp.send undefined).
      let wantsIpc = false;
      let ipcIndex = -1;
      let stdioSpec = opts.stdio;
      if (Array.isArray(stdioSpec)) {
        ipcIndex = stdioSpec.indexOf("ipc");
        if (ipcIndex !== -1) {
          wantsIpc = true;
          // Node turns the slot the caller put 'ipc' AT into the channel fd:
          // stdio:['pipe','ipc','pipe'] means the child's fd 1 IS the channel,
          // so the child's stdout writes arrive as IPC frames and take the
          // parent down on a JSON parse. oam cannot model that at all -- its
          // channel rides a loopback socket, never an fd -- so the entry has to
          // come out of the array, and removing it RENUMBERS every fd after it.
          //
          // The renumbering is a no-op in exactly one shape: 'ipc' LAST, at
          // index 3 or above, where nothing follows it and the three standard
          // slots keep their numbers. Anywhere else the removal is a silently
          // wrong answer -- a mis-numbered CDP pipe that fails far from its
          // cause, or a child handed an ordinary stdout where Node would have
          // handed it the channel -- so refuse loudly rather than guess.
          //
          // Checked here, before the splice and against the CALLER's array, so
          // it covers every array form. It used to sit inside the extra-fd
          // branch below, which left an 'ipc' below index 3 -- the ordinary
          // 3-slot path -- renumbering in silence.
          assertIpcSlotSupported(stdioSpec, ipcIndex);
          stdioSpec = stdioSpec.slice();
          stdioSpec.splice(ipcIndex, 1);
        }
      }
      // Extra-fd stdio (Chromium CDP pipe): an array with >3 entries routes to
      // the raw extra-fd spawn (CreateProcessW+lpReserved2 on Windows, Command+
      // pre_exec dup2 on Unix). Gated to platforms with a real native backend.
      if (
        Array.isArray(stdioSpec) &&
        stdioSpec.length > 3 &&
        (natives.platform === "win32" ||
          natives.platform === "linux" ||
          natives.platform === "darwin")
      ) {
        // No 'ipc' position check here: the guard above already rejected every
        // placement that would renumber these extra fds, so anything reaching
        // this line has 'ipc' last (or none at all) and `stdioSpec` is already
        // the correctly-numbered array.
        return spawnExtra(norm, stdioSpec, wantsIpc, ipcIndex);
      }
      const cp = new ChildProcess();
      const modes = stdioModes(stdioSpec);

      // Only a PIPED slot gets a stream. node reports null for 'inherit' and
      // 'ignore' -- and a Readable we never push to would otherwise sit
      // un-ended forever, so the shape and the plumbing agree here.
      cp.stdout = modes[1] === STDIO_PIPE ? new Readable({ read() {} }) : null;
      cp.stderr = modes[2] === STDIO_PIPE ? new Readable({ read() {} }) : null;
      cp.stdin = modes[0] !== STDIO_PIPE ? null : new Writable({
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
      // node exposes the same three objects as child.stdio; the ipc slot, when
      // one is attached, lands at [3] as null.
      cp.stdio = [cp.stdin, cp.stdout, cp.stderr];

      const nativeOpts = {
        stdio: modes,
        cwd: opts.cwd || undefined,
        // No explicit env: hand the child the LIVE process.env view, not the
        // pristine OS environment. node's process.env writes through to the
        // real environment, so a runtime `process.env.X = v` is inherited by
        // children; oam's proxy mutates a JS-side cache, so pass that.
        env: opts.env || globalThis.process.env,
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
        // NOT done at push(null) -- see readableFinished.
        await readableFinished(cp.stdout);
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
        await readableFinished(cp.stderr);
      };

      // SYNCHRONOUS spawn (node parity): pid must be readable the instant
      // spawn() returns. The native op throws on failure; the 'spawn' event
      // and everything after it stay ASYNC, as node emits them.
      //
      // The one exception is stdio:'ipc' below, which must know the channel
      // port before exec'ing and so binds first; that shape resolves pid a
      // tick later (fork() has the same property today).
      const launch = (extraEnv) => {
      if (extraEnv) {
        nativeOpts.env = Object.assign(
          {},
          opts.env || globalThis.process.env,
          extraEnv,
        );
      }
      let info;
      try {
        info = JSON.parse(natives.spawnAsync(norm.command, norm.args, nativeOpts));
      } catch (err) {
        info = null;
        queueMicrotask(() => handleSpawnFailure(err));
      }
      if (info) {
        cp._handle = info.handle;
      cp._flushPendingKill();
        cp.pid = info.pid;

        queueMicrotask(() => {
        cp.emit("spawn");

        // An inherited or ignored slot has no pipe to drain: the child owns
        // the parent's fd directly, so there is nothing for this process to
        // pump and no read to wait on before 'close'.
        const pumps = [];
        if (cp.stdout) pumps.push(readStdout(info.handle));
        if (cp.stderr) pumps.push(readStderr(info.handle));

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
          Promise.allSettled(pumps).then(() => {
            cp.emit("close", result.code, result.signal);
          });
        });
        });
      }
      };
      if (wantsIpc) {
        attachIpcChannel(cp, launch, ipcIndex);
      } else {
        launch(null);
      }
      function handleSpawnFailure(err) {
        const e = spawnFailureError(err, norm.command, norm.args);
        // The stdin side gets a STREAM-shaped error, not the child's ENOENT:
        // node settles the write that was already in flight with EPIPE, then
        // destroys the stream so anything written later fails with
        // ERR_STREAM_DESTROYED. Handing the spawn error to a write callback
        // reports the wrong failure to the wrong layer.
        const pipeErr = new Error("write EPIPE");
        pipeErr.code = "EPIPE";
        pipeErr.syscall = "write";
        // Latched so a waiter that registers AFTER this point still settles --
        // 'spawnfail' below is one-shot. See deferUntilSpawn.
        cp._spawnFailure = pipeErr;
        // An ipc channel bound for this child would otherwise keep listening
        // forever: 'exit' never fires for a child that never started.
        if (cp._ipcTeardown) cp._ipcTeardown();
        // The pending stdin write is settled by handing this error to its
        // write callback, which ERRORS the stream. node settles the callback
        // but never surfaces a spawn failure as a stdin 'error', and callers
        // listen on the CHILD -- so without this guard the most ordinary shape
        // (`cp.on('error', h); cp.stdin.end(payload)`) died on an uncaught
        // stream error even though the caller handled everything correctly.
        // Attached only on this path, so a real EPIPE still propagates.
        if (cp.stdin) cp.stdin.on("error", () => {});
        // Settle deferred stdin write/final waiters, then end the read streams
        // so consumers awaiting completion don't hang on a failed spawn.
        cp.emit("spawnfail", pipeErr);
        // Destroyed AFTER the in-flight write is settled, so a later write gets
        // ERR_STREAM_DESTROYED from the stream itself -- node's exact split.
        if (cp.stdin) cp.stdin.destroy();
        if (cp.stdout) cp.stdout.push(null);
        if (cp.stderr) cp.stderr.push(null);
        queueMicrotask(() => {
          cp.emit("error", e);
          // node follows 'error' with 'close' (code = the libuv errno) for a
          // child that never started. Consumers whose completion path is
          // 'close' -- the shape every run-a-tool-then-continue wrapper uses --
          // otherwise stall instead of taking their error branch.
          queueMicrotask(() => {
            cp.emit("close", typeof e.errno === "number" ? e.errno : null, null);
          });
        });
      }

      return cp;
    }

    function exec(command, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      const opts = Object.assign({ shell: true }, options);
      // node's exec/execFile hand spawn an explicit option WHITELIST (cwd, env,
      // gid, shell, signal, uid, windowsHide, windowsVerbatimArguments) and drop
      // `stdio` on the floor -- exec owns the pipes, because collecting stdout
      // for the callback is the whole contract. Forwarding it would make
      // `exec(cmd, {stdio:'inherit'})` hand back null streams where node hands
      // back real ones. (execSync/spawnSync DO honor stdio; only async exec
      // does not -- `execSync(cmd, {stdio:'inherit'})` is a normal idiom.)
      const spawnOpts = Object.assign({}, opts);
      delete spawnOpts.stdio;
      return collectExec(spawn(command, [], spawnOpts), command, opts, callback);
    }

    // The collector half of exec()/execFile(). Both own their pipes and deliver
    // (err, stdout, stderr) to a callback; they differ ONLY in how the child is
    // launched -- exec through a shell, execFile emphatically not -- so the
    // buffering, maxBuffer accounting and error shaping live here once.
    function collectExec(cp, command, opts, callback) {
      const stdout = [];
      const stderr = [];
      const maxBuffer = opts.maxBuffer || 50 * 1024 * 1024;
      let stdoutLen = 0;
      let stderrLen = 0;
      let maxBufferError = null;
      const timeout = Number(opts.timeout) > 0 ? Number(opts.timeout) : 0;
      let timer = null;
      let timedOut = false;

      // Running totals, NOT Buffer.concat(...).length per chunk: concatenating
      // everything accumulated so far on every chunk is quadratic, and at the
      // 50MB default that is gigabytes of copying for one noisy command. node
      // keeps the same two counters.
      //
      // On overflow node delivers EXACTLY maxBuffer bytes: the chunk that
      // crosses the limit is TRUNCATED to the remaining allowance and kept,
      // then the child is killed. Dropping that chunk whole instead hands the
      // caller whatever the pipe happened to coalesce -- a shorter, run-
      // dependent prefix -- and the prefix is the point, since it is what
      // diagnostics print when output was too big to keep.
      const overflow = (which) => {
        if (maxBufferError) return;
        maxBufferError = new Error(`${which} maxBuffer length exceeded`);
        maxBufferError.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
        cp.kill();
      };

      cp.on("spawn", () => {
        // Null when the caller passed stdio:'inherit'/'ignore' through: there
        // is no pipe to collect from, and node's exec guards the same way.
        cp.stdout?.on("data", (chunk) => {
          const used = stdoutLen;
          stdoutLen += chunk.length;
          if (stdoutLen > maxBuffer) {
            const room = maxBuffer - used;
            if (room > 0) stdout.push(chunk.subarray(0, room));
            overflow("stdout");
          } else stdout.push(chunk);
        });
        // maxBuffer binds BOTH streams in node. Enforcing it on stdout alone
        // let a child that only spews to stderr grow this array without limit.
        cp.stderr?.on("data", (chunk) => {
          const used = stderrLen;
          stderrLen += chunk.length;
          if (stderrLen > maxBuffer) {
            const room = maxBuffer - used;
            if (room > 0) stderr.push(chunk.subarray(0, room));
            overflow("stderr");
          } else stderr.push(chunk);
        });
        // `timeout` was accepted and ignored, so the standard way to bound a
        // shell-out (health checks, version probes, git calls) simply did not
        // bound it: a child that hung hung the caller forever. node arms the
        // timer once the child is running and kills with killSignal.
        if (timeout > 0) {
          timer = setTimeout(() => {
            timedOut = true;
            cp.kill(opts.killSignal || "SIGTERM");
          }, timeout);
        }
      });
      cp.on("close", (code, signal) => {
        if (timer) clearTimeout(timer);
        const stdoutBuf = Buffer.concat(stdout);
        const stderrBuf = Buffer.concat(stderr);
        const enc = opts.encoding || "utf8";
        const out = enc === "buffer" ? stdoutBuf : stdoutBuf.toString(enc);
        const errOut = enc === "buffer" ? stderrBuf : stderrBuf.toString(enc);
        // A maxBuffer kill wins over the generic "Command failed": the child
        // died because WE killed it, so the signal-shaped failure below would
        // describe the symptom rather than the cause.
        if (maxBufferError && callback) {
          callback(maxBufferError, out, errOut);
        } else if ((code !== 0 || timedOut) && callback) {
          const err = new Error(`Command failed: ${command}\n${errOut}`);
          err.code = code;
          // node reports a timed-out child as killed, with the signal it sent,
          // so `err.killed` is how callers tell "the command failed" from "we
          // gave up waiting".
          err.killed = timedOut || cp.killed;
          err.signal = timedOut ? opts.killSignal || "SIGTERM" : signal;
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
      const opts = norm.options || {};
      // node's execFile runs WITHOUT a shell and passes argv VERBATIM -- that is
      // the whole reason it sits next to exec(). Joining argv into one string
      // and shelling out re-splits arguments on whitespace (so a path or value
      // containing a space breaks) and EXECUTES shell metacharacters that
      // happen to appear inside an argument. `shell` stays honored if the
      // caller explicitly asks for one, which node also supports.
      const spawnOpts = Object.assign({}, opts, { shell: !!opts.shell });
      delete spawnOpts.stdio;
      const display = norm.args.length
        ? `${norm.command} ${norm.args.join(" ")}`
        : norm.command;
      return collectExec(
        spawn(norm.command, norm.args, spawnOpts),
        display,
        opts,
        callback,
      );
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

      // An explicit stdio array must still declare its channel. oam builds one
      // regardless (it rides a loopback socket, not an fd), so accepting the
      // array without 'ipc' would let code be written and tested here that
      // throws the moment it runs on node -- node's guard is the whole value,
      // and silently succeeding is what removes it.
      if (Array.isArray(opts.stdio) && opts.stdio.includes("ipc")) {
        // fork splices the entry out exactly as spawn does, so it owes the
        // caller the same refusal: an 'ipc' below index 3 renumbers the child's
        // fds, and Node would have made that very slot the channel.
        assertIpcSlotSupported(opts.stdio, opts.stdio.indexOf("ipc"));
      }
      if (Array.isArray(opts.stdio) && !opts.stdio.includes("ipc")) {
        const err = new TypeError(
          "Forked processes must have an IPC channel, missing value 'ipc' in options.stdio",
        );
        err.code = "ERR_CHILD_PROCESS_IPC_REQUIRED";
        throw err;
      }

      const cp = new ChildProcess();
      cp.connected = true;

      // node's fork default is 'inherit' -- a forked child's console output
      // shows up on the PARENT's stdout -- and only `silent: true` pipes it.
      // An explicit `stdio` option overrides both. The 'ipc' slot is dropped
      // here because oam carries the fork channel over a loopback socket
      // rather than an inherited fd.
      const forkStdio = Array.isArray(opts.stdio)
        ? opts.stdio.filter((e) => e !== "ipc")
        : opts.stdio;
      const modes =
        forkStdio === undefined
          ? (silent
              ? [STDIO_PIPE, STDIO_PIPE, STDIO_PIPE]
              : [STDIO_INHERIT, STDIO_INHERIT, STDIO_INHERIT])
          : stdioModes(forkStdio);

      cp.stdout = modes[1] === STDIO_PIPE ? new Readable({ read() {} }) : null;
      cp.stderr = modes[2] === STDIO_PIPE ? new Readable({ read() {} }) : null;
      // stdin is wired after the spawn resolves (below), like the pid.
      cp.stdin = null;
      cp.stdio = [null, cp.stdout, cp.stderr, null];

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
          stdio: modes,
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

        // Synchronous spawn (node parity: fork() also yields a live pid
        // immediately). Everything after stays async.
        let info = null;
        try {
          info = JSON.parse(natives.spawnAsync(execPath, spawnArgs, nativeOpts));
        } catch (err) {
          ipcServer.close();
          // Same Node-shaped error as spawn()/spawnExtra(): this catch was
          // missed when the others were converted, so a failed fork still
          // surfaced the raw native JSON body with err.code undefined.
          const e = spawnFailureError(err, execPath, spawnArgs);
          cp.connected = false;
          // End the piped streams and follow node's error -> close ordering, so
          // a consumer awaiting either does not hang on a fork that never ran.
          if (cp.stdout) cp.stdout.push(null);
          if (cp.stderr) cp.stderr.push(null);
          queueMicrotask(() => {
            cp.emit("error", e);
            queueMicrotask(() => {
              cp.emit("close", typeof e.errno === "number" ? e.errno : null, null);
            });
          });
        }
        if (info) {
          cp._handle = info.handle;
      cp._flushPendingKill();
          cp.pid = info.pid;
          queueMicrotask(() => {

          if (modes[0] === STDIO_PIPE) {
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
            cp.stdio[0] = cp.stdin;
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
            await readableFinished(cp.stdout);
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
            await readableFinished(cp.stderr);
          };
          // Nothing to drain for an inherited or ignored slot -- the child
          // holds the parent's fd directly.
          const pumps = [];
          if (cp.stdout) pumps.push(readStdout(info.handle));
          if (cp.stderr) pumps.push(readStderr(info.handle));

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
            Promise.allSettled(pumps).then(() => {
              cp.emit("close", result.code, result.signal);
            });
          });
          });
        }
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
        // The http2 layer manages this stream's close lifecycle; opt out of
        // Duplex autoDestroy to keep the prior behavior.
        super({ allowHalfOpen: true, autoDestroy: false });
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
        super({ allowHalfOpen: true, autoDestroy: false });
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
        super({ autoDestroy: false });
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
              this.push(globalThis.Buffer.from(data.buffer, data.byteOffset, data.length));
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
    const _untransferableSet = untransferable;

    class MessagePort extends EventEmitter {
      constructor() { super(); this._twin = null; this._active = true; }
      postMessage(data, transferList) {
        if (Array.isArray(transferList)) {
          for (const item of transferList) {
            if (untransferable.has(item)) {
              throw new DOMException(
                "Cannot transfer object of unsupported type.",
                "DataCloneError",
              );
            }
          }
        }
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
        // node accepts a string path OR a URL (the `new Worker(new URL("./w.js",
        // import.meta.url))` idiom its own docs use); only a string was taken
        // here, so the documented ESM spelling threw.
        if (filename instanceof URL || (typeof filename === "string" && filename.startsWith("file:"))) {
          filename = registry.get("url").fileURLToPath(filename);
        } else if (typeof filename !== "string") {
          throw nodeTypeError(
            "The \"filename\" argument must be of type string or an instance of URL. " +
              `Received ${describeArg(filename)}`,
          );
        }
        const resolved = pathMod.resolve(filename);
        const wd = opts.workerData !== undefined ? JSON.stringify(opts.workerData) : null;

        // Store worker name (Node >=12.11 option)
        this.name = opts.name || "";

        // Note SHARE_ENV usage (workers share process env by default in oam)
        this._shareEnv = opts.env === SHARE_ENV;

        // Node always exposes worker.stdout/.stderr; the option controls
        // whether the worker's output is ROUTED here instead of going
        // straight to the parent's console.
        const { Readable } = registry.get("stream");
        this.stdout = new Readable({ read() {} });
        this.stderr = new Readable({ read() {} });

        const result = natives.workerNew(
          resolved,
          wd,
          JSON.stringify({
            stdout: opts.stdout === true,
            stderr: opts.stderr === true,
            // Absent means INHERIT the parent's execArgv (Node's default);
            // an explicit [] means run the worker with no options.
            execArgv: Array.isArray(opts.execArgv)
              ? opts.execArgv.map(String)
              : (process.execArgv || []).map(String),
          }),
        );
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
            if (raw.type === "stdout" || raw.type === "stderr") {
              // latin1: the Rust side encoded each byte as one code unit.
              this[raw.type].push(Buffer.from(raw.data, "latin1"));
            }
            if (raw.type === "exit") {
              // EOF both streams so a waiting 'end' listener fires.
              this.stdout.push(null);
              this.stderr.push(null);
              this.emit("exit", raw.code);
              break;
            }
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
      // Start the receive loop LAZILY. It awaits a message that may never
      // come, and that pending op keeps the worker's event loop alive -- so
      // starting it eagerly meant any worker that merely required
      // worker_threads never exited, and a parent waiting on 'exit' hung
      // forever. Node refs the port when a 'message' listener is attached
      // (or start() is called), which is what this mirrors.
      let recvLoopStarted = false;
      const startRecvLoop = () => {
        if (recvLoopStarted) return;
        recvLoopStarted = true;
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
      };
      parentPort.on("newListener", (type) => {
        if (type === "message") startRecvLoop();
      });
      parentPort.start = function start() {
        startRecvLoop();
        return this;
      };
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
    // Captured real setTimeout (immune to a test swapping the global) used to
    // defer the auto-run to a macrotask.
    const realSetTimeout = globalThis.setTimeout;

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
      // A MACROTASK (not a microtask) so the run starts only after the test
      // file has FULLY evaluated -- including ESM top-level await, whose
      // continuations are microtasks that must all drain first. A pending
      // timer also keeps the event loop alive until the run begins.
      realSetTimeout(() => {
        runRoot();
      }, 0);
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
      let msg = e instanceof Error ? e.stack || e.message : String(e);
      // Drop oam's runner-internal frames (runTestEntry / runSuiteTree /
      // runRoot / runSubtest): the factory is evaluated inside the snapshot
      // with no resource name, so its frames read '<anonymous>'. Node's
      // node:test never prints them. User test files always carry a real path.
      msg = String(msg)
        .split("\n")
        .filter((line) => {
          const t = line.trim();
          if (!t.startsWith("at ")) return true; // keep the header + message
          // Runtime-internal frames are noise in a user-facing report. They
          // carry real origins now, so identifying them by "<anonymous>"
          // alone would let every oam: and node: frame through.
          return !t.includes("<anonymous>") && !t.includes("oam:") && !t.includes("node:");
        })
        .join("\n");
      log("  ---");
      for (const line of msg.split("\n")) log("  " + line);
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

    // require('node:test') / `import test from 'node:test'`: the module IS the
    // test function, with the other helpers attached as properties (Node
    // semantics -- test-util-text-decoder does `require('node:test')(...)`).
    Object.assign(test, {
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
    });
    return test;
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
    //
    // The same block also upgrades setTimeout/setInterval/setImmediate to
    // return Node's Timeout/Immediate OBJECTS (the native ops return a bare
    // uint32 id). The object carries .ref()/.unref()/.hasRef()/.refresh(),
    // [Symbol.toPrimitive] (the id, so clearTimeout(t) coercion + object-key
    // stringification work), [Symbol.dispose] (clears the timer), and
    // ._destroyed -- and it is registered in a JS-side active-timer map that
    // backs process.getActiveResourcesInfo() / process._getActiveHandles().
    //
    // ref/unref are Node-faithful: .unref() calls the native timerUnref(id),
    // which clears the timer's ref flag in the Rust TimerQueue. The event loop
    // stays alive only for ref'd timers (and inflight ops), so an unref'd timer
    // that is the sole remaining work does NOT fire and does NOT block exit --
    // but an unref'd timer still fires while other work keeps the loop open.
    // hasRef() reflects the JS-side _ref flag, kept in lockstep with native.
    {
      const bindToCurrentFrame = (fn) => {
        if (typeof fn !== "function") return fn;
        const frame = natives.getContinuationData();
        return function (...args) {
          const prev = natives.getContinuationData();
          natives.setContinuationData(frame);
          try {
            return Reflect.apply(fn, this, args);
          } finally {
            natives.setContinuationData(prev);
          }
        };
      };

      const nativeSetTimeout = globalThis.setTimeout;
      const nativeSetInterval = globalThis.setInterval;
      const nativeClearTimeout = globalThis.clearTimeout;
      const nativeClearInterval = globalThis.clearInterval;
      // Native ref/unref flip the TimerQueue ref flag by id so an unref'd timer
      // stops keeping the event loop alive. Tolerate their absence (older snap).
      const nativeTimerRef = typeof natives.timerRef === "function" ? natives.timerRef : null;
      const nativeTimerUnref =
        typeof natives.timerUnref === "function" ? natives.timerUnref : null;
      const applyNativeRef = (id, isRef) => {
        if (!id) return;
        if (isRef) {
          if (nativeTimerRef) nativeTimerRef(id);
        } else if (nativeTimerUnref) {
          nativeTimerUnref(id);
        }
      };

      // Active-timer registry: numeric id -> Timeout/Immediate object. Backs
      // process.getActiveResourcesInfo() and the _getActive* introspection.
      const activeTimers = new Map();
      registry._activeTimers = activeTimers;

      function makeTimerError(name) {
        return new codes.ERR_INVALID_ARG_TYPE(name, "function", undefined);
      }

      // A Timeout/Immediate handle. `kind` is "Timeout" or "Immediate";
      // `repeat` true for setInterval.
      function Timeout(callback, kind, repeat, delay, args) {
        if (typeof callback !== "function") throw makeTimerError("callback");
        // Node clamps delays over TIMEOUT_MAX (2^31-1) to 1 with a
        // TimeoutOverflowWarning (getTimerDuration).
        if (typeof delay === "number" && delay > 2147483647) {
          process.emitWarning(
            delay + " does not fit into a 32-bit signed integer." +
              "\nTimeout duration was set to 1.",
            "TimeoutOverflowWarning",
          );
          delay = 1;
        }
        this._kind = kind;
        this._repeat = repeat;
        this._delay = delay;
        this._args = args;
        this._origCallback = callback;
        // Node's legacy handle field: user code clears timers by nulling it
        // (timers.unenroll / test-timers-unenroll-unref-interval); the fire
        // closure re-checks it on every fire.
        this._onTimeout = callback;
        // Domain capture at SCHEDULE time (Node async_wrap parity): a timer
        // created inside domain.run() fires inside that domain, and a throw
        // routes to the domain's error handler, not uncaughtException.
        // process.domain is undefined until require('domain') installs it.
        this._domain = (typeof process !== "undefined" && process.domain) || null;
        this._ref = true;
        this._destroyed = false;
        this._idleTimeout = repeat ? delay : delay;
        this._id = 0;
        this._schedule();
      }
      Timeout.prototype._schedule = function _schedule() {
        const self = this;
        // Repeat-ness is re-evaluated on EVERY fire (Node's listOnTimeout):
        // mutating t._repeat converts a timeout into an interval and back.
        // Capture how this schedule was armed to detect the conversion.
        const scheduledAsRepeat = !!this._repeat;
        // Frame-bind a trampoline, not the ctor callback: Node invokes the
        // CURRENT _onTimeout on each fire (user code can swap it), while the
        // ALS frame is still captured at schedule time. Reflect.apply, not
        // .apply -- node tolerates callbacks whose own call/apply props are
        // poisoned (test-timers-user-call).
        const bound = bindToCurrentFrame(function (...a) {
          return Reflect.apply(self._onTimeout, this, a);
        });
        const wrapped = function () {
          // Node consults the handle's legacy fields before every fire: a
          // cleared/unenrolled handle (falsy _onTimeout or _idleTimeout -1)
          // is skipped and dropped even if the native timer already fired.
          if (!self._onTimeout || self._idleTimeout === -1) {
            (scheduledAsRepeat ? nativeClearInterval : nativeClearTimeout)(self._id);
            activeTimers.delete(self._id);
            self._destroyed = true;
            return;
          }
          // Node invokes the callback with the handle as `this`, and keeps a
          // one-shot's _destroyed === false DURING the callback so an in-callback
          // self.refresh() can re-arm it (self-rescheduling heartbeat/poll loops).
          // Only AFTER the callback returns -- and only if it was not refreshed/
          // rescheduled in the meantime (the _id is unchanged) -- is a spent
          // one-shot marked destroyed and dropped from the active set.
          const firedId = self._id;
          const dom = self._domain;
          // Node DEQUEUES an Immediate before running it: the immediate queue
          // entry is popped, then the callback runs, so a callback observing
          // process.getActiveResourcesInfo() never sees its OWN Immediate.
          // (Probed on v22.22.2: with 3 pending immediates, callback #1 sees 2
          // "Immediate" entries, #2 sees 1, #3 sees 0.) A Timeout is the
          // opposite -- it stays active for the duration of its own callback.
          if (self._kind === "Immediate") activeTimers.delete(firedId);
          try {
            // Node's domain wrap: enter before the callback, exit ONLY on
            // the success path. A throw unwinds with the domain still
            // entered, so the fatal dispatcher (__oamDispatchUncaught) sees
            // process.domain === the throwing callback's domain and routes
            // there -- and the engine's handled-throw tick-deferral applies
            // to domain-handled throws too (Node ordering).
            if (dom) dom.enter();
            const r = bound.apply(self, self._args);
            if (dom) dom.exit();
            return r;
          } finally {
            if (self._id === firedId) {
              if (!scheduledAsRepeat) {
                if (self._kind !== "Immediate" && self._repeat && self._idleTimeout !== -1 && self._onTimeout) {
                  // timeout -> interval conversion: Node re-inserts with
                  // _repeat as the new duration. Immediates are excluded --
                  // setting ._repeat on a fired setImmediate must not arm
                  // an endless interval.
                  nativeClearTimeout(self._id);
                  activeTimers.delete(self._id);
                  self._delay = self._idleTimeout = self._repeat;
                  self._schedule();
                } else {
                  self._destroyed = true;
                  activeTimers.delete(self._id);
                }
              } else if (!self._repeat || self._idleTimeout === -1 || !self._onTimeout) {
                // Interval stopped from inside its own callback (cleared,
                // unenrolled, or _repeat nulled): the native interval would
                // re-arm -- cancel it now like Node's re-insert model.
                nativeClearInterval(self._id);
                activeTimers.delete(self._id);
                self._destroyed = true;
              }
            }
          }
        };
        const native = this._repeat ? nativeSetInterval : nativeSetTimeout;
        this._id = native(wrapped, this._delay, ...this._args);
        this._destroyed = false;
        // A fresh native timer is ref'd by default; re-apply an unref'd state so
        // refresh()/re-schedule preserves the handle's ref flag (Node parity).
        if (!this._ref) applyNativeRef(this._id, false);
        activeTimers.set(this._id, this);
      };
      Timeout.prototype.ref = function ref() {
        this._ref = true;
        applyNativeRef(this._id, true);
        return this;
      };
      Timeout.prototype.unref = function unref() {
        this._ref = false;
        applyNativeRef(this._id, false);
        return this;
      };
      Timeout.prototype.hasRef = function hasRef() {
        return this._ref;
      };
      Timeout.prototype.refresh = function refresh() {
        // An explicitly-cleared timer (clearTimeout/close set _idleTimeout = -1)
        // is terminal -- refresh() is a no-op, matching Node. A timer that merely
        // FIRED keeps a non-negative _idleTimeout and re-arms here, so a
        // self-rescheduling setTimeout(..., t.refresh()) loop keeps running.
        if (this._idleTimeout < 0) return this;
        const native = this._repeat ? nativeClearInterval : nativeClearTimeout;
        native(this._id);
        activeTimers.delete(this._id);
        this._schedule();
        return this;
      };
      Timeout.prototype.close = function close() {
        const native = this._repeat ? nativeClearInterval : nativeClearTimeout;
        native(this._id);
        activeTimers.delete(this._id);
        this._destroyed = true;
        this._idleTimeout = -1; // terminal: a later refresh() must no-op (Node parity)
        return this;
      };
      Timeout.prototype[Symbol.toPrimitive] = function () {
        return this._id;
      };
      Timeout.prototype[Symbol.dispose] = function () {
        this.close();
      };

      const idOf = (handle) => {
        if (handle != null && typeof handle === "object" && typeof handle._id === "number") {
          return handle._id;
        }
        return handle;
      };

      globalThis.setTimeout = function setTimeout(callback, delay, ...args) {
        if (typeof callback !== "function") throw makeTimerError("callback");
        return new Timeout(callback, "Timeout", false, delay, args);
      };
      globalThis.setInterval = function setInterval(callback, delay, ...args) {
        if (typeof callback !== "function") throw makeTimerError("callback");
        return new Timeout(callback, "Timeout", true, delay, args);
      };
      // Node's Immediate is a distinct class (immediate.constructor.name ===
      // 'Immediate'); subclass Timeout so behavior/instanceof stay identical.
      function Immediate(callback, args) {
        Timeout.call(this, callback, "Immediate", false, 0, args);
      }
      Immediate.prototype = Object.create(Timeout.prototype);
      Object.defineProperty(Immediate.prototype, "constructor", {
        value: Immediate,
        writable: true,
        configurable: true,
      });
      globalThis.setImmediate = function setImmediate(callback, ...args) {
        if (typeof callback !== "function") throw makeTimerError("callback");
        return new Immediate(callback, args);
      };
      // Shared clear path -- captured natives, so it keeps working even after
      // a test deletes the globals (test-timers-api-refs).
      const clearTimer = (handle, nativeClear) => {
        const id = idOf(handle);
        if (handle != null && typeof handle === "object" && handle._id !== undefined) {
          handle._destroyed = true;
          handle._idleTimeout = -1; // terminal: a later refresh() must no-op (Node parity)
          activeTimers.delete(handle._id);
        } else {
          activeTimers.delete(id);
        }
        nativeClear(id);
      };
      globalThis.clearTimeout = function clearTimeout(handle) {
        clearTimer(handle, nativeClearTimeout);
      };
      globalThis.clearInterval = function clearInterval(handle) {
        clearTimer(handle, nativeClearInterval);
      };
      globalThis.clearImmediate = function clearImmediate(handle) {
        clearTimer(handle, nativeClearTimeout);
      };
      registry._Timeout = Timeout;
      // Exported for process.nextTick's per-entry ALS frame binding: each
      // queued tick captures the frame of ITS nextTick() call, not the frame
      // of whichever call scheduled the drain microtask.
      registry._bindToCurrentFrame = bindToCurrentFrame;
      // Frame-as-data, for callers that must not pay a wrapper closure's
      // stack frame (process.nextTick -- see the tick drain).
      registry._captureContinuationFrame = () => natives.getContinuationData();
      registry._setContinuationFrame = (frame) => {
        const prev = natives.getContinuationData();
        natives.setContinuationData(frame);
        return prev;
      };

      // queueMicrotask still just needs ALS frame-binding.
      {
        const native = globalThis.queueMicrotask;
        if (typeof native === "function") {
          // Captured native kept for internal schedulers that must not ride
          // a user monkey-patch of globalThis.queueMicrotask. (nextTick no
          // longer schedules microtasks at all -- the host drains it.)
          registry._nativeQueueMicrotask = native;
          const wrapped = function (fn) {
            const bound = bindToCurrentFrame(fn);
            return native(function () {
              try {
                return bound();
              } catch (e) {
                // Node delivers a microtask throw to uncaughtException
                // BETWEEN sibling microtasks, not after the checkpoint --
                // the same canonical ladder as the nextTick drain (monitor +
                // origin arg included). No consumer = rethrow into the
                // engine ledger (fatal path unchanged).
                const dispatch = globalThis.__oamDispatchUncaught;
                if (typeof dispatch === "function" && dispatch(e, "uncaughtException")) {
                  return;
                }
                throw e;
              }
            });
          };
          Object.defineProperty(wrapped, "name", { value: "queueMicrotask" });
          globalThis.queueMicrotask = wrapped;
        }
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
      // node's dirxml is a DISTINCT function that forwards to log (it is not
      // `console.log` itself -- `console.dirxml === console.log` is false).
      dirxml: (...args) => writeOut(args),
      // Inspector-only APIs. With no inspector session attached node's own
      // implementations do nothing and return undefined, which is the case
      // every non-debugged run takes -- so a no-op here is node's observable
      // behavior, not a stub standing in for missing work. Under
      // --inspect these should reach the CDP Profiler domain; that is
      // recorded in docs/node-divergences.md.
      profile: () => {},
      profileEnd: () => {},
      timeStamp: () => {},
      // Async-context task tagging for devtools. node returns an object whose
      // run(fn) invokes fn and returns its result; without an inspector the
      // tagging is all it adds, so run() IS the observable contract.
      createTask: (_name) => ({ run: (fn, ...args) => fn(...args) }),
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
