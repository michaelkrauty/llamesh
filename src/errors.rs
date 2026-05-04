use axum::{
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub error: ApiError,
    pub headers: Option<HeaderMap>,
}

impl AppError {
    pub fn new(status: StatusCode, message: impl Into<String>, type_: impl Into<String>) -> Self {
        Self {
            status,
            error: ApiError {
                message: message.into(),
                type_: type_.into(),
                param: None,
                code: None,
            },
            headers: None,
        }
    }

    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.error.param = Some(param.into());
        self
    }

    #[allow(dead_code)]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.error.code = Some(code.into());
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        let headers = self.headers.get_or_insert_with(HeaderMap::new);
        headers.insert(name, value);
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": {
                "message": self.error.message,
                "type": self.error.type_,
                "param": self.error.param,
                "code": self.error.code,
            }
        }));

        let mut response = (self.status, body).into_response();

        if let Some(headers) = self.headers {
            response.headers_mut().extend(headers);
        }

        response
    }
}

// Helper constructors for common errors
impl AppError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message, "invalid_request_error")
    }

    pub fn authentication_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message, "authentication_error")
    }

    pub fn model_not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message, "model_not_found")
    }

    pub fn service_unavailable(message: impl Into<String>, type_: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message, type_)
    }

    pub fn internal_server_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message, "internal_error")
    }

    pub fn request_timeout(message: impl Into<String>) -> Self {
        Self::new(StatusCode::REQUEST_TIMEOUT, message, "request_timeout")
    }

    pub fn client_disconnected() -> Self {
        Self::new(
            StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
            "Client closed connection",
            "client_disconnected",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_error_new() {
        let err = AppError::new(StatusCode::BAD_REQUEST, "test message", "test_type");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error.message, "test message");
        assert_eq!(err.error.type_, "test_type");
        assert!(err.error.param.is_none());
        assert!(err.error.code.is_none());
        assert!(err.headers.is_none());
    }

    #[test]
    fn test_with_param() {
        let err = AppError::new(StatusCode::BAD_REQUEST, "msg", "type").with_param("model");
        assert_eq!(err.error.param, Some("model".to_string()));
    }

    #[test]
    fn test_with_code() {
        let err = AppError::new(StatusCode::BAD_REQUEST, "msg", "type").with_code("ERR001");
        assert_eq!(err.error.code, Some("ERR001".to_string()));
    }

    #[test]
    fn test_with_header() {
        let err = AppError::new(StatusCode::TOO_MANY_REQUESTS, "msg", "type").with_header(
            HeaderName::from_static("retry-after"),
            HeaderValue::from_static("60"),
        );
        assert!(err.headers.is_some());
        let headers = err.headers.unwrap();
        assert_eq!(headers.get("retry-after").unwrap(), "60");
    }

    #[test]
    fn test_invalid_request() {
        let err = AppError::invalid_request("bad input");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error.type_, "invalid_request_error");
    }

    #[test]
    fn test_authentication_error() {
        let err = AppError::authentication_error("invalid key");
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert_eq!(err.error.type_, "authentication_error");
    }

    #[test]
    fn test_model_not_found() {
        let err = AppError::model_not_found("unknown model");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.error.type_, "model_not_found");
    }

    #[test]
    fn test_service_unavailable() {
        let err = AppError::service_unavailable("overloaded", "rate_limit_error");
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.error.type_, "rate_limit_error");
    }

    #[test]
    fn test_internal_server_error() {
        let err = AppError::internal_server_error("oops");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.error.type_, "internal_error");
    }

    #[test]
    fn test_into_response_status() {
        let err = AppError::invalid_request("test error").with_param("model");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_into_response_with_headers() {
        let err = AppError::new(StatusCode::TOO_MANY_REQUESTS, "slow down", "rate_limit")
            .with_header(
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("30"),
            );
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "30");
    }

    #[test]
    fn test_api_error_serialization() {
        let api_error = ApiError {
            message: "test".to_string(),
            type_: "error_type".to_string(),
            param: Some("param".to_string()),
            code: Some("code".to_string()),
        };
        let json = serde_json::to_string(&api_error).unwrap();
        assert!(json.contains("\"message\":\"test\""));
        assert!(json.contains("\"type\":\"error_type\""));
    }

    #[test]
    fn test_api_error_deserialization() {
        let json = r#"{"message":"err","type":"t","param":null,"code":null}"#;
        let api_error: ApiError = serde_json::from_str(json).unwrap();
        assert_eq!(api_error.message, "err");
        assert_eq!(api_error.type_, "t");
        assert!(api_error.param.is_none());
    }

    #[test]
    fn test_builder_chain() {
        let err = AppError::new(StatusCode::BAD_REQUEST, "msg", "type")
            .with_param("p")
            .with_code("c")
            .with_header(
                HeaderName::from_static("x-custom"),
                HeaderValue::from_static("val"),
            );
        assert_eq!(err.error.param, Some("p".to_string()));
        assert_eq!(err.error.code, Some("c".to_string()));
        assert!(err.headers.is_some());
    }
}
