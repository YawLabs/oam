// node 26's URLPattern: the global, url.URLPattern, and their behaviour.
//
// Pins the pattern strings of each component (init and constructor-string
// forms, with and without a base URL), test() / exec() over URL strings and
// init dictionaries, the exec result's shape (members in Web IDL
// lexicographic order, `inputs` as given, an init reduced to its string
// members), named, wildcard, optional / repeated and regular-expression
// groups, ignoreCase, the argument handling (an object second argument is
// the options; null is no base), and node's errors.
import url from "node:url";
import util from "node:util";

const T = (label, fn) => {
  try {
    console.log(label, fn());
  } catch (e) {
    console.log(label, "threw", e.name, e.code, JSON.stringify(e.message));
  }
};
const J = (v) => JSON.stringify(v);
console.log(url.URLPattern === URLPattern, URLPattern.length, Object.getOwnPropertyNames(URLPattern.prototype).join(','));
const p = new URLPattern({ pathname: '/books/:id' });
console.log(p.protocol, p.username, p.password, p.hostname, p.port, p.pathname, p.search, p.hash, p.hasRegExpGroups);
console.log(p.test('https://example.com/books/123'), JSON.stringify(p.exec('https://example.com/books/123')));
console.log(JSON.stringify(p.exec({ pathname: '/books/7' })));
console.log(p.exec('https://example.com/nope'));
T('noargs', () => { const q = new URLPattern(); return [q.pathname, q.protocol, q.test('http://x/y')]; });
T('string', () => { const q = new URLPattern('https://*.example.com/:a/:b?'); return [q.protocol, q.hostname, q.pathname, JSON.stringify(q.exec('https://www.example.com/x'))]; });
T('base', () => { const q = new URLPattern('/foo/:bar', 'https://example.com'); return [q.protocol, q.hostname, q.pathname, q.port]; });
T('relative no base', () => new URLPattern('/foo'));
T('bad pattern', () => new URLPattern({ pathname: '/(' }));
T('bad regexp', () => new URLPattern({ pathname: '/(a' }));
T('regexp groups', () => { const q = new URLPattern({ pathname: '/(\\d+)' }); return [q.hasRegExpGroups, JSON.stringify(q.exec({pathname:'/42'}))]; });
T('ignoreCase', () => { const q = new URLPattern({ pathname: '/ABC' }, { ignoreCase: true }); return q.test({ pathname: '/abc' }); });
T('init + base', () => new URLPattern({ pathname: '/a' }, 'https://x.com'));
T('test init + base', () => p.test({ pathname: '/a' }, 'https://x.com'));
T('input num', () => new URLPattern(5));
T('options bad', () => new URLPattern('https://a.com/', 5));
T('test invalid url', () => p.test('not a url'));
T('exec invalid url', () => p.exec('not a url'));
T('search hash', () => { const q = new URLPattern('https://a.com/x?q=:v#h:w'); return [q.search, q.hash, JSON.stringify(q.exec('https://a.com/x?q=1#h2'))]; });
T('wildcard groups', () => JSON.stringify(new URLPattern({ pathname: '/a/*' }).exec({ pathname: '/a/b/c' }).pathname));
T('toStringTag', () => Object.prototype.toString.call(p));
T('inspect', () => util.inspect(p));
T('getter brand', () => Object.getOwnPropertyDescriptor(URLPattern.prototype, 'pathname').get.call({}));
T('call no new', () => URLPattern({}));
T('baseURL in init', () => new URLPattern({ pathname: '/x', baseURL: 'https://h.com/' }).hostname);
T('port default', () => new URLPattern('https://a.com:443/').port);
T('optional modifiers', () => new URLPattern({ pathname: '/:a+/:b*/:c?' }).exec({ pathname: '/1/2/3/4' }).pathname.groups);
T('test no args', () => p.test());
T('exec obj with extra', () => JSON.stringify(p.exec({ pathname: '/books/1', foo: 1 })));
T('ipv6', () => new URLPattern({ hostname: '[::1]' }).hostname);
T('encoded', () => new URLPattern({ pathname: '/caf\u00e9' }).pathname);
T("non-string members ignored", () => new URLPattern({ pathname: { toString() { return "/z"; } } }).pathname);
T("init non-string dropped from inputs", () => J(new URLPattern({ pathname: "/x" }).exec({ pathname: "/x", hash: 5 }).inputs));
T("init inputs order", () => J(new URLPattern({ pathname: "/x" }).exec({ pathname: "/x", baseURL: "https://q.com/", hash: "h" }).inputs));
T("ignoreCase truthy", () => [
  new URLPattern({ pathname: "/A" }, { ignoreCase: "yes" }).test({ pathname: "/a" }),
  new URLPattern({ pathname: "/A" }, { ignoreCase: 0 }).test({ pathname: "/a" }),
]);
T("null second arg", () => new URLPattern({}, null).pathname);
T("relative with null base", () => new URLPattern("/x", null));
T("third arg type", () => new URLPattern("https://a.com/", "https://b.com", 5));
T("exec init + base", () => new URLPattern({ pathname: "/a" }).exec({ pathname: "/a" }, "https://x.com"));
T("exec type", () => new URLPattern({}).exec(5));
T("bad base in init", () => new URLPattern({ pathname: "/x", baseURL: "notaurl" }));
T("bad base on match", () => [new URLPattern("https://a.com/b").test("https://a.com/b", "bad base"), new URLPattern("/b", "https://a.com").exec("/b", "nope")]);
T("string inputs with base", () => J(new URLPattern("/x", "https://b.com/", { ignoreCase: true }).exec("/X", "https://b.com/").inputs));
T("router", () => {
  const routes = [new URLPattern({ pathname: "/users/:id(\\d+)" }), new URLPattern({ pathname: "/users/:name" })];
  return ["/users/42", "/users/ann", "/posts/1"].map((p) => {
    const hit = routes.find((r) => r.test({ pathname: p }));
    return hit ? J(hit.exec({ pathname: p }).pathname.groups) : "none";
  }).join(" ");
});
T("protocol alternation", () => {
  const p = new URLPattern("http{s}?://example.com/*");
  return [p.protocol, p.test("http://example.com/a"), p.test("https://example.com/a"), p.test("ftp://example.com/a")];
});
T("exec result prototype", () => {
  const r = new URLPattern({ pathname: "/:a" }).exec({ pathname: "/x" });
  return [Object.getPrototypeOf(r) === null, Object.getPrototypeOf(r.pathname) === Object.prototype, "toString" in r];
});
T("hostname groups", () => J(new URLPattern({ hostname: "{:sub.}?example.com" }).exec({ hostname: "api.example.com" }).hostname));
T("descriptors", () => {
  const d = Object.getOwnPropertyDescriptor(URLPattern.prototype, "pathname");
  return [typeof d.get, d.enumerable, d.configurable, Object.getOwnPropertyDescriptor(URLPattern.prototype, "exec").enumerable];
});
