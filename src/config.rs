use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub njalla_api_token: String,
    pub webhook_host: String,
    pub webhook_port: u16,
    pub domain_filter: Option<Vec<String>>,
    pub dry_run: bool,
    pub cache_ttl_seconds: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let njalla_api_token = env::var("NJALLA_API_TOKEN")
            .expect("NJALLA_API_TOKEN environment variable is required");

        let webhook_host = env::var("WEBHOOK_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string());

        let webhook_port = env::var("WEBHOOK_PORT")
            .unwrap_or_else(|_| "8888".to_string())
            .parse::<u16>()?;

        let domain_filter = env::var("DOMAIN_FILTER").ok().map(|s| {
            s.split(',')
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty())
                .collect()
        });

        let dry_run = env::var("DRY_RUN")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()?;

        let cache_ttl_seconds = env::var("CACHE_TTL_SECONDS")
            .unwrap_or_else(|_| "60".to_string())
            .parse::<u64>()?;

        Ok(Config {
            njalla_api_token,
            webhook_host,
            webhook_port,
            domain_filter,
            dry_run,
            cache_ttl_seconds,
        })
    }

    pub fn is_domain_allowed(&self, domain: &str) -> bool {
        match &self.domain_filter {
            Some(filter) => filter.iter().any(|d| domain.ends_with(d)),
            None => true,
        }
    }
}