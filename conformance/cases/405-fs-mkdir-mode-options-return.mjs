// fs.mkdir in its three forms -- mkdirSync, the callback form (C) and
// fs.promises.mkdir (P) -- against node, row by row:
//
// 1-3. what may follow the path: a number or a string is the mode, anything
//      else truthy is options whose `recursive` must be a boolean and whose
//      `mode` is checked under its own name (an argument's name in the
//      promise form, which also lets a `null` mode be the default);
// 4.   the order the arguments are checked in, which differs by form;
// 5.   what a recursive mkdir returns: the first directory THAT CALL made,
//      in the spelling node gives it (the namespaced absolute path on
//      Windows, the caller's own on unix), and nothing when it made none;
// 6.   the errors, and which path each names: a file where a directory
//      has to be is ENOTDIR for a component and EEXIST for the path itself,
//      and the callback and promise forms name the component that failed
//      where the sync form names the path asked for;
// 7.   the mode, given to every directory a recursive call makes and to
//      none that was already there, under the process's mask (unix).
//
// The temp root is printed as <T> and each scratch directory as cN.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const WIN = process.platform === 'win32';
const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'oam-case-405-')));
const origCwd = process.cwd();
let n = 0;
const fresh = () => {
  const d = path.join(base, 'c' + (n++));
  fs.mkdirSync(d);
  return d;
};

