use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
    #[error(transparent)]
    Template(#[from] askama::Error),
    #[error("{0}")]
    BadRequest(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (code, msg) = match &self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            AppError::Sqlx(sqlx::Error::RowNotFound) => {
                (StatusCode::NOT_FOUND, "Not found".to_string())
            }
            other => {
                tracing::error!("internal error: {other:?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>Error</title>\
             <body style=\"font-family:system-ui,sans-serif;background:#15171c;color:#e8e8e8;padding:3rem\">\
             <h1>{} {}</h1><p>{}</p><p><a style=\"color:#7aa2f7\" href=\"/\">\u{2190} Back</a></p>",
            code.as_u16(),
            code.canonical_reason().unwrap_or(""),
            html_escape(&msg),
        );
        (code, Html(body)).into_response()
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
