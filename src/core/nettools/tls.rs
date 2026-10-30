//! TLS support for the net tools: the self-signed server certificate generated
//! on demand (rcgen, `ring` provider) and the rustls client/server configs.
//!
//! llcom parity note: rcom is a debugging tool, so the TLS client offers an
//! explicit **accept any certificate** mode (the tools-page toggle and the
//! `rcom.net.tls_send` one-shot both use it). Without the toggle the client
//! verifies against the system-trusted Mozilla root set (`webpki-roots`).

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector, client::TlsStream as ClientTlsStream};

/// The generated self-signed server material, ready for a rustls [`ServerConfig`].
/// Stored as DER bytes; the rustls types are rebuilt per use (cheap clones of
/// small byte buffers — the key types are not `Clone`).
struct ServerCert {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

static SERVER_CERT: std::sync::OnceLock<ServerCert> = std::sync::OnceLock::new();

/// The self-signed server certificate, generated on first use (rcgen; valid for
/// localhost, `::1` and `127.0.0.1` so loopback tools work out of the box).
/// Generation failures become errors, never panics — the caller reports them on
/// the channel.
fn server_cert() -> Result<&'static ServerCert, String> {
    if let Some(cert) = SERVER_CERT.get() {
        return Ok(cert);
    }
    let certified = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .map_err(|error| format!("self-signed certificate generation failed: {error}"))?;
    let cert = ServerCert {
        cert_der: certified.cert.der().as_ref().to_vec(),
        key_der: certified.signing_key.serialize_der(),
    };
    // Another thread may have won the race; `get_or_init` cannot carry the
    // Result, so the loser simply drops its copy.
    let _ = SERVER_CERT.set(cert);
    Ok(SERVER_CERT
        .get()
        .expect("server cert was just initialised on this thread"))
}

/// Installs the process-default rustls crypto provider (ring — the only one this
/// build enables). Safe to call repeatedly; the first installation wins.
pub fn ensure_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The TLS server acceptor presenting the self-signed certificate. The server
/// does not authenticate clients (no client-certificate request).
pub fn server_acceptor() -> Result<TlsAcceptor, String> {
    ensure_provider();
    let cert = server_cert()?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone())),
        )
        .map_err(|error| format!("server TLS config failed: {error}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// The TLS client connector. With `accept_any` the server certificate is not
/// verified at all (llcom-parity debugging mode, an explicit user toggle);
/// otherwise the standard webpki verification against the Mozilla root set
/// applies (a self-signed peer then fails the handshake, which the tests pin).
pub fn client_connector(accept_any: bool) -> Result<TlsConnector, String> {
    Ok(TlsConnector::from(client_config(accept_any)?))
}

/// Builds the client config for the requested verification mode.
pub fn client_config(accept_any: bool) -> Result<Arc<ClientConfig>, String> {
    ensure_provider();
    let config: ClientConfig = if accept_any {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// The accept-any client config, cached (the one-shots build a fresh
/// `ClientConnection` per call; the config is immutable).
pub fn client_config_any() -> Result<Arc<ClientConfig>, String> {
    static ANY: std::sync::OnceLock<Result<Arc<ClientConfig>, String>> = std::sync::OnceLock::new();
    ANY.get_or_init(|| client_config(true)).clone()
}

/// Connects a TLS client stream to `addr`, presenting `server_name` (DNS name or
/// IP literal — rustls accepts both). This is the tokio half used by channels.
pub async fn connect_client(
    stream: TcpStream,
    server_name: &str,
    accept_any: bool,
) -> Result<ClientTlsStream<TcpStream>, String> {
    let name = ServerName::try_from(server_name.to_string())
        .map_err(|error| format!("invalid TLS server name {server_name:?}: {error}"))?;
    let connector = client_connector(accept_any)?;
    connector.connect(name, stream).await.map_err(|error| format!("TLS handshake failed: {error}"))
}

/// The verifier that accepts everything — the explicit debugging-tool escape
/// hatch (llcom parity: connect to devices with self-signed certificates).
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The self-signed server certificate generates once and is stable: the
    /// second call returns the same material (on-demand generation, cached).
    #[test]
    fn server_cert_generates_on_demand_and_caches() {
        let first = server_cert().expect("cert generation must succeed").cert_der.clone();
        let second = server_cert().expect("cached cert must succeed").cert_der.clone();
        assert!(!first.is_empty());
        assert_eq!(first, second, "the certificate must be generated once");
    }

    /// THE TLS proof: client and server over rustls with the self-signed pair
    /// complete a handshake and move bytes — with "accept any cert" (the
    /// debugging-tool mode) the handshake succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_client_and_server_roundtrip_with_accept_any() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = server_acceptor().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.expect("server handshake");
            tls.write_all(b"pong").await.unwrap();
            tls.flush().await.unwrap();
            let mut buf = [0u8; 16];
            let n = tls.read(&mut buf).await.unwrap();
            (n, buf[..n].to_vec())
        });

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client = connect_client(tcp, &addr.ip().to_string(), true)
            .await
            .expect("client handshake with accept-any");
        let mut buf = [0u8; 16];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"pong");
        client.write_all(b"ping").await.unwrap();
        client.flush().await.unwrap();

        let (n, got) = server.await.unwrap();
        assert_eq!(&got[..n], b"ping");
    }

    /// The toggle means something: without "accept any cert", the standard
    /// verification path rejects the self-signed server (webpki, untrusted
    /// root) — the handshake fails instead of silently trusting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn standard_verification_rejects_self_signed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = server_acceptor().unwrap();

        tokio::spawn(async move {
            // The server keeps accepting; the client failing the handshake is
            // the assertion — the accept error path just ends the task.
            while let Ok((stream, _)) = listener.accept().await {
                let _ = acceptor.accept(stream).await;
            }
        });

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let result = connect_client(tcp, &addr.ip().to_string(), false).await;
        assert!(result.is_err(), "self-signed must fail standard verification");
    }

    /// IP-literal server names (IPv4 and IPv6 shapes) are accepted by the name
    /// parser the client uses — the `::1` path included.
    #[test]
    fn server_name_parses_ip_literals() {
        for name in ["127.0.0.1", "::1", "[::1]", "localhost"] {
            // The bracketed form is not a valid rustls name; the channel strips
            // brackets before calling, which this asserts indirectly by
            // accepting the stripped shapes.
            let candidate = name.trim_matches(['[', ']']);
            assert!(
                ServerName::try_from(candidate.to_string()).is_ok(),
                "{candidate} must parse as a server name"
            );
        }
    }
}
