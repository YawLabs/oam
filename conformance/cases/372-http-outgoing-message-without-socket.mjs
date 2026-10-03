// node's OutgoingMessage#write / #end on a message that never gets a socket:
// one built by http.OutgoingMessage.call(this) alone, as nock's
// OverriddenClientRequest is when it refuses a request, and then ended by
// its caller. The bytes are buffered, headersSent and finished turn true,
// write() returns true, and 'finish' never fires. oam's ClientRequest#write
// and #end threw on such an object (#207).
import http from 'node:http';
function Bare() { http.OutgoingMessage.call(this); }
Object.setPrototypeOf(Bare.prototype, http.ClientRequest.prototype);
const r = new Bare();
r.on('finish', () => console.log('finish'));
r.on('error', (e) => console.log('error', e.code));
console.log('before', r.headersSent, r.finished, r.writableEnded);
console.log('write', r.write('abc'));
console.log('after write', r.headersSent, r.finished);
const ret = r.end('d', () => console.log('end cb'));
console.log('end returns this', ret === r);
console.log('after end', r.headersSent, r.finished, r.writableEnded, r.writableFinished);
const second = r.end(() => console.log('second end cb'));
console.log('second end returns this', second === r);
const r2 = new Bare();
console.log('end only', r2.end() === r2, r2.headersSent, r2.finished);
setTimeout(() => console.log('done'), 50);
