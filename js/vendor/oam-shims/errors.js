// 'internal/errors' shim for the vendored Node streams port
// (docs/design/streams-port.md section 5.2): the 17 ERR_* codes the vendored
// files consume, plus AbortError and aggregateTwoErrors, with v22-shaped
// message text. Deliberately self-contained -- NOT coupled to node_compat's
// lexical `codes` registry (unreachable from a separate snapshot file, and
// coupling would drift both). Message fidelity is close-not-byte-exact;
// byte-parity gaps surface in the node-suite triage (slice 4), not here.
"use strict";

// v22 determineSpecificType, approximated without util.inspect.
function determineSpecificType(value) {
  if (value === null) return "null";
  if (value === undefined) return "undefined";
  const t = typeof value;
  if (t === "function") {
    return `function ${value.name || "(anonymous)"}`;
  }
  if (t === "object") {
    const ctor = value.constructor;
    if (ctor && ctor.name) return `an instance of ${ctor.name}`;
    return "an instance of Object";
  }
  let printed;
  if (t === "string") {
    printed = value.length > 25 ? `'${value.slice(0, 25)}...'` : `'${value}'`;
  } else if (t === "bigint") {
    printed = `${value}n`;
  } else if (t === "symbol") {
    printed = value.toString();
  } else {
    printed = String(value);
  }
  return `type ${t} (${printed})`;
}

const argOrProp = (name) =>
  typeof name === "string" && name.includes(".") ? "property" : "argument";

// inspect-lite for ERR_INVALID_ARG_VALUE: v22 renders the RAW value there
// (NaN -> "NaN", -0 -> "-0"), unlike ERR_INVALID_ARG_TYPE's
// "type number (...)" shape. Objects fall back to String() -- within the
// shim's stated close-not-byte-exact policy.
function inspectValue(value) {
  if (typeof value === "string") return `'${value}'`;
  if (typeof value === "bigint") return `${value}n`;
  if (typeof value === "symbol") return value.toString();
  if (typeof value === "function") {
    return value.name ? `[Function: ${value.name}]` : "[Function (anonymous)]";
  }
  if (Object.is(value, -0)) return "-0";
  return String(value);
}

// Node's kTypes: these route to the "of type" bucket LOWERCASED -- including
// the uppercase 'Function'/'Object' entries the validators throw with. The
// class-name regex only applies to entries NOT in this set.
const kTypes = new Set([
  "string", "function", "number", "object",
  "Function", "Object", "boolean", "bigint", "symbol",
]);

// Node's list joiner: "a", "a or b", "a, b, or c" (oxford comma at 3+).
function formatList(list, conj = "or") {
  if (list.length === 1) return list[0];
  if (list.length === 2) return `${list[0]} ${conj} ${list[1]}`;
  return `${list.slice(0, -1).join(", ")}, ${conj} ${list[list.length - 1]}`;
}

function invalidArgTypeMessage(name, expected, actual) {
  if (!Array.isArray(expected)) expected = [expected];
  const types = [];
  const instances = [];
  const other = [];
  for (const item of expected) {
    if (kTypes.has(item)) types.push(item.toLowerCase());
    else if (/^[A-Z]/.test(item)) instances.push(item);
    else other.push(item);
  }
  const parts = [];
  if (types.length > 0) {
    parts.push(`${types.length > 1 ? "one of type" : "of type"} ${formatList(types)}`);
  }
  if (instances.length > 0) {
    parts.push(`an instance of ${formatList(instances)}`);
  }
  if (other.length > 0) {
    parts.push(other.length > 1 ? `one of ${formatList(other)}` : other[0]);
  }
  return (
    `The "${name}" ${argOrProp(name)} must be ${parts.join(" or ")}. ` +
    `Received ${determineSpecificType(actual)}`
  );
}

function makeCode(Base, code, formatter) {
  const cls = class extends Base {
    constructor(...args) {
      super(typeof formatter === "function" ? formatter(...args) : formatter);
      this.code = code;
    }
    toString() {
      return `${this.name} [${code}]: ${this.message}`;
    }
  };
  Object.defineProperty(cls, "name", { value: code, configurable: true });
  return cls;
}

