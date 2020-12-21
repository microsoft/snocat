use serde::{Serialize, Deserialize};

fn bound_check_path(s: &str) -> Result<std::path::PathBuf, anyhow::Error> {
  let cert_path_len = s.len();
  if cert_path_len == 0 {
    return Err(anyhow::Error::msg("Certificate path"))
  }
  if cert_path_len > (std::u8::MAX as usize) {
    return Err(anyhow::Error::msg("Certificate path too long"))
  }
  Ok(std::path::PathBuf::from(s))
}

#[repr(C)]
#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct QuinnTransportConfig {
  pub idle_timeout_ms: u32,
  pub keep_alive_interval_ms: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct ServerConfig {
  allow_migration: bool,
  certificate_path: String,
  certificate_key_path: String,
  transport_config: QuinnTransportConfig,
}

impl std::convert::TryInto<quinn::ServerConfig> for self::ServerConfig {
  type Error = anyhow::Error;

  fn try_into(self) -> Result<quinn::ServerConfig, Self::Error> {
    use anyhow::Context;
    use quinn::{ServerConfig, ServerConfigBuilder, TransportConfig, CertificateChain, PrivateKey};
    use std::sync::Arc;
    use crate::util;

    let cert_path = bound_check_path(&self.certificate_path)?;
    let key_path = bound_check_path(&self.certificate_key_path)?;

    let cert_pem = std::fs::read(&cert_path).context("Failed reading cert file")?;
    let priv_pem = std::fs::read(&key_path).context("Failed reading private key file")?;
    let priv_key =
      PrivateKey::from_pem(&priv_pem).context("Quinn .pem parsing of private key failed")?;

    // Building a quinn config is a little unintuitive, but this manages it
    let mut config = ServerConfigBuilder::default();
    config.use_stateless_retry(true);
    let mut transport_config = TransportConfig::default();
    transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport_config
      .max_idle_timeout(Some(std::time::Duration::from_secs(30)))
      .unwrap();
    let mut server_config = ServerConfig::default();
    server_config.transport = Arc::new(transport_config);
    server_config.migration(true);
    let mut cfg_builder = ServerConfigBuilder::new(server_config);
    cfg_builder.protocols(util::ALPN_QUIC_HTTP);
    cfg_builder.enable_keylog();
    let cert_chain = CertificateChain::from_pem(&cert_pem)?;
    cfg_builder.certificate(cert_chain, priv_key)?;
    Ok(cfg_builder.build())
  }
}
