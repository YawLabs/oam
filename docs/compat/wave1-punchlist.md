# node: compat wave 1 — known-divergence punch list

Source: adversarial review fleet over commit 4d90904 (four finders running
empirical oam-vs-node parity batteries, findings verified per-item). The
blocker and every `important` finding were fixed in the follow-up commit;
this file tracks the `minor` parity gaps that remain, so they are filed
rather than forgotten. None of these blocks the wave-1 packages we target;
fix opportunistically or when a real package trips one.

## Buffer / encodings (js/node_compat.js)

- Hex decode leniency overshoots Node: pairs with a valid first nibble and
  invalid second ('1x', ' 1', '+1') decode as a one-digit value instead of
  terminating the parse.
- Numeric write* methods silently wrap out-of-range values and accept
  fractional offsets where Node throws ERR_OUT_OF_RANGE.
- 'ascii' encode masks bytes with 0x7f; Node's ascii encode is
  byte-identical to latin1 (mask 0xff).
- TextDecoder non-fatal mode emits one U+FFFD per BYTE of a truncated
  sequence instead of one per maximal subpart (WHATWG/Node emit one).
- Parity batch: isEncoding(undefined)===true; Buffer.from(str,'') throws;
  lastIndexOf(str, negativeOffset) returns -1; indexOf(str,
  fractionalOffset) never matches; toString negative start is
  relative-from-end; Buffer.compare rejects plain Uint8Arrays; encodeInto
  may leak partial bytes; atob accepts Unicode whitespace.

## path / util / assert / events (js/node_compat.js)

- UNC share root: basename returns '', dirname/relative diverge on
  '\\\\host\\share' inputs.
- normalize() drops the trailing separator when the result collapses to '.'.
- extname('..') returns '.' and corrupts parse('..'); parse('.foo').dir is
  '.' instead of ''.
- path.format() does not insert the missing dot before ext (Node >= 19
  does).
- basename(p, suffix) keeps the base when base === suffix; Node strips it.
- deepStrictEqual ignores symbol keys; distinct boxed primitives compare
  equal.
- util.inspect/format: -0 prints as '0'; %s ignores a custom toString;
  strings inside objects are not escape-quoted.
- util.types.isProxy always returns false (no native hook yet).
- EventEmitter: removeAllListeners never emits 'removeListener';
  listenerCount ignores the optional listener argument.

## fs / os / process / module (js/node_compat.js + natives)

- fs.exists invokes its callback synchronously (zalgo).
- fsPromises.mkdir({recursive:true}) resolves to undefined instead of the
  first created path.
- Stats objects lack nlink; fs.constants lacks the O_* open flags.
- Async fs rejections carry .code only — .syscall/.path are dropped, and
  .errno is missing on both sync and async paths.
- writeFileSync/writeFile coerce non-string, non-view payloads through
  ToString — Node throws ERR_INVALID_ARG_TYPE.
- strip_unc_prefix mangles \\?\UNC\ network paths (strips to
  'UNC\server\...'); realpath on network shares is wrong.
- Pending exceptions from a throwing toString are replaced by the natives'
  own TypeError; missing args coerce to the string 'undefined'.
- node_error_code cannot produce EXDEV — cross-device rename surfaces as
  EIO, defeating the standard copy+unlink fallback.
- process.env / process.argv natives abort on non-Unicode environment
  entries (std::env::vars panics) — switch to vars_os with lossy decode.
- A panicking op future hangs the event loop: inflight never decrements
  and recv() blocks forever — wrap spawned ops in catch_unwind or send a
  poisoned completion on drop.
- Unhandled-rejection policy diverges from Node: late-attached handlers
  un-flag (Node warns immediately at the macrotask boundary), detection
  happens only at end of run.
- stat results hardcode mode: 0 and alias ctimeMs to mtimeMs.
- os.release()/os.version() return '' and cpus() reports model 'unknown'
  without a doc note; os.uptime() is process uptime, not system uptime.
- module.createRequire mishandles UNC file:// URLs (file://server/share).

## CLI

- Script flags arrive via `oam run file.ts -- --flag` (cargo convention);
  Node passes everything after the script path. Documented, revisit if it
  bites adopters.
