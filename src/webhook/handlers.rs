use super::types::*;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::njalla::{self, Client as NjallaClient, Domain, DomainLister};
use axum::{extract::Query, http::StatusCode, Json};
use std::{sync::Arc, time::Duration};
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
        let fetch = self.fetch_records(query);
        let timeout_secs = self.config.get_records_timeout_secs;
        if timeout_secs == 0 {
            return fetch.await;
        }

        tokio::time::timeout(Duration::from_secs(timeout_secs), fetch)
            .await
            .map_err(|_| {
                error!(
                    "GET /records exceeded its deadline of {} seconds",
                    timeout_secs
                );
                Error::GatewayTimeout(format!(
                    "GET /records exceeded its deadline of {timeout_secs} seconds"
                ))
            })?
    }

    async fn fetch_records(&self, query: Query<GetRecordsQuery>) -> Result<Json<Vec<Endpoint>>> {
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
                        return Err(Error::ServiceUnavailable(format!(
                            "Failed to fetch records for domain {domain}: {e}"
                        )));
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

        // Fetch existing records once so creation is idempotent. Njalla's
        // `add-record` always appends a new record (there is no upsert and no
        // uniqueness constraint), so a redundant CREATE — which external-dns
        // emits whenever its view of current records is missing this entry
        // (registry TXT format change, a transient list failure, a restart) —
        // silently produces a duplicate. Repeated over many reconciliations
        // this floods the zone with thousands of copies and eventually makes
        // the authoritative nameservers fail to serve it (SERVFAIL).
        // Skipped in dry-run, which must not touch the API at all.
        let existing = if self.config.dry_run {
            Vec::new()
        } else {
            self.njalla_client.list_records(&zone).await?
        };

        for target in &endpoint.targets {
            // Skip if an identical record (same name/type/content) already
            // exists. Name normalization mirrors `delete_endpoint`.
            let already_exists = existing.iter().any(|record| {
                let record_name = if record.name.is_empty() || record.name == "@" {
                    ""
                } else {
                    record.name.as_str()
                };
                record_name == name.as_str()
                    && record.record_type == endpoint.record_type
                    && record.content == *target
            });

            if already_exists {
                info!(
                    "Record already exists, skipping create: {} {} -> {}",
                    endpoint.record_type, endpoint.dns_name, target
                );
                continue;
            }

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
            // Pick the LONGEST matching zone among an exact match, a real subdomain (`.{d}`), or
            // external-dns's affixed apex registry-TXT shape (`_externaldns.a-<zone>`). Longest
            // wins so a nested zone resolves to itself — e.g. `_externaldns.a-sub.example.com`
            // -> `sub.example.com`, not its parent `example.com`. `is_affixed_apex_of` only fires
            // when the type marker isn't the leftmost label, so a real host (`a-example.com`,
            // `cname-foo.example.com`) is matched as itself / its parent, never as an affix.
            if let Some(domain) = domains
                .iter()
                .filter(|d| {
                    let d = d.as_str();
                    normalized_name == d
                        || normalized_name.ends_with(&format!(".{d}"))
                        || is_affixed_apex_of(&normalized_name, d)
                })
                .max_by_key(|d| d.len())
            {
                return Ok(domain.clone());
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

        // Longest matching zone among exact / subdomain / affixed-apex — same as the filter path.
        let best = owned_domains
            .iter()
            .filter(|dom| {
                let d = dom.name.to_ascii_lowercase();
                normalized_name == d
                    || normalized_name.ends_with(&format!(".{d}"))
                    || is_affixed_apex_of(&normalized_name, &d)
            })
            .max_by_key(|dom| dom.name.len());

        match best {
            Some(dom) => Ok(dom.name.clone()),
            None => Err(Error::InvalidRequest(format!(
                "No owned domain matches {dns_name}"
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

/// True when `name` is external-dns's affixed registry TXT for the APEX of `zone` — i.e.
/// `<prefix><type>-<zone>` (e.g. `_externaldns.a-whathefolk.com` for zone `whathefolk.com`). The
/// `<type>-` record-type marker fuses onto the zone's leftmost label, so the normal `.{zone}`
/// suffix check misses it. The match is constrained to a real DNS record-type marker so a sibling
/// domain like `api-example.com` is NOT misclassified as belonging to `example.com`.
fn is_affixed_apex_of(name: &str, zone: &str) -> bool {
    const AFFIX_TYPES: &[&str] = &[
        "a", "aaaa", "cname", "txt", "ns", "ptr", "srv", "mx", "naptr", "soa", "caa",
    ];
    // Strip the `-<zone>` tail; the remainder must be `<labels>.<type>` — i.e. the `<type>-`
    // marker is preceded by at least one label (the registry prefix, e.g. `_externaldns.`).
    // Requiring a non-empty prefix-before-the-marker is what distinguishes a real affix
    // (`_externaldns.a-<zone>`) from a real sibling host (`a-example.com`) or a real subdomain
    // (`cname-foo.example.com`), where the type-looking token IS the leftmost label.
    match name.strip_suffix(&format!("-{zone}")) {
        Some(prefix) => match prefix.rsplit_once('.') {
            Some((before, marker)) => !before.is_empty() && AFFIX_TYPES.contains(&marker),
            None => false,
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        response::Response,
        Router,
    };
    use serde_json::json;
    use tower::ServiceExt;
    use wiremock::{
        matchers::{body_partial_json, method},
        Mock, MockServer, ResponseTemplate,
    };

    fn mock_router(
        server: &MockServer,
        domain_filter: Option<Vec<&str>>,
        timeout_secs: u64,
    ) -> Router {
        let config = Config {
            njalla_api_token: "dummy-token".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8888,
            domain_filter: domain_filter
                .map(|domains| domains.into_iter().map(Config::normalize_domain).collect()),
            dry_run: false,
            cache_ttl_seconds: 60,
            get_records_timeout_secs: timeout_secs,
            njalla_max_retries: 2,
            njalla_retry_base_ms: 1,
        };
        let client = NjallaClient::with_api_url(
            "dummy-token",
            config.njalla_max_retries,
            Duration::from_millis(config.njalla_retry_base_ms),
            &server.uri(),
        )
        .unwrap();
        crate::webhook::routes::create_routes(client, config)
    }

    fn rpc_result(result: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "result": result, "id": 1
        }))
    }

    fn records_mock(domain: &str, response: ResponseTemplate) -> Mock {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "method": "list-records", "params": {"domain": domain}
            })))
            .respond_with(response)
    }

    async fn response_json(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn get_json(router: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        response_json(response).await
    }

    fn assert_error(status: StatusCode, body: &serde_json::Value, expected: StatusCode) {
        assert_eq!(status, expected, "unexpected response: {body}");
        assert!(body.is_object(), "expected an error object: {body}");
        assert!(body["error"].is_string(), "missing error message: {body}");
        assert_eq!(body["status"], expected.as_u16());
    }

    #[tokio::test]
    async fn get_records_stops_after_persistent_domain_failure() {
        for use_filter in [true, false] {
            let server = MockServer::start().await;
            let domains = vec!["first.com", "failing.com", "last.com"];
            Mock::given(body_partial_json(json!({"method": "list-domains"})))
                .respond_with(rpc_result(json!({"domains": domains.iter().map(|name| {
                    json!({"name": name, "status": "active"})
                }).collect::<Vec<_>>()})))
                .expect(if use_filter { 0 } else { 1 })
                .mount(&server)
                .await;
            records_mock(
                "first.com",
                rpc_result(json!({"records": [{
                    "id": "1", "name": "@", "type": "A", "content": "192.0.2.1"
                }]})),
            )
            .expect(1)
            .mount(&server)
            .await;
            records_mock("failing.com", ResponseTemplate::new(500))
                .expect(3)
                .mount(&server)
                .await;
            records_mock("last.com", rpc_result(json!({"records": []})))
                .expect(0)
                .mount(&server)
                .await;

            let router = mock_router(&server, use_filter.then_some(domains), 0);
            let (status, body) = get_json(router, "/records").await;
            assert_error(status, &body, StatusCode::SERVICE_UNAVAILABLE);
            assert!(body["error"].as_str().unwrap().contains("failing.com"));
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn get_records_returns_ordered_union_with_existing_type_filtering() {
        for use_filter in [true, false] {
            let server = MockServer::start().await;
            let domains = vec!["first.com", "second.com"];
            Mock::given(body_partial_json(json!({"method": "list-domains"})))
                .respond_with(rpc_result(json!({"domains": [
                    {"name": "first.com", "status": "active"},
                    {"name": "second.com", "status": "active"}
                ]})))
                .expect(if use_filter { 0 } else { 1 })
                .mount(&server)
                .await;
            let types = ["A", "AAAA", "CNAME", "TXT", "MX", "SRV"];
            let mut records: Vec<_> = types
                .iter()
                .map(|record_type| {
                    json!({
                        "id": "1", "name": "@", "type": record_type,
                        "content": "target", "ttl": 300
                    })
                })
                .collect();
            records.push(json!({
                "id": "2", "name": "@", "type": "NS", "content": "ns.first.com"
            }));
            records_mock("first.com", rpc_result(json!({"records": records})))
                .expect(1)
                .mount(&server)
                .await;
            records_mock(
                "second.com",
                rpc_result(json!({"records": [{
                    "id": "3", "name": "mail", "type": "MX", "content": "mx.second.com",
                    "ttl": 600, "priority": 10
                }]})),
            )
            .expect(1)
            .mount(&server)
            .await;

            let (status, body) = get_json(
                mock_router(&server, use_filter.then_some(domains), 0),
                "/records",
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let mut expected: Vec<_> = types
                .iter()
                .map(|record_type| {
                    json!({
                        "dnsName": "first.com", "recordType": record_type,
                        "targets": ["target"], "recordTTL": 300
                    })
                })
                .collect();
            expected.push(json!({
                "dnsName": "mail.second.com", "recordType": "MX",
                "targets": ["mx.second.com"], "recordTTL": 600,
                "providerSpecific": [{"name": "priority", "value": "10"}]
            }));
            assert_eq!(body, json!(expected));
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn get_records_upstream_failures_always_return_server_errors() {
        let failures = [
            ResponseTemplate::new(403),
            ResponseTemplate::new(500),
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "error": {"code": 403, "message": "denied"}, "id": 1
            })),
            ResponseTemplate::new(200).set_body_string("not JSON"),
            ResponseTemplate::new(200).set_body_json(json!({"result": {"records": []}})),
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": 1})),
            rpc_result(json!({"records": [{
                "id": 5, "name": "@", "type": "A", "content": "192.0.2.1"
            }]})),
            rpc_result(json!({"records": [{
                "id": "5", "type": "A", "content": "192.0.2.1"
            }]})),
            rpc_result(json!({})),
            rpc_result(json!({"records": {}})),
        ];
        for response in failures {
            for uri in ["/records", "/records?zone=example.com"] {
                let server = MockServer::start().await;
                records_mock("example.com", response.clone())
                    .mount(&server)
                    .await;
                let (status, body) =
                    get_json(mock_router(&server, Some(vec!["example.com"]), 0), uri).await;
                let expected = if uri == "/records" {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::BAD_GATEWAY
                };
                assert_error(status, &body, expected);
            }
        }
    }

    #[tokio::test]
    async fn get_records_domain_discovery_failures_return_server_errors() {
        for response in [
            ResponseTemplate::new(403),
            rpc_result(json!({"domains": [{"name": 5, "status": "active"}]})),
            rpc_result(json!({})),
        ] {
            let server = MockServer::start().await;
            Mock::given(body_partial_json(json!({"method": "list-domains"})))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let (status, body) = get_json(mock_router(&server, None, 0), "/records").await;
            assert_error(status, &body, StatusCode::BAD_GATEWAY);
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn get_records_empty_zone_is_successful() {
        let server = MockServer::start().await;
        records_mock("example.com", rpc_result(json!({"records": []})))
            .expect(2)
            .mount(&server)
            .await;
        for uri in ["/records", "/records?zone=example.com"] {
            let (status, body) =
                get_json(mock_router(&server, Some(vec!["example.com"]), 0), uri).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, json!([]));
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn get_records_disallowed_zone_stays_forbidden() {
        let server = MockServer::start().await;
        let (status, body) = get_json(
            mock_router(&server, Some(vec!["example.com"]), 1),
            "/records?zone=blocked.com",
        )
        .await;
        assert_error(status, &body, StatusCode::FORBIDDEN);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_records_deadline_covers_both_branches_and_domain_discovery() {
        // Exercise each read path concurrently so the test takes about one deadline.
        async fn check(uri: &str, filter: Option<Vec<&str>>, discovery: bool) {
            let server = MockServer::start().await;
            if discovery {
                Mock::given(body_partial_json(json!({"method": "list-domains"})))
                    .respond_with(
                        rpc_result(json!({"domains": []})).set_delay(Duration::from_millis(2500)),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
            } else {
                records_mock(
                    "example.com",
                    rpc_result(json!({"records": []})).set_delay(Duration::from_millis(2500)),
                )
                .expect(1)
                .mount(&server)
                .await;
            }
            let start = std::time::Instant::now();
            let (status, body) = get_json(mock_router(&server, filter, 1), uri).await;
            assert_error(status, &body, StatusCode::GATEWAY_TIMEOUT);
            assert!(body["error"].as_str().unwrap().contains("1 seconds"));
            assert!(start.elapsed() < Duration::from_millis(2500));
            server.verify().await;
        }
        tokio::join!(
            check("/records", Some(vec!["example.com"]), false),
            check(
                "/records?zone=example.com",
                Some(vec!["example.com"]),
                false
            ),
            check("/records", None, true),
        );
    }

    #[tokio::test]
    async fn get_records_without_deadline_allows_slow_success() {
        let server = MockServer::start().await;
        records_mock(
            "example.com",
            rpc_result(json!({"records": [{
                "id": "1", "name": "www", "type": "A", "content": "192.0.2.1", "ttl": 300
            }]}))
            .set_delay(Duration::from_millis(1500)),
        )
        .expect(2)
        .mount(&server)
        .await;
        let router = mock_router(&server, Some(vec!["example.com"]), 0);
        let (all, zone) = tokio::join!(
            get_json(router.clone(), "/records"),
            get_json(router, "/records?zone=example.com"),
        );
        for (status, body) in [all, zone] {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body,
                json!([{
                    "dnsName": "www.example.com", "recordType": "A",
                    "targets": ["192.0.2.1"], "recordTTL": 300
                }])
            );
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn apply_changes_upstream_decode_failure_returns_server_error() {
        let server = MockServer::start().await;
        records_mock("example.com", rpc_result(json!({"records": [{"id": 5}]})))
            .expect(1)
            .mount(&server)
            .await;
        let payload = json!({"create": [{
            "dnsName": "www.example.com", "recordType": "A", "targets": ["192.0.2.1"]
        }]});
        let request = Request::post("/records")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = mock_router(&server, Some(vec!["example.com"]), 1)
            .oneshot(request)
            .await
            .unwrap();
        let (status, body) = response_json(response).await;
        assert_error(status, &body, StatusCode::INTERNAL_SERVER_ERROR);
        server.verify().await;
    }

    fn test_handler() -> WebhookHandler {
        let config = Config {
            njalla_api_token: "dummy-token".to_string(),
            webhook_host: "127.0.0.1".to_string(),
            webhook_port: 8888,
            domain_filter: Some(vec![Config::normalize_domain("example.com")]),
            dry_run: true,
            cache_ttl_seconds: 60,
            get_records_timeout_secs: 0,
            njalla_max_retries: 3,
            njalla_retry_base_ms: 500,
        };

        let client = Arc::new(
            NjallaClient::new("dummy-token", 0, std::time::Duration::from_millis(0))
                .expect("client should build"),
        );
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
            get_records_timeout_secs: 0,
            njalla_max_retries: 3,
            njalla_retry_base_ms: 500,
        };
        let client = Arc::new(
            NjallaClient::new("dummy-token", 0, std::time::Duration::from_millis(0))
                .expect("client should build"),
        );
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
            get_records_timeout_secs: 0,
            njalla_max_retries: 3,
            njalla_retry_base_ms: 500,
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

        let client = Arc::new(
            NjallaClient::new("dummy-token", 0, std::time::Duration::from_millis(0))
                .expect("client should build"),
        );
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
    async fn extract_zone_affixed_apex_registry_txt() {
        // external-dns v0.14 writes a per-type ownership TXT for an apex record as
        // `_externaldns.a-<zone>` (the `a-` marker fuses onto the zone's leftmost label, so the
        // dotted-suffix check misses it). It must still resolve to the real zone, and the full
        // name is kept as the record name (Njalla stores it verbatim, so it round-trips).
        let handler = handler_with_filter(vec!["whathefolk.com"]);
        let zone = handler
            .extract_zone("_externaldns.a-whathefolk.com", None)
            .await
            .unwrap();
        assert_eq!(zone, "whathefolk.com");
        let name = handler.extract_record_name("_externaldns.a-whathefolk.com", &zone);
        assert_eq!(name, "_externaldns.a-whathefolk.com");
    }

    #[tokio::test]
    async fn extract_zone_old_format_apex_txt_still_resolves() {
        // Regression: the old-format apex ownership TXT keeps resolving via the dotted suffix.
        let handler = handler_with_filter(vec!["whathefolk.com"]);
        let zone = handler
            .extract_zone("_externaldns.whathefolk.com", None)
            .await
            .unwrap();
        assert_eq!(zone, "whathefolk.com");
        let name = handler.extract_record_name("_externaldns.whathefolk.com", &zone);
        assert_eq!(name, "_externaldns");
    }

    #[tokio::test]
    async fn extract_zone_sibling_domain_not_misclassified_as_affix() {
        // `api-example.com` is a real sibling apex, not an external-dns affix of `example.com`
        // (`api` isn't a record type), so it must resolve to itself — not to `example.com`.
        let handler = handler_with_filter(vec!["example.com", "api-example.com"]);
        let zone = handler.extract_zone("api-example.com", None).await.unwrap();
        assert_eq!(zone, "api-example.com");
    }

    #[tokio::test]
    async fn extract_zone_prefers_exact_sibling_over_affix() {
        // `a-example.com` is its own registered zone. Even though `a` IS a record-type marker
        // and `example.com` is listed FIRST, the exact match must win over the affix reading.
        let handler = handler_with_filter(vec!["example.com", "a-example.com"]);
        let zone = handler.extract_zone("a-example.com", None).await.unwrap();
        assert_eq!(zone, "a-example.com");
    }

    #[tokio::test]
    async fn extract_zone_nested_affixed_apex_resolves_to_longer_zone() {
        // `_externaldns.a-sub.example.com` is the affixed apex TXT for `sub.example.com`, not a
        // subdomain of `example.com` — the longer (more specific) zone must win.
        let handler = handler_with_filter(vec!["example.com", "sub.example.com"]);
        let zone = handler
            .extract_zone("_externaldns.a-sub.example.com", None)
            .await
            .unwrap();
        assert_eq!(zone, "sub.example.com");
    }

    #[tokio::test]
    async fn extract_zone_subdomain_with_type_like_label() {
        // A real subdomain whose leftmost label starts with a record-type token must resolve
        // to its parent zone (a dotted-suffix match), not be misread as an apex affix.
        let handler = handler_with_filter(vec!["example.com"]);
        let zone = handler
            .extract_zone("cname-foo.example.com", None)
            .await
            .unwrap();
        assert_eq!(zone, "example.com");
    }

    #[tokio::test]
    async fn extract_zone_affixed_apex_via_owned_domains() {
        // Same affix handling on the no-filter (owned-domains) path.
        let handler = handler_with_mock_domains(vec!["whathefolk.com"]);
        let zone = handler
            .extract_zone("_externaldns.cname-whathefolk.com", None)
            .await
            .unwrap();
        assert_eq!(zone, "whathefolk.com");
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
            get_records_timeout_secs: 0,
            njalla_max_retries: 3,
            njalla_retry_base_ms: 500,
        };
        let client = Arc::new(
            NjallaClient::new("dummy-token", 0, std::time::Duration::from_millis(0))
                .expect("client should build"),
        );
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
