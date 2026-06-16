#!/usr/bin/env python3
"""Batch 12: Node-shaped error codes factory, util.types expansion,
internal/errors module."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ──────────────────────────────────────────────────────────────────────
# 1. Replace makeNodeError with a full error code registry
# ──────────────────────────────────────────────────────────────────────

# Find existing makeNodeError
make_err_start = None
make_err_end = None
for i in range(len(lines)):
    if "function makeNodeError(code, message)" in lines[i]:
        make_err_start = i
    if make_err_start is not None and make_err_end is None and lines[i].strip() == "}":
        make_err_end = i + 1
        break

if make_err_start is None:
    print("ERROR: Could not find makeNodeError")
    sys.exit(1)

print(f"Found makeNodeError at lines {make_err_start + 1}-{make_err_end}")

error_infra = [
    "  // Node-shaped error codes: each entry creates a typed error with .code\n",
    "  function makeNodeError(code, message) {\n",
    "    var err = new Error(message);\n",
    "    err.code = code;\n",
    "    return err;\n",
    "  }\n",
    "\n",
    "  function E(code, Base, msgFn) {\n",
    "    function NodeError() {\n",
    "      var args = Array.prototype.slice.call(arguments);\n",
    "      var msg = typeof msgFn === \"function\" ? msgFn.apply(null, args) : msgFn;\n",
    "      var inst = new Base(msg);\n",
    "      inst.code = code;\n",
    "      inst.name = Base.name + \" [\" + code + \"]\";\n",
    "      return inst;\n",
    "    }\n",
    "    return NodeError;\n",
    "  }\n",
    "\n",
    "  var codes = {};\n",
    "  // ---- TypeError family ----\n",
    "  codes.ERR_INVALID_ARG_TYPE = E(\"ERR_INVALID_ARG_TYPE\", TypeError, function(name, expected, actual) {\n",
    "    return 'The \"' + name + '\" argument must be of type ' + expected + '. Received ' + typeof actual;\n",
    "  });\n",
    "  codes.ERR_INVALID_ARG_VALUE = E(\"ERR_INVALID_ARG_VALUE\", TypeError, function(name, value, reason) {\n",
    "    return 'The argument \"' + name + '\" is invalid. Received ' + String(value) + (reason ? \". \" + reason : \"\");\n",
    "  });\n",
    "  codes.ERR_INVALID_CALLBACK = E(\"ERR_INVALID_CALLBACK\", TypeError, function(name) {\n",
    "    return 'Callback must be a function. Received ' + String(name);\n",
    "  });\n",
    "  codes.ERR_INVALID_THIS = E(\"ERR_INVALID_THIS\", TypeError, function(expected) {\n",
    "    return 'Value of \"this\" must be of type ' + expected;\n",
    "  });\n",
    "  codes.ERR_INVALID_RETURN_VALUE = E(\"ERR_INVALID_RETURN_VALUE\", TypeError, function(input, name, value) {\n",
    "    return 'Expected ' + input + ' to be returned from the \"' + name + '\" function but got ' + typeof value + \".\";\n",
    "  });\n",
    "  codes.ERR_MISSING_ARGS = E(\"ERR_MISSING_ARGS\", TypeError, function() {\n",
    "    var args = Array.prototype.slice.call(arguments);\n",
    "    return 'The ' + args.map(function(a) { return '\"' + a + '\"'; }).join(\", \") + ' argument' + (args.length > 1 ? 's' : '') + ' must be specified';\n",
    "  });\n",
    "  codes.ERR_UNKNOWN_ENCODING = E(\"ERR_UNKNOWN_ENCODING\", TypeError, function(enc) {\n",
    "    return 'Unknown encoding: ' + enc;\n",
    "  });\n",
    "  codes.ERR_INVALID_URL = E(\"ERR_INVALID_URL\", TypeError, function(input) {\n",
    "    return 'Invalid URL: ' + input;\n",
    "  });\n",
    "  codes.ERR_INVALID_URL_SCHEME = E(\"ERR_INVALID_URL_SCHEME\", TypeError, function(expected) {\n",
    "    return 'The URL must be of scheme ' + expected;\n",
    "  });\n",
    "  codes.ERR_INVALID_PROTOCOL = E(\"ERR_INVALID_PROTOCOL\", TypeError, function(protocol, expected) {\n",
    "    return 'Protocol \"' + protocol + '\" not supported. Expected \"' + expected + '\"';\n",
    "  });\n",
    "  codes.ERR_METHOD_NOT_IMPLEMENTED = E(\"ERR_METHOD_NOT_IMPLEMENTED\", TypeError, function(name) {\n",
    "    return 'The ' + name + ' method is not implemented';\n",
    "  });\n",
    "  codes.ERR_SOCKET_BAD_TYPE = E(\"ERR_SOCKET_BAD_TYPE\", TypeError, function() {\n",
    "    return 'Bad socket type specified. Valid types are: udp4, udp6';\n",
    "  });\n",
    "  codes.ERR_UNKNOWN_SIGNAL = E(\"ERR_UNKNOWN_SIGNAL\", TypeError, function(signal) {\n",
    "    return 'Unknown signal: ' + signal;\n",
    "  });\n",
    "  codes.ERR_UNESCAPED_CHARACTERS = E(\"ERR_UNESCAPED_CHARACTERS\", TypeError, function(name) {\n",
    "    return name + ' contains unescaped characters';\n",
    "  });\n",
    "  // ---- RangeError family ----\n",
    "  codes.ERR_OUT_OF_RANGE = E(\"ERR_OUT_OF_RANGE\", RangeError, function(name, range, received) {\n",
    "    return 'The value of \"' + name + '\" is out of range. It must be ' + range + '. Received ' + received;\n",
    "  });\n",
    "  codes.ERR_BUFFER_OUT_OF_BOUNDS = E(\"ERR_BUFFER_OUT_OF_BOUNDS\", RangeError, function(name) {\n",
    "    return name ? '\"' + name + '\" is outside the bounds of the buffer' : 'Attempt to access memory outside buffer bounds';\n",
    "  });\n",
    "  codes.ERR_CHILD_CLOSED_BEFORE_REPLY = E(\"ERR_CHILD_CLOSED_BEFORE_REPLY\", RangeError, function() {\n",
    "    return 'Child closed before reply';\n",
    "  });\n",
    "  codes.ERR_SOCKET_BAD_PORT = E(\"ERR_SOCKET_BAD_PORT\", RangeError, function(name, port, allowZero) {\n",
    "    return '\"' + name + '\" option should be >= ' + (allowZero ? '0' : '1') + ' and < 65536. Received ' + port;\n",
    "  });\n",
    "  // ---- Error family ----\n",
    "  codes.ERR_STREAM_DESTROYED = E(\"ERR_STREAM_DESTROYED\", Error, function(name) {\n",
    "    return 'Cannot call ' + (name || 'write') + ' after a stream was destroyed';\n",
    "  });\n",
    "  codes.ERR_STREAM_PREMATURE_CLOSE = E(\"ERR_STREAM_PREMATURE_CLOSE\", Error, function() {\n",
    "    return 'Premature close';\n",
    "  });\n",
    "  codes.ERR_STREAM_NULL_VALUES = E(\"ERR_STREAM_NULL_VALUES\", TypeError, function() {\n",
    "    return 'May not write null values to stream';\n",
    "  });\n",
    "  codes.ERR_STREAM_WRITE_AFTER_END = E(\"ERR_STREAM_WRITE_AFTER_END\", Error, function() {\n",
    "    return 'write after end';\n",
    "  });\n",
    "  codes.ERR_STREAM_ALREADY_FINISHED = E(\"ERR_STREAM_ALREADY_FINISHED\", Error, function(name) {\n",
    "    return 'Cannot call ' + (name || 'write') + ' after a stream was finished';\n",
    "  });\n",
    "  codes.ERR_STREAM_PUSH_AFTER_EOF = E(\"ERR_STREAM_PUSH_AFTER_EOF\", Error, function() {\n",
    "    return 'stream.push() after EOF';\n",
    "  });\n",
    "  codes.ERR_STREAM_UNSHIFT_AFTER_END_EVENT = E(\"ERR_STREAM_UNSHIFT_AFTER_END_EVENT\", Error, function() {\n",
    "    return 'stream.unshift() after end event';\n",
    "  });\n",
    "  codes.ERR_MULTIPLE_CALLBACK = E(\"ERR_MULTIPLE_CALLBACK\", Error, function() {\n",
    "    return 'Callback called multiple times';\n",
    "  });\n",
    "  codes.ERR_INVALID_FILE_URL_PATH = E(\"ERR_INVALID_FILE_URL_PATH\", Error, function(msg) {\n",
    "    return 'File URL path ' + msg;\n",
    "  });\n",
    "  codes.ERR_INVALID_FILE_URL_HOST = E(\"ERR_INVALID_FILE_URL_HOST\", Error, function(host) {\n",
    "    return 'File URL host must be \"localhost\" or empty on ' + host;\n",
    "  });\n",
    "  codes.ERR_FS_CP_DIR_TO_NON_DIR = E(\"ERR_FS_CP_DIR_TO_NON_DIR\", Error, function(msg) {\n",
    "    return msg;\n",
    "  });\n",
    "  codes.ERR_FS_EISDIR = E(\"ERR_FS_EISDIR\", Error, function(msg) {\n",
    "    return msg || 'Path is a directory';\n",
    "  });\n",
    "  codes.ERR_MODULE_NOT_FOUND = E(\"ERR_MODULE_NOT_FOUND\", Error, function(path, base) {\n",
    "    return 'Cannot find module \"' + path + '\"' + (base ? ' imported from ' + base : '');\n",
    "  });\n",
    "  codes.ERR_PACKAGE_PATH_NOT_EXPORTED = E(\"ERR_PACKAGE_PATH_NOT_EXPORTED\", Error, function(pkgPath, subpath) {\n",
    "    return 'Package subpath \"' + subpath + '\" is not defined by \"exports\" in ' + pkgPath;\n",
    "  });\n",
    "  codes.ERR_PACKAGE_IMPORT_NOT_DEFINED = E(\"ERR_PACKAGE_IMPORT_NOT_DEFINED\", TypeError, function(specifier, pkgPath) {\n",
    "    return 'Package import specifier \"' + specifier + '\" is not defined in ' + pkgPath;\n",
    "  });\n",
    "  codes.ERR_UNSUPPORTED_DIR_IMPORT = E(\"ERR_UNSUPPORTED_DIR_IMPORT\", Error, function(path) {\n",
    "    return 'Directory import \"' + path + '\" is not supported';\n",
    "  });\n",
    "  codes.ERR_UNSUPPORTED_ESM_URL_SCHEME = E(\"ERR_UNSUPPORTED_ESM_URL_SCHEME\", Error, function(url) {\n",
    "    return 'Only URLs with a scheme in: file and data are supported by the default ESM loader. Received protocol \"' + url + '\"';\n",
    "  });\n",
    "  codes.ERR_ASSERTION = E(\"ERR_ASSERTION\", Error, function(msg) {\n",
    "    return msg || 'assertion error';\n",
    "  });\n",
    "  codes.ERR_CRYPTO_FIPS_FORCED = E(\"ERR_CRYPTO_FIPS_FORCED\", Error, function() {\n",
    "    return 'Cannot set FIPS mode, it was forced with --force-fips at startup.';\n",
    "  });\n",
    "  codes.ERR_WORKER_NOT_SUPPORTED = E(\"ERR_WORKER_NOT_SUPPORTED\", Error, function() {\n",
    "    return 'Worker threads are not supported in this environment';\n",
    "  });\n",
    "  codes.ERR_ENV_FILE_NOT_FOUND = E(\"ERR_ENV_FILE_NOT_FOUND\", Error, function(path) {\n",
    "    return 'Cannot find env file: ' + path;\n",
    "  });\n",
    "  codes.ERR_INVALID_BUFFER_SIZE = E(\"ERR_INVALID_BUFFER_SIZE\", RangeError, function() {\n",
    "    return 'Buffer size must be a multiple of 8';\n",
    "  });\n",
    "\n",
]

lines[make_err_start:make_err_end] = error_infra
offset1 = len(error_infra) - (make_err_end - make_err_start)
print(f"  Replaced makeNodeError with full error infrastructure ({len(error_infra)} lines, offset={offset1})")

# ──────────────────────────────────────────────────────────────────────
# 2. Add util.types expansions
# ──────────────────────────────────────────────────────────────────────

# Find "        isProxy: () => false," in util.types
for i in range(len(lines)):
    if "isProxy: () => false," in lines[i]:
        new_types = [
            "        isArrayBufferView: (v) => ArrayBuffer.isView(v),\n",
            "        isUint8ClampedArray: (v) => v instanceof Uint8ClampedArray,\n",
            "        isUint16Array: (v) => v instanceof Uint16Array,\n",
            "        isUint32Array: (v) => v instanceof Uint32Array,\n",
            "        isInt8Array: (v) => v instanceof Int8Array,\n",
            "        isInt16Array: (v) => v instanceof Int16Array,\n",
            "        isInt32Array: (v) => v instanceof Int32Array,\n",
            "        isFloat32Array: (v) => v instanceof Float32Array,\n",
            "        isFloat64Array: (v) => v instanceof Float64Array,\n",
            "        isBigInt64Array: (v) => typeof BigInt64Array !== \"undefined\" && v instanceof BigInt64Array,\n",
            "        isBigUint64Array: (v) => typeof BigUint64Array !== \"undefined\" && v instanceof BigUint64Array,\n",
            "        isMapIterator: (v) => { try { Map.prototype.has.call(v); return false; } catch { return String(v) === \"[object Map Iterator]\"; } },\n",
            "        isSetIterator: (v) => { try { Set.prototype.has.call(v); return false; } catch { return String(v) === \"[object Set Iterator]\"; } },\n",
            "        isGeneratorObject: (v) => v != null && typeof v.next === \"function\" && typeof v.throw === \"function\" && typeof v[Symbol.iterator] === \"function\",\n",
            "        isWeakRef: (v) => v instanceof WeakRef,\n",
            "        isModuleNamespaceObject: () => false,\n",
            "        isExternal: () => false,\n",
            "        isArgumentsObject: (v) => Object.prototype.toString.call(v) === \"[object Arguments]\",\n",
            "        isBooleanObject: (v) => v instanceof Boolean,\n",
            "        isNumberObject: (v) => v instanceof Number,\n",
            "        isStringObject: (v) => v instanceof String,\n",
            "        isSymbolObject: (v) => Object.prototype.toString.call(v) === \"[object Symbol]\" && typeof v === \"object\",\n",
            "        isCryptoKey: (v) => typeof CryptoKey !== \"undefined\" && v instanceof CryptoKey,\n",
            "        isKeyObject: () => false,\n",
        ]
        for j, line in enumerate(new_types):
            lines.insert(i + 1 + j, line)
        print(f"  Added {len(new_types)} util.types entries after line {i + 1}")
        break

# ──────────────────────────────────────────────────────────────────────
# 3. Wire internal/errors as a requireable module
# ──────────────────────────────────────────────────────────────────────

# Find the builtinModules list and add internal/errors factory near it
# Look for registry.factories["dns/promises"] as an anchor point (after dns)
for i in range(len(lines)):
    if 'registry.factories["dns/promises"]' in lines[i]:
        internal_errors_factory = [
            "\n",
            "  // internal/errors -- some packages (readable-stream, undici) import this\n",
            "  registry.factories[\"internal/errors\"] = () => ({ codes });\n",
        ]
        for j, line in enumerate(internal_errors_factory):
            lines.insert(i + 1 + j, line)
        print(f"  Added internal/errors factory after line {i + 1}")
        break

# Also add to builtinModules list
for i in range(len(lines)):
    if '"dns/promises",' in lines[i] and "builtinModules" not in lines[i]:
        # Check if this is in the builtinModules array (look back for context)
        if any("builtinModules" in lines[j] for j in range(max(0, i-20), i)):
            lines.insert(i + 1, '      "internal/errors",\n')
            print(f"  Added internal/errors to builtinModules at line {i + 2}")
            break

# Add to SUPPORTED_BUILTINS in npm.rs
# Actually, let me check if this is needed
print("\nNote: internal/errors may need adding to Rust SUPPORTED_BUILTINS in npm.rs")

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
