use super::types::*;
use crate::error::{Error, Result};
use reqwest::{header, Client as HttpClient, StatusCode};
use serde_json::json;
use std::time::Duration;
use tracing::{debug, info, warn};

const NJALLA_API_URL: &str = "https://njal.la/api/1/";

/// Upper bound on the exponential backoff delay between retries.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Outcome of a single Njalla API attempt that failed, carrying whether the
/// failure is worth retrying.
struct AttemptError {
    error: Error,
    retryable: bool,
}

/// A non-2xx HTTP status from Njalla is retryable when it is a rate-limit
/// (429) or a transient server-side error (5xx). All other statuses (4xx)
/// reflect a deterministic problem with the request and are not retried.
fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Exponential backoff: `base * 2^(retry - 1)`, capped at `MAX_BACKOFF`.
/// `retry` is 1-based (the delay before the first retry uses `retry == 1`).
fn backoff_delay(base: Duration, retry: u32) -> Duration {
    let factor = 2u32.saturating_pow(retry.saturating_sub(1));
    base.checked_mul(factor)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

pub struct Client {
    http_client: HttpClient,
    api_url: String,
    max_retries: u32,
    retry_base: Duration,
}

impl Client {
    pub fn new(api_token: &str, max_retries: u32, retry_base: Duration) -> Result<Self> {
        Self::with_api_url(api_token, max_retries, retry_base, NJALLA_API_URL)
    }

    fn with_api_url(
        api_token: &str,
        max_retries: u32,
        retry_base: Duration,
        api_url: &str,
    ) -> Result<Self> {
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

        Ok(Self {
            http_client,
            api_url: api_url.to_string(),
            max_retries,
            retry_base,
        })
    }

    /// Call the Njalla API, retrying transient failures (rate limits, 5xx,
    /// network errors) with exponential backoff. A single failed call used to
    /// bubble up as a hard error, which external-dns treats as a fatal
    /// "apply changes" failure and crashes on — so absorbing transient blips
    /// here keeps the reconcile loop alive.
    ///
    /// Note: mutating calls (add/remove-record) are retried as well. This is
    /// safe in practice because external-dns already re-runs the entire change
    /// batch on the next reconcile, so a retry is no worse than the existing
    /// crash-and-retry behaviour, and the common transient case (a 429/5xx
    /// where Njalla rejected the request outright) makes no change to retry.
    async fn call_api<T>(&self, request: JsonRpcRequest) -> Result<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let mut retries = 0u32;
        loop {
            match self.attempt_call_api::<T>(&request).await {
                Ok(value) => return Ok(value),
                Err(AttemptError { error, retryable }) => {
                    if retryable && retries < self.max_retries {
                        retries += 1;
                        let delay = backoff_delay(self.retry_base, retries);
                        warn!(
                            "Njalla API '{}' failed (attempt {}/{}): {} — retrying in {:?}",
                            request.method,
                            retries,
                            self.max_retries + 1,
                            error,
                            delay
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    /// Perform a single API attempt, classifying any failure as retryable or
    /// terminal.
    async fn attempt_call_api<T>(
        &self,
        request: &JsonRpcRequest,
    ) -> std::result::Result<T, AttemptError>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        debug!("Calling Njalla API: method={}", request.method);

        let response = match self
            .http_client
            .post(&self.api_url)
            .json(request)
            .send()
            .await
        {
            Ok(response) => response,
            // Connect/timeout/transport errors are transient.
            Err(e) => {
                return Err(AttemptError {
                    retryable: true,
                    error: Error::Network(e),
                });
            }
        };

        let status = response.status();
        if !status.is_success() {
            let retryable = is_retryable_status(status);
            let text = response.text().await.unwrap_or_default();
            return Err(AttemptError {
                retryable,
                error: Error::NjallaApi(format!("HTTP {}: {}", status, text)),
            });
        }

        let json_response: JsonRpcResponse<T> = match response.json().await {
            Ok(parsed) => parsed,
            // A 2xx body that fails to decode is likely a truncated/transient
            // response; allow a retry.
            Err(e) => {
                return Err(AttemptError {
                    retryable: true,
                    error: Error::Network(e),
                });
            }
        };

        if let Some(error) = json_response.error {
            // JSON-RPC application errors are deterministic; do not retry.
            return Err(AttemptError {
                retryable: false,
                error: Error::NjallaApi(format!("API error {}: {}", error.code, error.message)),
            });
        }

        json_response.result.ok_or_else(|| AttemptError {
            retryable: false,
            error: Error::NjallaApi("Empty response from Njalla API".to_string()),
        })
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

    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_and_server_errors_are_retryable() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn client_errors_are_not_retryable() {
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    #[test]
    fn backoff_grows_exponentially() {
        let base = Duration::from_millis(500);
        assert_eq!(backoff_delay(base, 1), Duration::from_millis(500));
        assert_eq!(backoff_delay(base, 2), Duration::from_millis(1000));
        assert_eq!(backoff_delay(base, 3), Duration::from_millis(2000));
        assert_eq!(backoff_delay(base, 4), Duration::from_millis(4000));
    }

    #[test]
    fn backoff_is_capped_and_overflow_safe() {
        let base = Duration::from_millis(500);
        // Large retry counts must saturate at MAX_BACKOFF rather than overflow.
        assert_eq!(backoff_delay(base, 100), MAX_BACKOFF);
        assert_eq!(backoff_delay(Duration::from_secs(3600), 5), MAX_BACKOFF);
    }

    fn test_client(server: &mockito::Server, max_retries: u32) -> Client {
        // Near-zero backoff keeps the tests fast.
        Client::with_api_url(
            "token",
            max_retries,
            Duration::from_millis(1),
            &server.url(),
        )
        .expect("client should build")
    }

    const SUCCESS_BODY: &str = r#"{"jsonrpc":"2.0","result":{"domains":[]},"id":1}"#;

    #[tokio::test]
    async fn retries_transient_failure_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // First attempt: rate-limited. Mockito serves mocks in creation order
        // until each is exhausted, so the 429 fires once, then the 200.
        let rate_limited = server
            .mock("POST", "/")
            .with_status(429)
            .with_body("rate limited")
            .expect(1)
            .create_async()
            .await;
        let ok = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(SUCCESS_BODY)
            .expect(1)
            .create_async()
            .await;

        let client = test_client(&server, 3);
        let result = client.list_domains().await;

        assert!(result.is_ok(), "expected success after retry: {result:?}");
        rate_limited.assert_async().await;
        ok.assert_async().await;
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_on_server_error() {
        let mut server = mockito::Server::new_async().await;
        // max_retries = 2 → 3 total attempts, all 503.
        let always_500 = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("unavailable")
            .expect(3)
            .create_async()
            .await;

        let client = test_client(&server, 2);
        let result = client.list_domains().await;

        assert!(result.is_err(), "expected failure after exhausting retries");
        always_500.assert_async().await;
    }

    #[tokio::test]
    async fn does_not_retry_on_client_error() {
        let mut server = mockito::Server::new_async().await;
        // A 400 is deterministic and must be attempted exactly once.
        let bad_request = server
            .mock("POST", "/")
            .with_status(400)
            .with_body("bad request")
            .expect(1)
            .create_async()
            .await;

        let client = test_client(&server, 3);
        let result = client.list_domains().await;

        assert!(result.is_err(), "client error must surface");
        bad_request.assert_async().await;
    }

    #[tokio::test]
    async fn does_not_retry_on_jsonrpc_application_error() {
        let mut server = mockito::Server::new_async().await;
        // A 200 carrying a JSON-RPC error is deterministic: attempt once.
        let app_error = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","error":{"code":403,"message":"forbidden"},"id":1}"#)
            .expect(1)
            .create_async()
            .await;

        let client = test_client(&server, 3);
        let result = client.list_domains().await;

        assert!(result.is_err(), "application error must surface");
        app_error.assert_async().await;
    }
}
