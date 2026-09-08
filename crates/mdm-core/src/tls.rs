//! TLS on Linux, with the missing half of a chain fetched when the server
//! forgets to send it.
//!
//! A server is supposed to send its leaf certificate *and* every intermediate
//! above it, stopping just short of the root. A surprising number send only
//! the leaf. rustls then verifies exactly what arrived, finds nothing linking
//! the leaf to a trusted root, and reports `UnknownIssuer` — for a download
//! every browser on the machine completes without comment.
//!
//! Browsers hide the gap. The leaf names the address of its own issuer in an
//! Authority Information Access extension, and Chrome and Firefox fetch the
//! missing certificate from there rather than fail. This does the same, so
//! that "the browser downloads it and the app does not" stops being true.
//!
//! It is worth being precise about what this does *not* loosen. The fetched
//! certificate is added to the chain being verified, never to the trust
//! store: the chain must still end at a root this machine already trusted, and
//! a signature must still hold at every link. Someone able to tamper with the
//! plain-HTTP fetch below gets to supply a certificate that fails to verify —
//! the same answer as supplying nothing at all. What changes is only that a
//! chain we could have completed is completed.
//!
//! Windows needs none of this: it uses SChannel, which repairs the gap itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error,
    SignatureScheme,
};

/// How long one fetch of a missing certificate may take.
///
/// This runs inside a handshake that `fetch` gives fifteen seconds to
/// complete, so it has to fit inside that with room for the connection around
/// it: a certificate is a couple of kilobytes from a CA's CDN, and an address
/// that has not answered in eight seconds is not about to rescue this
/// connection. Overrunning costs a retry rather than the download — the
/// answer is cached by then, so the next attempt does not wait again.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// The most a certificate fetch may download.
///
/// A certificate is ~1-2 KB and a bundle of them a few times that. The cap is
/// what stops a URL in an untrusted certificate from naming an endless
/// response and turning a handshake into a download of its own.
const MAX_BODY: usize = 64 * 1024;

/// How many missing links will be fetched for one handshake.
///
/// One is the case in the wild: a server that omits its single intermediate.
/// Two leaves room for a chain missing a pair without letting a certificate
/// that points at another point this into a long walk.
const MAX_MISSING_LINKS: usize = 2;

/// The TLS configuration the HTTP client should use, or `None` if it could not
/// be built — in which case the caller should leave reqwest's own default in
/// place rather than fail the download.
pub fn client_config() -> Option<ClientConfig> {
    static CONFIG: OnceLock<Option<ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(build).clone()
}

fn build() -> Option<ClientConfig> {
    // Whatever reqwest would have used, chosen the same way it chooses: a
    // provider the process installed if there is one, and ring otherwise.
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // The ordinary verifier, unchanged. Everything below only calls it again
    // with a longer chain; none of it decides what is trusted.
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| log::warn!("building the certificate verifier failed ({e}); \
                                 falling back to the default TLS setup"))
        .ok()?;

    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| log::warn!("no usable TLS versions ({e}); \
                                 falling back to the default TLS setup"))
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ChaseMissingIssuers { inner }))
        .with_no_client_auth();

    // reqwest sets this itself for the configuration it builds, but a
    // preconfigured one arrives at the connector untouched. The client is
    // HTTP/1.1 only, so this has to say so or a server offering h2 will be
    // handed a connection the client will not speak.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Some(config)
}

/// The ordinary web PKI verifier, plus one retry with the certificate the
/// server should have sent.
#[derive(Debug)]
struct ChaseMissingIssuers {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for ChaseMissingIssuers {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let mut chain: Vec<CertificateDer<'static>> =
            intermediates.iter().map(|c| c.clone().into_owned()).collect();

        let verify = |chain: &[CertificateDer<'static>]| {
            self.inner
                .verify_server_cert(end_entity, chain, server_name, ocsp_response, now)
        };

