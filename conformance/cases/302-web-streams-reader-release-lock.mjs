// reader.releaseLock() settles what node's ReadableStreamDefaultReaderRelease
// settles: `closed` rejects with ERR_INVALID_STATE "Reader released" (a
// reader whose stream already closed or errored gets a new, rejected
// `closed`), then every pending read rejects with ERR_INVALID_STATE
// "Releasing reader".
//
// oam rejected the pending reads with an uncoded TypeError and never
// settled `closed`, so code awaiting reader.closed after releasing the lock
// hung (the run died "top-level await never settled"). Also pinned: the
// order of those rejections, that each error is one shared object as in
// node, read() / cancel() on a released reader, the locked-stream errors,
// and that a released stream reads on through a new reader. Measured on
// node v22.22.2.
const show = (tag, e) => console.log(tag, e?.name, e?.code, e?.message, e instanceof TypeError);
{
  const r = new ReadableStream({}).getReader();
  const p = r.read(); const p2 = r.read();
  const order = [];
  r.closed.then(() => order.push('closed-ok'), (e) => order.push('closed:' + e.message));
  p.catch((e) => order.push('read1:' + e.message)); p2.catch((e) => order.push('read2:' + e.message));
  r.releaseLock();
  const e1 = await p.catch(e => e); show('read', e1);
  const e2 = await r.closed.catch(e => e); show('closed', e2);
  const e3 = await p2.catch(e => e); console.log('same err', e1 === e3, e1 === e2);
  console.log(order.join(' | '));
  show('read-after', await r.read().catch(e => e));
  show('cancel-after', await r.cancel().catch(e => e));
  r.releaseLock(); console.log('double release ok');
  const r2 = new ReadableStream({}).getReader(); r2.releaseLock();
  const ec = await r2.closed.catch(e => e); console.log('same released err across streams', ec === e2);
}
// release after close: closed replaced with rejected
{
  const rs = new ReadableStream({start(c){c.close()}});
  const r = rs.getReader(); const c1 = r.closed;
  await c1; r.releaseLock();
  console.log('closed replaced', r.closed !== c1);
  show('closed-after-close-release', await r.closed.catch(e => e));
  const r2 = rs.getReader(); await r2.closed; console.log('new reader closed resolves');
}
// release then stream errors: old closed remains rejected with released
{
  let c; const rs = new ReadableStream({start(x){c=x}});
  const r = rs.getReader(); r.releaseLock(); c.error(new Error('later'));
  show('released-then-error', await r.closed.catch(e => e));
  const r2 = rs.getReader(); show('new reader on errored', await r2.closed.catch(e => e));
}
// release with chunks queued; new reader reads them
{
  const rs = new ReadableStream({start(c){c.enqueue(1); c.enqueue(2)}});
  const r = rs.getReader(); console.log(await r.read()); r.releaseLock();
  const r2 = rs.getReader(); console.log(await r2.read());
}
// locked stream cancel
{
  const rs = new ReadableStream({}); rs.getReader();
  show('locked-cancel', await rs.cancel().catch(e => e));
  try { rs.getReader(); } catch (e) { show('locked-getReader', e); }
}
// for await break releases
{
  const rs = new ReadableStream({start(c){c.enqueue(1); c.enqueue(2)}});
  for await (const v of rs.values({preventCancel: true})) { console.log('v', v); break; }
  console.log('locked after break', rs.locked);
  const r = rs.getReader(); console.log(await r.read());
}
// writer released error is one object
{
  const w1 = new WritableStream().getWriter(); w1.releaseLock();
  const w2 = new WritableStream().getWriter(); w2.releaseLock();
  const a = await w1.closed.catch(e => e); const b = await w2.ready.catch(e => e);
  show('writer-released', a); console.log('writer same', a === b);
}
