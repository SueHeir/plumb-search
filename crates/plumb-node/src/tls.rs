//! HTTPS for remote control on a local network, without a certificate
//! authority: `plumb run --https-bind ADDR` serves the node over TLS with a
//! certificate the node makes for itself, and the desktop app trusts that
//! one certificate by its SHA-256 fingerprint, which the owner copies from
//! the node along with the token.
//!
//! The certificate and its private key stay in the data folder
//! (`DIR/remote-control-cert.der` and `DIR/remote-control-key.der`, the key
//! readable by its owner only on Unix), so the fingerprint stays the same
//! across restarts and new tokens. Deleting both files makes a new pair at
//! the next start; saved connections then need the new fingerprint.
//!
//! A pinned connection checks nothing but the fingerprint (and that the
//! server holds the certificate's key): no names, no dates, no authority.
//! That is what makes it work for a bare LAN address, and also why an
//! address with a pinned fingerprint is never checked any other way.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

/// The node's certificate, in DER.
pub const CERT_FILE: &str = "remote-control-cert.der";
/// Its private key, PKCS #8 DER.
pub const KEY_FILE: &str = "remote-control-key.der";

/// How long a client may take to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Handshakes under way at once; more connections wait to be accepted.
const MAX_HANDSHAKES: usize = 64;

/// The node's own certificate and key.
pub struct NodeCert {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl std::fmt::Debug for NodeCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCert")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl NodeCert {
    /// The fingerprint to pin, as [`fingerprint`] writes it.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn cert_path(dir: &Path) -> PathBuf {
    dir.join(CERT_FILE)
}

fn key_path(dir: &Path) -> PathBuf {
    dir.join(KEY_FILE)
}

/// The fingerprint of the certificate in data folder `dir`, if it has one.
pub fn existing_fingerprint(dir: &Path) -> Option<String> {
    let cert = std::fs::read(cert_path(dir)).ok()?;
    key_path(dir)
        .is_file()
        .then(|| fingerprint(&CertificateDer::from(cert)))
}

/// The certificate in data folder `dir`, made the first time.
pub fn load_or_create(dir: &Path) -> Result<NodeCert> {
    match (std::fs::read(cert_path(dir)), std::fs::read(key_path(dir))) {
        (Ok(cert), Ok(key)) => {
            let cert = NodeCert {
                cert: CertificateDer::from(cert),
                key: PrivatePkcs8KeyDer::from(key),
            };
            // Catch a damaged pair now rather than at the first connection.
            server_config(&cert).context("reading the node's HTTPS certificate")?;
            Ok(cert)
        }
        (Err(cert), Err(key))
            if cert.kind() == std::io::ErrorKind::NotFound
                && key.kind() == std::io::ErrorKind::NotFound =>
        {
            create(dir)
        }
        // The key is written first, so a key alone is a pair a crash cut
        // short: no fingerprint of it was ever shown. Made again.
        (Err(cert), Ok(_)) if cert.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                "{KEY_FILE} in {} has no {CERT_FILE}; making a new pair",
                dir.display()
            );
            create(dir)
        }
        (Err(err), _) | (_, Err(err)) => Err(err).with_context(|| {
            format!(
                "reading {CERT_FILE} and {KEY_FILE} in {}; delete both to make a new \
                 certificate (saved connections then need the new fingerprint)",
                dir.display()
            )
        }),
    }
}

fn create(dir: &Path) -> Result<NodeCert> {
    let made = rcgen::generate_simple_self_signed(vec!["plumb-node".to_string()])
        .context("making the node's HTTPS certificate")?;
    let cert = NodeCert {
        cert: made.cert.der().clone(),
        key: PrivatePkcs8KeyDer::from(made.key_pair.serialize_der()),
    };
    // The key first: a certificate on disk always has its key next to it.
    write_new(&key_path(dir), cert.key.secret_pkcs8_der(), true)?;
    write_new(&cert_path(dir), &cert.cert, false)?;
    Ok(cert)
}

