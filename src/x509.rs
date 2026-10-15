//! Just enough X.509 (RFC 5280) to tell a script whom a client certificate
//! names: the subject, the issuer and the subject alternative names.
//!
//! This is not a validator. It only ever reads a certificate rustls has
//! already verified against the listener's `client_ca`, and it makes no
//! decision: it turns three fields into text. Anything it does not understand
//! it leaves out, and a certificate it cannot walk yields `None`.

/// The subject, the issuer and the subject alternative names of one
/// certificate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Names {
    /// The subject as an RFC 4514 string, most specific RDN first:
    /// `CN=naf.example.com,O=Example,C=XX`.
    pub(crate) subject: String,
    /// The issuer, in the same form as the subject.
    pub(crate) issuer: String,
    /// `(kind, value)` with kind `dns`, `uri`, `email` or `ip`, in the order
    /// of the certificate.
    pub(crate) subject_alt_names: Vec<(&'static str, String)>,
}

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const OBJECT_IDENTIFIER: u8 = 0x06;
const BOOLEAN: u8 = 0x01;
/// `[0] EXPLICIT` version, `[3] EXPLICIT` extensions of TBSCertificate.
const VERSION: u8 = 0xa0;
const EXTENSIONS: u8 = 0xa3;

/// id-ce-subjectAltName, 2.5.29.17.
const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// Read one DER element off the front of `input`: its tag and its content.
///
/// Only what a certificate uses: single-octet tags and definite lengths of at
/// most four octets. Anything else, or a length past the end, is `None`.
fn element<'a>(input: &mut &'a [u8]) -> Option<(u8, &'a [u8])> {
    let (&tag, rest) = input.split_first()?;
    if tag & 0x1f == 0x1f {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (octets, rest) = rest.split_at(count);
        let length = octets
            .iter()
            .fold(0usize, |length, &octet| (length << 8) | usize::from(octet));
        (length, rest)
    };
    if rest.len() < length {
        return None;
    }
    let (content, rest) = rest.split_at(length);
    *input = rest;
    Some((tag, content))
}

/// The content of the next element, which must have `tag`.
fn expect<'a>(input: &mut &'a [u8], tag: u8) -> Option<&'a [u8]> {
    match element(input)? {
        (found, content) if found == tag => Some(content),
        _ => None,
    }
}

/// The subject, issuer and subject alternative names of a DER certificate.
pub(crate) fn names(certificate: &[u8]) -> Option<Names> {
    let mut input = certificate;
    let mut certificate = expect(&mut input, SEQUENCE)?;
    let mut tbs = expect(&mut certificate, SEQUENCE)?;

    // version [0] is absent in a v1 certificate.
    if tbs.first() == Some(&VERSION) {
        element(&mut tbs)?;
    }
    expect(&mut tbs, INTEGER)?; // serialNumber
    expect(&mut tbs, SEQUENCE)?; // signature
    let issuer = distinguished_name(expect(&mut tbs, SEQUENCE)?)?;
    expect(&mut tbs, SEQUENCE)?; // validity
    let subject = distinguished_name(expect(&mut tbs, SEQUENCE)?)?;
    expect(&mut tbs, SEQUENCE)?; // subjectPublicKeyInfo

    let mut subject_alt_names = Vec::new();
    while !tbs.is_empty() {
        // issuerUniqueID [1] and subjectUniqueID [2] may come first.
        let (tag, content) = element(&mut tbs)?;
        if tag != EXTENSIONS {
            continue;
        }
        let mut wrapper = content;
        let mut extensions = expect(&mut wrapper, SEQUENCE)?;
        while !extensions.is_empty() {
            let mut extension = expect(&mut extensions, SEQUENCE)?;
            let identifier = expect(&mut extension, OBJECT_IDENTIFIER)?;
            if extension.first() == Some(&BOOLEAN) {
                element(&mut extension)?; // critical
            }
            let value = expect(&mut extension, OCTET_STRING)?;
            if identifier == SUBJECT_ALT_NAME {
                subject_alt_names = general_names(value)?;
            }
        }
    }
    Some(Names {
        subject,
        issuer,
        subject_alt_names,
    })
}

