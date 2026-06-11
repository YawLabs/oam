// URL + URLSearchParams semantics.
const u = new URL("https://user:pw@example.com:8443/a/b?x=1&y=2#frag");
console.log(u.protocol, u.username, u.host, u.port, u.pathname, u.search, u.hash, u.origin);
console.log(new URL("../up?q", "https://example.com/a/b/c").href);
console.log(new URL("http://bücher.de/p").hostname);
u.port = "8080 ";
u.hash = "new";
console.log(u.href);
u.hostname = "b.com:99"; // setter no-ops whole on ':'
console.log(u.hostname);
const du = new URL("data:text/plain,abc");
du.pathname = "xyz"; // opaque path: no-op
console.log(du.href);
const q = new URL("https://x.example/p?");
console.log(JSON.stringify(q.search), q.href);
console.log(URL.canParse("not a url"), URL.canParse("https://ok.dev"));

const sp = new URLSearchParams("a=1&b=two+words&a=3&empty=&plain");
console.log(sp.get("a"), sp.getAll("a").join(","), sp.get("b"), JSON.stringify(sp.get("plain")), sp.size);
sp.append("c", "x y");
sp.set("a", "only");
sp.delete("plain");
sp.sort();
console.log(sp.toString(), [...sp.keys()].join(","));
console.log(new URLSearchParams("e=\u{1F984}").toString());
const live = new URLSearchParams("a=1&b=2&c=3&d=4");
for (const [k] of live) live.delete(k);
console.log(live.toString(), live.size);
const linked = new URL("https://x.dev/p?one=1");
linked.searchParams.set("two", "2");
console.log(linked.href);
