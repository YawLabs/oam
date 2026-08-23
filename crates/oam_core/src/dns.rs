use std::net::IpAddr;
use std::sync::OnceLock;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::proto::rr::RecordType;

use super::OpOutcome;

/// The nameservers the resolver was actually built from, in config order.
///
/// `Resolver` exposes `options()` but NOT its `ResolverConfig`, so the list has
/// to be captured at build time. This replicates what `builder_tokio()` does
/// internally -- `read_system_conf()` then `builder_with_config()` -- rather
/// than reading the system config a second time, so what `dns.getServers()`
/// reports is by construction the same list the queries go to.
static NAME_SERVERS: OnceLock<Vec<String>> = OnceLock::new();

/// `dns.getServers()`: the configured nameservers in node's string format.
///
/// Node formats a plain IPv4/IPv6 address, bracketing IPv6 and appending
/// `:port` only when the port is not the default 53. hickory keeps one
/// `NameServerConfig` per address (protocols hang off it), but a config can
/// still list the same IP twice, and node reports each server once -- so
/// duplicates are dropped while preserving order.
pub fn dns_get_servers() -> Vec<String> {
    // Touch the resolver first: it is what populates NAME_SERVERS.
    let _ = resolver();
    NAME_SERVERS.get().cloned().unwrap_or_default()
}

fn resolver() -> &'static TokioResolver {
    static RESOLVER: OnceLock<TokioResolver> = OnceLock::new();
    RESOLVER.get_or_init(|| {
        let (config, options) = hickory_resolver::system_conf::read_system_conf()
            .expect("read system DNS configuration");
        let _ = NAME_SERVERS.set(config.name_servers().iter().map(format_name_server).fold(
            Vec::new(),
            |mut acc, s| {
                if !acc.contains(&s) {
                    acc.push(s);
                }
                acc
            },
        ));
        let mut builder = TokioResolver::builder_with_config(
            config,
            hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
        );
        *builder.options_mut() = options;
        // Node parity: c-ares always advertises EDNS0 (1232-byte UDP payloads,
        // DNS flag day 2020). hickory's system-conf path mirrors glibc instead
        // -- EDNS only with an `options edns0` line in resolv.conf -- and some
        // forwarders (Tailscale MagicDNS) answer non-EDNS queries with
        // oversized TC=1 datagrams that then fail to parse, so every large
        // answer (TXT sets) errors where Node succeeds. try_tcp_on_error
        // covers the same forwarder shape for answers past the EDNS size.
        let opts = builder.options_mut();
        opts.edns0 = true;
        opts.try_tcp_on_error = true;
        builder.build().unwrap()
    })
}

/// A nameserver in node's `dns.getServers()` string form.
fn format_name_server(ns: &hickory_resolver::config::NameServerConfig) -> String {
    match ns.ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6.to_string(),
    }
}

/// A DNS name as node reports it: without the root label's trailing dot.
///
/// hickory's `Display`/`to_string` emits a fully-qualified name with the final
/// `.` (`dns.google.`); node strips it on every field that carries a name --
/// verified against node v22.22.2 for reverse, PTR, NS, CNAME, MX.exchange,
/// SOA.nsname, SOA.hostmaster and NAPTR.replacement. The ROOT name is `.` and
/// must stay `.` rather than becoming an empty string.
fn host(name: &hickory_resolver::proto::rr::Name) -> String {
    // to_ascii, NOT to_string/to_utf8: Display un-punycodes IDNA labels, while
    // c-ares hands node the wire bytes. Verified against node v22.22.2 --
    // resolveNs("xn--p1acf") gives node "ns1.nic.xn--p1acf" where Display gave
    // the decoded Cyrillic. Fixing only the dot would leave that mismatch.
    let s = name.to_ascii();
    // strip_suffix removes exactly ONE dot; trim_end_matches would eat an
    // escaped trailing sequence. The ROOT name is "." and correctly becomes ""
    // -- c-ares does the same, so an RFC 7505 null MX reports exchange:"" on
    // both runtimes (probe-verified against example.com).
    s.strip_suffix('.').map(str::to_owned).unwrap_or(s)
}