/// GeneralNames (RFC 5280 clause 4.2.1.6), keeping the four forms a script
/// can compare with: rfc822Name [1], dNSName [2], uniformResourceIdentifier
/// [6] and iPAddress [7].
fn general_names(value: &[u8]) -> Option<Vec<(&'static str, String)>> {
    let mut input = value;
    let mut names = expect(&mut input, SEQUENCE)?;
    let mut found = Vec::new();
    while !names.is_empty() {
        let (tag, content) = element(&mut names)?;
        let text = || String::from_utf8_lossy(content).into_owned();
        match tag {
            0x81 => found.push(("email", text())),
            0x82 => found.push(("dns", text())),
            0x86 => found.push(("uri", text())),
            0x87 => {
                if let Some(address) = ip_address(content) {
                    found.push(("ip", address));
                }
            }
            _ => {}
        }
    }
    Some(found)
}

fn ip_address(octets: &[u8]) -> Option<String> {
    if let Ok(v4) = <[u8; 4]>::try_from(octets) {
        return Some(std::net::Ipv4Addr::from(v4).to_string());
    }
    <[u8; 16]>::try_from(octets)
        .ok()
        .map(|v6| std::net::Ipv6Addr::from(v6).to_string())
}

/// A Name as an RFC 4514 string: the RDNs in reverse order, joined by `,`,
/// the attributes of a multi-valued RDN joined by `+`.
fn distinguished_name(mut name: &[u8]) -> Option<String> {
    let mut rdns = Vec::new();
    while !name.is_empty() {
        let mut rdn = expect(&mut name, SET)?;
        let mut attributes = Vec::new();
        while !rdn.is_empty() {
            let mut attribute = expect(&mut rdn, SEQUENCE)?;
            let identifier = expect(&mut attribute, OBJECT_IDENTIFIER)?;
            let (tag, value) = element(&mut attribute)?;
            attributes.push(format!(
                "{}={}",
                attribute_type(identifier),
                attribute_value(tag, value)
            ));
        }
        rdns.push(attributes.join("+"));
    }
    rdns.reverse();
    Some(rdns.join(","))
}

/// The short name RFC 4514 clause 3 gives an attribute type, or its dotted
/// object identifier.
fn attribute_type(identifier: &[u8]) -> String {
    let short = match identifier {
        [0x55, 0x04, 0x03] => "CN",
        [0x55, 0x04, 0x05] => "serialNumber",
        [0x55, 0x04, 0x06] => "C",
        [0x55, 0x04, 0x07] => "L",
        [0x55, 0x04, 0x08] => "ST",
        [0x55, 0x04, 0x09] => "STREET",
        [0x55, 0x04, 0x0a] => "O",
        [0x55, 0x04, 0x0b] => "OU",
        // 0.9.2342.19200300.100.1.25 and .1.1
        [0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x19] => "DC",
        [0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x01] => "UID",
        // 1.2.840.113549.1.9.1
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x01] => "emailAddress",
        _ => return dotted(identifier),
    };
    short.to_string()
}

