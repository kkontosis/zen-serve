//! CBOR request extractor and response wrapper.

use crate::error::{ApiError, bad_request};
use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zen_proto::{CBOR, from_cbor, to_cbor};

/// A CBOR body (request or response).
pub struct Cbor<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Cbor<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError::new(e.status(), "too_large", e.body_text()))?;
        if bytes.is_empty() {
            // An empty body is `{}`.
            return from_cbor(&[0xa0])
                .map(Cbor)
                .map_err(|e| bad_request(format!("CBOR: {e}")));
        }
        from_cbor(&bytes)
            .map(Cbor)
            .map_err(|e| bad_request(format!("CBOR: {e}")))
    }
}

impl<T: Serialize> IntoResponse for Cbor<T> {
    fn into_response(self) -> Response {
        ([(header::CONTENT_TYPE, CBOR)], to_cbor(&self.0)).into_response()
    }
}