        // Nothing below runs for a server that sent a complete chain, or for
        // one whose certificate is wrong in any other way: an expired
        // certificate or the wrong hostname is a real answer, and fetching
        // more certificates would not change it.
        let complete = verify(&chain);
        if !is_unknown_issuer(&complete) {
            return complete;
        }

        // The link that is missing is the one above the highest certificate we
        // hold — the last of the intermediates, or the leaf where the server
        // sent none at all.
        let mut tip = match chain.last() {
            Some(last) => last.clone(),
            None => end_entity.clone().into_owned(),
        };

        for _ in 0..MAX_MISSING_LINKS {
            let fetched: Vec<CertificateDer<'static>> = issuers_of(&tip)
                .into_iter()
                .filter(|c| !chain.iter().any(|held| held.as_ref() == c.as_ref()))
                .filter(|c| c.as_ref() != tip.as_ref())
                .collect();

            // Nothing new to add: either the certificate named no issuer, the
            // address did not answer, or it handed back what we already had.
            let Some(highest) = fetched.last().cloned() else {
                break;
            };
            chain.extend(fetched);

            match verify(&chain) {
                // Deliberately loud, and once per address thanks to the cache
                // below: this is the app quietly repairing somebody else's
                // misconfiguration, and a log line is how that stays visible.
                Ok(verified) => {
                    log::info!(
                        "{server_name:?} sent an incomplete certificate chain; \
                         completed it from the issuer the certificate names"
                    );
                    return Ok(verified);
                }
                // Still short a link — climb and try again.
                Err(e) if is_unknown_issuer_error(&e) => tip = highest,
                // Any other verdict is the real one.
                Err(e) => return Err(e),
            }
        }

        // Report the original failure, not one from a chain we assembled: the
        // server's chain is what the user needs told about.
        complete
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

fn is_unknown_issuer(result: &Result<ServerCertVerified, Error>) -> bool {
    matches!(result, Err(e) if is_unknown_issuer_error(e))
}

fn is_unknown_issuer_error(error: &Error) -> bool {
    matches!(error, Error::InvalidCertificate(CertificateError::UnknownIssuer))
}

/// The certificates named by a certificate's own Authority Information Access
/// extension, fetched.
fn issuers_of(certificate: &CertificateDer<'_>) -> Vec<CertificateDer<'static>> {
    for url in ca_issuer_urls(certificate.as_ref()) {
        // http only, and by design. RFC 5280 expects these to be plain HTTP
        // precisely because fetching one over TLS would need a certificate
        // verified, which is what we are in the middle of doing. Nothing is
        // trusted on the strength of where it came from, so an unprotected
        // fetch costs nothing here.
        if !url.starts_with("http://") {
            log::debug!("skipping issuer address {url:?}: not plain HTTP");
            continue;
        }
        let certificates = cached_fetch(&url);
        if !certificates.is_empty() {
            return certificates;
        }
    }
    Vec::new()
}

/// One fetch per address for the life of the process, failures included.
///
/// A segmented download opens a dozen connections at once and every one of
/// them verifies the same chain against the same missing certificate, so each
/// address gets a lock of its own: the connections that arrive while the first
/// fetch is in flight wait for its answer rather than each firing a request.
/// The lock is per address and not over the whole table on purpose — two
/// different servers have no reason to wait for each other, and a handshake
/// that waits too long is one `fetch` gives up on.
fn cached_fetch(url: &str) -> Vec<CertificateDer<'static>> {
    /// `None` until somebody has been to the address and found out.
    type Answer = Arc<Mutex<Option<Vec<CertificateDer<'static>>>>>;
    static CACHE: OnceLock<Mutex<HashMap<String, Answer>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let answer = lock(cache).entry(url.to_string()).or_default().clone();
    let mut answer = lock(&answer);
    if let Some(certificates) = answer.as_ref() {
        return certificates.clone();
    }

    let certificates = fetch(url).map(|body| certificates_in(&body)).unwrap_or_default();
    if certificates.is_empty() {
        log::debug!("no usable certificate at {url}");
    }
    *answer = Some(certificates.clone());
    certificates
}

/// A poisoned lock here means a fetch panicked, which loses nothing worth
/// protecting: the value behind it is a cache, and the next caller can carry
/// on with it exactly as it stands.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fetch a URL from inside a handshake.
///
/// The handshake is being driven by the async runtime, and this is a blocking
/// call in the middle of it, so the request runs on a thread of its own with
/// its own small runtime and this one waits on the answer. Borrowing the
/// caller's runtime instead would be the way to deadlock a single-threaded
/// one; a thread cannot deadlock against a runtime it is not part of.
fn fetch(url: &str) -> Option<Vec<u8>> {
    let url = url.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("issuer-fetch".into())
        .spawn(move || {
            let _ = tx.send(blocking_get(&url));
        })
        .map_err(|e| log::debug!("could not start the issuer fetch ({e})"))
        .ok()?;

    // The request has a timeout of its own; this one only covers a thread that
    // somehow never reports back, so it is the same budget with room to spare.
    rx.recv_timeout(FETCH_TIMEOUT + Duration::from_secs(5)).ok().flatten()
}

fn blocking_get(url: &str) -> Option<Vec<u8>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| log::debug!("no runtime for the issuer fetch ({e})"))
        .ok()?;

    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| log::debug!("no client for the issuer fetch ({e})"))
            .ok()?;

        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|e| log::debug!("fetching {url} failed ({e})"))
            .ok()?;
        if !response.status().is_success() {
            log::debug!("{url} answered {}", response.status());
            return None;
        }

        // Read in pieces rather than trusting Content-Length, so a server that
        // understates the size cannot talk us past the cap.
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok().flatten() {
            if body.len() + chunk.len() > MAX_BODY {
                log::debug!("{url} is larger than a certificate should be");
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        Some(body)
    })
}

