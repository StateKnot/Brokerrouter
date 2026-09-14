use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("invalid or revoked virtual key")]
    Unauthorized,
    #[error("resource not found or not permitted")]
    NotFound,
    #[error("budget exhausted")]
    Budget,
    #[error("idempotency key conflicts with a different request")]
    Conflict,
    #[error("no effective USD/CNY exchange rate is configured")]
    MissingFx,
    #[error("operation is already in progress or awaiting reconciliation")]
    InProgress,
    #[error("database schema does not match this gateway version")]
    SchemaMismatch,
    #[error("no authorized endpoint satisfies this request")]
    NoEndpoint,
    #[error("upstream outcome is uncertain; inspect the request before taking further action")]
    UpstreamUnknown,
    #[error("all candidate endpoints failed before submission")]
    NotSubmitted,
    #[error("gateway configuration is missing or invalid")]
    Configuration,
    #[error("stored result is expired or unavailable; it will not be regenerated automatically")]
    ResultUnavailable,
    #[error("gateway concurrency capacity is full")]
    Busy,
    #[error("private asset storage is unavailable; retry the same immutable operation")]
    Storage,
    #[error("private asset storage quota exhausted")]
    AssetQuota,
    #[error("a trusted current-turn context is required")]
    GovernanceContext,
    #[error("human approval is required before continuing")]
    ApprovalRequired(serde_json::Value),
    #[error("turn is closed or expired")]
    TurnClosed,
    #[error("action was rejected or replaced by user feedback")]
    ActionRejected,
    #[error("policy changed; recheck the original operation before approval")]
    PolicyChanged,
    #[error("valid request-bound human attestation required")]
    HumanUnauthorized,
    #[error("console permission is missing or revoked")]
    ConsoleForbidden,
    #[error("console query deadline exceeded")]
    ConsoleTimeout,
    #[error("active purpose-scoped scanner credential required")]
    ScannerUnauthorized,
    #[error("scan lease expired, superseded or closed")]
    ScanLeaseEnded,
    #[error("technical output safety checks failed; human approval cannot override them")]
    OutputUnsafe,
    #[error("an active asset_scan output policy bound to the current scan profile is required")]
    OutputPolicyMissing,
    #[error("internal persistence failure")]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid_virtual_key"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Budget => (StatusCode::TOO_MANY_REQUESTS, "budget_exceeded"),
            Self::Conflict => (StatusCode::CONFLICT, "idempotency_conflict"),
            Self::MissingFx => (StatusCode::SERVICE_UNAVAILABLE, "exchange_rate_missing"),
            Self::InProgress => (StatusCode::CONFLICT, "operation_in_progress"),
            Self::SchemaMismatch => (StatusCode::SERVICE_UNAVAILABLE, "schema_mismatch"),
            Self::NoEndpoint => (StatusCode::BAD_REQUEST, "no_compatible_endpoint"),
            Self::UpstreamUnknown => (StatusCode::BAD_GATEWAY, "submission_unknown"),
            Self::NotSubmitted => (StatusCode::BAD_GATEWAY, "not_submitted"),
            Self::Configuration => (StatusCode::SERVICE_UNAVAILABLE, "configuration_error"),
            Self::ResultUnavailable => (StatusCode::GONE, "result_unavailable"),
            Self::Busy => (StatusCode::SERVICE_UNAVAILABLE, "gateway_busy"),
            Self::Storage => (StatusCode::SERVICE_UNAVAILABLE, "asset_storage_unavailable"),
            Self::AssetQuota => (StatusCode::TOO_MANY_REQUESTS, "asset_quota_exceeded"),
            Self::GovernanceContext => (StatusCode::BAD_REQUEST, "governance_context_required"),
            Self::ApprovalRequired(_) => (StatusCode::CONFLICT, "approval_required"),
            Self::TurnClosed => (StatusCode::GONE, "turn_closed"),
            Self::ActionRejected => (StatusCode::CONFLICT, "action_rejected"),
            Self::PolicyChanged => (StatusCode::CONFLICT, "policy_changed"),
            Self::HumanUnauthorized => (StatusCode::UNAUTHORIZED, "invalid_human_attestation"),
            Self::ConsoleForbidden => (StatusCode::FORBIDDEN, "console_permission_denied"),
            Self::ConsoleTimeout => (StatusCode::SERVICE_UNAVAILABLE, "console_query_timeout"),
            Self::ScannerUnauthorized => (StatusCode::UNAUTHORIZED, "invalid_scanner_credential"),
            Self::ScanLeaseEnded => (StatusCode::CONFLICT, "scan_lease_ended"),
            Self::OutputUnsafe => (StatusCode::CONFLICT, "output_unsafe"),
            Self::OutputPolicyMissing => (StatusCode::CONFLICT, "output_policy_missing"),
            Self::Database(_) => (StatusCode::SERVICE_UNAVAILABLE, "persistence_unavailable"),
        };
        // Database errors can include sensitive query data. Log only the category.
        if status.is_server_error() {
            tracing::error!(error_code = code, "request failed");
        }
        let mut body =
            json!({"error":{"code":code,"message":self.to_string(),"type":"gateway_error"}});
        if let Self::ApprovalRequired(approval) = &self {
            body["error"]["approval"] = approval.clone();
        }
        let mut response = (status, Json(body)).into_response();
        response
            .headers_mut()
            .insert("x-should-retry", "false".parse().expect("static header"));
        response
    }
}

pub type Result<T> = std::result::Result<T, Error>;
