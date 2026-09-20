//! A certificate's names as Node prints and matches them (measured on
//! v22.22.2, which follows OpenSSL 3 and its own `PrintGeneralName`):
//!
//! - `subjectaltname` / `X509Certificate#subjectAltName` and `infoAccess`:
//!   each GeneralName printed the way Node prints it, ", "-separated. A name
//!   that could be mistaken for more than one -- a comma, a quote, a
//!   backslash, a control character, anything outside printable ASCII in a
//!   byte string -- is printed as a JSON string literal
//!   (`URI:"http://x\u002c DNS:victim.example"`), so the list splits
//!   unambiguously. This is what Node's `checkServerIdentity` relies on
//!   (CVE-2021-44532): a name inside a URI, an e-mail address or a
//!   directory name can never be read as a `DNS:` entry.
//! - The identity Node's `checkServerIdentity` checks a host against: the
//!   subjectAltName's DNS names and IP addresses, and the subject's CN when
//!   there is no DNS name. Read here off the DER itself, entry by entry --
//!   never by splitting a printed list.
//!
//! The ASN.1 is read by a small DER reader of its own: a name OpenSSL
//! decodes (an IA5String with bytes outside ASCII, say) must be decoded here
//! too, where a stricter parser gives up on the whole extension.

use x509_parser::certificate::X509Certificate;

// ----------------------------------------------------------------- DER

/// One DER element: its identifier octet, its contents, and the whole
/// encoding (identifier, length and contents).
#[derive(Clone, Copy, Debug)]
pub struct Der<'a> {
    pub tag: u8,
    pub content: &'a [u8],
    pub raw: &'a [u8],
}

/// One element off the front of `input`, and what follows it. Definite
/// lengths only (DER); a high tag number is refused.
pub(crate) fn read_der(input: &[u8]) -> Option<(Der<'_>, &[u8])> {
    let (&tag, rest) = input.split_first()?;
    if tag & 0x1f == 0x1f {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let len = rest[..count]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    let header = input.len() - rest.len();
    Some((
        Der {
            tag,
            content: &rest[..len],
            raw: &input[..header + len],
        },
        &rest[len..],
    ))
}

/// Every element in `content`, in order; None if any does not parse.
pub(crate) fn der_items(mut content: &[u8]) -> Option<Vec<Der<'_>>> {
    let mut items = Vec::new();
    while !content.is_empty() {
        let (item, rest) = read_der(content)?;
        items.push(item);
        content = rest;
    }
    Some(items)
}

/// An OBJECT IDENTIFIER's content octets as dotted text (OBJ_obj2txt with
/// no_name set).
pub fn oid_text(content: &[u8]) -> String {
    let mut arcs: Vec<u128> = Vec::new();
    let mut acc: u128 = 0;
    let mut first = true;
    for b in content {
        acc = (acc << 7) | u128::from(b & 0x7f);
        if b & 0x80 != 0 {
            continue;
        }
        if first {
            first = false;
            if acc < 80 {
                arcs.push(acc / 40);
                arcs.push(acc % 40);
            } else {
                arcs.push(2);
                arcs.push(acc - 80);
            }
        } else {
            arcs.push(acc);
        }
        acc = 0;
    }
    arcs.iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// OpenSSL's short name for a name attribute (OBJ_nid2sn), for the ones
/// certificates carry; None for an OID it prints as dotted text.
pub fn attr_short_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "2.5.4.3" => "CN",
        "2.5.4.4" => "SN",
        "2.5.4.5" => "serialNumber",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "street",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "2.5.4.12" => "title",
        "2.5.4.13" => "description",
        "2.5.4.15" => "businessCategory",
        "2.5.4.17" => "postalCode",
        "2.5.4.41" => "name",
        "2.5.4.42" => "GN",
        "2.5.4.43" => "initials",
        "2.5.4.44" => "generationQualifier",
        "2.5.4.46" => "dnQualifier",
        "2.5.4.65" => "pseudonym",
        "2.5.4.97" => "organizationIdentifier",
        "0.9.2342.19200300.100.1.1" => "UID",
        "0.9.2342.19200300.100.1.25" => "DC",
        "1.2.840.113549.1.9.1" => "emailAddress",
        "1.3.6.1.4.1.311.60.2.1.1" => "jurisdictionL",
        "1.3.6.1.4.1.311.60.2.1.2" => "jurisdictionST",
        "1.3.6.1.4.1.311.60.2.1.3" => "jurisdictionC",
        _ => return None,
    })
}

// ------------------------------------------------------------ strings

