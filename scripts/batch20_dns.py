#!/usr/bin/env python3
"""Patch node_compat.js: replace the dns module with full record-type support."""
import re, pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD_DNS = '''  registry.factories.dns = (natives) => {
    function notImpl(name) {
      return (...args) => {
        const cb = typeof args[args.length - 1] === "function" ? args[args.length - 1] : null;
        const err = Object.assign(
          new Error(`dns.${name} is not implemented in oam -- DNS record-type queries land with a later wave`),
          { code: "ENOSYS" },
        );
        if (cb) queueMicrotask(() => cb(err));
        else throw err;
      };
    }
    function notImplPromise(name) {
      return () => Promise.reject(
        Object.assign(
          new Error(`dns.promises.${name} is not implemented in oam -- DNS record-type queries land with a later wave`),
          { code: "ENOSYS" },
        ),
      );
    }

    function lookup(hostname, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      if (typeof options === "number") options = { family: options };
      const opts = options || {};
      const family = opts.family || 0;
      const all = !!opts.all;

      natives.dnsLookup(String(hostname), family, all).then(
        (result) => {
          if (all) {
            callback(null, result);
          } else {
            callback(null, result.address, result.family);
          }
        },
        (err) => {
          callback(err);
        },
      );
    }

    function resolve(hostname, rrtype, callback) {
      if (typeof rrtype === "function") {
        callback = rrtype;
        rrtype = "A";
      }
      rrtype = (rrtype || "A").toUpperCase();
      if (rrtype === "A") {
        natives.dnsLookup(String(hostname), 4, true).then(
          (results) => callback(null, results.map((r) => r.address)),
          (err) => callback(err),
        );
      } else if (rrtype === "AAAA") {
        natives.dnsLookup(String(hostname), 6, true).then(
          (results) => callback(null, results.map((r) => r.address)),
          (err) => callback(err),
        );
      } else {
        notImpl(`resolve(${rrtype})`)(hostname, callback);
      }
    }

    function resolve4(hostname, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      natives.dnsLookup(String(hostname), 4, true).then(
        (results) => {
          if (options && options.ttl) {
            callback(null, results.map((r) => ({ address: r.address, ttl: 0 })));
          } else {
            callback(null, results.map((r) => r.address));
          }
        },
        (err) => callback(err),
      );
    }

    function resolve6(hostname, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      natives.dnsLookup(String(hostname), 6, true).then(
        (results) => {
          if (options && options.ttl) {
            callback(null, results.map((r) => ({ address: r.address, ttl: 0 })));
          } else {
            callback(null, results.map((r) => r.address));
          }
        },
        (err) => callback(err),
      );
    }

    const RRTYPE_OK = new Set(["A", "AAAA"]);

    const promises = {
      lookup(hostname, options) {
        const opts = typeof options === "number" ? { family: options } : (options || {});
        const family = opts.family || 0;
        const all = !!opts.all;
        return natives.dnsLookup(String(hostname), family, all);
      },
      resolve(hostname, rrtype) {
        rrtype = (rrtype || "A").toUpperCase();
        if (rrtype === "A") {
          return natives.dnsLookup(String(hostname), 4, true).then((r) => r.map((x) => x.address));
        }
        if (rrtype === "AAAA") {
          return natives.dnsLookup(String(hostname), 6, true).then((r) => r.map((x) => x.address));
        }
        return notImplPromise(`resolve(${rrtype})`)();
      },
      resolve4(hostname, options) {
        return natives.dnsLookup(String(hostname), 4, true).then((r) => {
          if (options && options.ttl) return r.map((x) => ({ address: x.address, ttl: 0 }));
          return r.map((x) => x.address);
        });
      },
      resolve6(hostname, options) {
        return natives.dnsLookup(String(hostname), 6, true).then((r) => {
          if (options && options.ttl) return r.map((x) => ({ address: x.address, ttl: 0 }));
          return r.map((x) => x.address);
        });
      },
      resolveAny: notImplPromise("resolveAny"),
      resolveCname: notImplPromise("resolveCname"),
      resolveMx: notImplPromise("resolveMx"),
      resolveTxt: notImplPromise("resolveTxt"),
    };

    class Resolver {
      constructor() { this._servers = []; }
      resolve(hostname, rrtype, cb) {
        if (typeof rrtype === "function") { cb = rrtype; rrtype = "A"; }
        resolve(hostname, rrtype, cb);
      }
      resolve4(hostname, opts, cb) { resolve4(hostname, opts, cb); }
      resolve6(hostname, opts, cb) { resolve6(hostname, opts, cb); }
      cancel() {}
      getServers() { return this._servers.slice(); }
      setServers(servers) { this._servers = (servers || []).slice(); }
    }

    const ADDRCONFIG = 0;
    const V4MAPPED = 0;
    const ALL = 0;

    return {
      lookup,
      resolve,
      resolve4,
      resolve6,
      Resolver,
      promises,
      setDefaultResultOrder() {},
      setServers() {},
      getServers: () => [],
      ADDRCONFIG,
      V4MAPPED,
      ALL,
      NODATA: "NODATA",
      FORMERR: "FORMERR",
      SERVFAIL: "SERVFAIL",
      NOTFOUND: "NOTFOUND",
      NOTIMP: "NOTIMP",
      REFUSED: "REFUSED",
      BADQUERY: "BADQUERY",
      BADNAME: "BADNAME",
      BADFAMILY: "BADFAMILY",
      BADRESP: "BADRESP",
      CONNREFUSED: "CONNREFUSED",
      TIMEOUT: "TIMEOUT",
      EOF: "EOF",
      FILE: "FILE",
      NOMEM: "NOMEM",
      DESTRUCTION: "DESTRUCTION",
      BADSTR: "BADSTR",
      BADFLAGS: "BADFLAGS",
      NONAME: "NONAME",
      BADHINTS: "BADHINTS",
      NOTINITIALIZED: "NOTINITIALIZED",
      LOADIPHLPAPI: "LOADIPHLPAPI",
      ADDRGETNETWORKPARAMS: "ADDRGETNETWORKPARAMS",
      CANCELLED: "CANCELLED",
    };
  };'''