const codes = {
  ERR_ILLEGAL_CONSTRUCTOR: makeCode(TypeError, "ERR_ILLEGAL_CONSTRUCTOR", "Illegal constructor"),
  ERR_INVALID_ARG_TYPE: makeCode(TypeError, "ERR_INVALID_ARG_TYPE", invalidArgTypeMessage),
  ERR_INVALID_ARG_VALUE: makeCode(
    TypeError,
    "ERR_INVALID_ARG_VALUE",
    (name, value, reason = "is invalid") => {
      let inspected = inspectValue(value);
      if (inspected.length > 128) inspected = `${inspected.slice(0, 128)}...`;
      return `The ${argOrProp(name)} '${name}' ${reason}. Received ${inspected}`;
    },
  ),
  ERR_INVALID_RETURN_VALUE: makeCode(
    TypeError,
    "ERR_INVALID_RETURN_VALUE",
    (input, name, value) =>
      `Expected ${input} to be returned from the "${name}" function but got ${determineSpecificType(value)}.`,
  ),
  ERR_METHOD_NOT_IMPLEMENTED: makeCode(
    Error,
    "ERR_METHOD_NOT_IMPLEMENTED",
    (name) => `The ${name} method is not implemented`,
  ),
  ERR_MISSING_ARGS: makeCode(TypeError, "ERR_MISSING_ARGS", (...args) => {
    const names = args.map((a) => `"${a}"`);
    let list;
    if (names.length === 1) list = names[0];
    else if (names.length === 2) list = `${names[0]} and ${names[1]}`;
    else list = `${names.slice(0, -1).join(", ")}, and ${names[names.length - 1]}`;
    return `The ${list} argument${names.length > 1 ? "s" : ""} must be specified`;
  }),
  ERR_MULTIPLE_CALLBACK: makeCode(Error, "ERR_MULTIPLE_CALLBACK", "Callback called multiple times"),
  ERR_OUT_OF_RANGE: makeCode(RangeError, "ERR_OUT_OF_RANGE", (name, range, value) => {
    return `The value of "${name}" is out of range. It must be ${range}. Received ${String(value)}`;
  }),
  ERR_STREAM_ALREADY_FINISHED: makeCode(
    Error,
    "ERR_STREAM_ALREADY_FINISHED",
    (name) => `Cannot call ${name} after a stream was finished`,
  ),
  ERR_STREAM_CANNOT_PIPE: makeCode(Error, "ERR_STREAM_CANNOT_PIPE", "Cannot pipe, not readable"),
  ERR_STREAM_DESTROYED: makeCode(
    Error,
    "ERR_STREAM_DESTROYED",
    (name) => `Cannot call ${name} after a stream was destroyed`,
  ),
  ERR_STREAM_NULL_VALUES: makeCode(TypeError, "ERR_STREAM_NULL_VALUES", "May not write null values to stream"),
  ERR_STREAM_PREMATURE_CLOSE: makeCode(Error, "ERR_STREAM_PREMATURE_CLOSE", "Premature close"),
  ERR_STREAM_PUSH_AFTER_EOF: makeCode(Error, "ERR_STREAM_PUSH_AFTER_EOF", "stream.push() after EOF"),
  ERR_STREAM_UNSHIFT_AFTER_END_EVENT: makeCode(
    Error,
    "ERR_STREAM_UNSHIFT_AFTER_END_EVENT",
    "stream.unshift() after end event",
  ),
  ERR_STREAM_WRITE_AFTER_END: makeCode(Error, "ERR_STREAM_WRITE_AFTER_END", "write after end"),
  ERR_UNKNOWN_ENCODING: makeCode(
    TypeError,
    "ERR_UNKNOWN_ENCODING",
    // v22 formats the encoding with util.format's %s ({} -> '{}', not
    // '[object Object]'). Delegate to the registry's util at CONSTRUCTION
    // time (runtime-only; same call-time pattern as the loader's public
    // fallback) with a template-literal fallback if util is unreachable.
    (enc) => {
      try {
        return globalThis.__oamNode.get("util").format("Unknown encoding: %s", enc);
      } catch (_e) {
        return `Unknown encoding: ${enc}`;
      }
    },
  ),
};

class AbortError extends Error {
  constructor(message = "The operation was aborted", options = undefined) {
    if (options !== undefined && typeof options !== "object") {
      throw new codes.ERR_INVALID_ARG_TYPE("options", "Object", options);
    }
    super(message, options);
    this.code = "ABORT_ERR";
    this.name = "AbortError";
  }
}

// v22 aggregateTwoErrors: prefer the outer error's identity/code, carry the
// inner one alongside.
function aggregateTwoErrors(innerError, outerError) {
  if (innerError && outerError && innerError !== outerError) {
    if (Array.isArray(outerError.errors)) {
      outerError.errors.push(innerError);
      return outerError;
    }
    const err = new AggregateError([outerError, innerError], outerError.message);
    err.code = outerError.code;
    return err;
  }
  return innerError || outerError;
}

module.exports = { codes, AbortError, aggregateTwoErrors };
