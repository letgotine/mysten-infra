// Copyright(C) 2022, Mysten Labs
// SPDX-License-Identifier: Apache-2.0
use std::{fmt, time::SystemTime};

use rustls::{
    client::{ServerCertVerified, ServerCertVerifier},
    server::{ClientCertVerified, ClientCertVerifier},
};
use serde::{
    de::{Error, Visitor},
    Deserialize, Deserializer, Serialize,
};
use x509_parser::certificate::X509Certificate;
use x509_parser::{traits::FromDer, x509::SubjectPublicKeyInfo};

#[cfg(test)]
#[path = "tests/psk.rs"]
pub mod psk;

type SignatureAlgorithms = &'static [&'static webpki::SignatureAlgorithm];
static SUPPORTED_SIG_ALGS: SignatureAlgorithms = &[&webpki::ECDSA_P256_SHA256, &webpki::ED25519];

/// X.509 `SubjectPublicKeyInfo` (SPKI) as defined in [RFC 5280 Section 4.1.2.7].
///
/// ASN.1 structure containing an [`AlgorithmIdentifier`] and public key
/// data in an algorithm specific format.
///
/// ```text
///    SubjectPublicKeyInfo  ::=  SEQUENCE  {
///         algorithm            AlgorithmIdentifier,
///         subjectPublicKey     BIT STRING  }
/// ```
///
/// [RFC 5280 Section 4.1.2.7]: https://tools.ietf.org/html/rfc5280#section-4.1.2.7
///
/// We only support ECDSA P-256 & Ed25519 (for now).

#[derive(PartialEq, Clone, Debug)]
pub struct Psk<'a>(pub SubjectPublicKeyInfo<'a>);

impl<'a> Eq for Psk<'a> {}

////////////////////////////////////////////////////////////////
/// Ser/de
////////////////////////////////////////////////////////////////

impl<'a> Serialize for Psk<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.0.raw)
    }
}

struct DerBytesVisitor;

impl<'de> Visitor<'de> for DerBytesVisitor {
    type Value = Psk<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a borrowed Subject Public Key Info in DER format")
    }

    fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
    where
        E: Error,
    {
        let (_, spki) = SubjectPublicKeyInfo::from_der(v).map_err(Error::custom)?;
        Ok(Psk(spki))
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        let (_, spki) = SubjectPublicKeyInfo::from_der(v.as_bytes()).map_err(Error::custom)?;
        Ok(Psk(spki))
    }
}

impl<'de> Deserialize<'de> for Psk<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_bytes(DerBytesVisitor)
    }
}

////////////////////////////////////////////////////////////////
/// end Ser/de
////////////////////////////////////////////////////////////////

/// A `ClientCertVerifier` that will ensure that every client provides a valid, expected
/// certificate, without any name checking.
impl<'a> ClientCertVerifier for Psk<'a> {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> Option<bool> {
        Some(true)
    }

    fn client_auth_root_subjects(&self) -> Option<rustls::DistinguishedNames> {
        // We can't guarantee subjects before having seen the cert
        None
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::Certificate,
        intermediates: &[rustls::Certificate],
        now: SystemTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Check this matches the key we expect
        let cert = X509Certificate::from_der(&end_entity.0[..])
            .map_err(|_| rustls::Error::InvalidCertificateEncoding)?;
        let spki = cert.1.public_key().clone();
        if spki != self.0 {
            return Err(rustls::Error::InvalidCertificateData(format!(
                "invalid peer certificate: received {:?} instead of expected {:?}",
                spki, self.0
            )));
        }

        // We now check we're receiving correctly signed data with the expected key
        let (cert, chain, trustroots) = prepare_for_self_signed(end_entity, intermediates)?;
        let now = webpki::Time::try_from(now).map_err(|_| rustls::Error::FailedToGetCurrentTime)?;
        cert.verify_is_valid_tls_client_cert(
            SUPPORTED_SIG_ALGS,
            &webpki::TlsClientTrustAnchors(&trustroots),
            &chain,
            now,
        )
        .map_err(pki_error)
        .map(|_| ClientCertVerified::assertion())
    }
}

impl<'a> ServerCertVerifier for Psk<'a> {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::Certificate,
        intermediates: &[rustls::Certificate],
        server_name: &rustls::ServerName,
        scts: &mut dyn Iterator<Item = &[u8]>,
        ocsp_response: &[u8],
        now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        // Check this matches the key we expect
        let cert = X509Certificate::from_der(&end_entity.0[..])
            .map_err(|_| rustls::Error::InvalidCertificateEncoding)?;
        let spki = cert.1.public_key().clone();
        if spki != self.0 {
            return Err(rustls::Error::InvalidCertificateData(format!(
                "invalid peer certificate: received {:?} instead of expected {:?}",
                spki, self.0
            )));
        }

        // Then we check this is actually a valid self-signed certificate with matching name
        let (cert, chain, trustroots) = prepare_for_self_signed(end_entity, intermediates)?;
        let webpki_now =
            webpki::Time::try_from(now).map_err(|_| rustls::Error::FailedToGetCurrentTime)?;

        let dns_nameref = match server_name {
            rustls::ServerName::DnsName(dns_name) => {
                webpki::DnsNameRef::try_from_ascii_str(dns_name.as_ref())
                    .map_err(|_| rustls::Error::UnsupportedNameType)?
            }
            _ => return Err(rustls::Error::UnsupportedNameType),
        };

        let cert = cert
            .verify_is_valid_tls_server_cert(
                SUPPORTED_SIG_ALGS,
                &webpki::TlsServerTrustAnchors(&trustroots),
                &chain,
                webpki_now,
            )
            .map_err(pki_error)
            .map(|_| cert)?;

        let mut peekable = scts.peekable();
        if peekable.peek().is_none() {
            tracing::trace!("Met unvalidated certificate transparency data");
        }

        if !ocsp_response.is_empty() {
            tracing::trace!("Unvalidated OCSP response: {:?}", ocsp_response.to_vec());
        }

        cert.verify_is_valid_for_dns_name(dns_nameref)
            .map_err(pki_error)
            .map(|_| ServerCertVerified::assertion())
    }
}

type CertChainAndRoots<'a> = (
    webpki::EndEntityCert<'a>,
    Vec<&'a [u8]>,
    Vec<webpki::TrustAnchor<'a>>,
);

fn prepare_for_self_signed<'a>(
    end_entity: &'a rustls::Certificate,
    intermediates: &'a [rustls::Certificate],
) -> Result<CertChainAndRoots<'a>, rustls::Error> {
    // EE cert must appear first.
    let cert = webpki::EndEntityCert::try_from(end_entity.0.as_ref()).map_err(pki_error)?;

    let intermediates: Vec<&'a [u8]> = intermediates.iter().map(|cert| cert.0.as_ref()).collect();

    // reinterpret the certificate as a root, materializing the self-signed policy
    let root = webpki::TrustAnchor::try_from_cert_der(end_entity.0.as_ref()).map_err(pki_error)?;

    Ok((cert, intermediates, vec![root]))
}

fn pki_error(error: webpki::Error) -> rustls::Error {
    use webpki::Error::*;
    match error {
        BadDer | BadDerTime => rustls::Error::InvalidCertificateEncoding,
        InvalidSignatureForPublicKey => rustls::Error::InvalidCertificateSignature,
        UnsupportedSignatureAlgorithm | UnsupportedSignatureAlgorithmForPublicKey => {
            rustls::Error::InvalidCertificateSignatureType
        }
        e => rustls::Error::InvalidCertificateData(format!("invalid peer certificate: {}", e)),
    }
}