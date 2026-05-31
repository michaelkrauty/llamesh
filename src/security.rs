use crate::config::{AuthConfig, ClusterTlsConfig, ServerTlsConfig};
use anyhow::{Context, Result};
use axum::http::HeaderMap;
use reqwest::{Client, ClientBuilder};
use rustls::pki_types::pem::PemObject;
use rustls::ServerConfig;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct PeerIdentity {
    pub node_id: Option<String>,
    pub authenticated: bool,
}

/// Check API key authentication against the provided config and headers.
/// Returns Ok(()) if authentication succeeds or is not required.
/// Returns Err("Unauthorized") if authentication fails.
///
/// The presented key is compared against the configured keys in constant time
/// (see [`allowed_key_matches`]), matching how the cluster handshake token is
/// verified, so a partial match is not distinguishable from a full mismatch by
/// response timing.
pub fn check_api_key_auth(
    auth_config: Option<&AuthConfig>,
    headers: &HeaderMap,
) -> Result<(), &'static str> {
    if let Some(config) = auth_config {
        if config.enabled {
            let api_key = config.get_header_value(headers);
            let authorized = match api_key {
                Some(key) => allowed_key_matches(&config.allowed_keys, key),
                None => false,
            };
            if !authorized {
                return Err("Unauthorized");
            }
        }
    }
    Ok(())
}

/// Compare two byte strings in constant time.
///
/// Returns early when the lengths differ — the length of a configured key is
/// not treated as secret — but when the lengths match, every byte is compared
/// with no short-circuit, so the running time does not reveal how many leading
/// bytes of a candidate match a real key. Mirrors the constant-time token
/// comparison used for the cluster handshake (`noise::handshake::verify_token`).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Return whether `candidate` matches any configured API key, comparing in
/// constant time. Every configured key is checked — the loop does not stop at
/// the first match — so that neither which key matched nor how many keys were
/// examined is exposed through response timing.
fn allowed_key_matches(allowed_keys: &[String], candidate: &str) -> bool {
    let mut authorized = false;
    for key in allowed_keys {
        // Bitwise `|=` (not `||`) so every key is always compared; short-circuit
        // evaluation here would reintroduce a timing side channel.
        authorized |= constant_time_eq(key.as_bytes(), candidate.as_bytes());
    }
    authorized
}

pub fn extract_peer_identity(certs: &[rustls::pki_types::CertificateDer<'_>]) -> PeerIdentity {
    if let Some(cert) = certs.first() {
        if let Ok((_, parsed)) = x509_parser::parse_x509_certificate(cert.as_ref()) {
            // Iterate over RDNs to find Common Name (CN)
            // x509-parser 0.16 usage
            for rdn in parsed.subject().iter() {
                for attr in rdn.iter() {
                    if *attr.attr_type() == x509_parser::oid_registry::OID_X509_COMMON_NAME {
                        if let Ok(cn) = attr.attr_value().as_str() {
                            return PeerIdentity {
                                node_id: Some(cn.to_string()),
                                authenticated: true,
                            };
                        }
                    }
                }
            }
        }
    }
    PeerIdentity {
        node_id: None,
        authenticated: false,
    }
}

pub async fn load_server_config(
    server_config: &ServerTlsConfig,
    cluster_config: Option<&ClusterTlsConfig>,
) -> Result<ServerConfig> {
    let certs = load_certs(&server_config.cert_path)?;
    let key = load_private_key(&server_config.key_path)?;

    let builder = ServerConfig::builder();

    let config = if let Some(cluster_tls) = cluster_config.filter(|c| c.enabled) {
        let ca_certs = load_certs(&cluster_tls.ca_cert_path)?;
        let mut root_store = rustls::RootCertStore::empty();
        for cert in ca_certs {
            root_store
                .add(cert)
                .context("Failed to add CA cert to store")?;
        }

        // Allow both anonymous (public) and authenticated (cluster) clients.
        // NOTE: This design means cluster-specific endpoints must enforce authentication
        // at the application layer (e.g., checking PeerIdentity). The TLS layer alone
        // cannot distinguish cluster vs public traffic since both share this listener.
        // If strict cluster isolation is needed, consider separate listeners or
        // always requiring client certs with an "anonymous" cert for public clients.
        let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .allow_unauthenticated()
            .build()
            .context("Failed to build client verifier")?;

        builder
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs, key)?
    } else {
        builder.with_no_client_auth().with_single_cert(certs, key)?
    };

    Ok(config)
}

