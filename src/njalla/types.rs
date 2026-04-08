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
        // Wraps on u32 overflow; acceptable per spec
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
    use std::sync::Mutex;

    /// All tests that touch REQUEST_ID must hold this lock to prevent
    /// interleaving with the reset in `request_id_starts_at_one`.
    static COUNTER_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn request_id_starts_at_one() {
        let _guard = COUNTER_LOCK.lock().unwrap();
        // Reset to 1, generate one ID, assert it is exactly 1.
        REQUEST_ID.store(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new("test", serde_json::json!({}));
        assert_eq!(req.id, 1, "First ID must be 1");
    }

    #[test]
    fn sequential_unique_ids() {
        let _guard = COUNTER_LOCK.lock().unwrap();
        let ids: Vec<u32> = (0..100)
            .map(|_| JsonRpcRequest::new("test", serde_json::json!({})).id)
            .collect();
        // All IDs must be nonzero (counter starts at 1, not 0)
        assert!(
            ids.iter().all(|&id| id >= 1),
            "All IDs must be >= 1; got a zero ID"
        );
        // All IDs must be unique
        let unique: HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 100);
        // IDs must be strictly monotonically increasing
        for pair in ids.windows(2) {
            assert!(
                pair[0] < pair[1],
                "IDs not monotonic: {} >= {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_unique_ids() {
        let _guard = COUNTER_LOCK.lock().unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(100));
        let handles: Vec<_> = (0..100)
            .map(|_| {
                let b = barrier.clone();
                tokio::spawn(async move {
                    b.wait().await;
                    JsonRpcRequest::new("test", serde_json::json!({})).id
                })
            })
            .collect();

        let mut ids = HashSet::new();
        for handle in handles {
            ids.insert(handle.await.unwrap());
        }
        assert_eq!(ids.len(), 100);
    }
}
