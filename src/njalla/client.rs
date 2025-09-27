use super::types::*;
use crate::error::{Error, Result};
use reqwest::{header, Client as HttpClient};
use serde_json::json;
use std::time::Duration;
use tracing::{debug, info};

const NJALLA_API_URL: &str = "https://njal.la/api/1/";

pub struct Client {
    http_client: HttpClient,
}

impl Client {
    pub fn new(api_token: &str) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Njalla {}", api_token))
                .map_err(|e| Error::Configuration(format!("Invalid API token: {}", e)))?,
        );
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );

        let http_client = HttpClient::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Configuration(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self { http_client })
    }

    async fn call_api<T>(&self, request: JsonRpcRequest) -> Result<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        debug!("Calling Njalla API: method={}", request.method);

        let response = self
            .http_client
            .post(NJALLA_API_URL)
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::NjallaApi(format!("HTTP {}: {}", status, text)));
        }

        let json_response: JsonRpcResponse<T> = response.json().await?;

        if let Some(error) = json_response.error {
            return Err(Error::NjallaApi(format!(
                "API error {}: {}",
                error.code, error.message
            )));
        }

        json_response
            .result
            .ok_or_else(|| Error::NjallaApi("Empty response from Njalla API".to_string()))
    }

    pub async fn list_domains(&self) -> Result<Vec<Domain>> {
        let request = JsonRpcRequest::new("list-domains", json!({}));
        let response: serde_json::Value = self.call_api(request).await?;

        let domains = response["domains"]
            .as_array()
            .ok_or_else(|| Error::NjallaApi("Invalid domains response".to_string()))?
            .iter()
            .map(|d| serde_json::from_value(d.clone()))
            .collect::<std::result::Result<Vec<Domain>, _>>()?;

        info!("Listed {} domains", domains.len());
        Ok(domains)
    }

    pub async fn list_records(&self, domain: &str) -> Result<Vec<DnsRecord>> {
        let request = JsonRpcRequest::new(
            "list-records",
            json!({
                "domain": domain
            }),
        );

        let response: serde_json::Value = self.call_api(request).await?;

        let records = response["records"]
            .as_array()
            .ok_or_else(|| Error::NjallaApi("Invalid records response".to_string()))?
            .iter()
            .map(|r| serde_json::from_value(r.clone()))
            .collect::<std::result::Result<Vec<DnsRecord>, _>>()?;

        info!("Listed {} records for domain {}", records.len(), domain);
        Ok(records)
    }

    pub async fn add_record(&self, request: AddRecordRequest) -> Result<DnsRecord> {
        let params = json!({
            "domain": request.domain,
            "type": request.record_type,
            "name": request.name,
            "content": request.content,
            "ttl": request.ttl,
            "priority": request.priority,
        });

        let rpc_request = JsonRpcRequest::new("add-record", params);
        let record: DnsRecord = self.call_api(rpc_request).await?;

        info!(
            "Added {} record {} -> {} for domain {}",
            record.record_type, record.name, record.content, request.domain
        );
        Ok(record)
    }

    pub async fn update_record(&self, request: UpdateRecordRequest) -> Result<DnsRecord> {
        let params = json!({
            "domain": request.domain,
            "id": request.id,
            "content": request.content,
            "ttl": request.ttl,
        });

        let rpc_request = JsonRpcRequest::new("edit-record", params);
        let record: DnsRecord = self.call_api(rpc_request).await?;

        info!(
            "Updated record {} for domain {}",
            request.id, request.domain
        );
        Ok(record)
    }

    pub async fn remove_record(&self, request: RemoveRecordRequest) -> Result<()> {
        let params = json!({
            "domain": request.domain,
            "id": request.id,
        });

        let rpc_request = JsonRpcRequest::new("remove-record", params);
        let _: serde_json::Value = self.call_api(rpc_request).await?;

        info!(
            "Removed record {} from domain {}",
            request.id, request.domain
        );
        Ok(())
    }
}