fn load_certs(path: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_file_iter(path)
        .with_context(|| format!("Failed to open cert file: {path}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to load certs: {e}"))?;

    // Validate certificate expiry for the first (leaf) certificate
    if let Some(cert) = certs.first() {
        if let Ok((_, parsed)) = x509_parser::parse_x509_certificate(cert.as_ref()) {
            let validity = parsed.validity();

            // Fail fast if certificate is expired or not yet valid
            if !validity.is_valid() {
                tracing::error!(
                    "Certificate at {} is EXPIRED or not yet valid (valid from {} to {})",
                    path,
                    validity.not_before,
                    validity.not_after
                );
                return Err(anyhow::anyhow!(
                    "Certificate at {path} is expired or not yet valid"
                ));
            }

            // Warn if expiring within 30 days
            let now = std::time::SystemTime::now();
            if let Ok(duration) = now.duration_since(std::time::UNIX_EPOCH) {
                let now_secs = duration.as_secs();
                let thirty_days_secs = 30 * 24 * 60 * 60;
                let expiry_timestamp = validity.not_after.timestamp() as u64;

                if expiry_timestamp > now_secs && expiry_timestamp < now_secs + thirty_days_secs {
                    tracing::warn!(
                        "Certificate at {} expires soon (at {})",
                        path,
                        validity.not_after
                    );
                }
            }
        }
    }

    Ok(certs)
}

fn load_private_key(path: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    rustls::pki_types::PrivateKeyDer::from_pem_file(path)
        .with_context(|| format!("Failed to load private key: {path}"))
}

