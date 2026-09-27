//! Web Push DTOs shared by the subscribe flow (app) and its handlers (server).

use serde::{Deserialize, Serialize};

/// Response of `GET /api/push/vapid`: base64url (no padding) uncompressed
/// P-256 public key — exactly what `PushManager.subscribe` wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VapidResponse {
    pub public_key: String,
}

/// Body of `POST /api/push/subscribe` — one browser's PushSubscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

/// Body of `DELETE /api/push/subscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsubscribeRequest {
    pub endpoint: String,
}