/// The certificates in whatever an issuer address answered with.
///
/// Three shapes are in use: a bare DER certificate (`application/pkix-cert`,
/// the common one), the same in PEM, and a PKCS#7 bundle (`.p7c`) holding
/// several. The content type is not consulted — servers get it wrong, and the
/// bytes say which of the three this is without being asked.
fn certificates_in(body: &[u8]) -> Vec<CertificateDer<'static>> {
    if let Some(pem) = pem_certificates(body) {
        return pem;
    }
    let Some((outer, _)) = split(body) else {
        return Vec::new();
    };
    if outer.tag != SEQUENCE {
        return Vec::new();
    }
    // A certificate opens with its tbsCertificate, another SEQUENCE. A PKCS#7
    // ContentInfo opens with the object identifier saying what it wraps. One
    // byte tells them apart.
    match split(outer.value) {
        Some((first, _)) if first.tag == SEQUENCE => vec![CertificateDer::from(body.to_vec())],
        Some((first, _)) if first.tag == OID => pkcs7_certificates(outer.value),
        _ => Vec::new(),
    }
}

fn pem_certificates(body: &[u8]) -> Option<Vec<CertificateDer<'static>>> {
    use base64::Engine as _;

    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let text = std::str::from_utf8(body).ok()?;
    if !text.contains(BEGIN) {
        return None;
    }
    let mut certificates = Vec::new();
    for block in text.split(BEGIN).skip(1) {
        let Some(block) = block.split(END).next() else {
            continue;
        };
        let base64: String = block.split_whitespace().collect();
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(base64) {
            certificates.push(CertificateDer::from(der));
        }
    }
    Some(certificates)
}

