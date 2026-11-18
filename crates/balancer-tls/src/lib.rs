use anyhow::{bail, Context, Result};
use axum::Router;
use axum_server::{tls_rustls::RustlsConfig, Handle};
use balancer_config::TlsConfig;
use futures_util::StreamExt;
use rustls::{server::{ResolvesServerCertUsingSni, ResolvesServerCert, ClientHello}, sign::CertifiedKey, ServerConfig};
use rustls_acme::{caches::DirCache, AcmeConfig};
use std::{collections::HashSet, fs::File, io::BufReader, net::SocketAddr, path::Path, sync::Arc};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn server_config(resolver: Arc<dyn ResolvesServerCert>) -> Result<Arc<ServerConfig>> {
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn manual_config(tls: &TlsConfig) -> Result<Arc<ServerConfig>> {
    let mut resolver = ResolvesServerCertUsingSni::new();
    for cert in &tls.certificates {
        let mut chain = BufReader::new(File::open(&cert.cert)
            .with_context(|| format!("open certificate {}", cert.cert.display()))?);
        let certs = rustls_pemfile::certs(&mut chain).collect::<std::io::Result<Vec<_>>>()?;
        if certs.is_empty() { bail!("empty certificate chain: {}", cert.cert.display()); }
        let mut key = BufReader::new(File::open(&cert.key)
            .with_context(|| format!("open private key {}", cert.key.display()))?);
        let key = rustls_pemfile::private_key(&mut key)?.context("no PEM private key found")?;
        let certified = CertifiedKey::from_der(certs, key, &provider())
            .context("invalid certificate/private key pair")?;
        for domain in &cert.domains {
            resolver.add(domain, certified.clone()).with_context(|| format!("certificate does not cover {domain}"))?;
        }
    }
    server_config(Arc::new(resolver))
}

// The ACME resolver can share one SAN certificate across domain names.
// Reject missing or unknown SNI for ordinary HTTPS handshakes.
#[derive(Debug)]
struct AllowedNames {
    names: HashSet<String>,
    resolver: Arc<dyn ResolvesServerCert>,
}

impl ResolvesServerCert for AllowedNames {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        if !self.names.contains(&name) { return None; }
        self.resolver.resolve(hello)
    }
}

fn prepare_cache(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path).context("create ACME state directory")?;
    }
    let meta = std::fs::symlink_metadata(path).context("inspect ACME state directory")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("ACME cache must be a real directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            bail!("ACME state directory must be private (chmod 700)");
        }
    }
    Ok(())
}

/// Validate local PEM files without starting a listener or contacting an ACME CA.
pub fn validate(tls: &TlsConfig) -> Result<()> {
    if tls.acme.is_none() { manual_config(tls)?; }
    Ok(())
}

pub fn manual_handle(tls: &TlsConfig) -> Result<Option<RustlsConfig>> {
    if tls.acme.is_some() { Ok(None) }
    else { Ok(Some(RustlsConfig::from_config(manual_config(tls)?))) }
}

pub fn reload_manual(tls: &TlsConfig, handle: &RustlsConfig) -> Result<()> {
    if tls.acme.is_some() { bail!("ACME certificates renew automatically"); }
    let config = manual_config(tls)?;
    handle.reload_from_config(config);
    Ok(())
}

pub async fn serve(tls: TlsConfig, app: Router, handle: Handle<SocketAddr>, manual: Option<RustlsConfig>) -> Result<()> {
    let listener = std::net::TcpListener::bind(tls.listen).context("bind HTTPS listener")?;
    listener.set_nonblocking(true)?;
    tracing::info!(address = %listener.local_addr()?, "HTTPS listener bound");
    if let Some(acme) = tls.acme {
        prepare_cache(&acme.cache_dir)?;
        let mut state = AcmeConfig::new(acme.domains.clone())
            .contact([format!("mailto:{}", acme.email)])
            .cache(DirCache::new(acme.cache_dir))
            .directory_lets_encrypt(acme.production)
            .state();
        let resolver = AllowedNames {
            names: acme.domains.into_iter().collect(),
            resolver: state.resolver(),
        };
        let acceptor = state.axum_acceptor(server_config(Arc::new(resolver))?);
        let worker = tokio::spawn(async move {
            while let Some(event) = state.next().await {
                match event {
                    Ok(event) => tracing::info!(?event, "ACME certificate event"),
                    Err(error) => tracing::warn!(?error, "ACME certificate error"),
                }
            }
        });
        // Abort the ACME task if the listener is dropped or returns.
        struct Worker(tokio::task::JoinHandle<()>);
        impl Drop for Worker {
            fn drop(&mut self) { self.0.abort(); }
        }
        let _worker = Worker(worker);
        tracing::info!(production = acme.production, "ACME TLS-ALPN-01 enabled");
        axum_server::from_tcp(listener)?.acceptor(acceptor).handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>()).await
            .context("serve ACME HTTPS")
    } else {
        let config = manual.context("missing manual TLS handle")?;
        axum_server::from_tcp_rustls(listener, config)?.handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>()).await
            .context("serve manual HTTPS")
    }
}