/// OpenSSL's width of a string type's characters (`tag2nbyte`): 0 for
/// UTF-8, 1, 2 (BMPString) or 4 (UniversalString) bytes; None for a type
/// that is not a string it can print.
fn char_width(tag: u8) -> Option<u8> {
    match tag {
        0x0c => Some(0),
        0x12 | 0x13 | 0x14 | 0x16 | 0x17 | 0x18 | 0x1a => Some(1),
        0x1c => Some(4),
        0x1e => Some(2),
        _ => None,
    }
}

/// The code points of a string of `width`-byte characters (1, 2 or 4).
fn wide_chars(content: &[u8], width: usize) -> Option<Vec<u32>> {
    if !content.len().is_multiple_of(width) {
        return None;
    }
    Some(
        content
            .chunks(width)
            .map(|c| c.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b)))
            .collect(),
    )
}

/// OpenSSL's ASN1_STRING_to_UTF8: a string attribute's value as text (a
/// byte string's bytes read as Latin-1, BMP and Universal strings decoded),
/// or None where OpenSSL fails (a type it does not convert, invalid UTF-8,
/// a surrogate or an out-of-range character).
pub fn asn1_string_to_utf8(tag: u8, content: &[u8]) -> Option<String> {
    match char_width(tag)? {
        0 => std::str::from_utf8(content).ok().map(str::to_string),
        1 => Some(content.iter().map(|&b| char::from(b)).collect()),
        width => wide_chars(content, usize::from(width))?
            .into_iter()
            .map(char::from_u32)
            .collect(),
    }
}

/// Node's `IsSafeAltName`: whether a name can be printed as it is. A
/// comma, a quote, a backslash or an apostrophe never can; nor can a
/// control character; nor, in a byte string, anything outside printable
/// ASCII (a UTF-8 string keeps its multi-byte characters).
fn is_safe_alt_name(name: &[u8], utf8: bool) -> bool {
    name.iter().all(|&c| match c {
        b'"' | b'\\' | b',' | b'\'' => false,
        _ if utf8 => c >= b' ' && c != 0x7f,
        _ => (b' '..=b'~').contains(&c),
    })
}

/// Node's `PrintAltName`: the name as it is when it is safe (after
/// `prefix:`), else a JSON string literal of it -- `\\`, `\"`, and `\u00XX`
/// for each byte that is a comma, a control character, or (in a byte
/// string) outside ASCII.
fn print_alt_name(out: &mut Vec<u8>, name: &[u8], utf8: bool, prefix: Option<&str>) {
    if is_safe_alt_name(name, utf8) {
        if let Some(prefix) = prefix {
            out.extend_from_slice(prefix.as_bytes());
            out.push(b':');
        }
        out.extend_from_slice(name);
        return;
    }
    out.push(b'"');
    if let Some(prefix) = prefix {
        out.extend_from_slice(prefix.as_bytes());
        out.push(b':');
    }
    for &c in name {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            _ if (c >= b' ' && c != b',' && c <= b'~') || (utf8 && c & 0x80 != 0) => out.push(c),
            _ => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
        }
    }
    out.push(b'"');
}

// ------------------------------------------------------- directory names

/// OpenSSL's RFC 2253 escapes for one byte of a name value
/// (ASN1_STRFLGS_ESC_2253; Node leaves control characters and bytes over
/// 0x7f as they are): `,+"\<>;` always, `#` first, a space first or last.
fn escape_2253(out: &mut Vec<u8>, c: u8, first: bool, last: bool) {
    let escaped = matches!(c, b',' | b'+' | b'"' | b'\\' | b'<' | b'>' | b';')
        || (first && matches!(c, b'#' | b' '))
        || (last && c == b' ');
    if escaped {
        out.push(b'\\');
    }
    out.push(c);
}

/// One name attribute's value as X509_NAME_print_ex writes it under Node's
/// flags (RFC 2253 escaping, UTF-8 out): a string's characters -- escaped
/// where they must be, the first and last with OpenSSL's quirk that a
/// one-character value is only ever "last" -- or, for a value that is not a
/// string OpenSSL prints (or an attribute it does not know), `#` and the
/// hex of the value's DER.
fn print_name_value(out: &mut Vec<u8>, value: &Der<'_>, known_field: bool) {
    let width = if known_field {
        char_width(value.tag)
    } else {
        None
    };
    let Some(width) = width else {
        out.push(b'#');
        for b in value.raw {
            out.extend_from_slice(format!("{b:02X}").as_bytes());
        }
        return;
    };
    // A UTF8String is read a byte at a time (OpenSSL: "interpret it as 1
    // byte per character to avoid converting twice"); the others are
    // decoded and written as UTF-8.
    let chars: Vec<u32> = match width {
        0 | 1 => value.content.iter().map(|&b| u32::from(b)).collect(),
        width => match wide_chars(value.content, usize::from(width)) {
            Some(chars) => chars,
            None => value.content.iter().map(|&b| u32::from(b)).collect(),
        },
    };
    let count = chars.len();
    for (i, &c) in chars.iter().enumerate() {
        // OpenSSL sets "first" for the first character, then replaces it
        // with "last" for the last one.
        let last = i + 1 == count;
        let first = i == 0 && !last;
        if width == 0 || c < 0x80 {
            // One byte (a UTF8String's raw byte, or ASCII).
            let byte = c as u8;
            if byte < 0x80 {
                escape_2253(out, byte, first, last);
            } else {
                out.push(byte);
            }
        } else {
            let mut buf = [0u8; 4];
            match char::from_u32(c) {
                Some(ch) => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
                None => out.extend_from_slice("\u{fffd}".as_bytes()),
            }
        }
    }
}

