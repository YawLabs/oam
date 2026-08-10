use std::net::IpAddr;
use std::sync::OnceLock;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::proto::rr::RecordType;

use super::OpOutcome;

fn resolver() -> &'static TokioResolver {
    static RESOLVER: OnceLock<TokioResolver> = OnceLock::new();
    RESOLVER.get_or_init(|| {
        let mut builder = TokioResolver::builder_tokio().unwrap();
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
                        RData::CNAME(c) => Some(serde_json::json!(c.0.to_string())),
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
                            "exchange": mx.exchange.to_string(),
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
                        RData::NS(ns) => Some(serde_json::json!(ns.0.to_string())),
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
                            "name": srv.target.to_string(),
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
                            "nsname": soa.mname.to_string(),
                            "hostmaster": soa.rname.to_string(),
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
                        RData::PTR(p) => Some(serde_json::json!(p.0.to_string())),
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
                            "replacement": naptr.replacement.to_string(),
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
                    RData::PTR(p) => Some(serde_json::json!(p.0.to_string())),
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
    let code = match e {
        NetError::Dns(DnsError::NoRecordsFound(_)) => "ENOTFOUND",
        _ => "ESERVFAIL",
    };
    OpOutcome::node_failed(code.to_string(), format!("{code} {query}: {e}"))
}
