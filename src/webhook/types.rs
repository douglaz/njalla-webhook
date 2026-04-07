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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    #[serde(default, alias = "Create")]
    pub create: Vec<Endpoint>,
    #[serde(default, alias = "UpdateOld")]
    pub update_old: Vec<Endpoint>,
    #[serde(default, alias = "UpdateNew")]
    pub update_new: Vec<Endpoint>,
    #[serde(default, alias = "Delete")]
    pub delete: Vec<Endpoint>,
}

// Request/Response types for webhook API

#[derive(Debug, Deserialize)]
pub struct GetRecordsQuery {
    #[serde(rename = "zone")]
    pub zone_name: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct GetRecordsResponse {
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ApplyChangesRequest {
    // Prefer wrapped requests first so Serde does not eagerly accept them as an empty Direct request.
    Wrapped {
        #[serde(rename = "changes", alias = "Changes")]
        changes: Changes,
    },
    // External-DNS sends changes directly at the root level.
    Direct(Changes),
}

impl ApplyChangesRequest {
    pub fn into_changes(self) -> Changes {
        match self {
            ApplyChangesRequest::Direct(changes) => changes,
            ApplyChangesRequest::Wrapped { changes } => changes,
        }
    }
}

#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_endpoint(name: &str) -> serde_json::Value {
        json!({
            "dnsName": name,
            "targets": ["192.0.2.10"],
            "recordType": "A"
        })
    }

    #[test]
    fn deserializes_external_dns_lower_camel_case_changes() {
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "create": [sample_endpoint("new.example.com")],
            "updateOld": [sample_endpoint("old.example.com")],
            "updateNew": [sample_endpoint("newer.example.com")],
            "delete": [sample_endpoint("delete.example.com")]
        }))
        .expect("lowerCamelCase payload should deserialize");

        let changes = request.into_changes();

        assert_eq!(changes.create.len(), 1);
        assert_eq!(changes.update_old.len(), 1);
        assert_eq!(changes.update_new.len(), 1);
        assert_eq!(changes.delete.len(), 1);
        assert_eq!(changes.create[0].dns_name, "new.example.com");
    }

    #[test]
    fn deserializes_legacy_pascal_case_changes() {
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "Create": [sample_endpoint("new.example.com")],
            "UpdateOld": [sample_endpoint("old.example.com")],
            "UpdateNew": [sample_endpoint("newer.example.com")],
            "Delete": [sample_endpoint("delete.example.com")]
        }))
        .expect("PascalCase payload should deserialize");

        let changes = request.into_changes();

        assert_eq!(changes.create.len(), 1);
        assert_eq!(changes.update_old.len(), 1);
        assert_eq!(changes.update_new.len(), 1);
        assert_eq!(changes.delete.len(), 1);
    }

    #[test]
    fn deserializes_wrapped_changes_payload() {
        let request: ApplyChangesRequest = serde_json::from_value(json!({
            "changes": {
                "create": [sample_endpoint("wrapped.example.com")]
            }
        }))
        .expect("wrapped payload should deserialize");

        let changes = request.into_changes();
        assert_eq!(changes.create.len(), 1);
        assert_eq!(changes.create[0].dns_name, "wrapped.example.com");
    }
}
