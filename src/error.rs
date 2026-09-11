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
            Self::Database(_) => (StatusCode::SERVICE_UNAVAILABLE, "persistence_unavailable"),
        };
        // Database errors can include sensitive query data. Log only the category.
        if status.is_server_error() {
            tracing::error!(error_code = code, "request failed");
        }
        let mut response = (
            status,
            Json(json!({"error":{"code":code,"message":self.to_string(),"type":"gateway_error"}})),
        )
            .into_response();
        response
            .headers_mut()
            .insert("x-should-retry", "false".parse().expect("static header"));
        response
    }
}

pub type Result<T> = std::result::Result<T, Error>;
