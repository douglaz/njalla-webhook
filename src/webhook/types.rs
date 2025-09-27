use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// External-DNS webhook types based on the specification

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainFilter {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub regex: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    #[serde(rename = "dnsName")]
    pub dns_name: String,
    pub targets: Vec<String>,
    #[serde(rename = "recordType")]
    pub record_type: String,
    #[serde(rename = "setIdentifier", skip_serializing_if = "Option::is_none")]
    pub set_identifier: Option<String>,
    #[serde(rename = "recordTTL", skip_serializing_if = "Option::is_none")]
    pub record_ttl: Option<i64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    #[serde(
        rename = "providerSpecific",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub provider_specific: Vec<ProviderSpecific>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSpecific {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changes {
    #[serde(rename = "Create", default)]
    pub create: Vec<Endpoint>,
    #[serde(rename = "UpdateOld", default)]
    pub update_old: Vec<Endpoint>,
    #[serde(rename = "UpdateNew", default)]
    pub update_new: Vec<Endpoint>,
    #[serde(rename = "Delete", default)]
    pub delete: Vec<Endpoint>,
}

// Request/Response types for webhook API

#[derive(Debug, Deserialize)]
pub struct GetRecordsQuery {
    #[serde(rename = "zone")]
    pub zone_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetRecordsResponse {
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
pub struct ApplyChangesRequest {
    pub changes: Changes,
}

#[derive(Debug, Serialize)]
pub struct ApplyChangesResponse {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct AdjustEndpointsResponse {
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

// Helper implementations

impl Endpoint {
    #[allow(dead_code)]
    pub fn new(dns_name: String, record_type: String, targets: Vec<String>) -> Self {
        Self {
            dns_name,
            targets,
            record_type,
            set_identifier: None,
            record_ttl: None,
            labels: HashMap::new(),
            provider_specific: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn with_ttl(mut self, ttl: i64) -> Self {
        self.record_ttl = Some(ttl);
        self
    }

    pub fn from_njalla_record(record: &crate::njalla::DnsRecord, zone: &str) -> Self {
        let dns_name = if record.name.is_empty() || record.name == "@" {
            zone.to_string()
        } else if record.name.ends_with(zone) {
            record.name.clone()
        } else {
            format!("{}.{}", record.name, zone)
        };

        Self {
            dns_name,
            targets: vec![record.content.clone()],
            record_type: record.record_type.clone(),
            set_identifier: None,
            record_ttl: record.ttl.map(|ttl| ttl as i64),
            labels: HashMap::new(),
            provider_specific: if let Some(priority) = record.priority {
                vec![ProviderSpecific {
                    name: "priority".to_string(),
                    value: priority.to_string(),
                }]
            } else {
                Vec::new()
            },
        }
    }
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty()
            && self.update_old.is_empty()
            && self.update_new.is_empty()
            && self.delete.is_empty()
    }
}
