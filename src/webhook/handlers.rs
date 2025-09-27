use super::types::*;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::njalla::{self, Client as NjallaClient};
use axum::{extract::Query, Json};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

pub struct WebhookHandler {
    njalla_client: Arc<NjallaClient>,
    config: Config,
}

impl WebhookHandler {
    pub fn new(njalla_client: Arc<NjallaClient>, config: Config) -> Self {
        Self {
            njalla_client,
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

    pub async fn get_records(&self, query: Query<GetRecordsQuery>) -> Result<Json<GetRecordsResponse>> {
        let zone_name = query.zone_name.as_ref()
            .ok_or_else(|| Error::InvalidRequest("zone parameter is required".to_string()))?;

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
                matches!(r.record_type.as_str(), "A" | "AAAA" | "CNAME" | "TXT" | "MX" | "SRV")
            })
            .map(|r| Endpoint::from_njalla_record(r, zone_name))
            .collect();

        info!("Returning {} endpoints for zone {}", endpoints.len(), zone_name);

        Ok(Json(GetRecordsResponse { endpoints }))
    }

    pub async fn apply_changes(&self, Json(request): Json<ApplyChangesRequest>) -> Result<Json<ApplyChangesResponse>> {
        info!("Applying changes: {} creates, {} updates, {} deletes",
            request.changes.create.len(),
            request.changes.update_new.len(),
            request.changes.delete.len()
        );

        if request.changes.is_empty() {
            return Ok(Json(ApplyChangesResponse {
                message: "No changes to apply".to_string(),
            }));
        }

        let mut applied_count = 0;
        let mut errors = Vec::new();

        // Process deletions first
        for endpoint in &request.changes.delete {
            if let Err(e) = self.delete_endpoint(endpoint).await {
                error!("Failed to delete endpoint {}: {}", endpoint.dns_name, e);
                errors.push(format!("Delete {}: {}", endpoint.dns_name, e));
            } else {
                applied_count += 1;
            }
        }

        // Process updates (delete old, create new)
        for (old, new) in request.changes.update_old.iter().zip(request.changes.update_new.iter()) {
            if let Err(e) = self.update_endpoint(old, new).await {
                error!("Failed to update endpoint {}: {}", new.dns_name, e);
                errors.push(format!("Update {}: {}", new.dns_name, e));
            } else {
                applied_count += 1;
            }
        }

        // Process creations
        for endpoint in &request.changes.create {
            if let Err(e) = self.create_endpoint(endpoint).await {
                error!("Failed to create endpoint {}: {}", endpoint.dns_name, e);
                errors.push(format!("Create {}: {}", endpoint.dns_name, e));
            } else {
                applied_count += 1;
            }
        }

        if !errors.is_empty() && applied_count == 0 {
            return Err(Error::Internal(format!("All operations failed: {:?}", errors)));
        }

        let message = if errors.is_empty() {
            format!("Successfully applied {} changes", applied_count)
        } else {
            format!("Applied {} changes with {} errors: {:?}", applied_count, errors.len(), errors)
        };

        Ok(Json(ApplyChangesResponse { message }))
    }

    pub async fn adjust_endpoints(&self, Json(endpoints): Json<Vec<Endpoint>>) -> Result<Json<AdjustEndpointsResponse>> {
        // This is optional - we just return the endpoints as-is
        debug!("Adjusting {} endpoints", endpoints.len());
        Ok(Json(AdjustEndpointsResponse { endpoints }))
    }

    // Helper methods for record operations

    async fn create_endpoint(&self, endpoint: &Endpoint) -> Result<()> {
        let zone = self.extract_zone(&endpoint.dns_name)?;

        if !self.config.is_domain_allowed(&zone) {
            return Err(Error::DomainNotAllowed(zone));
        }

        let name = self.extract_record_name(&endpoint.dns_name, &zone);

        for target in &endpoint.targets {
            let priority = endpoint.provider_specific
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

    async fn update_endpoint(&self, old: &Endpoint, new: &Endpoint) -> Result<()> {
        // For simplicity, delete old and create new
        self.delete_endpoint(old).await?;
        self.create_endpoint(new).await?;
        Ok(())
    }

    async fn delete_endpoint(&self, endpoint: &Endpoint) -> Result<()> {
        let zone = self.extract_zone(&endpoint.dns_name)?;

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

            if record_name == name &&
               record.record_type == endpoint.record_type &&
               endpoint.targets.contains(&record.content) {

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

    fn extract_zone(&self, dns_name: &str) -> Result<String> {
        // Find the zone by checking against configured domains
        if let Some(ref domains) = self.config.domain_filter {
            for domain in domains {
                if dns_name == domain || dns_name.ends_with(&format!(".{}", domain)) {
                    return Ok(domain.clone());
                }
            }
        }

        // Fall back to extracting last two parts as zone
        let parts: Vec<&str> = dns_name.split('.').collect();
        if parts.len() >= 2 {
            Ok(format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]))
        } else {
            Err(Error::InvalidRequest(format!("Cannot extract zone from {}", dns_name)))
        }
    }

    fn extract_record_name(&self, dns_name: &str, zone: &str) -> String {
        if dns_name == zone {
            "".to_string()
        } else if dns_name.ends_with(&format!(".{}", zone)) {
            dns_name[..dns_name.len() - zone.len() - 1].to_string()
        } else {
            dns_name.to_string()
        }
    }
}