/// Writes `bytes` to `path` in one go; `private` makes it readable by its
/// owner only, on Unix.
fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    use std::io::Write as _;
    let tmp = crate::temp_path_for(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("writing {}", path.display()));
    }
    // So the key is on disk before the certificate, after a power cut too.
    crate::sync_parent_dir(path);
    Ok(())
}

/// `SHA256:` and the certificate's SHA-256 in uppercase hex, in pairs
/// separated by colons, as browsers and `openssl x509 -fingerprint` show it.
pub fn fingerprint(cert: &[u8]) -> String {
    let mut text = String::from("SHA256:");
    for (i, byte) in Sha256::digest(cert).iter().enumerate() {
        if i > 0 {
            text.push(':');
        }
        let _ = write!(text, "{byte:02X}");
    }
    text
}

/// Reads a fingerprint as pasted: with or without `SHA256:` (or
/// `sha256/`, `SHA-256`), colons, spaces or dashes, in either case.
pub fn parse_fingerprint(text: &str) -> Result<[u8; 32]> {
    let compact: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-' | '/'))
        .collect::<String>()
        .to_ascii_lowercase();
    // "sha" is not hex, so the name can be told from the digits.
    let hex = compact.strip_prefix("sha256").unwrap_or(&compact);
    let wrong = || {
        anyhow!(
            "A certificate fingerprint is 64 hexadecimal digits, such as \
             SHA256:3F:A1:...; copy the whole line from the node."
        )
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(wrong());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| wrong())?;
    }
    Ok(out)
}

/// `fingerprint` in the form [`fingerprint`] writes, or an error.
pub fn normalize_fingerprint(text: &str) -> Result<String> {
    let bytes = parse_fingerprint(text)?;
    let mut out = String::from("SHA256:");
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        let _ = write!(out, "{byte:02X}");
    }
    Ok(out)
}

fn server_config(cert: &NodeCert) -> Result<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.clone()],
            PrivateKeyDer::Pkcs8(cert.key.clone_key()),
        )?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// A TLS client configuration that trusts exactly the certificate whose
