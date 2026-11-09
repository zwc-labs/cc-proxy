use crate::config::Config;
use axum::{extract::State, routing::get, Json, Router};
use std::sync::Arc;

pub fn routes(config: Arc<Config>) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .with_state(config)
}

async fn handle_health(State(config): State<Arc<Config>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "cc-proxy",
        // Effective model config, so ops can confirm which file is live without
        // reading the launchd environment.
        "config": {
            "default_model": config.default_model,
            "model_profiles": config.model_profiles.len(),
            "providers": config.providers.len(),
            "mapping_keys": config.model_mapping.len(),
        },
        // Resolution counters: a non-zero `fallbacks` count means at least one
        // inbound model name silently degraded to the default model.
        "model_resolution": crate::model_resolution::snapshot(),
    }))
}
