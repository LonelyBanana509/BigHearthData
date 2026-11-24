//! TLS 1.3 for the gateway: the certificate and key from PEM files, and a reload that swaps them
//! without dropping an open connection (wire-protocol spec, decisions 1 and 2, test W12). Lane B.
//!
//! One crypto provider (`ring`) and TLS 1.3 only. A new connection takes the configuration that is
//! current when it is accepted; a connection that is already open keeps the one it started with, so
//! a reload never closes anything. A reload that fails (a missing file, a key that does not match)
//! leaves the old certificate in place.
//!
//! The server keeps no TLS session cache and sends no resumption tickets: the client pins the key and
//! logs in again with SCRAM on every connection, so a cache would only be memory held per client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use rustls::server::NoServerSessionStorage;
use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

/// Why the certificate or key could not be used.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file could not be read or parsed.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// What went wrong.
        source: rustls_pki_types::pem::Error,
    },
    /// The certificate file has no certificate in it.
    #[error("{0} holds no certificate")]
    NoCertificate(String),
    /// rustls refused the pair (for instance the key does not match the certificate).
    #[error("TLS configuration refused: {0}")]
    Config(#[from] rustls::Error),
}

fn build(cert_path: &Path, key_path: &Path) -> Result<Arc<ServerConfig>, TlsError> {
    let shown = |p: &Path| p.display().to_string();
    let certs = CertificateDer::pem_file_iter(cert_path)
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .map_err(|source| TlsError::Read {
            path: shown(cert_path),
            source,
        })?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate(shown(cert_path)));
    }
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|source| TlsError::Read {
        path: shown(key_path),
        source,
    })?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(Arc::new(config))
}

/// The server's certificate, reloadable.
pub struct TlsState {
    cert_path: PathBuf,
    key_path: PathBuf,
    current: RwLock<Arc<ServerConfig>>,
}

impl std::fmt::Debug for TlsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsState")
            .field("cert_path", &self.cert_path)
            .finish_non_exhaustive()
    }
}

impl TlsState {
    /// Reads the certificate chain and the private key.
    ///
    /// # Errors
    /// [`TlsError`] when a file is missing or not PEM, has no certificate, or the pair does not fit.
    pub fn load(cert_path: &Path, key_path: &Path) -> Result<Self, TlsError> {
        Ok(TlsState {
            cert_path: cert_path.to_owned(),
            key_path: key_path.to_owned(),
            current: RwLock::new(build(cert_path, key_path)?),
        })
    }

    /// Reads the two files again. New connections use the result; open ones are not touched. On an
    /// error the old certificate stays in force.
    ///
    /// # Errors
    /// As [`TlsState::load`].
    pub fn reload(&self) -> Result<(), TlsError> {
        let fresh = build(&self.cert_path, &self.key_path)?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// An acceptor with the certificate that is current now.
    #[must_use]
    pub fn acceptor(&self) -> TlsAcceptor {
        let config = Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner));
        TlsAcceptor::from(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pair(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = dir.join(format!("{name}.crt"));
        let keyfile = dir.join(format!("{name}.key"));
        std::fs::write(&cert, key.cert.pem()).unwrap();
        std::fs::write(&keyfile, key.key_pair.serialize_pem()).unwrap();
        (cert, keyfile)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bhdata-tls-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_good_pair_loads_and_a_missing_file_is_named() {
        let dir = temp_dir("load");
        let (cert, key) = write_pair(&dir, "a");
        assert!(TlsState::load(&cert, &key).is_ok());
        let err = TlsState::load(&dir.join("nope.crt"), &key).unwrap_err();
        assert!(err.to_string().contains("nope.crt"), "{err}");
    }

    #[test]
    fn a_file_with_no_certificate_is_refused() {
        let dir = temp_dir("empty");
        let (_, key) = write_pair(&dir, "a");
        let empty = dir.join("empty.crt");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(
            TlsState::load(&empty, &key),
            Err(TlsError::NoCertificate(_) | TlsError::Read { .. })
        ));
    }

    #[test]
    fn a_key_that_does_not_match_is_refused_and_a_failed_reload_keeps_the_old_pair() {
        let dir = temp_dir("mismatch");
        let (cert_a, key_a) = write_pair(&dir, "a");
        let (_, key_b) = write_pair(&dir, "b");
        assert!(TlsState::load(&cert_a, &key_b).is_err());
        let state = TlsState::load(&cert_a, &key_a).unwrap();
        // Break the key file, then reload: the call fails and the state still works.
        std::fs::copy(&key_b, &key_a).unwrap();
        assert!(state.reload().is_err());
        let _still_usable = state.acceptor();
    }
}
