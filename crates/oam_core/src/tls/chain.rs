//! The certificate chain a node:tls endpoint presents when its `cert` is a
//! leaf on its own: OpenSSL's automatic chain (`ssl_add_cert_chain` with no
//! chain certificates set), built from the secure context's store -- the
//! `ca` option's certificates, or without one the bundled roots plus
//! NODE_EXTRA_CA_CERTS, and a pfx's CA certificates either way.
//!
//! Node's behaviour, measured on v22.22.2 with `openssl s_client -showcerts`
//! against a node server:
//!
//! - `{ cert: leaf, ca: intermediate }` serves leaf, intermediate.
//! - `{ cert: leaf, ca: [intermediate, root] }` (in either order) serves
//!   leaf, intermediate, root -- the trust anchor too: OpenSSL sends every
//!   certificate X509_verify_cert put in the chain.
//! - `{ cert: leaf + intermediate, ca: root }` serves leaf, intermediate: a
//!   `cert` that brings its own chain is sent as it is, never extended.
//! - no `ca`, NODE_EXTRA_CA_CERTS naming the root: a leaf the root signed
//!   is served with the root.
//! - a `ca` that did not issue the leaf changes nothing.
//!
//! A client's certificate goes out the same way (the same context code on
//! both sides): a client with `{ cert: leaf, ca: [intermediate, root] }` is
//! authorized by a server that trusts only the root.
//!
//! The chain is built as OpenSSL's `build_chain` does, minus the signature
//! check it leaves to verification: from the leaf, the issuer is a store
//! certificate whose subject is the leaf's issuer name, whose subject key id
//! agrees with the leaf's authority key id when both are present, and which
//! may sign certificates when it says what it may do (X509_check_issued);
//! among several, the first valid now, else the one that expires last
//! (X509_STORE_CTX_get1_issuer). It stops at a self-issued certificate, at
//! an issuer the store does not hold, or at a certificate already in the
//! chain. Names are compared as encoded, where OpenSSL compares a
//! canonical form (case and spacing folded); two encodings of one name
//! that differ only there do not chain here.

use rustls::pki_types::CertificateDer;
use std::sync::OnceLock;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::parse_x509_certificate;

use super::names::read_der;

/// OpenSSL's default verify depth: a chain of at most this many
/// certificates above the leaf.
const MAX_DEPTH: usize = 100;

/// The subject Name of a certificate, as encoded, read straight off the
/// DER (TBSCertificate: [0] version, serial, signature, issuer, validity,
/// subject) -- cheap enough to index a whole root store with.
fn subject_raw(der: &[u8]) -> Option<&[u8]> {
    let (cert, _) = read_der(der)?;
    let (tbs, _) = read_der(cert.content)?;
    let mut rest = tbs.content;
    let (first, after) = read_der(rest)?;
    rest = if first.tag == 0xa0 { after } else { rest };
    // serial, signature, issuer, validity
    for _ in 0..4 {
        let (_, after) = read_der(rest)?;
        rest = after;
    }
    let (subject, _) = read_der(rest)?;
    (subject.tag == 0x30).then_some(subject.raw)
}

/// The authority key identifier's keyIdentifier, if the certificate has one.
fn authority_key_id<'a>(cert: &'a X509Certificate<'a>) -> Option<&'a [u8]> {
    cert.extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(akid) => {
                akid.key_identifier.as_ref().map(|id| id.0)
            }
            _ => None,
        })
}

/// The subject key identifier, if the certificate has one.
fn subject_key_id<'a>(cert: &'a X509Certificate<'a>) -> Option<&'a [u8]> {
    cert.extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0),
            _ => None,
        })
}

/// The parts of OpenSSL's X509_check_issued that decide whether `issuer`
/// issued `subject`: the names, the key identifiers when both are there,
/// and keyCertSign when the issuer carries a KeyUsage at all.
fn check_issued(issuer: &X509Certificate<'_>, subject: &X509Certificate<'_>) -> bool {
    if issuer.subject().as_raw() != subject.issuer().as_raw() {
        return false;
    }
    if let (Some(akid), Some(skid)) = (authority_key_id(subject), subject_key_id(issuer))
        && akid != skid
    {
        return false;
    }
    match issuer.key_usage() {
        Ok(Some(usage)) => usage.value.key_cert_sign(),
        _ => true,
    }
}

