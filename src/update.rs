//! Firmware update endpoints (placeholder).
//!
//! The HTTP surface is wired up now so a real updater can be added later
//! without changing routing elsewhere:
//!
//!   * `GET  /api/version` — current build info.
//!   * `POST /api/update`  — apply an update. Currently returns `501`.
//!
//! To implement real updates, replace [`apply_update`] with logic that, e.g.,
//! verifies a signed image, writes it to the inactive partition/slot, and
//! schedules a reboot — keeping the same route + response shape.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::json;

use crate::camera::AppState;

/// Router for the update/version endpoints. Merged into the main app router.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/version", get(version))
        .route("/api/update", post(apply_update))
}

#[derive(Serialize)]
struct VersionInfo {
    name: &'static str,
    version: &'static str,
    description: &'static str,
}

/// Report the running build's name/version (compile-time package metadata).
async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        description: env!("CARGO_PKG_DESCRIPTION"),
    })
}

/// Placeholder: updates are not implemented yet. Returns `501 Not Implemented`
/// with a JSON body so clients can feature-detect.
async fn apply_update(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "status": "not_implemented",
            "message": "Firmware update is not available yet."
        })),
    )
}
