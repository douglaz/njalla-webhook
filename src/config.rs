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
    /// Maximum number of retries for a transient Njalla API failure (rate
    /// limits, 5xx, network errors). Total attempts = retries + 1.
    pub njalla_max_retries: u32,
    /// Base delay in milliseconds for the exponential backoff between retries.
    pub njalla_retry_base_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let njalla_api_token = env::var("NJALLA_API_TOKEN")
            .expect("NJALLA_API_TOKEN environment variable is required");

        let webhook_host = env::var("WEBHOOK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        let webhook_port = env::var("WEBHOOK_PORT")
            .unwrap_or_else(|_| "8888".to_string())
            .parse::<u16>()?;

        let domain_filter = env::var("DOMAIN_FILTER").ok().map(|s| {
            s.split(',')
                .map(|d| Self::normalize_domain(d.trim()))
                .filter(|d| !d.is_empty())
                .collect()
        });

        let dry_run = env::var("DRY_RUN")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()?;

        let cache_ttl_seconds = env::var("CACHE_TTL_SECONDS")
            .unwrap_or_else(|_| "60".to_string())
            .parse::<u64>()?;

        let njalla_max_retries = env::var("NJALLA_MAX_RETRIES")
            .unwrap_or_else(|_| "3".to_string())
            .parse::<u32>()?;

        let njalla_retry_base_ms = env::var("NJALLA_RETRY_BASE_MS")
            .unwrap_or_else(|_| "500".to_string())
            .parse::<u64>()?;

        Ok(Config {
            njalla_api_token,
            webhook_host,
            webhook_port,
            domain_filter,
            dry_run,
            cache_ttl_seconds,
            njalla_max_retries,
            njalla_retry_base_ms,
        })
    }

    pub fn is_domain_allowed(&self, domain: &str) -> bool {
        match &self.domain_filter {
            Some(filter) => {
                let domain = Self::normalize_domain(domain);
                filter
                    .iter()
                    .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
            }
            None => true,
        }
    }

    /// Canonicalize a domain name: strip trailing dot and lowercase.
    pub(crate) fn normalize_domain(s: &str) -> String {
        s.strip_suffix('.').unwrap_or(s).to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_filter(domains: Vec<&str>) -> Config {
        Config {
            njalla_api_token: String::new(),
            webhook_host: String::new(),
            webhook_port: 8888,
            domain_filter: Some(domains.into_iter().map(Config::normalize_domain).collect()),
            dry_run: false,
            cache_ttl_seconds: 60,
            njalla_max_retries: 3,
            njalla_retry_base_ms: 500,
        }
    }

    #[test]
    fn exact_match_is_allowed() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(config.is_domain_allowed("example.com"));
    }

    #[test]
    fn proper_subdomain_is_allowed() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(config.is_domain_allowed("www.example.com"));
    }

    #[test]
    fn nested_subdomain_is_allowed() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(config.is_domain_allowed("sub.deep.example.com"));
    }

    #[test]
    fn sibling_domain_is_rejected() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(!config.is_domain_allowed("badexample.com"));
    }

    #[test]
    fn another_sibling_domain_is_rejected() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(!config.is_domain_allowed("notexample.com"));
    }

    #[test]
    fn case_insensitive_match() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(config.is_domain_allowed("WWW.Example.COM"));
    }

    #[test]
    fn trailing_dot_on_domain() {
        let config = config_with_filter(vec!["example.com"]);
        assert!(config.is_domain_allowed("www.example.com."));
    }

    #[test]
    fn trailing_dot_on_filter() {
        let config = config_with_filter(vec!["example.com."]);
        assert!(config.is_domain_allowed("www.example.com"));
    }

    #[test]
    fn none_filter_allows_all() {
        let config = Config {
            njalla_api_token: String::new(),
            webhook_host: String::new(),
            webhook_port: 8888,
            domain_filter: None,
            dry_run: false,
            cache_ttl_seconds: 60,
            njalla_max_retries: 3,
            njalla_retry_base_ms: 500,
        };
        assert!(config.is_domain_allowed("anything.com"));
    }
}