/// The certificates carried in a PKCS#7 `SignedData`, given the contents of
/// the outer `ContentInfo` sequence.
///
/// `ContentInfo` is an identifier saying "signed data" followed by the signed
/// data itself under an explicit `[0]`; inside that, the certificates sit
/// under another `[0]`, concatenated. Only that one field is wanted, so the
/// signature machinery around it is walked past rather than understood.
fn pkcs7_certificates(content_info: &[u8]) -> Vec<CertificateDer<'static>> {
    let Some(signed_data) = child_with_tag(content_info, CONTEXT_0) else {
        return Vec::new();
    };
    let Some((signed_data, _)) = split(signed_data) else {
        return Vec::new();
    };
    let Some(certificates) = child_with_tag(signed_data.value, CONTEXT_0) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    let mut rest = certificates;
    while let Some((certificate, tail)) = split(rest) {
        // Every element is re-cut from the original bytes: a certificate is
        // the whole element, header included, not the contents of one.
        let len = rest.len() - tail.len();
        if certificate.tag == SEQUENCE {
            found.push(CertificateDer::from(rest[..len].to_vec()));
        }
        rest = tail;
    }
    found
}

// ---------------------------------------------------------------------------
// Just enough DER to read one extension.
//
// A full ASN.1 stack would be a large dependency for a single URL, so this
// reads the handful of elements between a certificate and its Authority
// Information Access extension and nothing else. Every step is length-checked
// and returns `None` rather than trusting what it is walking: the input is a
// certificate that has not been verified yet, so it is somebody else's bytes.
// ---------------------------------------------------------------------------

const SEQUENCE: u8 = 0x30;
const OID: u8 = 0x06;
const BOOLEAN: u8 = 0x01;
const OCTET_STRING: u8 = 0x04;
/// `[0]`, the constructed context-specific tag PKCS#7 wraps its parts in.
const CONTEXT_0: u8 = 0xa0;
/// `[3] EXPLICIT`, where a certificate keeps its extensions.
const EXTENSIONS: u8 = 0xa3;
/// `[6] IMPLICIT IA5String`, a `GeneralName` that is a URI.
const URI: u8 = 0x86;

/// 1.3.6.1.5.5.7.1.1 — id-pe-authorityInfoAccess.
const OID_AUTHORITY_INFO_ACCESS: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x01];
/// 1.3.6.1.5.5.7.48.2 — id-ad-caIssuers, "the certificate above this one".
const OID_CA_ISSUERS: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x02];

struct Element<'a> {
    tag: u8,
    value: &'a [u8],
}

/// Cuts the first element off `input`, returning it and what follows.
fn split(input: &[u8]) -> Option<(Element<'_>, &[u8])> {
    let (&tag, rest) = input.split_first()?;
    // The high-tag-number form spreads a tag over several bytes. Nothing in a
    // certificate's structure uses it, and refusing it keeps this small.
    if tag & 0x1f == 0x1f {
        return None;
    }

    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        // 0x80 is the indefinite length, which DER forbids; beyond four bytes
        // is a length no certificate has.
        if count == 0 || count > 4 {
            return None;
        }
        if rest.len() < count {
            return None;
        }
        let (bytes, rest) = rest.split_at(count);
        (bytes.iter().fold(0usize, |len, b| (len << 8) | *b as usize), rest)
    };

    if rest.len() < len {
        return None;
    }
    let (value, rest) = rest.split_at(len);
    Some((Element { tag, value }, rest))
}

/// The contents of the first element carrying `tag`, among a run of them.
fn child_with_tag(mut input: &[u8], tag: u8) -> Option<&[u8]> {
    while let Some((element, rest)) = split(input) {
        if element.tag == tag {
            return Some(element.value);
        }
        input = rest;
    }
    None
}

/// Every `caIssuers` address a certificate names, in the order it names them.
fn ca_issuer_urls(certificate: &[u8]) -> Vec<String> {
    let Some(access) = authority_info_access(certificate) else {
        return Vec::new();
    };
    let Some((descriptions, _)) = split(access) else {
        return Vec::new();
    };
    if descriptions.tag != SEQUENCE {
        return Vec::new();
    }

    let mut urls = Vec::new();
    let mut rest = descriptions.value;
    while let Some((description, tail)) = split(rest) {
        rest = tail;
        // Each is a method and a place. Anything else — an OCSP responder, an
        // address in a form that is not a URL — is simply not this.
        let Some((method, location)) = split(description.value) else {
            continue;
        };
        if method.tag != OID || method.value != OID_CA_ISSUERS {
            continue;
        }
        let Some((location, _)) = split(location) else {
            continue;
        };
        if location.tag != URI {
            continue;
        }
        if let Ok(url) = std::str::from_utf8(location.value) {
            urls.push(url.to_string());
        }
    }
    urls
}

