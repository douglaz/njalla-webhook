use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::time::Instant;
use tracing::{error, info, warn};

pub async fn logging_middleware(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path().to_string();

    // Log request
    info!(
        method = %method,
        path = %path,
        query = ?uri.query(),
        "Incoming request"
    );

    // Extract and log body for POST requests to /records
    if method == "POST" && path == "/records" {
        let (parts, body) = request.into_parts();

        // Read the body
        let bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(err) => {
                error!("Failed to read request body: {}", err);
                return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response();
            }
        };

        // Log the raw body
        let body_str = String::from_utf8_lossy(&bytes);
        info!("Raw request body for /records POST: {}", body_str);

        // Try to parse as JSON to debug structure
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(json) => {
                info!(
                    "Parsed JSON structure: {}",
                    serde_json::to_string_pretty(&json).unwrap_or_default()
                );
            }
            Err(e) => {
                warn!("Failed to parse body as JSON: {}", e);
            }
        }

        // Reconstruct the request with the body
        let request = Request::from_parts(parts, Body::from(bytes));

        let response = next.run(request).await;
        let duration = start.elapsed();
        let status = response.status();

        if status.is_client_error() || status.is_server_error() {
            error!(
                method = %method,
                path = %path,
                status = %status,
                duration_ms = %duration.as_millis(),
                "Request failed"
            );
        } else {
            info!(
                method = %method,
                path = %path,
                status = %status,
                duration_ms = %duration.as_millis(),
                "Request completed"
            );
        }

        response
    } else {
        let response = next.run(request).await;
        let duration = start.elapsed();
        let status = response.status();

        info!(
            method = %method,
            path = %path,
            status = %status,
            duration_ms = %duration.as_millis(),
            "Request completed"
        );

        response
    }
}

pub async fn error_handling_middleware(request: Request, next: Next) -> Response {
    let response = next.run(request).await;

    // If we get a 422, log more details
    if response.status() == StatusCode::UNPROCESSABLE_ENTITY {
        error!("422 Unprocessable Entity - likely JSON deserialization issue");
    }

    response
}