/// SHA-256 is `pin`, for any address.
pub fn pinned_client_config(pin: [u8; 32]) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let verifier = PinnedVerifier {
        pin,
        provider: provider.clone(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Accepts the one certificate whose SHA-256 is `pin`. The handshake's
/// signatures are still checked, so the server must hold its key.
#[derive(Debug)]
struct PinnedVerifier {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

/// The message a pinned certificate mismatch carries, which the desktop
/// recognizes to explain it.
pub const PIN_MISMATCH: &str = "the node's certificate does not match the saved fingerprint";

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = Sha256::digest(end_entity.as_ref());
        if seen.as_slice() == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(PIN_MISMATCH.to_string()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The address a connection to the HTTPS listener came from. The web
/// server copies it into `ConnectInfo<SocketAddr>`, which its local-only
/// and local-network checks read, as for plain HTTP.
#[derive(Debug, Clone, Copy)]
pub struct TlsPeer(pub SocketAddr);

/// TLS connections, already through their handshakes, for `axum::serve`.
/// Handshakes run in their own tasks, so a slow client holds up no one.
pub struct TlsListener {
    local: SocketAddr,
    accepted: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    accept_loop: tokio::task::JoinHandle<()>,
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept_loop.abort();
    }
}

impl TlsListener {
    /// Listens on `addr` with the node's certificate.
    pub async fn bind(addr: SocketAddr, cert: &NodeCert) -> Result<Self> {
        let acceptor = TlsAcceptor::from(Arc::new(server_config(cert)?));
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening for HTTPS on {addr}"))?;
        let local = tcp.local_addr().context("reading the HTTPS address")?;
        let (send, accepted) = mpsc::channel(MAX_HANDSHAKES);
        let accept_loop = tokio::spawn(accept_loop(tcp, acceptor, send));
        Ok(TlsListener {
            local,
            accepted,
            accept_loop,
        })
    }
}

async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    send: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    let permits = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        let Ok(permit) = permits.clone().acquire_owned().await else {
            return;
        };
        let (stream, peer) = match tcp.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                // Out of file descriptors, most likely; as axum does.
                warn!("accepting an HTTPS connection: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let send = send.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => {
                    let _ = send.send((tls, peer)).await;
                }
                Ok(Err(err)) => debug!("TLS handshake with {peer} failed: {err}"),
                Err(_) => debug!("TLS handshake with {peer} timed out"),
            }
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.accepted.recv().await {
            Some(accepted) => accepted,
            // The accept loop never ends while the listener lives.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>>
    for TlsPeer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        TlsPeer(*stream.remote_addr())
    }
}

/// Copies the HTTPS peer address into `ConnectInfo<SocketAddr>`.
pub async fn copy_peer(mut request: axum::extract::Request) -> axum::extract::Request {
    use axum::extract::ConnectInfo;
    if let Some(ConnectInfo(TlsPeer(peer))) =
        request.extensions().get::<ConnectInfo<TlsPeer>>().copied()
    {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    request
}

/// Fails for an address the HTTPS listener cannot share with HTTP.
pub fn check_bind(http: SocketAddr, https: SocketAddr) -> Result<()> {
    if https.port() != 0 && https.port() == http.port() {
        bail!("--https-bind must use another port than --bind ({http})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_read_back_in_any_form() {
        let fp = fingerprint(b"certificate");
        assert!(fp.starts_with("SHA256:"));
        assert_eq!(fp.len(), "SHA256:".len() + 32 * 3 - 1);
        let bytes = parse_fingerprint(&fp).unwrap();
        assert_eq!(bytes.as_slice(), Sha256::digest(b"certificate").as_slice());
        let bare: String = fp[7..].chars().filter(|c| *c != ':').collect();
        for form in [
            bare.clone(),
            bare.to_lowercase(),
            format!(" sha256/{bare} "),
            fp.to_lowercase(),
            fp.replace(':', " ").replacen("SHA256 ", "", 1),
        ] {
            assert_eq!(parse_fingerprint(&form).unwrap(), bytes, "{form}");
            assert_eq!(normalize_fingerprint(&form).unwrap(), fp, "{form}");
        }
        for bad in [
            "",
            "SHA256:",
            "abc",
            &bare[2..],
            &format!("{bare}00"),
            &bare.replace('A', "G"),
        ] {
            if bad != bare {
                assert!(parse_fingerprint(bad).is_err(), "{bad}");
            }
        }
    }

    #[test]
    fn keeps_one_certificate_per_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(existing_fingerprint(dir.path()), None);
        let first = load_or_create(dir.path()).unwrap();
        let again = load_or_create(dir.path()).unwrap();
        assert_eq!(first.fingerprint(), again.fingerprint());
        assert_eq!(existing_fingerprint(dir.path()), Some(first.fingerprint()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A certificate without its key is not silently replaced.
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        assert!(load_or_create(dir.path()).is_err());
        assert_eq!(existing_fingerprint(dir.path()), None);
    }

    #[test]
    fn a_key_left_alone_by_a_crash_is_made_again_with_its_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(CERT_FILE)).unwrap();
        let made = load_or_create(dir.path()).unwrap();
        assert_ne!(made.fingerprint(), first.fingerprint());
        assert_eq!(existing_fingerprint(dir.path()), Some(made.fingerprint()));
        let again = load_or_create(dir.path()).unwrap();
        assert_eq!(again.fingerprint(), made.fingerprint());
    }

    #[test]
    fn https_needs_its_own_port() {
        let http: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(check_bind(http, "0.0.0.0:8080".parse().unwrap()).is_err());
        assert!(check_bind(http, "0.0.0.0:8443".parse().unwrap()).is_ok());
    }
}
