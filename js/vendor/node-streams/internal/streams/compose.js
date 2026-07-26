// Vendored from Node.js v22.22.2 (commit 2645dc73720b1b4f27c49f395d3c66025ce126cc)
// Source: https://raw.githubusercontent.com/nodejs/node/v22.22.2/lib/internal/streams/compose.js
// Retrieved: 2026-07-25. Local modifications: none (loaded via oam's
// build-time define() wrapper -- see docs/design/streams-port.md section 3).
// License: MIT -- see the "Node.js" entry in THIRD_PARTY_LICENSES.md and any
// retained upstream header below. UPSTREAM (same dir) records the pristine
// sha256 of the body below this banner; re-vendor procedure lives there.

'use strict';

const {
  ArrayPrototypeSlice,
} = primordials;

const { pipeline } = require('internal/streams/pipeline');
const Duplex = require('internal/streams/duplex');
const { destroyer } = require('internal/streams/destroy');
const {
  isNodeStream,
  isReadable,
  isWritable,
  isWebStream,
  isTransformStream,
  isWritableStream,
  isReadableStream,
} = require('internal/streams/utils');
const {
  AbortError,
  codes: {
    ERR_INVALID_ARG_VALUE,
    ERR_MISSING_ARGS,
  },
} = require('internal/errors');
const eos = require('internal/streams/end-of-stream');

module.exports = function compose(...streams) {
  if (streams.length === 0) {
    throw new ERR_MISSING_ARGS('streams');
  }

  if (streams.length === 1) {
    return Duplex.from(streams[0]);
  }

  const orgStreams = ArrayPrototypeSlice(streams);

  if (typeof streams[0] === 'function') {
    streams[0] = Duplex.from(streams[0]);
  }

  if (typeof streams[streams.length - 1] === 'function') {
    const idx = streams.length - 1;
    streams[idx] = Duplex.from(streams[idx]);
  }

  for (let n = 0; n < streams.length; ++n) {
    if (!isNodeStream(streams[n]) && !isWebStream(streams[n])) {
      // TODO(ronag): Add checks for non streams.
      continue;
    }
    if (
      n < streams.length - 1 &&
      !(
        isReadable(streams[n]) ||
        isReadableStream(streams[n]) ||
        isTransformStream(streams[n])
      )
    ) {
      throw new ERR_INVALID_ARG_VALUE(
        `streams[${n}]`,
        orgStreams[n],
        'must be readable',
      );
    }
    if (
      n > 0 &&
      !(
        isWritable(streams[n]) ||
        isWritableStream(streams[n]) ||
        isTransformStream(streams[n])
      )
    ) {
      throw new ERR_INVALID_ARG_VALUE(
        `streams[${n}]`,
        orgStreams[n],
        'must be writable',
      );
    }
  }

  let ondrain;
  let onfinish;
  let onreadable;
  let onclose;
  let d;

  function onfinished(err) {
    const cb = onclose;
    onclose = null;

    if (cb) {
      cb(err);
    } else if (err) {
      d.destroy(err);
    } else if (!readable && !writable) {
      d.destroy();
    }
  }

  const head = streams[0];
  const tail = pipeline(streams, onfinished);

  const writable = !!(
    isWritable(head) ||
    isWritableStream(head) ||
    isTransformStream(head)
  );
  const readable = !!(
    isReadable(tail) ||
    isReadableStream(tail) ||
    isTransformStream(tail)
  );

  // TODO(ronag): Avoid double buffering.
  // Implement Writable/Readable/Duplex traits.
  // See, https://github.com/nodejs/node/pull/33515.
  d = new Duplex({
    // TODO (ronag): highWaterMark?
    writableObjectMode: !!head?.writableObjectMode,
    readableObjectMode: !!tail?.readableObjectMode,
    writable,
    readable,
  });

  if (writable) {
    if (isNodeStream(head)) {
      d._write = function(chunk, encoding, callback) {
        if (head.write(chunk, encoding)) {
          callback();
        } else {
          ondrain = callback;
        }
      };

      d._final = function(callback) {
        head.end();
        onfinish = callback;
      };

      head.on('drain', function() {
        if (ondrain) {
          const cb = ondrain;
          ondrain = null;
          cb();
        }
      });
    } else if (isWebStream(head)) {
      const writable = isTransformStream(head) ? head.writable : head;
      const writer = writable.getWriter();

      d._write = async function(chunk, encoding, callback) {
        try {
          await writer.ready;
          writer.write(chunk).catch(() => {});
          callback();
        } catch (err) {
          callback(err);
        }
      };

      d._final = async function(callback) {
        try {
          await writer.ready;
          writer.close().catch(() => {});
          onfinish = callback;
        } catch (err) {
          callback(err);
        }
      };
    }

    const toRead = isTransformStream(tail) ? tail.readable : tail;

    eos(toRead, () => {
      if (onfinish) {
        const cb = onfinish;
        onfinish = null;
        cb();
      }
    });
  }

  if (readable) {
    if (isNodeStream(tail)) {
      tail.on('readable', function() {
        if (onreadable) {
          const cb = onreadable;
          onreadable = null;
          cb();
        }
      });

      tail.on('end', function() {
        d.push(null);
      });

      d._read = function() {
        while (true) {
          const buf = tail.read();
          if (buf === null) {
            onreadable = d._read;
            return;
          }

          if (!d.push(buf)) {
            return;
          }
        }
      };
    } else if (isWebStream(tail)) {
      const readable = isTransformStream(tail) ? tail.readable : tail;
      const reader = readable.getReader();
      d._read = async function() {
        while (true) {
          try {
            const { value, done } = await reader.read();

            if (!d.push(value)) {
              return;
            }

            if (done) {
              d.push(null);
              return;
            }
          } catch {
            return;
          }
        }
      };
    }
  }

  d._destroy = function(err, callback) {
    if (!err && onclose !== null) {
      err = new AbortError();
    }

    onreadable = null;
    ondrain = null;
    onfinish = null;

    if (isNodeStream(tail)) {
      destroyer(tail, err);
    }

    if (onclose === null) {
      callback(err);
    } else {
      onclose = callback;
    }
  };

  return d;
};