fn valid_at(cert: &X509Certificate<'_>, now: i64) -> bool {
    let validity = cert.validity();
    validity.not_before.timestamp() <= now && now <= validity.not_after.timestamp()
}

/// The chain OpenSSL presents for `leaf` given alone, from `store`: the leaf,
/// then each issuer the store holds, up to and including a self-issued one.
/// `now` is the Unix time the issuers' validity is judged at.
pub(crate) fn complete_chain(
    leaf: &CertificateDer<'static>,
    store: &[&CertificateDer<'static>],
    now: i64,
) -> Vec<CertificateDer<'static>> {
    let mut chain = vec![leaf.clone()];
    let Ok((_, leaf_parsed)) = parse_x509_certificate(leaf.as_ref()) else {
        return chain;
    };
    // Only the certificates whose subject could be an issuer in this chain
    // are parsed; the rest of the store is skipped by its subject bytes.
    let mut used = vec![false; store.len()];
    let mut current = leaf_parsed;
    for _ in 0..MAX_DEPTH {
        if current.subject().as_raw() == current.issuer().as_raw() {
            break;
        }
        let wanted = current.issuer().as_raw();
        let mut chosen: Option<(usize, X509Certificate<'_>)> = None;
        for (index, candidate) in store.iter().enumerate() {
            if used[index] || subject_raw(candidate.as_ref()) != Some(wanted) {
                continue;
            }
            let Ok((_, parsed)) = parse_x509_certificate(candidate.as_ref()) else {
                continue;
            };
            if !check_issued(&parsed, &current) {
                continue;
            }
            if valid_at(&parsed, now) {
                chosen = Some((index, parsed));
                break;
            }
            // None valid yet: keep the one that expires last.
            let later = chosen.as_ref().is_none_or(|(_, best)| {
                parsed.validity().not_after.timestamp() > best.validity().not_after.timestamp()
            });
            if later {
                chosen = Some((index, parsed));
            }
        }
        let Some((index, issuer)) = chosen else {
            break;
        };
        used[index] = true;
        chain.push(store[index].clone());
        current = issuer;
    }
    chain
}

/// The issuers Node's getPeerCertificate(true) adds to a peer's chain from
/// the connection's store (GetLastIssuedCert): from the leaf, the peer's own
/// certificates are linked issuer to subject as far as they go
/// (AddIssuerChainToObject), then the store's issuers of the last one are
/// looked up as OpenSSL looks them up. Empty when the peer's chain already
/// ends in a self-issued certificate or the store holds no issuer.
pub(crate) fn store_issuers(
    peer: &[CertificateDer<'static>],
    store: &[&CertificateDer<'static>],
    now: i64,
) -> Vec<CertificateDer<'static>> {
    let Some(parsed) = peer
        .iter()
        .map(|cert| parse_x509_certificate(cert.as_ref()).ok().map(|(_, c)| c))
        .collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };
    if parsed.is_empty() {
        return Vec::new();
    }
    let mut current = 0;
    let mut rest: Vec<usize> = (1..parsed.len()).collect();
    while let Some(at) = rest
        .iter()
        .position(|&i| check_issued(&parsed[i], &parsed[current]))
    {
        current = rest.remove(at);
    }
    let mut completed = complete_chain(&peer[current], store, now);
    completed.remove(0);
    completed
}

/// The store a context without a `ca` option builds chains from:
/// NODE_EXTRA_CA_CERTS, then the bundled roots.
pub(crate) fn default_store() -> &'static [CertificateDer<'static>] {
    static STORE: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    STORE.get_or_init(|| {
        let mut store = super::extra_ca_certs().certs.clone();
        store.extend(super::roots::bundled().iter().cloned());
        store
    })
}

/// Unix time now, for the issuers' validity.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;

    // A root, an intermediate it signed, a leaf the intermediate signed, a
    // leaf the root signed, and an unrelated self-signed CA (P-256, valid
    // 2025-2125), generated with openssl for this test.
    const ROOT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBUDCB96ADAgECAgEBMAoGCCqGSM49BAMCMA8xDTALBgNVBAMMBHJvb3QwIBcN\n\
