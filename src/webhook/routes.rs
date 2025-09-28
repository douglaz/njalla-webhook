use super::handlers::WebhookHandler;
use crate::config::Config;
use crate::njalla::Client as NjallaClient;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub fn create_routes(njalla_client: NjallaClient, config: Config) -> Router {
    let handler = Arc::new(WebhookHandler::new(Arc::new(njalla_client), config));

    Router::new()
        .route("/", {
            let h = handler.clone();
            get(move || async move { h.negotiate().await })
        })
        .route("/healthz", {
            let h = handler.clone();
            get(move || async move { h.health().await })
        })
        .route("/ready", {
            let h = handler.clone();
            get(move || async move { h.ready().await })
        })
        .route("/records", {
            let h = handler.clone();
            get(move |query| async move { h.get_records(query).await })
        })
        .route("/records", {
            let h = handler.clone();
            post(move |body| async move { h.apply_changes(body).await })
        })
        .route("/adjustendpoints", {
            let h = handler.clone();
            post(move |body| async move { h.adjust_endpoints(body).await })
        })
}
