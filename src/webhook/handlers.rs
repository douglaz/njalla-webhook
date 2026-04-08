use super::types::*;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::njalla::{self, Client as NjallaClient, Domain, DomainLister};
use axum::{extract::Query, http::StatusCode, Json};
use std::sync::Arc;
use tracing::{debug, error, info};

pub struct WebhookHandler {
    njalla_client: Arc<NjallaClient>,
    domain_lister: Arc<dyn DomainLister>,
    config: Config,
}

impl WebhookHandler {
    pub fn new(njalla_client: Arc<NjallaClient>, config: Config) -> Self {
        let domain_lister = njalla_client.clone() as Arc<dyn DomainLister>;
        Self {
            njalla_client,
            domain_lister,
            config,
        }
    }

    pub async fn health(&self) -> Result<Json<HealthResponse>> {
        Ok(Json(HealthResponse {
            status: "healthy".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    pub async fn ready(&self) -> Result<Json<HealthResponse>> {
        // Check if we can connect to Njalla API
        let domains = self.njalla_client.list_domains().await?;
        info!("Ready check: found {} domains", domains.len());

        Ok(Json(HealthResponse {
            status: "ready".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    pub async fn negotiate(&self) -> Result<impl axum::response::IntoResponse> {
        // External-DNS expects negotiation endpoint to return domain filters
        // with proper content type header
        let filters = self.config.domain_filter.clone().unwrap_or_default();

        Ok((
            StatusCode::OK,
            [(
                "content-type",
                "application/external.dns.webhook+json;version=1",
            )],
            Json(serde_json::json!({
                "filters": filters
            })),
        ))
    }

    pub async fn get_records(&self, query: Query<GetRecordsQuery>) -> Result<Json<Vec<Endpoint>>> {
        // If zone_name is provided, get records for that specific zone
        if let Some(zone_name) = query.zone_name.as_ref() {
            info!("Getting records for zone: {}", zone_name);

            // Check if domain is allowed
            if !self.config.is_domain_allowed(zone_name) {
                return Err(Error::DomainNotAllowed(zone_name.to_string()));
            }

            // Fetch records from Njalla
            let records = self.njalla_client.list_records(zone_name).await?;

            // Convert Njalla records to external-dns endpoints
            let endpoints: Vec<Endpoint> = records
                .iter()
                .filter(|r| {
                    // Filter out records that external-dns doesn't handle
                    matches!(
                        r.record_type.as_str(),
                        "A" | "AAAA" | "CNAME" | "TXT" | "MX" | "SRV"
                    )
                })
                .map(|r| Endpoint::from_njalla_record(r, zone_name))
                .collect();

            info!(
                "Returning {} endpoints for zone {}",
                endpoints.len(),
                zone_name
            );

            Ok(Json(endpoints))
        } else {
            // No zone specified - return records for all configured domains
            info!("Getting records for all configured domains");

            let domains = if let Some(ref domain_filter) = self.config.domain_filter {
                // Use configured domain filter
                info!("Using configured domain filter: {:?}", domain_filter);
                domain_filter.clone()
            } else {
                // List all domains from Njalla API
                info!("Fetching all domains from Njalla API");
                self.njalla_client
                    .list_domains()
                    .await?
                    .into_iter()
                    .map(|d| d.name)
                    .collect()
            };

            let mut all_endpoints = Vec::new();

            for domain in &domains {
                info!("Fetching records for domain: {}", domain);

                match self.njalla_client.list_records(domain).await {
                    Ok(records) => {
                        let endpoints: Vec<Endpoint> = records
                            .iter()
                            .filter(|r| {
                                matches!(
                                    r.record_type.as_str(),
                                    "A" | "AAAA" | "CNAME" | "TXT" | "MX" | "SRV"
                                )
                            })
                            .map(|r| Endpoint::from_njalla_record(r, domain))
                            .collect();

                        info!("Found {} endpoints for domain {}", endpoints.len(), domain);
                        all_endpoints.extend(endpoints);
                    }
                    Err(e) => {
                        error!("Failed to fetch records for domain {}: {}", domain, e);
                        // Continue with other domains even if one fails
                    }
                }
            }

            info!("Returning {} total endpoints", all_endpoints.len());
            Ok(Json(all_endpoints))
        }
    }

    pub async fn apply_changes(
        &self,
        Json(request): Json<ApplyChangesRequest>,
    ) -> Result<StatusCode> {
        let changes = request.into_changes();
        info!(
            "Applying changes: {} creates, {} updates, {} deletes",
            changes.create.len(),
            changes.update_new.len(),
            changes.delete.len()
        );

        // Log the actual changes being requested
        for endpoint in &changes.create {
            info!(
                "CREATE: {} -> {}",
                endpoint.dns_name,
                endpoint.targets.join(", ")
            );
        }
        for (old, new) in changes.update_old.iter().zip(changes.update_new.iter()) {
            info!(
                "UPDATE: {} from {} to {}",
                new.dns_name,
                old.targets.join(", "),
                new.targets.join(", ")
            );
        }
        for endpoint in &changes.delete {
            info!(
                "DELETE: {} -> {}",
                endpoint.dns_name,
                endpoint.targets.join(", ")
            );
        }

        if changes.is_empty() {
            info!("No changes to apply");
            return Ok(StatusCode::NO_CONTENT);
        }

        // Pre-fetch owned domains once for the entire batch when no domain filter is set.
        let owned_domains = if self.config.domain_filter.is_none() {
            match self.domain_lister.list_domains().await {
                Ok(domains) => Some(domains),
                Err(e) => {
                    tracing::warn!(
                        "Pre-fetch of owned domains failed, will retry per-endpoint: {}",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
        let owned_domains_ref = owned_domains.as_deref();

        let mut applied_count = 0;
        let mut errors = Vec::new();

        // Process deletions first
        for endpoint in &changes.delete {
            if let Err(e) = self.delete_endpoint(endpoint, owned_domains_ref).await {
                error!("Failed to delete endpoint {}: {}", endpoint.dns_name, e);
                errors.push(format!("Delete {}: {}", endpoint.dns_name, e));
            } else {
                applied_count += 1;
            }
        }

        // Process updates (delete old, create new)
        for (old, new) in changes.update_old.iter().zip(changes.update_new.iter()) {
            if let Err(e) = self.update_endpoint(old, new, owned_domains_ref).await {
                error!("Failed to update endpoint {}: {}", new.dns_name, e);
                errors.push(format!("Update {}: {}", new.dns_name, e));
            } else {
                applied_count += 1;
            }
        }

        // Process creations
        for endpoint in &changes.create {
            if let Err(e) = self.create_endpoint(endpoint, owned_domains_ref).await {
                error!("Failed to create endpoint {}: {}", endpoint.dns_name, e);
                errors.push(format!("Create {}: {}", endpoint.dns_name, e));
            } else {
                applied_count += 1;
            }
        }

        if errors.is_empty() {
            info!("Successfully applied {} changes", applied_count);
            Ok(StatusCode::NO_CONTENT)
        } else {
            error!(
                "Applied {} changes with {} errors: {:?}",
                applied_count,
                errors.len(),
                errors
            );
            Err(Error::Internal(format!(
                "Partial failure: {} succeeded, {} failed: {:?}",
                applied_count,
                errors.len(),
                errors
            )))
        }
    }

    pub async fn adjust_endpoints(
        &self,
        Json(endpoints): Json<Vec<Endpoint>>,
    ) -> Result<Json<Vec<Endpoint>>> {
        // This is optional - we just return the endpoints as-is
        debug!("Adjusting {} endpoints", endpoints.len());
        Ok(Json(endpoints))
    }

    // Helper methods for record operations

    async fn create_endpoint(
        &self,
        endpoint: &Endpoint,
        owned_domains: Option<&[Domain]>,
    ) -> Result<()> {
        let zone = self.extract_zone(&endpoint.dns_name, owned_domains).await?;

        if !self.config.is_domain_allowed(&zone) {
            return Err(Error::DomainNotAllowed(zone));
        }

        let name = self.extract_record_name(&endpoint.dns_name, &zone);

        for target in &endpoint.targets {
            let priority = endpoint
                .provider_specific
                .iter()
                .find(|ps| ps.name == "priority")
                .and_then(|ps| ps.value.parse().ok());

            let request = njalla::AddRecordRequest {
                domain: zone.clone(),
                name: name.clone(),
                record_type: endpoint.record_type.clone(),
                content: target.clone(),
                ttl: endpoint.record_ttl.unwrap_or(3600) as u32,
                priority,
            };

            if self.config.dry_run {
                info!("DRY RUN: Would create record: {:?}", request);
            } else {
                self.njalla_client.add_record(request).await?;
            }
        }

        Ok(())
    }

    async fn update_endpoint(
        &self,
        old: &Endpoint,
        new: &Endpoint,
        owned_domains: Option<&[Domain]>,
    ) -> Result<()> {
        // For simplicity, delete old and create new
        self.delete_endpoint(old, owned_domains).await?;
        self.create_endpoint(new, owned_domains).await?;
        Ok(())
    }

    async fn delete_endpoint(
        &self,
        endpoint: &Endpoint,
        owned_domains: Option<&[Domain]>,
    ) -> Result<()> {
        let zone = self.extract_zone(&endpoint.dns_name, owned_domains).await?;

        if !self.config.is_domain_allowed(&zone) {
            return Err(Error::DomainNotAllowed(zone));
        }

        // Find matching records
        let records = self.njalla_client.list_records(&zone).await?;
        let name = self.extract_record_name(&endpoint.dns_name, &zone);

        for record in records {
            let record_name = if record.name.is_empty() || record.name == "@" {
                "".to_string()
            } else {
                record.name.clone()
            };

            if record_name == name
                && record.record_type == endpoint.record_type
                && endpoint.targets.contains(&record.content)
            {
                let request = njalla::RemoveRecordRequest {
                    domain: zone.clone(),
                    id: record.id,
                };

                if self.config.dry_run {
                    info!("DRY RUN: Would delete record: {:?}", request);
                } else {
                    self.njalla_client.remove_record(request).await?;
                }
            }
        }

        Ok(())
    }

    async fn extract_zone(
        &self,
        dns_name: &str,
        prefetched_domains: Option<&[Domain]>,
    ) -> Result<String> {
        // Normalize dns_name; filter entries are already canonical from Config::from_env
        let normalized_name = dns_name
            .strip_suffix('.')
            .unwrap_or(dns_name)
            .to_ascii_lowercase();

        // Stage 1: DOMAIN_FILTER-set path — check configured domains
        if let Some(ref domains) = self.config.domain_filter {
            for domain in domains {
                if normalized_name == *domain || normalized_name.ends_with(&format!(".{}", domain))
                {
                    return Ok(domain.clone());
                }
            }

            // Filter is set but no match — fall back to naive two-label derivation
            let parts: Vec<&str> = normalized_name.split('.').collect();
            if parts.len() >= 2 {
                return Ok(format!(
                    "{}.{}",
                    parts[parts.len() - 2],
                    parts[parts.len() - 1]
                ));
            } else {
                return Err(Error::InvalidRequest(format!(
                    "Cannot extract zone from {}",
                    dns_name
                )));
            }
        }

        // Stage 2: No filter set — use pre-fetched domains or query Njalla
        let fetched;
        let owned_domains = match prefetched_domains {
            Some(domains) => domains,
            None => {
                fetched = self.domain_lister.list_domains().await?;
                &fetched
            }
        };

        let mut best_match: Option<&str> = None;

        for domain in owned_domains {
            let d = &domain.name;
            let d_lower = d.to_ascii_lowercase();
            let is_match =
                normalized_name == d_lower || normalized_name.ends_with(&format!(".{}", d_lower));
            if is_match {
                match best_match {
                    Some(current) if d.len() <= current.len() => {}
                    _ => best_match = Some(d.as_str()),
                }
            }
        }

        match best_match {
            Some(zone) => Ok(zone.to_string()),
            None => Err(Error::InvalidRequest(format!(
                "No owned domain matches {}",
                dns_name
            ))),
        }
    }

    fn extract_record_name(&self, dns_name: &str, zone: &str) -> String {
        // Normalize dns_name to match the canonical zone returned by extract_zone
        let normalized = dns_name
            .strip_suffix('.')
            .unwrap_or(dns_name)
            .to_ascii_lowercase();
        if normalized == zone {
            "".to_string()
        } else if normalized.ends_with(&format!(".{}", zone)) {
            normalized[..normalized.len() - zone.len() - 1].to_string()
        } else {
            normalized
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_handler() -> WebhookHandler {
        let config = Config {
            njalla_api_token: "dummy-token".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8888,
            domain_filter: Some(vec![Config::normalize_domain("example.com")]),
            dry_run: true,
            cache_ttl_seconds: 60,
        };

        let client = Arc::new(NjallaClient::new("dummy-token").expect("client should build"));
        WebhookHandler::new(client, config)
    }

    struct MockDomainLister {
        domains: Vec<Domain>,
    }

    #[async_trait::async_trait]
    impl DomainLister for MockDomainLister {
        async fn list_domains(&self) -> crate::error::Result<Vec<Domain>> {
            Ok(self.domains.clone())
        }
    }

    struct PanickingDomainLister;

    #[async_trait::async_trait]
    impl DomainLister for PanickingDomainLister {
        async fn list_domains(&self) -> crate::error::Result<Vec<Domain>> {
            panic!("list_domains should not be called when domain filter is set");
        }
    }

    fn handler_with_filter(domains: Vec<&str>) -> WebhookHandler {
        let config = Config {
            njalla_api_token: "dummy-token".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8888,
            domain_filter: Some(domains.into_iter().map(Config::normalize_domain).collect()),
            dry_run: true,
            cache_ttl_seconds: 60,
        };
        let client = Arc::new(NjallaClient::new("dummy-token").expect("client should build"));
        WebhookHandler::new(client, config)
    }

    fn handler_with_mock_domains(domains: Vec<&str>) -> WebhookHandler {
        let config = Config {
            njalla_api_token: "dummy-token".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8888,
            domain_filter: None,
            dry_run: true,
            cache_ttl_seconds: 60,
        };

        let mock_lister = Arc::new(MockDomainLister {
            domains: domains
                .into_iter()
                .map(|name| Domain {
                    name: name.to_string(),
                    status: "active".to_string(),
                    expiry: None,
                })
                .collect(),
        });

        let client = Arc::new(NjallaClient::new("dummy-token").expect("client should build"));
        WebhookHandler {
            njalla_client: client,
            domain_lister: mock_lister,
            config,
        }
    }

    #[tokio::test]
    async fn extract_zone_returns_canonical_zone() {
        let handler = test_handler();
        let zone = handler.extract_zone("www.example.com", None).await.unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn extract_zone_mixed_case_filter() {
        let handler = handler_with_filter(vec!["Example.COM"]);
        let zone = handler.extract_zone("www.example.com", None).await.unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn extract_zone_trailing_dot_filter() {
        let handler = handler_with_filter(vec!["example.com."]);
        let zone = handler.extract_zone("www.example.com", None).await.unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn extract_zone_trailing_dot_dns_name() {
        let handler = test_handler();
        let zone = handler
            .extract_zone("www.example.com.", None)
            .await
            .unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn extract_zone_fallback_strips_trailing_dot() {
        let handler = handler_with_filter(vec!["other.com"]);
        let zone = handler
            .extract_zone("www.fallback.org.", None)
            .await
            .unwrap();
        assert_eq!(zone, "fallback.org");
    }

    #[tokio::test]
    async fn extract_record_name_with_mixed_case() {
        let handler = handler_with_filter(vec!["Example.COM"]);
        let zone = handler.extract_zone("WWW.Example.COM", None).await.unwrap();
        let name = handler.extract_record_name("WWW.Example.COM", &zone);
        assert_eq!(name, "www");
    }

    #[tokio::test]
    async fn extract_record_name_with_trailing_dot() {
        let handler = test_handler();
        let zone = handler
            .extract_zone("app.example.com.", None)
            .await
            .unwrap();
        let name = handler.extract_record_name("app.example.com.", &zone);
        assert_eq!(name, "app");
    }

    #[tokio::test]
    async fn extract_record_name_exact_zone_match() {
        let handler = test_handler();
        let zone = handler.extract_zone("example.com", None).await.unwrap();
        let name = handler.extract_record_name("example.com", &zone);
        assert_eq!(name, "");
    }

    #[tokio::test]
    async fn extract_zone_multi_label_tld_returns_longest_match() {
        let handler = handler_with_mock_domains(vec!["co.uk", "example.co.uk"]);
        let zone = handler
            .extract_zone("app.example.co.uk", None)
            .await
            .unwrap();
        assert_eq!(zone, "example.co.uk");
    }

    #[tokio::test]
    async fn extract_zone_case_insensitive_match() {
        let handler = handler_with_mock_domains(vec!["example.com"]);
        let zone = handler.extract_zone("App.Example.COM", None).await.unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn extract_zone_no_match_returns_error() {
        let handler = handler_with_mock_domains(vec!["example.com"]);
        let result = handler.extract_zone("unknown.org", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn extract_zone_with_domain_filter_does_not_call_list_domains() {
        let config = Config {
            njalla_api_token: "dummy-token".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8888,
            domain_filter: Some(vec!["example.com".to_string()]),
            dry_run: true,
            cache_ttl_seconds: 60,
        };
        let client = Arc::new(NjallaClient::new("dummy-token").expect("client should build"));
        let handler = WebhookHandler {
            njalla_client: client,
            domain_lister: Arc::new(PanickingDomainLister),
            config,
        };
        let zone = handler.extract_zone("app.example.com", None).await.unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn apply_changes_returns_error_on_partial_failure() {
        let handler = test_handler();
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "create": [
                {
                    "dnsName": "app.example.com",
                    "targets": ["192.0.2.10"],
                    "recordType": "A"
                },
                {
                    "dnsName": "app.blocked.com",
                    "targets": ["192.0.2.11"],
                    "recordType": "A"
                }
            ]
        }))
        .expect("payload should deserialize");

        let err = handler
            .apply_changes(Json(request))
            .await
            .expect_err("partial failure should return error");
        assert!(
            matches!(err, Error::Internal(_)),
            "expected Error::Internal, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn apply_changes_returns_no_content_on_empty_changes() {
        let handler = test_handler();
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "create": [],
            "updateOld": [],
            "updateNew": [],
            "delete": []
        }))
        .expect("payload should deserialize");

        let status = handler
            .apply_changes(Json(request))
            .await
            .expect("empty changes should succeed");
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn apply_changes_returns_error_when_all_operations_fail() {
        let handler = test_handler();
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "create": [
                {
                    "dnsName": "app.blocked.com",
                    "targets": ["192.0.2.10"],
                    "recordType": "A"
                },
                {
                    "dnsName": "app.other.com",
                    "targets": ["192.0.2.11"],
                    "recordType": "A"
                }
            ]
        }))
        .expect("payload should deserialize");

        let err = handler
            .apply_changes(Json(request))
            .await
            .expect_err("all-disallowed should return error");
        assert!(
            matches!(err, Error::Internal(_)),
            "expected Error::Internal, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn apply_changes_accepts_external_dns_payload_and_returns_no_content() {
        let handler = test_handler();
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "create": [
                {
                    "dnsName": "app.example.com",
                    "targets": ["192.0.2.10"],
                    "recordType": "A"
                }
            ]
        }))
        .expect("payload should deserialize");

        let status = handler
            .apply_changes(Json(request))
            .await
            .expect("request should succeed");

        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}