/// A Name (SEQUENCE OF RelativeDistinguishedName) the way Node prints a
/// DirName: X509_NAME_print_ex with RFC 2253's order (last RDN first),
/// `,` between RDNs and `+` inside one, short names (`CN=`), and each
/// value escaped as RFC 2253 says. None if the Name does not parse.
fn print_directory_name(name: &Der<'_>) -> Option<Vec<u8>> {
    if name.tag != 0x30 {
        return None;
    }
    // Flattened as X509_NAME keeps it: (RDN index, type OID, value).
    let mut entries: Vec<(usize, Der<'_>, Der<'_>)> = Vec::new();
    for (set, rdn) in der_items(name.content)?.into_iter().enumerate() {
        if rdn.tag != 0x31 {
            return None;
        }
        for attribute in der_items(rdn.content)? {
            if attribute.tag != 0x30 {
                return None;
            }
            let parts = der_items(attribute.content)?;
            let [oid, value] = parts.as_slice() else {
                return None;
            };
            if oid.tag != 0x06 {
                return None;
            }
            entries.push((set, *oid, *value));
        }
    }
    let mut out = Vec::new();
    let mut previous: Option<usize> = None;
    for (set, oid, value) in entries.iter().rev() {
        if let Some(previous) = previous {
            out.push(if previous == *set { b'+' } else { b',' });
        }
        previous = Some(*set);
        let dotted = oid_text(oid.content);
        let short = attr_short_name(&dotted);
        out.extend_from_slice(short.unwrap_or(&dotted).as_bytes());
        out.push(b'=');
        print_name_value(&mut out, value, short.is_some());
    }
    Some(out)
}

// ------------------------------------------------------- general names

/// One GeneralName (RFC 5280), as read off the DER.
#[derive(Clone, Copy, Debug)]
pub enum GeneralName<'a> {
    /// otherName: its type OID's content octets and its value.
    Other {
        type_id: &'a [u8],
        value: Der<'a>,
    },
    Email(&'a [u8]),
    Dns(&'a [u8]),
    X400,
    /// directoryName: the Name inside it.
    Directory(Der<'a>),
    EdiParty,
    Uri(&'a [u8]),
    Ip(&'a [u8]),
    /// registeredID: the OID's content octets.
    Registered(&'a [u8]),
}

/// A GeneralNames (SEQUENCE OF GeneralName); None if it does not parse --
/// what OpenSSL cannot decode, Node prints nothing for.
pub fn general_names(der: &[u8]) -> Option<Vec<GeneralName<'_>>> {
    let (seq, _) = read_der(der)?;
    if seq.tag != 0x30 {
        return None;
    }
    der_items(seq.content)?
        .into_iter()
        .map(|item| general_name(&item))
        .collect()
}

fn general_name<'a>(item: &Der<'a>) -> Option<GeneralName<'a>> {
    Some(match item.tag {
        0xa0 => {
            let parts = der_items(item.content)?;
            let [type_id, explicit] = parts.as_slice() else {
                return None;
            };
            if type_id.tag != 0x06 || explicit.tag != 0xa0 {
                return None;
            }
            let (value, rest) = read_der(explicit.content)?;
            if !rest.is_empty() {
                return None;
            }
            GeneralName::Other {
                type_id: type_id.content,
                value,
            }
        }
        0x81 => GeneralName::Email(item.content),
        0x82 => GeneralName::Dns(item.content),
        0xa3 => GeneralName::X400,
        0xa4 => {
            let (name, rest) = read_der(item.content)?;
            if !rest.is_empty() {
                return None;
            }
            // A Name that does not parse is an undecodable extension.
            print_directory_name(&name)?;
            GeneralName::Directory(name)
        }
        0xa5 => GeneralName::EdiParty,
        0x86 => GeneralName::Uri(item.content),
        0x87 => GeneralName::Ip(item.content),
        0x88 => GeneralName::Registered(item.content),
        // Constructed encodings of the string forms do not occur in DER.
        _ => return None,
    })
}

/// Node's prefix for the otherName types it prints, and whether the value
/// is a UTF8String (else an IA5String).
fn other_name_prefix(type_id: &[u8]) -> Option<(&'static str, bool)> {
    Some(match oid_text(type_id).as_str() {
        "1.3.6.1.5.5.7.8.9" => ("SmtpUTF8Mailbox", true),
        "1.3.6.1.5.5.7.8.5" => ("XmppAddr", true),
        "1.3.6.1.5.5.7.8.7" => ("SRVName", false),
        "1.3.6.1.4.1.311.20.2.3" => ("UPN", true),
        "1.3.6.1.5.5.7.8.8" => ("NAIRealm", true),
        _ => return None,
    })
}

/// An IP address entry's text as OpenSSL writes it: a dotted quad, or eight
/// uppercase hex groups with no compression; another length is invalid.
fn ip_entry_text(bytes: &[u8]) -> String {
    match bytes.len() {
        4 => format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]),
        16 => bytes
            .chunks(2)
            .map(|pair| format!("{:X}", u16::from_be_bytes([pair[0], pair[1]])))
            .collect::<Vec<_>>()
            .join(":"),
        n => format!("<invalid length={n}>"),
    }
}