/// The raw contents of a certificate's Authority Information Access extension.
fn authority_info_access(certificate: &[u8]) -> Option<&[u8]> {
    let (certificate, _) = split(certificate)?;
    if certificate.tag != SEQUENCE {
        return None;
    }
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
    let (tbs, _) = split(certificate.value)?;
    if tbs.tag != SEQUENCE {
        return None;
    }
    // The fields before the extensions are all optional in one way or another,
    // so the extensions are found by their tag rather than counted to.
    let extensions = child_with_tag(tbs.value, EXTENSIONS)?;
    let (extensions, _) = split(extensions)?;
    if extensions.tag != SEQUENCE {
        return None;
    }

    let mut rest = extensions.value;
    while let Some((extension, tail)) = split(rest) {
        rest = tail;
        // Extension ::= SEQUENCE { extnID, critical DEFAULT FALSE, extnValue }
        let Some((id, after_id)) = split(extension.value) else {
            continue;
        };
        if id.tag != OID || id.value != OID_AUTHORITY_INFO_ACCESS {
            continue;
        }
        let Some((next, after_next)) = split(after_id) else {
            continue;
        };
        // `critical` is present only when it is true, so it may or may not sit
        // between the identifier and the value.
        let value = if next.tag == BOOLEAN {
            match split(after_next) {
                Some((value, _)) => value,
                None => continue,
            }
        } else {
            next
        };
        if value.tag == OCTET_STRING {
            return Some(value.value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps `value` in a DER header, using the long length form where it has
    /// to. Only the test needs this — real certificates arrive encoded.
    fn der(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = value.len();
        if len < 0x80 {
            out.push(len as u8);
        } else if len < 0x100 {
            out.extend_from_slice(&[0x81, len as u8]);
        } else {
            out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]);
        }
        out.extend_from_slice(value);
        out
    }

    fn certificate_naming(url: &str, critical: bool) -> Vec<u8> {
        let description = der(
            SEQUENCE,
            &[der(OID, OID_CA_ISSUERS), der(URI, url.as_bytes())].concat(),
        );
        let mut extension = der(OID, OID_AUTHORITY_INFO_ACCESS);
        if critical {
            extension.extend(der(BOOLEAN, &[0xff]));
        }
        extension.extend(der(OCTET_STRING, &der(SEQUENCE, &description)));

        // An extension the reader has no interest in, sitting where a real
        // certificate would have a dozen of them.
        let other = der(
            SEQUENCE,
            &[der(OID, &[0x55, 0x1d, 0x0f]), der(OCTET_STRING, &[0x03, 0x02, 0x01, 0x06])].concat(),
        );
        let extensions = der(EXTENSIONS, &der(SEQUENCE, &[other, der(SEQUENCE, &extension)].concat()));

        // A stand-in for the fields a certificate carries before its
        // extensions: a serial number and a name, skipped by tag.
        let serial = der(0x02, &[0x01, 0x02, 0x03]);
        let subject = der(SEQUENCE, &[]);
        let tbs = der(SEQUENCE, &[serial, subject, extensions].concat());
        let signature = der(0x03, &[0x00, 0xde, 0xad]);
        der(SEQUENCE, &[tbs, signature].concat())
    }

    #[test]
    fn the_address_of_the_missing_certificate_is_read_out_of_the_one_we_have() {
        let certificate = certificate_naming("http://yr1.i.lencr.org/", false);
        assert_eq!(ca_issuer_urls(&certificate), vec!["http://yr1.i.lencr.org/"]);
    }

    #[test]
    fn a_critical_flag_between_the_identifier_and_the_value_is_stepped_over() {
        // Rare on this extension, but legal, and reading `critical` as the
        // value would lose the address on exactly those certificates.
        let certificate = certificate_naming("http://ca.example/i.crt", true);
        assert_eq!(ca_issuer_urls(&certificate), vec!["http://ca.example/i.crt"]);
    }

    #[test]
    fn an_ocsp_address_is_not_mistaken_for_an_issuer() {
        // The same extension holds both. Fetching the responder and calling it
        // a certificate would fail verification for a new reason.
        const OID_OCSP: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01];
        let ocsp = der(
            SEQUENCE,
            &[der(OID, OID_OCSP), der(URI, b"http://ocsp.example/")].concat(),
        );
        let extension = [
            der(OID, OID_AUTHORITY_INFO_ACCESS),
            der(OCTET_STRING, &der(SEQUENCE, &ocsp)),
        ]
        .concat();
        let extensions = der(EXTENSIONS, &der(SEQUENCE, &der(SEQUENCE, &extension)));
        let tbs = der(SEQUENCE, &extensions);
        let certificate = der(SEQUENCE, &tbs);
        assert!(ca_issuer_urls(&certificate).is_empty());
    }

    #[test]
    fn a_certificate_that_names_nobody_asks_for_nothing() {
        let tbs = der(SEQUENCE, &der(0x02, &[0x01]));
        let certificate = der(SEQUENCE, &tbs);
        assert!(ca_issuer_urls(&certificate).is_empty());
    }

    #[test]
    fn truncated_and_nonsense_bytes_are_refused_rather_than_panicked_on() {
        // These arrive from a certificate that has not been verified, so the
        // only acceptable answer to malformed input is an empty one.
        let certificate = certificate_naming("http://ca.example/i.crt", false);
        for cut in 0..certificate.len() {
            let _ = ca_issuer_urls(&certificate[..cut]);
            let _ = ca_issuer_urls(&certificate[cut..]);
            let _ = certificates_in(&certificate[..cut]);
        }
        // A length that claims far more than the input holds.
        let _ = ca_issuer_urls(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff, 0x30]);
        // A length header that never ends.
        let _ = ca_issuer_urls(&[0x30, 0xff]);
        let _ = ca_issuer_urls(&[]);
    }

    #[test]
    fn a_fetched_certificate_is_recognised_in_the_shapes_a_ca_serves_it() {
        use base64::Engine as _;

        // The bare DER an `application/pkix-cert` endpoint returns. Round-trip
        // rather than shape-match: what goes to the verifier must be the whole
        // certificate, header and all.
        let certificate = certificate_naming("http://ca.example/i.crt", false);
        let found = certificates_in(&certificate);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].as_ref(), certificate.as_slice());

        // The same certificate as PEM, which plenty of endpoints serve
        // whatever their content type claims.
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(&certificate)
        );
        let found = certificates_in(pem.as_bytes());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].as_ref(), certificate.as_slice());

        // A PKCS#7 bundle: two certificates under the `[0]` inside SignedData.
        const OID_SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
        let second = certificate_naming("http://ca.example/other.crt", false);
        let signed_data = der(
            SEQUENCE,
            &[
                der(0x02, &[0x01]),
                der(0x31, &[]),
                der(SEQUENCE, &der(OID, OID_SIGNED_DATA)),
                der(CONTEXT_0, &[certificate.clone(), second.clone()].concat()),
            ]
            .concat(),
        );
        let bundle = der(
            SEQUENCE,
            &[der(OID, OID_SIGNED_DATA), der(CONTEXT_0, &signed_data)].concat(),
        );
        let found = certificates_in(&bundle);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].as_ref(), certificate.as_slice());
        assert_eq!(found[1].as_ref(), second.as_slice());
    }

    #[test]
    fn the_configuration_speaks_only_the_protocol_the_client_uses() {
        let config = client_config().expect("the bundled roots always build a verifier");
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }
}
