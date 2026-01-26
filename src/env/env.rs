use anyhow::Result;
use std::env;

/// Structured environment configuration for the DKG + Signing servers.
#[derive(Debug, Clone)]
pub struct BaseEnv {
    pub n: u16,
    pub node_id: u64,
    pub redis_url: String,
    pub default_session: String,
}

impl BaseEnv {
    /// Load all env variables and apply safe defaults.
    pub fn load() -> Result<Self> {
        Ok(Self {
            n: env::var("N").unwrap_or_else(|_| "2".into()).parse()?,
            node_id: env::var("NODE_ID").unwrap_or_else(|_| "0".into()).parse()?,
            redis_url: env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".into()),
            default_session: env::var("DEFAULT_SESSION_ID")
                .unwrap_or_else(|_| "session-001".into()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ServerEnv {
    pub base: BaseEnv,
    pub dkg_addr: String,
    pub sign_addr: String,
     pub auditor_dkg_addr: String,
}

impl ServerEnv {
    pub fn load() -> Result<Self> {
        Ok(Self {
            base: BaseEnv::load()?,
            dkg_addr: env::var("DKG_SERVER_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:7001".into()),
            sign_addr: env::var("SIGN_SERVER_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:7002".into()),
                auditor_dkg_addr: std::env::var("AUDITOR_DKG_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:7003".into()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ClientEnv {
    pub base: BaseEnv,
    pub dkg_server_addr: String,
    pub sign_server_addr: String,
    pub auditor_dkg_server_addr: String,
}

impl ClientEnv {
    pub fn load() -> Result<Self> {
        Ok(Self {
            base: BaseEnv::load()?,
            dkg_server_addr: env::var("DKG_SERVER_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:7001".into()),
            sign_server_addr: env::var("SIGN_SERVER_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:7002".into()),
                auditor_dkg_server_addr: std::env::var("AUDITOR_DKG_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:7003".into()),
        })
    }
}