/// Node's `PrintGeneralName`: one entry of `subjectaltname` / `infoAccess`.
pub fn general_name_text(name: &GeneralName<'_>) -> String {
    let mut out: Vec<u8> = Vec::new();
    match name {
        GeneralName::Dns(bytes) => {
            out.extend_from_slice(b"DNS:");
            print_alt_name(&mut out, bytes, false, None);
        }
        GeneralName::Email(bytes) => {
            out.extend_from_slice(b"email:");
            print_alt_name(&mut out, bytes, false, None);
        }
        GeneralName::Uri(bytes) => {
            out.extend_from_slice(b"URI:");
            print_alt_name(&mut out, bytes, false, None);
        }
        GeneralName::Directory(der) => {
            out.extend_from_slice(b"DirName:");
            match print_directory_name(der) {
                Some(text) => print_alt_name(&mut out, &text, true, None),
                None => out.extend_from_slice(b"<unsupported>"),
            }
        }
        GeneralName::Ip(bytes) => {
            out.extend_from_slice(b"IP Address:");
            out.extend_from_slice(ip_entry_text(bytes).as_bytes());
        }
        GeneralName::Registered(oid) => {
            out.extend_from_slice(b"Registered ID:");
            out.extend_from_slice(oid_text(oid).as_bytes());
        }
        GeneralName::Other { type_id, value } => {
            let wanted = other_name_prefix(type_id)
                .filter(|(_, unicode)| value.tag == if *unicode { 0x0c } else { 0x16 });
            match wanted {
                Some((prefix, unicode)) => {
                    out.extend_from_slice(b"othername:");
                    print_alt_name(&mut out, value.content, unicode, Some(prefix));
                }
                None => out.extend_from_slice(b"othername:<unsupported>"),
            }
        }
        GeneralName::X400 => out.extend_from_slice(b"X400Name:<unsupported>"),
        GeneralName::EdiParty => out.extend_from_slice(b"EdiPartyName:<unsupported>"),
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A subjectAltName extension's value (the GeneralNames DER) as Node's
/// `subjectaltname`; None when it does not decode.
pub fn alt_names_text(ext_value: &[u8]) -> Option<String> {
    let names = general_names(ext_value)?;
    Some(
        names
            .iter()
            .map(general_name_text)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// An authorityInfoAccess extension's value as Node's `infoAccess`: one
/// "METHOD - LOCATION" line per access description, OpenSSL's long names
/// for the two well-known methods; None when it does not decode.
pub fn info_access_text(ext_value: &[u8]) -> Option<String> {
    let (seq, _) = read_der(ext_value)?;
    if seq.tag != 0x30 {
        return None;
    }
    let mut lines = Vec::new();
    for description in der_items(seq.content)? {
        let parts = der_items(description.content)?;
        let [method, location] = parts.as_slice() else {
            return None;
        };
        if description.tag != 0x30 || method.tag != 0x06 {
            return None;
        }
        let method = match oid_text(method.content).as_str() {
            "1.3.6.1.5.5.7.48.1" => "OCSP".to_string(),
            "1.3.6.1.5.5.7.48.2" => "CA Issuers".to_string(),
            other => other.to_string(),
        };
        let location = general_name_text(&general_name(location)?);
        lines.push(format!("{method} - {location}"));
    }
    Some(lines.join("\n"))
}

// ------------------------------------------------------------ addresses

/// libuv's inet_pton for IPv4: four decimal octets, no leading zeros.
fn inet_pton4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut octets = 0usize;
    let mut saw_digit = false;
    let mut current: u32 = 0;
    for &ch in text {
        if ch.is_ascii_digit() {
            let next = current * 10 + u32::from(ch - b'0');
            if saw_digit && current == 0 {
                return None;
            }
            if next > 255 {
                return None;
            }
            current = next;
            if !saw_digit {
                octets += 1;
                if octets > 4 {
                    return None;
                }
                saw_digit = true;
            }
            out[octets - 1] = current as u8;
        } else if ch == b'.' && saw_digit {
            if octets == 4 {
                return None;
            }
            current = 0;
            saw_digit = false;
        } else {
            return None;
        }
    }
    (octets == 4).then_some(out)
}

/// libuv's inet_pton for IPv6 (a `%zone` suffix is dropped first, as
/// uv_inet_pton drops it).
fn inet_pton6(text: &[u8]) -> Option<[u8; 16]> {
    let text = match text.iter().position(|&c| c == b'%') {
        Some(at) if at > 45 => return None,
        Some(at) => &text[..at],
        None => text,
    };
    let mut out = [0u8; 16];
    let mut tp = 0usize;
    let mut colon: Option<usize> = None;
    let mut src = text;
    if src.first() == Some(&b':') {
        if src.get(1) != Some(&b':') {
            return None;
        }
        src = &src[1..];
    }
    let mut token_start = 0usize;
    let mut seen_digits = 0;
    let mut value: u32 = 0;
    let mut i = 0usize;
    while i < src.len() {
        let ch = src[i];
        i += 1;
        if let Some(digit) = (ch as char).to_digit(16) {
            value = (value << 4) | digit;
            seen_digits += 1;
            if seen_digits > 4 {
                return None;
            }
            continue;
        }
        if ch == b':' {
            token_start = i;
            if seen_digits == 0 {
                if colon.is_some() {
                    return None;
                }
                colon = Some(tp);
                continue;
            } else if i == src.len() {
                return None;
            }
            if tp + 2 > 16 {
                return None;
            }
            out[tp] = (value >> 8) as u8;
            out[tp + 1] = value as u8;
            tp += 2;
            seen_digits = 0;
            value = 0;
            continue;
        }
        if ch == b'.'
            && tp + 4 <= 16
            && let Some(v4) = inet_pton4(&src[token_start..])
        {
            out[tp..tp + 4].copy_from_slice(&v4);
            tp += 4;
            seen_digits = 0;
            break;
        }
        return None;
    }
    if seen_digits > 0 {
        if tp + 2 > 16 {
            return None;
        }
        out[tp] = (value >> 8) as u8;
        out[tp + 1] = value as u8;
        tp += 2;
    }
    if let Some(colon) = colon {
        if tp == 16 {
            return None;
        }
        let moved = tp - colon;
        for k in 1..=moved {
            out[16 - k] = out[colon + moved - k];
            out[colon + moved - k] = 0;
        }
        tp = 16;
    }
    (tp == 16).then_some(out)
}

/// libuv's inet_ntop for IPv6 (BIND's): lowercase groups, the longest run
/// of two or more zero groups as `::`, and the last 32 bits as a dotted
/// quad for an IPv4-compatible or IPv4-mapped address.
fn inet_ntop6(bytes: &[u8; 16]) -> String {
    let words: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    let (mut best_base, mut best_len) = (None::<usize>, 0usize);
    let (mut cur_base, mut cur_len) = (None::<usize>, 0usize);
    for (i, &word) in words.iter().enumerate() {
        if word == 0 {
            match cur_base {
                None => {
                    cur_base = Some(i);
                    cur_len = 1;
                }
                Some(_) => cur_len += 1,
            }
        } else if let Some(base) = cur_base.take()
            && (best_base.is_none() || cur_len > best_len)
        {
            best_base = Some(base);
            best_len = cur_len;
        }
    }
    if let Some(base) = cur_base
        && (best_base.is_none() || cur_len > best_len)
    {
        best_base = Some(base);
        best_len = cur_len;
    }
    if best_len < 2 {
        best_base = None;
    }
    let mut out = String::new();
    for i in 0..8 {
        if let Some(base) = best_base
            && i >= base
            && i < base + best_len
        {
            if i == base {
                out.push(':');
            }
            continue;
        }
        if i != 0 {
            out.push(':');
        }
        if i == 6
            && best_base == Some(0)
            && (best_len == 6
                || (best_len == 7 && words[7] != 0x0001)
                || (best_len == 5 && words[5] == 0xffff))
        {
            out.push_str(&format!(
                "{}.{}.{}.{}",
                bytes[12], bytes[13], bytes[14], bytes[15]
            ));
            return out;
        }
        out.push_str(&format!("{:x}", words[i]));
    }
    if let Some(base) = best_base
        && base + best_len == 8
    {
        out.push(':');
    }
    out
}

/// An address's canonical text (4 or 16 bytes), as libuv prints it.
pub fn ip_text(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => Some(format!(
            "{}.{}.{}.{}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        )),
        16 => {
            let mut array = [0u8; 16];
            array.copy_from_slice(bytes);
            Some(inet_ntop6(&array))
        }
        _ => None,
    }
}

/// Node's `canonicalizeIP`: the address re-printed by libuv, or None when
/// libuv does not read it as one.
pub fn canonicalize_ip(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if let Some(v4) = inet_pton4(bytes) {
        return ip_text(&v4);
    }
    inet_pton6(bytes).map(|v6| inet_ntop6(&v6))
}

// ------------------------------------------------------------- identity

/// What Node's `checkServerIdentity` reads off a certificate: the
/// `subjectaltname` text (for its message), the DNS names and IP addresses
/// in it, and the subject's CN values.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HostIdentity {
    /// `cert.subjectaltname`: absent without the extension (or when it does
    /// not decode, where Node has null).
    pub alt_names: Option<String>,
    /// Each DNS name, a byte per character (what Node's JSON unescaping of
    /// a `\u00XX` gives back).
    pub dns_names: Vec<String>,
    /// Each IP address entry's canonical text; empty for one of another
    /// length (Node's `canonicalizeIP` answers undefined there, and it
    /// matches nothing).
    pub ips: Vec<String>,
    /// `cert.subject.CN`, every value; None when the subject has none or
    /// one of its values cannot be converted (Node's subject is undefined
    /// then).
    pub cn: Option<Vec<String>>,
}

/// The first extension with this OID's value, from the parsed certificate.
fn extension_value<'a>(cert: &'a X509Certificate<'a>, oid: &str) -> Option<&'a [u8]> {
    cert.extensions()
        .iter()
        .find(|ext| ext.oid.to_id_string() == oid)
        .map(|ext| ext.value)
}

impl HostIdentity {
    pub fn from_certificate(cert: &X509Certificate<'_>) -> Self {
        let mut identity = HostIdentity::default();
        if let Some(value) = extension_value(cert, "2.5.29.17")
            && let Some(names) = general_names(value)
        {
            identity.alt_names = Some(
                names
                    .iter()
                    .map(general_name_text)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            for name in &names {
                match name {
                    GeneralName::Dns(bytes) => identity
                        .dns_names
                        .push(bytes.iter().map(|&b| char::from(b)).collect()),
                    GeneralName::Ip(bytes) => identity.ips.push(ip_text(bytes).unwrap_or_default()),
                    _ => {}
                }
            }
        }
        let mut cn = Vec::new();
        let mut convertible = true;
        for rdn in cert.subject().iter() {
            for attribute in rdn.iter() {
                let value = attribute.attr_value();
                let text = asn1_string_to_utf8(value.header.tag().0 as u8, value.data);
                match text {
                    Some(text) if attribute.attr_type().to_id_string() == "2.5.4.3" => {
                        cn.push(text)
                    }
                    Some(_) => {}
                    None => convertible = false,
                }
            }
        }
        identity.cn = (convertible && !cn.is_empty()).then_some(cn);
        identity
    }

    pub fn from_der(der: &[u8]) -> Self {
        match x509_parser::parse_x509_certificate(der) {
            Ok((_, cert)) => Self::from_certificate(&cert),
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(tag: u8, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if bytes.len() < 0x80 {
            out.push(bytes.len() as u8);
        } else {
            out.push(0x81);
            out.push(bytes.len() as u8);
        }
        out.extend_from_slice(bytes);
        out
    }
    fn oid(text: &str) -> Vec<u8> {
        let parts: Vec<u64> = text.split('.').map(|p| p.parse().unwrap()).collect();
        let mut out = vec![(40 * parts[0] + parts[1]) as u8];
        for &n in &parts[2..] {
            let mut bytes = vec![(n & 0x7f) as u8];
            let mut v = n >> 7;
            while v > 0 {
                bytes.insert(0, 0x80 | (v & 0x7f) as u8);
                v >>= 7;
            }
            out.extend(bytes);
        }
        tlv(0x06, &out)
    }
    fn seq(items: &[Vec<u8>]) -> Vec<u8> {
        tlv(0x30, &items.concat())
    }
    fn set(items: &[Vec<u8>]) -> Vec<u8> {
        tlv(0x31, &items.concat())
    }
    fn atv(o: &str, tag: u8, value: &[u8]) -> Vec<u8> {
        seq(&[oid(o), tlv(tag, value)])
    }
    fn other(o: &str, value: Vec<u8>) -> Vec<u8> {
        tlv(0xa0, &[oid(o), tlv(0xa0, &value)].concat())
    }
    fn text(entries: &[Vec<u8>]) -> String {
        alt_names_text(&seq(entries)).unwrap()
    }

    /// Each expectation is node v22.22.2's `x509.subjectAltName` for a
    /// certificate carrying exactly these entries.
    #[test]
    fn byte_strings_print_as_node_prints_them() {
        assert_eq!(
            text(&[
                tlv(0x82, b"a\x01b.test"),
                tlv(0x82, b"a\xe9b.test"),
                tlv(0x82, b"a\"b\\c'd.test"),
                tlv(0x82, b"sp ace.test"),
                tlv(0x82, b"x\x7f.test"),
                tlv(0x82, b"victim.test\x00.evil.test"),
            ]),
            r#"DNS:"a\u0001b.test", DNS:"a\u00e9b.test", DNS:"a\"b\\c'd.test", DNS:sp ace.test, DNS:"x\u007f.test", DNS:"victim.test\u0000.evil.test""#
        );
        assert_eq!(
            text(&[
                tlv(0x86, b"http://x/\x7f?a=1,b=2"),
                tlv(0x81, b"a\tb@x"),
                tlv(0x81, b"plain@x"),
            ]),
            r#"URI:"http://x/\u007f?a=1\u002cb=2", email:"a\u0009b@x", email:plain@x"#
        );
        // The injections Node escapes (CVE-2021-44532): a second name
        // inside one entry stays inside it.
        assert_eq!(
            text(&[
                tlv(0x82, b"other.test"),
                tlv(0x86, b"http://x, DNS:victim.test"),
            ]),
            r#"DNS:other.test, URI:"http://x\u002c DNS:victim.test""#
        );
    }

    #[test]
    fn addresses_ids_and_other_names_print_as_node_prints_them() {
        assert_eq!(
            text(&[
                tlv(0x87, &[1, 2, 3, 4, 5]),
                tlv(
                    0x87,
                    &[0x20, 1, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
                ),
                tlv(0x87, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4]),
                tlv(
                    0x87,
                    &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4]
                ),
                tlv(0x82, b"other.test"),
            ]),
            "IP Address:<invalid length=5>, IP Address:2001:DB8:0:0:0:0:0:1, IP Address:0:0:0:0:0:0:102:304, IP Address:0:0:0:0:0:FFFF:102:304, DNS:other.test"
        );
        let upn = "1.3.6.1.4.1.311.20.2.3";
        let srv = "1.3.6.1.5.5.7.8.7";
        assert_eq!(
            text(&[
                other(upn, tlv(0x0c, "us\u{e9}r\u{1}".as_bytes())),
                other(upn, tlv(0x0c, b"plain@x")),
                other(srv, tlv(0x16, b"_x._tcp.a,b")),
                other(srv, tlv(0x16, b"_x._tcp.ok")),
                other("1.3.6.1.5.5.7.8.9", tlv(0x0c, "m\u{e9}@x".as_bytes())),
                other("1.3.6.1.5.5.7.8.5", tlv(0x0c, b"x@j")),
                other("1.3.6.1.5.5.7.8.8", tlv(0x0c, b"realm")),
                other("1.2.3", tlv(0x0c, b"x")),
                other(upn, tlv(0x16, b"ia5")),
                tlv(0x88, &oid("1.2.840.113549")[2..]),
            ]),
            "othername:\"UPN:us\u{e9}r\\u0001\", othername:UPN:plain@x, othername:\"SRVName:_x._tcp.a\\u002cb\", othername:SRVName:_x._tcp.ok, othername:SmtpUTF8Mailbox:m\u{e9}@x, othername:XmppAddr:x@j, othername:NAIRealm:realm, othername:<unsupported>, othername:<unsupported>, Registered ID:1.2.840.113549"
        );
        assert_eq!(
            text(&[
                tlv(0xa3, &seq(&[])),
                tlv(0xa5, &tlv(0xa1, &tlv(0x0c, b"ab"))),
                tlv(0x82, b"other.test"),
            ]),
            "X400Name:<unsupported>, EdiPartyName:<unsupported>, DNS:other.test"
        );
    }

    #[test]
    fn directory_names_print_as_node_prints_them() {
        let first = seq(&[
            set(&[atv("2.5.4.6", 0x13, b"US")]),
            set(&[atv("2.5.4.10", 0x0c, "O\u{e9}, Inc".as_bytes())]),
            set(&[
                atv("2.5.4.3", 0x0c, b"#h\"+<>;\\ x "),
                atv("2.5.4.11", 0x13, b"u"),
            ]),
            set(&[atv("1.2.3.4", 0x13, b"pr")]),
            set(&[atv("0.9.2342.19200300.100.1.25", 0x16, b"dc")]),
            set(&[atv("1.2.840.113549.1.9.1", 0x16, b"e@x")]),
        ]);
        let second = seq(&[
            set(&[atv("2.5.4.3", 0x1e, &[0, 0x61, 0, 0xe9])]),
            set(&[atv("2.5.4.3", 0x14, b"t\xe9"), atv("2.5.4.5", 0x13, b"123")]),
        ]);
        let third = seq(&[set(&[atv("2.5.4.3", 0x0c, b"x\x01y")])]);
        assert_eq!(
            text(&[
                tlv(0xa4, &first),
                tlv(0xa4, &second),
                tlv(0xa4, &seq(&[])),
                tlv(0xa4, &third),
            ]),
            "DirName:\"emailAddress=e@x\\u002cDC=dc\\u002c1.2.3.4=#13027072\\u002cOU=u+CN=\\\\#h\\\\\\\"\\\\+\\\\<\\\\>\\\\;\\\\\\\\ x\\\\ \\u002cO=O\u{e9}\\\\\\u002c Inc\\u002cC=US\", DirName:\"serialNumber=123+CN=t\u{e9}\\u002cCN=a\u{e9}\", DirName:, DirName:\"CN=x\\u0001y\""
        );
    }

    #[test]
    fn only_dns_entries_are_dns_names() {
        let names = seq(&[
            tlv(0x82, b"other.test"),
            tlv(0x86, b"http://x, DNS:victim.test"),
            tlv(0x81, b"a@x, DNS:victim.test"),
            tlv(0x82, b"other.test, DNS:victim.test"),
            tlv(0x87, &[127, 0, 0, 1]),
            tlv(0x87, &[1, 2, 3]),
        ]);
        let parsed = general_names(&names).unwrap();
        let dns: Vec<&[u8]> = parsed
            .iter()
            .filter_map(|n| match n {
                GeneralName::Dns(b) => Some(*b),
                _ => None,
            })
            .collect();
        assert_eq!(dns, [&b"other.test"[..], b"other.test, DNS:victim.test"]);
    }

    /// libuv's parse and print, which Node's `canonicalizeIP` is (each row
    /// measured on v22.22.2 through tls.checkServerIdentity's message).
    #[test]
    fn canonical_addresses_are_libuvs() {
        let rows = [
            ("127.0.0.1", Some("127.0.0.1")),
            ("0:0:0:0:0:0:0:1", Some("::1")),
            ("::1", Some("::1")),
            ("::", Some("::")),
            ("::2", Some("::2")),
            ("0:0:0:0:0:0:1:0", Some("::0.1.0.0")),
            ("0:0:0:0:0:ffff:0:1", Some("::ffff:0.0.0.1")),
            ("::fffe:1.2.3.4", Some("::fffe:102:304")),
            ("0:0:0:0:0:0:102:304", Some("::1.2.3.4")),
            ("::ffff:1.2.3.4", Some("::ffff:1.2.3.4")),
            ("2001:DB8:0:0:0:0:0:1", Some("2001:db8::1")),
            ("2001:db8:0:0:1:0:0:1", Some("2001:db8::1:0:0:1")),
            ("1:0:0:2:0:0:0:3", Some("1:0:0:2::3")),
            ("fe80::1%eth0", Some("fe80::1")),
            ("1:2:3:4:5:6:7:8", Some("1:2:3:4:5:6:7:8")),
            ("01.2.3.4", None),
            ("1.2.3", None),
            ("1.2.3.256", None),
            ("1::2::3", None),
            ("12345::", None),
            (":1::", None),
            ("1:2:3:4:5:6:7:8:9", None),
            ("<invalid length=5>", None),
            ("", None),
        ];
        for (input, expected) in rows {
            assert_eq!(canonicalize_ip(input).as_deref(), expected, "{input:?}");
        }
    }

    #[test]
    fn strings_convert_as_asn1_string_to_utf8_converts_them() {
        assert_eq!(
            asn1_string_to_utf8(0x0c, "\u{e9}".as_bytes()).as_deref(),
            Some("\u{e9}")
        );
        assert_eq!(asn1_string_to_utf8(0x0c, b"\xff"), None);
        assert_eq!(
            asn1_string_to_utf8(0x14, b"t\xe9").as_deref(),
            Some("t\u{e9}")
        );
        assert_eq!(
            asn1_string_to_utf8(0x1e, &[0, 0x61, 0x20, 0xac]).as_deref(),
            Some("a\u{20ac}")
        );
        assert_eq!(asn1_string_to_utf8(0x1e, &[0xd8, 0]), None);
        assert_eq!(
            asn1_string_to_utf8(0x1c, &[0, 1, 0xf6, 0x00]).as_deref(),
            Some("\u{1f600}")
        );
        assert_eq!(asn1_string_to_utf8(0x02, &[1]), None);
    }
}