NEW_DNS = '''  registry.factories.dns = (natives) => {
    function lookup(hostname, options, callback) {
      if (typeof options === "function") {
        callback = options;
        options = {};
      }
      if (typeof options === "number") options = { family: options };
      const opts = options || {};
      const family = opts.family || 0;
      const all = !!opts.all;

      natives.dnsLookup(String(hostname), family, all).then(
        (result) => {
          if (all) {
            callback(null, result);
          } else {
            callback(null, result.address, result.family);
          }
        },
        (err) => callback(err),
      );
    }

    function _resolveNative(hostname, rrtype, callback) {
      natives.dnsResolve(String(hostname), rrtype).then(
        (result) => callback(null, result),
        (err) => callback(err),
      );
    }

    function resolve(hostname, rrtype, callback) {
      if (typeof rrtype === "function") {
        callback = rrtype;
        rrtype = "A";
      }
      rrtype = (rrtype || "A").toUpperCase();
      _resolveNative(hostname, rrtype, callback);
    }

    function resolve4(hostname, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      natives.dnsResolve(String(hostname), "A").then(
        (results) => {
          if (options && options.ttl) {
            callback(null, results.map((r) => ({ address: r, ttl: 0 })));
          } else {
            callback(null, results);
          }
        },
        (err) => callback(err),
      );
    }

    function resolve6(hostname, options, callback) {
      if (typeof options === "function") { callback = options; options = {}; }
      natives.dnsResolve(String(hostname), "AAAA").then(
        (results) => {
          if (options && options.ttl) {
            callback(null, results.map((r) => ({ address: r, ttl: 0 })));
          } else {
            callback(null, results);
          }
        },
        (err) => callback(err),
      );
    }

    function resolveCname(hostname, callback) {
      _resolveNative(hostname, "CNAME", callback);
    }

    function resolveMx(hostname, callback) {
      _resolveNative(hostname, "MX", callback);
    }

    function resolveTxt(hostname, callback) {
      _resolveNative(hostname, "TXT", callback);
    }

    function resolveNs(hostname, callback) {
      _resolveNative(hostname, "NS", callback);
    }

    function resolveSrv(hostname, callback) {
      _resolveNative(hostname, "SRV", callback);
    }

    function resolveSoa(hostname, callback) {
      _resolveNative(hostname, "SOA", callback);
    }

    function resolvePtr(hostname, callback) {
      _resolveNative(hostname, "PTR", callback);
    }

    function resolveCaa(hostname, callback) {
      _resolveNative(hostname, "CAA", callback);
    }

    function resolveNaptr(hostname, callback) {
      _resolveNative(hostname, "NAPTR", callback);
    }

    function resolveAny(hostname, callback) {
      const err = Object.assign(
        new Error("dns.resolveAny is not supported by oam (deprecated in Node.js)"),
        { code: "ENOSYS" },
      );
      if (typeof callback === "function") queueMicrotask(() => callback(err));
      else throw err;
    }

    function reverse(ip, callback) {
      natives.dnsReverse(String(ip)).then(
        (result) => callback(null, result),
        (err) => callback(err),
      );
    }

    const promises = {
      lookup(hostname, options) {
        const opts = typeof options === "number" ? { family: options } : (options || {});
        const family = opts.family || 0;
        const all = !!opts.all;
        return natives.dnsLookup(String(hostname), family, all);
      },
      resolve(hostname, rrtype) {
        rrtype = (rrtype || "A").toUpperCase();
        return natives.dnsResolve(String(hostname), rrtype);
      },
      resolve4(hostname, options) {
        return natives.dnsResolve(String(hostname), "A").then((r) => {
          if (options && options.ttl) return r.map((x) => ({ address: x, ttl: 0 }));
          return r;
        });
      },
      resolve6(hostname, options) {
        return natives.dnsResolve(String(hostname), "AAAA").then((r) => {
          if (options && options.ttl) return r.map((x) => ({ address: x, ttl: 0 }));
          return r;
        });
      },
      resolveCname(hostname) { return natives.dnsResolve(String(hostname), "CNAME"); },
      resolveMx(hostname) { return natives.dnsResolve(String(hostname), "MX"); },
      resolveTxt(hostname) { return natives.dnsResolve(String(hostname), "TXT"); },
      resolveNs(hostname) { return natives.dnsResolve(String(hostname), "NS"); },
      resolveSrv(hostname) { return natives.dnsResolve(String(hostname), "SRV"); },
      resolveSoa(hostname) { return natives.dnsResolve(String(hostname), "SOA"); },
      resolvePtr(hostname) { return natives.dnsResolve(String(hostname), "PTR"); },
      resolveCaa(hostname) { return natives.dnsResolve(String(hostname), "CAA"); },
      resolveNaptr(hostname) { return natives.dnsResolve(String(hostname), "NAPTR"); },
      resolveAny() {
        return Promise.reject(Object.assign(
          new Error("dns.resolveAny is not supported by oam (deprecated in Node.js)"),
          { code: "ENOSYS" },
        ));
      },
      reverse(ip) { return natives.dnsReverse(String(ip)); },
    };

    class Resolver {
      constructor() { this._servers = []; }
      resolve(hostname, rrtype, cb) {
        if (typeof rrtype === "function") { cb = rrtype; rrtype = "A"; }
        resolve(hostname, rrtype, cb);
      }
      resolve4(hostname, opts, cb) { resolve4(hostname, opts, cb); }
      resolve6(hostname, opts, cb) { resolve6(hostname, opts, cb); }
      resolveCname(hostname, cb) { resolveCname(hostname, cb); }
      resolveMx(hostname, cb) { resolveMx(hostname, cb); }
      resolveTxt(hostname, cb) { resolveTxt(hostname, cb); }
      resolveNs(hostname, cb) { resolveNs(hostname, cb); }
      resolveSrv(hostname, cb) { resolveSrv(hostname, cb); }
      resolveSoa(hostname, cb) { resolveSoa(hostname, cb); }
      resolvePtr(hostname, cb) { resolvePtr(hostname, cb); }
      resolveCaa(hostname, cb) { resolveCaa(hostname, cb); }
      resolveNaptr(hostname, cb) { resolveNaptr(hostname, cb); }
      reverse(ip, cb) { reverse(ip, cb); }
      cancel() {}
      getServers() { return this._servers.slice(); }
      setServers(servers) { this._servers = (servers || []).slice(); }
    }

    const ADDRCONFIG = 0;
    const V4MAPPED = 0;
    const ALL = 0;

    return {
      lookup,
      resolve,
      resolve4,
      resolve6,
      resolveCname,
      resolveMx,
      resolveTxt,
      resolveNs,
      resolveSrv,
      resolveSoa,
      resolvePtr,
      resolveCaa,
      resolveNaptr,
      resolveAny,
      reverse,
      Resolver,
      promises,
      setDefaultResultOrder() {},
      setServers() {},
      getServers: () => [],
      ADDRCONFIG,
      V4MAPPED,
      ALL,
      NODATA: "NODATA",
      FORMERR: "FORMERR",
      SERVFAIL: "SERVFAIL",
      NOTFOUND: "NOTFOUND",
      NOTIMP: "NOTIMP",
      REFUSED: "REFUSED",
      BADQUERY: "BADQUERY",
      BADNAME: "BADNAME",
      BADFAMILY: "BADFAMILY",
      BADRESP: "BADRESP",
      CONNREFUSED: "CONNREFUSED",
      TIMEOUT: "TIMEOUT",
      EOF: "EOF",
      FILE: "FILE",
      NOMEM: "NOMEM",
      DESTRUCTION: "DESTRUCTION",
      BADSTR: "BADSTR",
      BADFLAGS: "BADFLAGS",
      NONAME: "NONAME",
      BADHINTS: "BADHINTS",
      NOTINITIALIZED: "NOTINITIALIZED",
      LOADIPHLPAPI: "LOADIPHLPAPI",
      ADDRGETNETWORKPARAMS: "ADDRGETNETWORKPARAMS",
      CANCELLED: "CANCELLED",
    };
  };'''

assert OLD_DNS in src, f"OLD_DNS not found in node_compat.js"
src = src.replace(OLD_DNS, NEW_DNS, 1)
p.write_text(src, encoding="utf-8")
print(f"OK -- patched dns module ({len(NEW_DNS)} chars)")
