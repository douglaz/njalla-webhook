use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};

static REQUEST_ID: AtomicU32 = AtomicU32::new(1);

// JSON-RPC 2.0 types
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
    pub id: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse<T> {
    pub jsonrpc: String,
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
    pub id: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// Njalla domain types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Domain {
    pub name: String,
    pub status: String,
    pub expiry: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: String,
    pub content: String,
    pub ttl: Option<u32>,
    pub priority: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct AddRecordRequest {
    pub domain: String,
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: String,
    pub content: String,
    pub ttl: u32,
    pub priority: Option<u32>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct UpdateRecordRequest {
    pub domain: String,
    pub id: String,
    pub content: String,
    pub ttl: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct RemoveRecordRequest {
    pub domain: String,
    pub id: String,
}

impl JsonRpcRequest {
    pub fn new(method: &str, params: serde_json::Value) -> Self {
        let id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);

        Self {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sequential_unique_ids() {
        let ids: HashSet<u32> = (0..100)
            .map(|_| JsonRpcRequest::new("test", serde_json::json!({})).id)
            .collect();
        assert_eq!(ids.len(), 100);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_unique_ids() {
        let handles: Vec<_> = (0..100)
            .map(|_| tokio::spawn(async { JsonRpcRequest::new("test", serde_json::json!({})).id }))
            .collect();

        let mut ids = HashSet::new();
        for handle in handles {
            ids.insert(handle.await.unwrap());
        }
        assert_eq!(ids.len(), 100);
    }
}
