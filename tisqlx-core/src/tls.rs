//! TLS upgrade. tisqlx always uses rustls (ring provider).

use std::sync::Arc;

use tokio_rustls::rustls;

use crate::error::{Error, Result};
use crate::options::ConnectOptions;
use crate::protocol::stream::PacketStream;

/// Upgrade to TLS, verifying the certificate for `server_name` (the host name, never the
/// resolved address).
pub(crate) async fn upgrade(stream: &mut PacketStream, options: &ConnectOptions, server_name: &str) -> Result<()> {
    use tokio_rustls::rustls::pki_types::ServerName;

    use crate::protocol::stream::Socket;

    // Built once per ConnectOptions: also keeps rustls' session-resumption cache across connections.
    let config = match options.tls_config.get() {
        Some(c) => c.clone(),
        None => {
            let c = client_config(options)?;
            let _ = options.tls_config.set(c.clone());
            c
        }
    };
    let server_name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| Error::Tls(format!("invalid TLS server name: {e}").into()))?;
    let connector = tokio_rustls::TlsConnector::from(config);
    let socket = stream.take_socket();
    // A TLS failure (bad certificate, alert...) wraps a `rustls::Error`; anything else is the
    // network failing mid-handshake, which must stay an I/O error so that connecting fails over.
    let tls = connector.connect(server_name, socket).await.map_err(|e| {
        if e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) { Error::Tls(e.into()) } else { Error::Io(e) }
    })?;
    stream.replace_socket(Socket::Tls(Box::new(tls)));
    stream.is_tls = true;
    Ok(())
}

fn client_config(options: &ConnectOptions) -> Result<Arc<rustls::ClientConfig>> {
    use rustls::{ClientConfig, RootCertStore};

    use crate::options::SslMode;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.into()))?;

    let mut roots = RootCertStore::empty();
    match &options.ssl_ca {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|e| Error::Tls(format!("reading {}: {e}", path.display()).into()))?;
            use rustls::pki_types::{CertificateDer, pem::PemObject};
            for cert in CertificateDer::pem_slice_iter(&pem) {
                roots.add(cert.map_err(|e| Error::Tls(e.into()))?).map_err(|e| Error::Tls(e.into()))?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }

    let config = match options.ssl_mode {
        SslMode::VerifyIdentity => builder.with_root_certificates(roots).with_no_client_auth(),
        SslMode::VerifyCa => {
            let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .map_err(|e| Error::Tls(e.into()))?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier::NoHostname { inner }))
                .with_no_client_auth()
        }
        _ => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier::NoVerify { provider }))
            .with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

mod verifier {
    use std::sync::Arc;

    use tokio_rustls::rustls::client::WebPkiServerVerifier;
    use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use tokio_rustls::rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{CertificateError, DigitallySignedStruct, Error, SignatureScheme};

    /// Accepts any certificate (ssl-mode=preferred/required).
    #[derive(Debug)]
    pub(super) struct NoVerify {
        pub(super) provider: Arc<CryptoProvider>,
    }

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider.signature_verification_algorithms.supported_schemes()
        }
    }

    /// Verifies the chain but not the hostname (ssl-mode=verify-ca).
    #[derive(Debug)]
    pub(super) struct NoHostname {
        pub(super) inner: Arc<WebPkiServerVerifier>,
    }

    impl ServerCertVerifier for NoHostname {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            ocsp: &[u8],
            now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            match self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
                Err(Error::InvalidCertificate(CertificateError::NotValidForName))
                | Err(Error::InvalidCertificate(CertificateError::NotValidForNameContext { .. })) => {
                    Ok(ServerCertVerified::assertion())
                }
                other => other,
            }
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
    }
}
