//! API errors: `{code, message}` as CBOR with an HTTP status (api.md §1).

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use zen_proto::{CBOR, ErrorBody, to_cbor};

/// An API error.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Error code.
    pub code: &'static str,
    /// Message.
    pub message: String,
}

impl ApiError {
    /// Build an error.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }
}

/// API result.
pub type ApiResult<T> = Result<T, ApiError>;

macro_rules! ctor {
    ($name:ident, $status:ident, $code:literal) => {
        #[doc = concat!("`", $code, "`")]
        pub fn $name(message: impl Into<String>) -> ApiError {
            ApiError::new(StatusCode::$status, $code, message)
        }
    };
}

ctor!(bad_request, BAD_REQUEST, "bad_request");
ctor!(unauthorized, UNAUTHORIZED, "unauthorized");
ctor!(forbidden, FORBIDDEN, "forbidden");
ctor!(not_found, NOT_FOUND, "not_found");
ctor!(conflict, CONFLICT, "conflict");
ctor!(too_old, CONFLICT, "too_old");
ctor!(version_mismatch, CONFLICT, "version_mismatch");
ctor!(group_exists, CONFLICT, "group_exists");
ctor!(commit_id_reused, CONFLICT, "commit_id_reused");
ctor!(cursor_moved, PRECONDITION_FAILED, "cursor_moved");
ctor!(not_leader, PRECONDITION_FAILED, "not_leader");
ctor!(claim_lost, PRECONDITION_FAILED, "claim_lost");
ctor!(too_large, PAYLOAD_TOO_LARGE, "too_large");
ctor!(quota, TOO_MANY_REQUESTS, "quota");
ctor!(not_implemented, NOT_IMPLEMENTED, "not_implemented");
ctor!(internal, INTERNAL_SERVER_ERROR, "internal");

impl From<zen_store::Error> for ApiError {
    fn from(e: zen_store::Error) -> Self {
        match e {
            zen_store::Error::Conflict => conflict("transaction conflict"),
            zen_store::Error::TooOld => too_old("read version too old"),
            other => {
                tracing::error!(error = %other, "storage error");
                internal("storage error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = to_cbor(&ErrorBody {
            code: self.code.into(),
            message: self.message,
        });
        (self.status, [(header::CONTENT_TYPE, CBOR)], body).into_response()
    }
}