MjUwMTAxMDAwMDAwWhgPMjEyNTAxMDEwMDAwMDBaMA8xDTALBgNVBAMMBHJvb3Qw\n\
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASs0ZDWHk3l6U56tL3fzfF1cptQ6H7H\n\
vb8gavm1nCdP0YGimSQ1zOaRE9ICfTFyxy+hQ7TXnUSDyYhJhrLc+r0Oo0IwQDAP\n\
BgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUtr7JtwFg\n\
mkJuy3aLGoVE8J7VLi0wCgYIKoZIzj0EAwIDSAAwRQIgbWk6n3kLvB4CGIkSjd7J\n\
f4LAGwwb3b7gXypuw1/7Pc8CIQCoRYqLyR2B3/xAttrVMNMVSQw2VJeCCuBtrsB4\n\
SIpWlA==\n\
-----END CERTIFICATE-----\n";
    const INTERMEDIATE: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBcDCCARegAwIBAgIBAzAKBggqhkjOPQQDAjAPMQ0wCwYDVQQDDARyb290MCAX\n\
DTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAOMQwwCgYDVQQDDANpbnQw\n\
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQeTpIEF+UrbPtdI82MeVbvD0Vq/DrH\n\
ppMjWBFvy5TzMYSSGVCUgQkwh7OWaacfHjAulvTQ8cRjOZD/8Qm9XWuVo2MwYTAP\n\
BgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUPUOZVUQy\n\
6cJwM1str3GmQaHLcdIwHwYDVR0jBBgwFoAUtr7JtwFgmkJuy3aLGoVE8J7VLi0w\n\
CgYIKoZIzj0EAwIDRwAwRAIgWZ3nZe6BK01gGwMFRYqWM/y9JPDsz3BqdoutHVEX\n\
8sECIHH0PssnNrPEjfNa+f2019uiMd+Np5lkuxTVMsZr6lIT\n\
-----END CERTIFICATE-----\n";
    const LEAF: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBaDCCAQ6gAwIBAgIBBDAKBggqhkjOPQQDAjAOMQwwCgYDVQQDDANpbnQwIBcN\n\
MjUwMTAxMDAwMDAwWhgPMjEyNTAxMDEwMDAwMDBaMA8xDTALBgNVBAMMBGxlYWYw\n\
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQkdnKx7BuC5vWhNeCYdK6/JVZ6IJ++\n\
OvCLJSkSjgCHzYRT5ZYtO69ji1HldOwPxlHxj/XPY3CxyJOi2qWrgRCmo1owWDAJ\n\
BgNVHRMEAjAAMAsGA1UdDwQEAwIHgDAdBgNVHQ4EFgQUGedqtJ6xXXbktg4wg35V\n\
6MMBJcgwHwYDVR0jBBgwFoAUPUOZVUQy6cJwM1str3GmQaHLcdIwCgYIKoZIzj0E\n\
AwIDSAAwRQIgDLf52YFK+WyRnK44eIDaDHhw2Py/QnHxZ9uroD4wnWUCIQD8c0cF\n\
A76DSavfjBtz9l9/BvPSMD3rGI4KiLNPWS/3fQ==\n\
-----END CERTIFICATE-----\n";
    const ROOT_LEAF: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBbjCCARSgAwIBAgIBBTAKBggqhkjOPQQDAjAPMQ0wCwYDVQQDDARyb290MCAX\n\
DTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlyb290\n\
LWxlYWYwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQMbafikh5tIiiMS0qeh//D\n\
VQY6dFaT97/tRIFlv6E6eCKsbkTMr7BPMmSex9mXtXkRmfBarwGzmPYKXPUrrXmi\n\
o1owWDAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDAdBgNVHQ4EFgQUSRzmog/Kk4z3\n\
NmAmUVP919UNRqUwHwYDVR0jBBgwFoAUtr7JtwFgmkJuy3aLGoVE8J7VLi0wCgYI\n\
KoZIzj0EAwIDSAAwRQIgJ/IdW/PrQGHQrzyp3kd6MvPgJn5I2eXNkGx0zn8o+Q8C\n\
IQDVAbmOE9GU9zvfBBnz3yPVmLt1ujddt4oJemv1IV0SHQ==\n\
-----END CERTIFICATE-----\n";
    const OTHER: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBUzCB+aADAgECAgECMAoGCCqGSM49BAMCMBAxDjAMBgNVBAMMBW90aGVyMCAX\n\
DTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAQMQ4wDAYDVQQDDAVvdGhl\n\
cjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABErphvgL7h421PF3ROD/r92F93k5\n\
S/m3JHuOwb4T7HNUdmtO/CrzYLY/V9+tAeW4L6g3p4luX6i5FZHnnDHsfC2jQjBA\n\
MA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEGMB0GA1UdDgQWBBTEp1Iy\n\
I5xY08DuJvnAR9mv/p7PKjAKBggqhkjOPQQDAgNJADBGAiEAvo9kOUJeb6YYPbGq\n\
BX160+1gXEohkNayc20hTVRk8kMCIQCN1zpX5sTEKvtJ3LSgPIV640H3MHEZo0VW\n\
ZZ3CZS4oSg==\n\
-----END CERTIFICATE-----\n";

    fn der(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    fn cn_chain(chain: &[CertificateDer<'static>]) -> Vec<String> {
        chain
            .iter()
            .map(|c| {
                let (_, parsed) = parse_x509_certificate(c.as_ref()).unwrap();
                parsed
                    .subject()
                    .iter_common_name()
                    .next()
                    .and_then(|cn| cn.as_str().ok())
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    const NOW: i64 = 1_800_000_000; // 2027

    #[test]
    fn a_leaf_is_completed_from_the_store_as_node_serves_it() {
        let (root, int, leaf, root_leaf, other) = (
            der(ROOT),
            der(INTERMEDIATE),
            der(LEAF),
            der(ROOT_LEAF),
            der(OTHER),
        );
        let complete = |leaf: &CertificateDer<'static>, store: &[&CertificateDer<'static>]| {
            cn_chain(&complete_chain(leaf, store, NOW))
        };
        assert_eq!(complete(&leaf, &[&int]), ["leaf", "int"]);
        assert_eq!(complete(&leaf, &[&int, &root]), ["leaf", "int", "root"]);
        assert_eq!(complete(&leaf, &[&root, &int]), ["leaf", "int", "root"]);
        assert_eq!(complete(&root_leaf, &[&root]), ["root-leaf", "root"]);
        assert_eq!(complete(&leaf, &[]), ["leaf"]);
        assert_eq!(complete(&leaf, &[&other]), ["leaf"]);
        assert_eq!(complete(&leaf, &[&root]), ["leaf"]);
        assert_eq!(complete(&leaf, &[&int, &int]), ["leaf", "int"]);
        // A self-signed certificate is its own chain.
        assert_eq!(complete(&root, &[&root, &int]), ["root"]);
    }

    #[test]
    fn an_issuer_outside_its_validity_is_used_only_when_nothing_else_is() {
        let (int, leaf) = (der(INTERMEDIATE), der(LEAF));
        // Before the intermediate's notBefore: still the only candidate.
        assert_eq!(
            cn_chain(&complete_chain(&leaf, &[&int], 0)),
            ["leaf", "int"]
        );
    }

    #[test]
    fn store_issuers_go_on_from_the_last_certificate_the_peer_sent() {
        let (root, int, leaf) = (der(ROOT), der(INTERMEDIATE), der(LEAF));
        let issuers = |peer: &[CertificateDer<'static>], store: &[&CertificateDer<'static>]| {
            cn_chain(&store_issuers(peer, store, NOW))
        };
        assert_eq!(
            issuers(std::slice::from_ref(&leaf), &[&int, &root]),
            ["int", "root"]
        );
        assert_eq!(issuers(&[leaf.clone(), int.clone()], &[&root]), ["root"]);
        // Served out of order: linked from the leaf all the same.
        assert_eq!(
            issuers(&[leaf.clone(), root.clone(), int.clone()], &[&root]),
            Vec::<String>::new()
        );
        assert_eq!(
            issuers(std::slice::from_ref(&leaf), &[&root]),
            Vec::<String>::new()
        );
        assert_eq!(
            issuers(std::slice::from_ref(&root), &[&root]),
            Vec::<String>::new()
        );
        assert_eq!(issuers(&[], &[&root]), Vec::<String>::new());
    }

    #[test]
    fn subjects_are_read_off_the_der() {
        for pem in [ROOT, INTERMEDIATE, LEAF, ROOT_LEAF, OTHER] {
            let cert = der(pem);
            let (_, parsed) = parse_x509_certificate(cert.as_ref()).unwrap();
            assert_eq!(subject_raw(cert.as_ref()), Some(parsed.subject().as_raw()));
        }
        assert_eq!(subject_raw(b"\x30\x00"), None);
    }
}