pub fn build_cluster_client(config: &Option<ClusterTlsConfig>) -> Result<Client> {
    let mut builder = ClientBuilder::new();

    if let Some(tls) = config {
        if tls.enabled {
            // Load CA
            let mut buf = Vec::new();
            File::open(&tls.ca_cert_path)
                .with_context(|| format!("Failed to open CA cert: {}", tls.ca_cert_path))?
                .read_to_end(&mut buf)?;
            let cert = reqwest::Certificate::from_pem(&buf)?;
            builder = builder.add_root_certificate(cert);

            // Load Client Identity (Cert + Key)
            // We concat them to form a PEM with both
            let mut cert_buf = Vec::new();
            File::open(&tls.client_cert_path)
                .with_context(|| format!("Failed to open client cert: {}", tls.client_cert_path))?
                .read_to_end(&mut cert_buf)?;

            let mut key_buf = Vec::new();
            File::open(&tls.client_key_path)
                .with_context(|| format!("Failed to open client key: {}", tls.client_key_path))?
                .read_to_end(&mut key_buf)?;

            let mut identity_buf = cert_buf;
            identity_buf.push(b'\n');
            identity_buf.extend(key_buf);

            let identity = reqwest::Identity::from_pem(&identity_buf)
                .context("Failed to create identity from cert and key")?;
            builder = builder.identity(identity);
        }
    }

    builder.build().context("Failed to build HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_auth_config(enabled: bool, header: &str, keys: Vec<&str>) -> AuthConfig {
        AuthConfig {
            enabled,
            required_header: header.to_string(),
            allowed_keys: keys.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn test_auth_disabled() {
        let config = make_auth_config(false, "Authorization", vec!["secret"]);
        let headers = HeaderMap::new();
        assert!(check_api_key_auth(Some(&config), &headers).is_ok());
    }

    #[test]
    fn test_auth_none_config() {
        let headers = HeaderMap::new();
        assert!(check_api_key_auth(None, &headers).is_ok());
    }

    #[test]
    fn test_auth_valid_key() {
        let config = make_auth_config(true, "Authorization", vec!["Bearer sk-test123"]);
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer sk-test123".parse().unwrap());
        assert!(check_api_key_auth(Some(&config), &headers).is_ok());
    }

    #[test]
    fn test_auth_invalid_key() {
        let config = make_auth_config(true, "Authorization", vec!["Bearer sk-test123"]);
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer wrong-key".parse().unwrap());
        assert_eq!(
            check_api_key_auth(Some(&config), &headers),
            Err("Unauthorized")
        );
    }

    #[test]
    fn test_auth_missing_key() {
        let config = make_auth_config(true, "Authorization", vec!["Bearer sk-test123"]);
        let headers = HeaderMap::new();
        assert_eq!(
            check_api_key_auth(Some(&config), &headers),
            Err("Unauthorized")
        );
    }

    #[test]
    fn test_auth_custom_header() {
        let config = make_auth_config(true, "X-API-Key", vec!["my-secret-key"]);
        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", "my-secret-key".parse().unwrap());
        assert!(check_api_key_auth(Some(&config), &headers).is_ok());
    }

    #[test]
    fn test_auth_multiple_allowed_keys() {
        let config = make_auth_config(true, "Authorization", vec!["key1", "key2", "key3"]);
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "key2".parse().unwrap());
        assert!(check_api_key_auth(Some(&config), &headers).is_ok());
    }

    #[test]
    fn test_auth_last_allowed_key_matches() {
        // The match is on the last configured key; the constant-time check must
        // not stop early at an earlier non-match.
        let config = make_auth_config(true, "Authorization", vec!["key1", "key2", "key3"]);
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "key3".parse().unwrap());
        assert!(check_api_key_auth(Some(&config), &headers).is_ok());
    }

    #[test]
    fn test_auth_one_byte_off_rejected() {
        let config = make_auth_config(true, "Authorization", vec!["Bearer sk-test123"]);
        let mut headers = HeaderMap::new();
        // Identical length, only the final byte differs.
        headers.insert("Authorization", "Bearer sk-test124".parse().unwrap());
        assert_eq!(
            check_api_key_auth(Some(&config), &headers),
            Err("Unauthorized")
        );
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"sk-secret", b"sk-secret"));
        assert!(constant_time_eq(b"", b""));
        // One byte off, same length.
        assert!(!constant_time_eq(b"sk-secret", b"sk-secreT"));
        // Length mismatches.
        assert!(!constant_time_eq(b"sk-secret", b"sk-secre"));
        assert!(!constant_time_eq(b"sk-secret", b"sk-secret-extra"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn test_allowed_key_matches() {
        let keys = vec!["key1".to_string(), "key2".to_string(), "key3".to_string()];
        assert!(allowed_key_matches(&keys, "key1"));
        assert!(allowed_key_matches(&keys, "key3"));
        assert!(!allowed_key_matches(&keys, "key4"));
        // Prefix of a real key: rejected (length differs).
        assert!(!allowed_key_matches(&keys, "key"));
        // No keys configured: nothing matches.
        assert!(!allowed_key_matches(&[], "anything"));
    }

    #[test]
    fn test_peer_identity_default() {
        let identity = PeerIdentity::default();
        assert!(identity.node_id.is_none());
        assert!(!identity.authenticated);
    }

    #[test]
    fn test_extract_peer_identity_empty() {
        let certs: Vec<rustls::pki_types::CertificateDer<'_>> = vec![];
        let identity = extract_peer_identity(&certs);
        assert!(identity.node_id.is_none());
        assert!(!identity.authenticated);
    }
}
