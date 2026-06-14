use std::net::IpAddr;

use super::OpOutcome;

pub async fn dns_lookup(hostname: String, family: i32, all: bool) -> OpOutcome {
    let lookup = format!("{}:0", hostname);
    let addrs: Vec<std::net::SocketAddr> = match tokio::net::lookup_host(&lookup).await {
        Ok(iter) => iter.collect(),
        Err(_) => {
            return OpOutcome::NodeFailed {
                code: "ENOTFOUND".to_string(),
                message: format!("getaddrinfo ENOTFOUND {hostname}"),
            };
        }
    };

    if addrs.is_empty() {
        return OpOutcome::NodeFailed {
            code: "ENOTFOUND".to_string(),
            message: format!("getaddrinfo ENOTFOUND {hostname}"),
        };
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
        return OpOutcome::NodeFailed {
            code: "ENOTFOUND".to_string(),
            message: format!("getaddrinfo ENOTFOUND {hostname}"),
        };
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
