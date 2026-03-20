use crate::config::{AuthConfig, ClusterTlsConfig, ServerTlsConfig};
use anyhow::{Context, Result};
use axum::http::HeaderMap;
use reqwest::{Client, ClientBuilder};
use rustls::ServerConfig;
use std::fs::File;
use std::io::{BufReader, Read};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct PeerIdentity {
    pub node_id: Option<String>,
    pub authenticated: bool,
}

/// Check API key authentication against the provided config and headers.
/// Returns Ok(()) if authentication succeeds or is not required.
/// Returns Err("Unauthorized") if authentication fails.
pub fn check_api_key_auth(
    auth_config: Option<&AuthConfig>,
    headers: &HeaderMap,
) -> Result<(), &'static str> {
    if let Some(config) = auth_config {
        if config.enabled {
            let api_key = config.get_header_value(headers);
            let authorized = match api_key {
                Some(key) => config.allowed_keys.contains(&key.to_string()),
                None => false,
            };
            if !authorized {
                return Err("Unauthorized");
            }
        }
    }
    Ok(())
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
    let file = File::open(path).with_context(|| format!("Failed to open cert file: {}", path))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Failed to load certs: {}", e))?;

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
                    "Certificate at {} is expired or not yet valid",
                    path
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
    let file = File::open(path).with_context(|| format!("Failed to open key file: {}", path))?;
    let mut reader = BufReader::new(file);

    for item in rustls_pemfile::read_all(&mut reader) {
        match item? {
            rustls_pemfile::Item::Pkcs1Key(key) => return Ok(key.into()),
            rustls_pemfile::Item::Pkcs8Key(key) => return Ok(key.into()),
            rustls_pemfile::Item::Sec1Key(key) => return Ok(key.into()),
            _ => {}
        }
    }

    Err(anyhow::anyhow!("No private key found in {}", path))
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