/// An object identifier in dotted form (X.690 clause 8.19).
fn dotted(identifier: &[u8]) -> String {
    let mut arcs: Vec<u64> = Vec::new();
    let mut arc: u64 = 0;
    for &octet in identifier {
        arc = (arc << 7) | u64::from(octet & 0x7f);
        if octet & 0x80 != 0 {
            continue;
        }
        if arcs.is_empty() {
            let first = (arc / 40).min(2);
            arcs.push(first);
            arcs.push(arc - first * 40);
        } else {
            arcs.push(arc);
        }
        arc = 0;
    }
    arcs.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// An attribute value as RFC 4514 clause 2.4 writes it: the string, escaped,
/// for the string types, and `#` with the hexadecimal content otherwise.
fn attribute_value(tag: u8, value: &[u8]) -> String {
    let text = match tag {
        // UTF8String, PrintableString, TeletexString, IA5String, NumericString
        0x0c | 0x13 | 0x14 | 0x16 | 0x12 => String::from_utf8_lossy(value).into_owned(),
        // BMPString: UTF-16, big endian.
        0x1e => {
            let units: Vec<u16> = value
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => {
            let hex: String = value.iter().map(|octet| format!("{octet:02x}")).collect();
            return format!("#{hex}");
        }
    };
    let last = text.chars().count().saturating_sub(1);
    let mut escaped = String::with_capacity(text.len());
    for (index, character) in text.chars().enumerate() {
        match character {
            '"' | '+' | ',' | ';' | '<' | '>' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '#' | ' ' if index == 0 => {
                escaped.push('\\');
                escaped.push(character);
            }
            ' ' if index == last => escaped.push_str("\\ "),
            '\0' => escaped.push_str("\\00"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;

    fn der(pem: &str) -> Vec<u8> {
        CertificateDer::from_pem_slice(pem.as_bytes())
            .expect("a PEM certificate")
            .to_vec()
    }

    const CLIENT: &str = include_str!("../tests/fixtures/tls/client.pem");
    const SERVER: &str = include_str!("../tests/fixtures/tls/server.pem");
    const CA: &str = include_str!("../tests/fixtures/tls/ca.pem");

    // The expected values are what `openssl x509 -noout -subject -nameopt
    // RFC2253 -ext subjectAltName` prints for each file.

    #[test]
    fn the_subject_is_the_rfc_4514_string() {
        assert_eq!(
            names(&der(CLIENT)).unwrap().subject,
            "CN=naf.example.com,OU=Application Functions,O=Example Test,C=XX"
        );
        assert_eq!(
            names(&der(SERVER)).unwrap().subject,
            "CN=localhost,O=Example Test"
        );
    }

    #[test]
    fn the_alternative_names_keep_their_kind_and_order() {
        assert_eq!(
            names(&der(CLIENT)).unwrap().subject_alt_names,
            vec![
                ("dns", "naf.example.com".to_string()),
                ("uri", "urn:example:naf:1".to_string()),
                ("ip", "192.0.2.7".to_string()),
                ("email", "naf@example.com".to_string()),
            ]
        );
        assert_eq!(
            names(&der(SERVER)).unwrap().subject_alt_names,
            vec![
                ("dns", "localhost".to_string()),
                ("ip", "127.0.0.1".to_string()),
            ]
        );
    }

    #[test]
    fn the_issuer_reads_like_openssl_prints_it() {
        // `openssl x509 -noout -issuer -nameopt RFC2253`
        assert_eq!(
            names(&der(CLIENT)).unwrap().issuer,
            "CN=Example Test CA,O=Example Test"
        );
    }

    #[test]
    fn a_certificate_without_the_extension_has_no_alternative_names() {
        let ca = names(&der(CA)).unwrap();
        assert_eq!(ca.subject, "CN=Example Test CA,O=Example Test");
        assert!(ca.subject_alt_names.is_empty());
    }

    #[test]
    fn truncated_or_foreign_octets_are_refused_not_read_past() {
        let certificate = der(CLIENT);
        for length in 0..certificate.len() {
            assert!(names(&certificate[..length]).is_none(), "prefix {length}");
        }
        assert!(names(b"not a certificate").is_none());
        // An indefinite length and a length past the end.
        assert!(names(&[0x30, 0x80, 0x00, 0x00]).is_none());
        assert!(names(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]).is_none());
    }

    #[test]
    fn values_are_escaped_as_rfc_4514_requires() {
        assert_eq!(attribute_value(0x0c, b"Example, Inc."), "Example\\, Inc.");
        assert_eq!(attribute_value(0x13, b" padded "), "\\ padded\\ ");
        assert_eq!(attribute_value(0x0c, b"#1"), "\\#1");
        assert_eq!(attribute_value(0x0c, b"a+b=c"), "a\\+b=c");
        assert_eq!(attribute_value(0x1e, &[0x00, 0x41, 0x00, 0x42]), "AB");
        // A type that is not a string: the content in hexadecimal.
        assert_eq!(attribute_value(0x03, &[0x00, 0xff]), "#00ff");
    }

    #[test]
    fn an_attribute_type_without_a_short_name_is_dotted() {
        // 2.5.4.42 givenName, 1.3.6.1.4.1.311.60.2.1.3.
        assert_eq!(attribute_type(&[0x55, 0x04, 0x2a]), "2.5.4.42");
        assert_eq!(
            attribute_type(&[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x3c, 0x02, 0x01, 0x03]),
            "1.3.6.1.4.1.311.60.2.1.3"
        );
        assert_eq!(attribute_type(&[0x55, 0x04, 0x03]), "CN");
    }
}
