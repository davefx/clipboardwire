// SPDX-License-Identifier: GPL-3.0-or-later

//! Build a [`tokio_tungstenite::Connector`] from a [`ClientConfig`].
//!
//! The connector is plumbed into `tokio_tungstenite::connect_async_tls_with_config`
//! by `transport.rs` so the same code path serves three deployment shapes:
//!
//! - **Public TLS cert (`wss://…` with default trust).** Uses the bundled
//!   Mozilla webpki roots — `tls_ca_file` unset, `tls_insecure` false.
//! - **Self-signed cert (`wss://…` with a private CA).** The user points
//!   `tls_ca_file` at their PEM bundle; the rustls config trusts those certs
//!   *in addition to* the bundled roots.
//! - **`tls_insecure = true`.** Skips verification entirely. Only safe on a
//!   fully trusted network (loopback, controlled VPN). Documented as
//!   dangerous.
//!
//! For plain `ws://` URLs the connector is unused; tokio-tungstenite picks
//! the TCP path automatically.

use std::io::BufReader;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig as RustlsClientConfig, DigitallySignedStruct, RootCertStore};
use tokio_tungstenite::Connector;

use super::config::ClientConfig;

pub fn make_connector(config: &ClientConfig) -> Result<Connector> {
    let builder = RustlsClientConfig::builder();

    let rustls_config = if config.tls_insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier::new()))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = &config.tls_ca_file {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading TLS CA file {}", path.display()))?;
            let mut reader = BufReader::new(bytes.as_slice());
            let mut added = 0usize;
            for cert in rustls_pemfile::certs(&mut reader) {
                let cert = cert.context("parsing TLS CA file")?;
                roots
                    .add(cert)
                    .context("adding cert from CA file to trust store")?;
                added += 1;
            }
            if added == 0 {
                anyhow::bail!("CA file {} contained no certificates", path.display());
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };

    Ok(Connector::Rustls(Arc::new(rustls_config)))
}

/// Skips every check. Use only behind a `tls_insecure = true` flag with an
/// understood threat model.
#[derive(Debug)]
struct InsecureCertVerifier {
    provider: Arc<CryptoProvider>,
}

impl InsecureCertVerifier {
    fn new() -> Self {
        let provider = CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        Self { provider }
    }
}

impl ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_base() -> ClientConfig {
        ClientConfig {
            server: "wss://example.com/sync".into(),
            user: "u".into(),
            password: "p".into(),
            poll_ms: 300,
            tls_ca_file: None,
            tls_insecure: false,
            hub: None,
        }
    }

    #[test]
    fn builds_default_connector() {
        let connector = make_connector(&cfg_base()).unwrap();
        assert!(matches!(connector, Connector::Rustls(_)));
    }

    #[test]
    fn builds_insecure_connector() {
        let mut cfg = cfg_base();
        cfg.tls_insecure = true;
        let connector = make_connector(&cfg).unwrap();
        assert!(matches!(connector, Connector::Rustls(_)));
    }

    #[test]
    fn rejects_missing_ca_file() {
        let mut cfg = cfg_base();
        cfg.tls_ca_file = Some(PathBuf::from("/nonexistent/ca.pem"));
        let err = match make_connector(&cfg) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("CA file"));
    }
}