pub async fn dns_lookup(hostname: String, family: i32, all: bool) -> OpOutcome {
    let lookup = format!("{}:0", hostname);
    let addrs: Vec<std::net::SocketAddr> = match tokio::net::lookup_host(&lookup).await {
        Ok(iter) => iter.collect(),
        Err(_) => {
            return OpOutcome::node_failed(
                "ENOTFOUND",
                format!("getaddrinfo ENOTFOUND {hostname}"),
            );
        }
    };

    if addrs.is_empty() {
        return OpOutcome::node_failed("ENOTFOUND", format!("getaddrinfo ENOTFOUND {hostname}"));
    }

    let filtered: Vec<_> = addrs
        .iter()
        .filter(|a| match family {
            4 => matches!(a.ip(), IpAddr::V4(_)),
            6 => matches!(a.ip(), IpAddr::V6(_)),
            _ => true,
        })
        .collect();

    if filtered.is_empty() {
        return OpOutcome::node_failed("ENOTFOUND", format!("getaddrinfo ENOTFOUND {hostname}"));
    }

    if all {
        let results: Vec<serde_json::Value> = filtered
            .iter()
            .map(|a| {
                let fam = if a.ip().is_ipv4() { 4 } else { 6 };
                serde_json::json!({ "address": a.ip().to_string(), "family": fam })
            })
            .collect();
        OpOutcome::Json(serde_json::json!(results).to_string())
    } else {
        let addr = filtered[0];
        let fam = if addr.ip().is_ipv4() { 4 } else { 6 };
        OpOutcome::Json(
            serde_json::json!({ "address": addr.ip().to_string(), "family": fam }).to_string(),
        )
    }
}

