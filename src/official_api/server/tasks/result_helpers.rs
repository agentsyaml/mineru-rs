// Shared error constructors for the server-side job/task modules.
use crate::official_api::wire::WireError;
use axum::http::StatusCode;

pub(crate) fn error(status: StatusCode, code: &'static str, message: &'static str) -> WireError {
    WireError {
        status,
        code,
        message,
        param: None,
    }
}

pub(crate) fn not_found() -> WireError {
    WireError {
        status: StatusCode::NOT_FOUND,
        code: "not_found",
        message: "The requested resource was not found.",
        param: None,
    }
}

pub(crate) fn internal() -> WireError {
    WireError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "server_error",
        message: "The request could not be processed.",
        param: None,
    }
}
