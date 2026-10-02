// The ReadableStream, WritableStream and TransformStream constructors
// validate their arguments as node's do, in node's order.
//
// oam took `type: 'bytes'` with a strategy.size and silently dropped the
// size, where node throws RangeError ERR_INVALID_ARG_VALUE; it built a
// default stream for any other source.type (node: TypeError
// ERR_INVALID_ARG_VALUE); and it read source.type before the strategy,
// where the standard reads the strategy first. Also pinned: the object
// checks on the source / sink / transformer and strategy arguments, a
// sink.type or transformer.readableType / writableType (refused), a
// stringifying type, source.autoAllocateChunkSize 0 (refused), and the
// order every getter is read in. Measured on node v22.22.2.
const show = (tag, e) => console.log(tag, e?.constructor?.name, e?.code, String(e));
const readableTries = [
  ['bytes+size', () => new ReadableStream({type:'bytes'}, {size(){return 1}})],
  ['bytes+size-null', () => new ReadableStream({type:'bytes'}, {size: null})],
  ['bytes+size-undef', () => new ReadableStream({type:'bytes'}, {size: undefined})],
  ['foo', () => new ReadableStream({type:'foo'})],
  ['null-type', () => new ReadableStream({type:null})],
  ['bytes-obj', () => new ReadableStream({type:{toString(){return 'bytes'}}})],
  ['bytes-strobj', () => new ReadableStream({type:new String('bytes')})],
  ['sym', () => new ReadableStream({type:Symbol('x')})],
  ['foo+badhwm', () => new ReadableStream({type:'foo'}, {highWaterMark: -1})],
  ['bytes+size+badhwm', () => new ReadableStream({type:'bytes'}, {highWaterMark: -1, size(){}})],
  ['foo+badsize', () => new ReadableStream({type:'foo'}, {size: 5})],
  ['badhwm', () => new ReadableStream({}, {highWaterMark: -1})],
  ['empty-type', () => new ReadableStream({type:''})],
];
for (const [t, f] of readableTries) {
  try { const s = f(); console.log(t, "constructed", Object.prototype.toString.call(s)); } catch (e) { show(t, e); }
}
const tries = [
  ['ws-null', () => new WritableStream(null)],
  ['ws-num', () => new WritableStream(5)],
  ['ws-strat-null', () => new WritableStream({}, null)],
  ['ws-strat-num', () => new WritableStream({}, 5)],
  ['ws-type', () => new WritableStream({ type: 'bytes' })],
  ['ws-type-null', () => new WritableStream({ type: null })],
  ['ws-badsize', () => new WritableStream({}, { size: 1, highWaterMark: -1 })],
  ['ts-null', () => new TransformStream(null)],
  ['ts-rt', () => new TransformStream({ readableType: 'bytes' })],
  ['ts-wt', () => new TransformStream({ writableType: 'bytes' })],
  ['ts-ws-num', () => new TransformStream({}, 5)],
  ['ts-rs-num', () => new TransformStream({}, {}, 5)],
  ['ts-null-strats', () => new TransformStream({}, null, null)],
  ['ts-both-bad', () => new TransformStream({}, { highWaterMark: -2 }, { highWaterMark: -1 })],
  ['rs-fn', () => new ReadableStream(function () {})],
  ['rs-arr', () => new ReadableStream([])],
  ['rs-null', () => new ReadableStream(null)],
  ['rs-str', () => new ReadableStream('x')],
  ['rs-strat-null', () => new ReadableStream({}, null)],
  ['rs-strat-num', () => new ReadableStream({}, 5)],
  ['aacs0', () => new ReadableStream({ type: 'bytes', autoAllocateChunkSize: 0 })],
  ['aacs-default', () => new ReadableStream({ autoAllocateChunkSize: 0 })],
  ['aacs1024', () => new ReadableStream({ type: 'bytes', autoAllocateChunkSize: 1024 })],
];
for (const [t, f] of tries) {
  try { const s = f(); console.log(t, 'constructed', Object.prototype.toString.call(s)); } catch (e) { show(t, e); }
}
const log = [];
const g = (o, tag) => new Proxy(o, { get(t, k, r) { if (typeof k === 'string') log.push(`${tag}.${k}`); return Reflect.get(t, k, r); } });
new WritableStream(g({}, 'sink'), g({}, 'st')); console.log(log.join(',')); log.length = 0;
new TransformStream(g({}, 'tr'), g({}, 'ws'), g({}, 'rs')); console.log(log.join(',')); log.length = 0;
new ReadableStream(g({}, 'src'), g({}, 'st')); console.log(log.join(',')); log.length = 0;
new ReadableStream(g({ type: 'bytes' }, 'src'), g({}, 'st')); console.log(log.join(',')); log.length = 0;