function scrub(s) {
  if (typeof s !== 'string') return s;
  const forms = [base, base.replace(/\\/g, '/'), base.replace(/\\/g, '\\\\')];
  for (const f of forms) {
    let i;
    const low = s.toLowerCase();
    const fl = f.toLowerCase();
    let out = '';
    let pos = 0;
    while ((i = low.indexOf(fl, pos)) !== -1) {
      out += s.slice(pos, i) + '<T>';
      pos = i + f.length;
    }
    s = out + s.slice(pos);
  }
  return s.replace(/c\d+(?=[\\/'"]|$)/g, 'cN');
}
function show(v) {
  if (v instanceof Error) {
    const o = { name: v.name, code: v.code, message: v.message };
    for (const k of ['errno', 'syscall', 'path', 'dest']) if (k in v) o[k] = v[k];
    o.keys = Object.keys(v).join(',');
    return scrub('ERR ' + JSON.stringify(o));
  }
  if (v === undefined) return 'undefined';
  if (typeof v === 'string') return scrub('str ' + JSON.stringify(v));
  return scrub(typeof v + ' ' + String(v));
}
const out = (label, s) => console.log(label.padEnd(46) + ' | ' + s);

function sync(label, fn) {
  let r;
  try { r = 'ret ' + show(fn()); } catch (e) { r = 'throw ' + show(e); }
  out('S ' + label, r);
}
function cb(label, call) {
  return new Promise((resolve) => {
    let done = false;
    const t = setTimeout(() => { if (!done) { done = true; out('C ' + label, 'NO CALLBACK in 2s'); resolve(); } }, 2000);
    const f = function (...args) {
      if (done) return;
      done = true; clearTimeout(t);
      out('C ' + label, 'cb argc=' + args.length + ' [' + args.map((a) => a === null ? 'null' : show(a)).join(', ') + ']');
      resolve();
    };
    try {
      const r = call(f);
      if (r !== undefined) out('C ' + label + ' (return)', show(r));
    } catch (e) {
      done = true; clearTimeout(t);
      out('C ' + label, 'sync-throw ' + show(e));
      resolve();
    }
  });
}
async function prom(label, call) {
  let p;
  try { p = call(); } catch (e) { out('P ' + label, 'sync-throw ' + show(e)); return; }
  try { out('P ' + label, 'resolve ' + show(await p)); } catch (e) { out('P ' + label, 'reject ' + show(e)); }
}
const modeOf = (p) => { try { return (fs.statSync(p).mode & 0o7777).toString(8); } catch (e) { return 'ERR ' + e.code; } };
const exists = (p) => fs.existsSync(p);

(async () => {
  console.log('## platform ' + process.platform + ' umask ' + process.umask().toString(8));

  console.log('## 1. options kinds (sync), target is a fresh non-existing dir each time');
  const kinds = [
    ['undefined', undefined], ['null', null], ['false', false], ['true', true], ['0', 0], ['0o700', 0o700],
    ['-1', -1], ['1.5', 1.5], ['NaN', NaN], ['2**32', 2 ** 32], ['Infinity', Infinity],
    ['"700"', '700'], ['"0o700"', '0o700'], ['"0700"', '0700'], ['"abc"', 'abc'], ['""', ''], ['"8"', '8'], ['"-1"', '-1'],
    ['"77777777777777"', '77777777777777'],
    ['function', () => {}], ['[]', []], ['[true]', [true]], ['Symbol', Symbol('s')], ['1n', 1n], ['new Number(448)', new Number(448)],
    ['{}', {}], ['{recursive:true}', { recursive: true }], ['{recursive:false}', { recursive: false }],
    ['{recursive:undefined}', { recursive: undefined }], ['{recursive:null}', { recursive: null }],
    ['{recursive:"yes"}', { recursive: 'yes' }], ['{recursive:1}', { recursive: 1 }], ['{recursive:0}', { recursive: 0 }],
    ['{recursive:{}}', { recursive: {} }],
    ['{mode:0o700}', { mode: 0o700 }], ['{mode:"700"}', { mode: '700' }], ['{mode:"abc"}', { mode: 'abc' }],
    ['{mode:null}', { mode: null }], ['{mode:undefined}', { mode: undefined }], ['{mode:-1}', { mode: -1 }],
    ['{mode:1.5}', { mode: 1.5 }], ['{mode:true}', { mode: true }], ['{mode:{}}', { mode: {} }], ['{mode:2**32}', { mode: 2 ** 32 }],
    ['{mode:1n}', { mode: 1n }],
    ['{recursive:"yes",mode:"abc"}', { recursive: 'yes', mode: 'abc' }],
    ['{mode:"abc",recursive:"yes"}', { mode: 'abc', recursive: 'yes' }],
    ['{recursive:true,mode:0o700}', { recursive: true, mode: 0o700 }],
    ['Object.create({recursive:"yes"})', Object.create({ recursive: 'yes' })],
  ];
  for (const [label, o] of kinds) {
    const d = path.join(fresh(), 'x');
    let r;
    try { r = 'ret ' + show(fs.mkdirSync(d, o)); } catch (e) { r = 'throw ' + show(e); }
    out('S opts=' + label, r + ' | created=' + exists(d) + (WIN ? '' : ' mode=' + modeOf(d)));
  }

  console.log('## 2. options kinds (callback form)');
  for (const [label, o] of kinds) {
    if (label === 'function') continue;
    const d = path.join(fresh(), 'x');
    await cb('opts=' + label, (f) => fs.mkdir(d, o, f));
  }
  console.log('## 3. options kinds (promises)');
  for (const [label, o] of kinds) {
    const d = path.join(fresh(), 'x');
    await prom('opts=' + label, () => fs.promises.mkdir(d, o));
  }

  console.log('## 4. validation order');
  sync('path=123, opts={recursive:"yes"}', () => fs.mkdirSync(123, { recursive: 'yes' }));
  sync('path=123, opts="abc"', () => fs.mkdirSync(123, 'abc'));
  sync('path=123, opts={mode:"abc"}', () => fs.mkdirSync(123, { mode: 'abc' }));
  sync('path=123', () => fs.mkdirSync(123));
  sync('path=undefined', () => fs.mkdirSync());
  sync('path=null', () => fs.mkdirSync(null));
  sync('path="a\\0b"', () => fs.mkdirSync('a\0b'));
  sync('path="a\\0b", {recursive:true}', () => fs.mkdirSync('a\0b', { recursive: true }));
  sync('path=""', () => fs.mkdirSync(''));
  sync('path="", {recursive:true}', () => fs.mkdirSync('', { recursive: true }));
  sync('path={}', () => fs.mkdirSync({}));
  await cb('path=123, opts={recursive:"yes"}, cb=notfn', () => fs.mkdir(123, { recursive: 'yes' }, 'notfn'));
  await cb('path=123, opts={}, cb=notfn', () => fs.mkdir(123, {}, 'notfn'));
  await cb('path=123, opts={}, cb=undefined', () => fs.mkdir(123, {}));
  await cb('path=123, cb only', (f) => fs.mkdir(123, f));
  await cb('path=123, opts={}, cb', (f) => fs.mkdir(123, {}, f));
  await cb('path=valid, no cb', () => fs.mkdir(path.join(fresh(), 'x')));
  await cb('path=valid, opts="abc", cb=notfn', () => fs.mkdir(path.join(fresh(), 'x'), 'abc', 5));
  await cb('path=valid, opts=fn, cb=fn2 (which is called)', (f) => fs.mkdir(path.join(fresh(), 'x'), f, () => out('C   second fn', 'CALLED')));
  await cb('path=valid, opts=null, cb', (f) => fs.mkdir(path.join(fresh(), 'x'), null, f));
  await cb('path=valid, opts=undefined, cb', (f) => fs.mkdir(path.join(fresh(), 'x'), undefined, f));
  await cb('path="a\\0b", cb', (f) => fs.mkdir('a\0b', f));
  await prom('path=123, opts={recursive:"yes"}', () => fs.promises.mkdir(123, { recursive: 'yes' }));
  await prom('path=123, opts="abc"', () => fs.promises.mkdir(123, 'abc'));
  await prom('path=123, opts={mode:"abc"}', () => fs.promises.mkdir(123, { mode: 'abc' }));
  await prom('path=123', () => fs.promises.mkdir(123));
  await prom('path=undefined', () => fs.promises.mkdir());
  await prom('path="a\\0b"', () => fs.promises.mkdir('a\0b'));
  await prom('path=""', () => fs.promises.mkdir(''));
  await prom('path="", {recursive:true}', () => fs.promises.mkdir('', { recursive: true }));
  {
    // does a rejected validation leave the directory uncreated / a valid sibling option unread?
    const order = [];
    const o = { get recursive() { order.push('recursive'); return true; }, get mode() { order.push('mode'); return 0o700; } };
    const d = fresh();
    fs.mkdirSync(path.join(d, 'a'), o); out('S getter order', order.join(','));
    order.length = 0;
    await new Promise((r) => fs.mkdir(path.join(d, 'b'), o, r)); out('C getter order', order.join(','));
    order.length = 0;
    await fs.promises.mkdir(path.join(d, 'c'), o); out('P getter order', order.join(','));
  }

  console.log('## 5. return values');
  const three = async (label, mk, opts, after) => {
    for (const form of ['S', 'C', 'P']) {
      const d = fresh();
      const p = mk(d);
      process.chdir(d);
      try {
        if (form === 'S') sync(label, () => fs.mkdirSync(p, opts));
        else if (form === 'C') await cb(label, (f) => fs.mkdir(p, opts, f));
        else await prom(label, () => fs.promises.mkdir(p, opts));
        if (after) out('  ' + form + ' after', scrub(after(d)));
      } finally { process.chdir(origCwd); }
    }
  };
  const R = { recursive: true };
  await three('nonrec new abs', (d) => path.join(d, 'a'));
  await three('nonrec {recursive:false}', (d) => path.join(d, 'a'), { recursive: false });
  await three('rec abs a/b/c none exist', (d) => path.join(d, 'a', 'b', 'c'), R);
  await three('rec abs, a exists', (d) => { fs.mkdirSync(path.join(d, 'a')); return path.join(d, 'a', 'b', 'c'); }, R);
  await three('rec abs, a/b exists (only leaf new)', (d) => { fs.mkdirSync(path.join(d, 'a', 'b'), R); return path.join(d, 'a', 'b', 'c'); }, R);
  await three('rec abs, all exist', (d) => { fs.mkdirSync(path.join(d, 'a', 'b', 'c'), R); return path.join(d, 'a', 'b', 'c'); }, R);
  await three('rec abs single new leaf', (d) => path.join(d, 'a'), R);
  await three('rec relative "a/b/c"', () => 'a/b/c', R);
  await three('rec relative "./a/b/c"', () => './a/b/c', R);
  await three('rec relative "a"', () => 'a', R);
  await three('rec relative "a\\\\b" (backslash)', () => 'a\\b', R, (d) => 'entries=' + fs.readdirSync(d).join(','));
  await three('rec abs forward slashes', (d) => d.replace(/\\/g, '/') + '/a/b/c', R);
  await three('rec abs mixed + double seps', (d) => d + '/a//b/c', R);
  await three('rec trailing slash', (d) => path.join(d, 'a', 'b') + '/', R);
  await three('rec trailing backslash', (d) => path.join(d, 'a', 'b') + '\\', R, (d) => 'entries=' + fs.readdirSync(d).join(','));
  await three('rec with .. (a/../b/c)', (d) => d + path.sep + 'a' + path.sep + '..' + path.sep + 'b' + path.sep + 'c', R, (d) => 'entries=' + fs.readdirSync(d).join(','));
  await three('rec relative with .. (x/../y/z)', () => 'x/../y/z', R, (d) => 'entries=' + fs.readdirSync(d).join(','));
  await three('rec with . (a/./b)', (d) => d + path.sep + 'a' + path.sep + '.' + path.sep + 'b', R);
  await three('rec Buffer path', (d) => Buffer.from(path.join(d, 'a', 'b')), R);
  await three('rec URL path', (d) => pathToFileURL(path.join(d, 'a', 'b')), R);
  await three('rec "."', () => '.', R);
  if (WIN) {
    await three('rec \\\\?\\ namespaced abs', (d) => '\\\\?\\' + path.join(d, 'a', 'b'), R);
    await three('rec lower-case drive letter', (d) => d[0].toLowerCase() + d.slice(1) + '\\a\\b', R);
    await three('rec UPPERCASED existing prefix', (d) => d.toUpperCase() + '\\a\\b', R);
    await three('rec drive root', (d) => d.slice(0, 3), R);
    await three('rec drive-relative "C:a\\b"', (d) => d.slice(0, 2) + 'a\\b', R, (d) => 'entries=' + fs.readdirSync(d).join(','));
    await three('rec rooted no drive "\\<T-without-drive>\\a\\b"', (d) => d.slice(2) + '\\a\\b', R);
    await three('rec long path (>260)', (d) => path.join(d, 'L'.repeat(120), 'M'.repeat(120), 'N'.repeat(120)), R, (d) => 'exists=' + exists(path.join(d, 'L'.repeat(120), 'M'.repeat(120), 'N'.repeat(120))));
  }
  await three('rec through symlinked dir', (d) => {
    fs.mkdirSync(path.join(d, 'real'));
    try { fs.symlinkSync(path.join(d, 'real'), path.join(d, 'lnk'), 'junction'); } catch (e) { return path.join(d, 'real', 'a', 'b'); }
    return path.join(d, 'lnk', 'a', 'b');
  }, R);

  console.log('## 6. errors');
  await three('nonrec on existing dir', (d) => { fs.mkdirSync(path.join(d, 'a')); return path.join(d, 'a'); });
  await three('nonrec on existing file', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return path.join(d, 'f'); });
  await three('nonrec missing parent', (d) => path.join(d, 'no', 'a'));
  await three('nonrec parent is a file', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return path.join(d, 'f', 'a'); });
  await three('nonrec relative missing parent "no/a"', () => 'no/a');
  await three('nonrec relative existing "."', () => '.');
  await three('nonrec trailing slash on existing', (d) => { fs.mkdirSync(path.join(d, 'a')); return path.join(d, 'a') + '/'; });
  await three('rec on existing file', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return path.join(d, 'f'); }, R);
  await three('rec relative on existing file "f"', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return 'f'; }, R);
  await three('rec through file component f/a', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return path.join(d, 'f', 'a'); }, R);
  await three('rec through file component f/a/b', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return path.join(d, 'f', 'a', 'b'); }, R);
  await three('rec through file deep x/f/a/b (x new?)', (d) => { fs.mkdirSync(path.join(d, 'x')); fs.writeFileSync(path.join(d, 'x', 'f'), ''); return path.join(d, 'x', 'f', 'a', 'b'); }, R);
  await three('rec relative through file "f/a/b"', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return 'f/a/b'; }, R);
  await three('rec forward-slash through file', (d) => { fs.writeFileSync(path.join(d, 'f'), ''); return d.replace(/\\/g, '/') + '/f/a'; }, R);
  await three('nonrec forward-slash existing (err.path form)', (d) => { fs.mkdirSync(path.join(d, 'a')); return d.replace(/\\/g, '/') + '/a'; });
  await three('nonrec Buffer existing', (d) => { fs.mkdirSync(path.join(d, 'a')); return Buffer.from(path.join(d, 'a')); });
  await three('nonrec URL existing', (d) => { fs.mkdirSync(path.join(d, 'a')); return pathToFileURL(path.join(d, 'a')); });
  await three('rec dangling symlink leaf', (d) => {
    try { fs.symlinkSync(path.join(d, 'gone'), path.join(d, 'lnk'), 'junction'); } catch (e) { return path.join(d, 'nolink-skip'); }
    return path.join(d, 'lnk');
  }, R);
  if (WIN) {
    await three('nonrec invalid name "a<b"', (d) => path.join(d, 'a<b'));
    await three('rec invalid name "a<b/c"', (d) => path.join(d, 'a<b', 'c'), R);
    await three('rec reserved "a:b"', (d) => path.join(d, 'a:b', 'c'), R);
  } else {
    await three('rec parent not writable', (d) => { fs.mkdirSync(path.join(d, 'ro'), 0o500); return path.join(d, 'ro', 'a', 'b'); }, R);
    await three('nonrec parent not writable', (d) => { fs.mkdirSync(path.join(d, 'ro'), 0o500); return path.join(d, 'ro', 'a'); });
  }
  {
    const d = fresh();
    try { fs.mkdirSync(path.join(d, 'a')); fs.mkdirSync(path.join(d, 'a')); } catch (e) {
      out('S err proto/stack', 'instanceof Error=' + (e instanceof Error) + ' ctor=' + e.constructor.name + ' stackHead=' + scrub(String(e.stack).split('\n')[0]));
    }
  }

  console.log('## 7. mode (meaningful on unix only)');
  const modes = async (label, opts, um) => {
    for (const form of ['S', 'C', 'P']) {
      const d = fresh();
      const p = path.join(d, 'a', 'b', 'c');
      const old = um === undefined ? undefined : process.umask(um);
      let r;
      try {
        if (form === 'S') r = fs.mkdirSync(p, opts);
        else if (form === 'C') r = await new Promise((res, rej) => fs.mkdir(p, opts, (e, v) => e ? rej(e) : res(v)));
        else r = await fs.promises.mkdir(p, opts);
        r = show(r);
      } catch (e) { r = show(e); }
      if (old !== undefined) process.umask(old);
      out(form + ' ' + label, 'a=' + modeOf(path.join(d, 'a')) + ' a/b=' + modeOf(path.join(d, 'a', 'b')) + ' a/b/c=' + modeOf(p) + ' | ' + r);
      try { for (const q of [path.join(d, 'a'), path.join(d, 'a', 'b'), p]) fs.chmodSync(q, 0o700); } catch (e) {}
    }
  };
  await modes('rec default', { recursive: true });
  await modes('rec mode 0o700', { recursive: true, mode: 0o700 });
  await modes('rec mode "750"', { recursive: true, mode: '750' });
  await modes('rec mode 0o777 umask 0', { recursive: true, mode: 0o777 }, 0);
  await modes('rec mode 0o777 umask 077', { recursive: true, mode: 0o777 }, 0o077);
  await modes('rec mode 0o1777 umask 0 (sticky)', { recursive: true, mode: 0o1777 }, 0);
  await modes('rec mode 0o2755 umask 0 (setgid)', { recursive: true, mode: 0o2755 }, 0);
  await modes('rec mode 0o500 (no w: can it descend?)', { recursive: true, mode: 0o500 });
  await modes('rec mode 0o300 umask 0', { recursive: true, mode: 0o300 }, 0);
  await modes('rec mode 0 umask 0', { recursive: true, mode: 0 }, 0);
  await modes('rec mode 0o40700 (type bits)', { recursive: true, mode: 0o40700 }, 0);
  const single = async (label, opts, um) => {
    for (const form of ['S', 'C', 'P']) {
      const d = path.join(fresh(), 'x');
      const old = um === undefined ? undefined : process.umask(um);
      let r;
      try {
        if (form === 'S') r = fs.mkdirSync(d, opts);
        else if (form === 'C') r = await new Promise((res, rej) => fs.mkdir(d, opts, (e, v) => e ? rej(e) : res(v)));
        else r = await fs.promises.mkdir(d, opts);
        r = show(r);
      } catch (e) { r = show(e); }
      if (old !== undefined) process.umask(old);
      out(form + ' ' + label, 'mode=' + modeOf(d) + ' | ' + r);
    }
  };
  await single('nonrec default');
  await single('nonrec 0o700 (number)', 0o700);
  await single('nonrec "700" (string)', '700');
  await single('nonrec {mode:0o700}', { mode: 0o700 });
  await single('nonrec {mode:"0o711"}', { mode: '0o711' });
  await single('nonrec 0o777 umask 0', 0o777, 0);
  await single('nonrec 0o777 umask 027', 0o777, 0o027);
  await single('nonrec 0 umask 0', 0, 0);
  await single('nonrec 0o4755 umask 0', 0o4755, 0);
  await single('nonrec 0o1777 umask 0', 0o1777, 0);
  await single('nonrec 0o40700 umask 0', 0o40700, 0);
  await single('nonrec 0o777777 umask 0', 0o777777, 0);
  // existing dir is not re-moded by a recursive call
  {
    const d = fresh();
    fs.mkdirSync(path.join(d, 'a'), 0o755);
    fs.mkdirSync(path.join(d, 'a', 'b'), { recursive: true, mode: 0o700 });
    out('S rec mode leaves existing parent', 'a=' + modeOf(path.join(d, 'a')) + ' a/b=' + modeOf(path.join(d, 'a', 'b')));
    fs.mkdirSync(path.join(d, 'a', 'b'), { recursive: true, mode: 0o711 });
    out('S rec mode on all-existing', 'a/b=' + modeOf(path.join(d, 'a', 'b')));
  }
})().catch((e) => console.log('PROBE CRASH ' + (e && e.stack))).finally(() => {
  process.chdir(origCwd);
  const fix = (p) => { try { for (const e of fs.readdirSync(p, { withFileTypes: true })) if (e.isDirectory() && !e.isSymbolicLink()) { const q = path.join(p, e.name); try { fs.chmodSync(q, 0o700); } catch (x) {} fix(q); } } catch (x) {} };
  if (!WIN) fix(base);
  try { fs.rmSync(base, { recursive: true, force: true }); } catch (e) { console.log('cleanup failed ' + e.message); }
});
