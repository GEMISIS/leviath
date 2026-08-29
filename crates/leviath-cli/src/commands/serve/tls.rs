//! Serving the API over HTTPS (issue #300).
//!
//! # Why this exists at all
//!
//! The browser console at `https://leviath.dev/lair` cannot call a `lev serve`
//! that is not on loopback. Browsers block a mixed-content request *inside the
//! browser*, before it is sent - so the server never sees it and no response
//! header lifts it. `--cors` is not involved and cannot help.
//!
//! Loopback is the only exemption, and this is the part that surprises people:
//! `http://localhost` is treated as potentially trustworthy, and
//! `http://192.168.1.50:3000` is blocked exactly like a public address.
//!
//! # Bring your own certificate
//!
//! Nothing here generates one. A self-signed certificate made on the user's
//! behalf would not fix the console anyway - a subresource `fetch` gets no
//! interstitial to click through - so it would make `lev serve` look secure
//! while teaching people to click past TLS warnings. `mkcert` and
//! `tailscale cert` both produce certificates that actually work; the docs
//! explain which to reach for.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// A certificate and its key, both present.
///
/// Constructing one is the only way to reach the HTTPS path, which is what
/// makes "one flag without the other" unrepresentable rather than a check
/// somebody has to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TlsPaths {
    /// PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// PEM private key for the leaf.
    pub key: PathBuf,
}

/// Decide whether to serve HTTPS, from the two flags.
///
/// One flag without the other is an error rather than a silent fallback to
/// HTTP. Falling back would start a server on the scheme the user did not ask
/// for, and they would find out from a mixed-content error in a browser on
/// another machine - which is the failure this whole feature exists to end.
pub(crate) fn resolve(cert: Option<PathBuf>, key: Option<PathBuf>) -> Result<Option<TlsPaths>> {
    match (cert, key) {
        (Some(cert), Some(key)) => Ok(Some(TlsPaths { cert, key })),
        (None, None) => Ok(None),
        (Some(_), None) => {
            anyhow::bail!("--tls-cert was given without --tls-key; HTTPS needs both")
        }
        (None, Some(_)) => {
            anyhow::bail!("--tls-key was given without --tls-cert; HTTPS needs both")
        }
    }
}

/// The URL scheme the server will actually answer on.
///
/// Used for the startup banner, which matters more than it looks: the line it
/// prints is the URL a user copies into the console, and a banner saying
/// `http://` for an HTTPS server sends them to an endpoint that cannot work.
pub(crate) fn scheme(tls: Option<&TlsPaths>) -> &'static str {
    match tls {
        Some(_) => "https",
        None => "http",
    }
}

/// Read the certificate and key into a server config.
///
/// Fails before the listener is bound. A server that binds and then rejects
/// every handshake looks like a network problem from the other machine and is
/// far harder to diagnose than one that refuses to start with a message naming
/// the file it could not read.
///
/// The error names the path, because "invalid certificate" without one is
/// unactionable when two files were supplied.
pub(crate) async fn load(paths: &TlsPaths) -> Result<axum_server::tls_rustls::RustlsConfig> {
    axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert, &paths.key)
        .await
        .with_context(|| {
            format!(
                "loading the TLS certificate ({}) and key ({}). Both must be PEM, the key must \
                 match the certificate, and both must be readable by the user running `lev serve`",
                paths.cert.display(),
                paths.key.display()
            )
        })
}

#[cfg(test)]
pub(super) mod tests;