/// dns.resolve(hostname, rrtype) -- query specific DNS record types.
pub async fn dns_resolve(hostname: String, rrtype: String) -> OpOutcome {
    let r = resolver();
    match rrtype.as_str() {
        "A" => match r.lookup(&hostname, RecordType::A).await {
            Ok(lookup) => {
                let addrs: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::A(a) => Some(serde_json::json!(a.0.to_string())),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(addrs).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "AAAA" => match r.lookup(&hostname, RecordType::AAAA).await {
            Ok(lookup) => {
                let addrs: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::AAAA(a) => Some(serde_json::json!(a.0.to_string())),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(addrs).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "CNAME" => match r.lookup(&hostname, RecordType::CNAME).await {
            Ok(lookup) => {
                let names: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::CNAME(c) => Some(serde_json::json!(host(&c.0))),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "MX" => match r.lookup(&hostname, RecordType::MX).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::MX(mx) => Some(serde_json::json!({
                            "priority": mx.preference,
                            "exchange": host(&mx.exchange),
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "TXT" => match r.lookup(&hostname, RecordType::TXT).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::TXT(txt) => {
                            let parts: Vec<String> = txt
                                .txt_data
                                .iter()
                                .map(|b| String::from_utf8_lossy(b).into_owned())
                                .collect();
                            Some(serde_json::json!(parts))
                        }
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "NS" => match r.lookup(&hostname, RecordType::NS).await {
            Ok(lookup) => {
                let names: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::NS(ns) => Some(serde_json::json!(host(&ns.0))),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "SRV" => match r.lookup(&hostname, RecordType::SRV).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::SRV(srv) => Some(serde_json::json!({
                            "priority": srv.priority,
                            "weight": srv.weight,
                            "port": srv.port,
                            "name": host(&srv.target),
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "SOA" => match r.lookup(&hostname, RecordType::SOA).await {
            Ok(lookup) => {
                if let Some(soa) = lookup.answers().iter().find_map(|rec| match &rec.data {
                    RData::SOA(soa) => Some(soa),
                    _ => None,
                }) {
                    OpOutcome::Json(
                        serde_json::json!({
                            "nsname": host(&soa.mname),
                            "hostmaster": host(&soa.rname),
                            "serial": soa.serial,
                            "refresh": soa.refresh,
                            "retry": soa.retry,
                            "expire": soa.expire,
                            "minttl": soa.minimum,
                        })
                        .to_string(),
                    )
                } else {
                    OpOutcome::Json("null".to_string())
                }
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "PTR" => match r.lookup(&hostname, RecordType::PTR).await {
            Ok(lookup) => {
                let names: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::PTR(p) => Some(serde_json::json!(host(&p.0))),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "CAA" => match r.lookup(&hostname, RecordType::CAA).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::CAA(caa) => Some(serde_json::json!({
                            "critical": caa.issuer_critical as u8,
                            "issue": caa.tag.as_str(),
                            "value": String::from_utf8_lossy(&caa.value).into_owned(),
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        "NAPTR" => match r.lookup(&hostname, RecordType::NAPTR).await {
            Ok(lookup) => {
                let records: Vec<serde_json::Value> = lookup
                    .answers()
                    .iter()
                    .filter_map(|rec| match &rec.data {
                        RData::NAPTR(naptr) => Some(serde_json::json!({
                            "order": naptr.order,
                            "preference": naptr.preference,
                            "flags": String::from_utf8_lossy(&naptr.flags).into_owned(),
                            "service": String::from_utf8_lossy(&naptr.services).into_owned(),
                            "regexp": String::from_utf8_lossy(&naptr.regexp).into_owned(),
                            "replacement": host(&naptr.replacement),
                        })),
                        _ => None,
                    })
                    .collect();
                OpOutcome::Json(serde_json::json!(records).to_string())
            }
            Err(e) => dns_err(&hostname, &e),
        },
        other => OpOutcome::Failed(format!("dns.resolve: unsupported rrtype '{other}'")),
    }
}

/// dns.reverse(ip) -- PTR lookup for an IP address.
pub async fn dns_reverse(ip_str: String) -> OpOutcome {
    let ip: IpAddr = match ip_str.parse() {
        Ok(ip) => ip,
        Err(_) => {
            return OpOutcome::node_failed("EINVAL", format!("reverse EINVAL {ip_str}"));
        }
    };

    let r = resolver();
    match r.reverse_lookup(ip).await {
        Ok(lookup) => {
            let names: Vec<serde_json::Value> = lookup
                .answers()
                .iter()
                .filter_map(|rec| match &rec.data {
                    RData::PTR(p) => Some(serde_json::json!(host(&p.0))),
                    _ => None,
                })
                .collect();
            if names.is_empty() {
                OpOutcome::node_failed(
                    "ENOTFOUND".to_string(),
                    format!("reverse ENOTFOUND {ip_str}"),
                )
            } else {
                OpOutcome::Json(serde_json::json!(names).to_string())
            }
        }
        Err(e) => dns_err(&ip_str, &e),
    }
}

fn dns_err(query: &str, e: &hickory_resolver::net::NetError) -> OpOutcome {
    use hickory_resolver::net::{DnsError, NetError};
    use hickory_resolver::proto::op::ResponseCode;
    // "no records" is TWO different node errors, and collapsing them to
    // ENOTFOUND made a name that exists look like one that does not.
    // hickory's NoRecords carries the response code and documents the split:
    // NXDomain means the name itself does not exist, NoError means it does but
    // carries no record of the requested type. c-ares -- and therefore node --
    // reports those as ENOTFOUND and ENODATA respectively.
    let code = match e {
        NetError::Dns(DnsError::NoRecordsFound(no_records)) => {
            match no_records.response_code {
                ResponseCode::NXDomain => "ENOTFOUND",
                ResponseCode::NoError => "ENODATA",
                // Any other rcode is a real server-side failure, not an
                // absence, so it keeps the generic mapping below.
                _ => "ESERVFAIL",
            }
        }
        _ => "ESERVFAIL",
    };
    OpOutcome::node_failed(code.to_string(), format!("{code} {query}: {e}"))
}
