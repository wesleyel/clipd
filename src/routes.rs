//! HTTP surface. Everything here is shaped by what Apple's Shortcuts app can
//! actually send: a bare GET query, a raw file body, or a multipart form.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Query, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tokio::task;
use tracing::{info, warn};

use crate::clipboard::{ClipboardHandle, Payload};
use crate::imaging;

const TOKEN_HEADER: &str = "x-clipd-token";

pub struct AppState {
    pub clipboard: ClipboardHandle,
    pub token: Option<String>,
    pub notify: bool,
}

pub fn router(state: Arc<AppState>, body_limit: usize) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/set_clipboard", get(set_from_query).post(set_from_body))
        .route("/get_clipboard", get(get_text))
        .route("/get_clipboard/image", get(get_image))
        .route("/get_clipboard/auto", get(get_auto))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok\n"
}

#[derive(Deserialize)]
struct TextQuery {
    text: Option<String>,
}

/// `GET /set_clipboard?text=...` — the shape the original Shortcut used.
async fn set_from_query(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TextQuery>,
) -> Result<Response, AppError> {
    let text = query
        .text
        .ok_or_else(|| AppError::bad_request("missing `text` query parameter"))?;
    store(&state, Payload::Text(text)).await
}

/// `POST /set_clipboard` — accepts text or an image, in whichever body format
/// the caller happened to pick.
async fn set_from_body(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, AppError> {
    let mime = content_type(&request);

    let payload = if mime == "multipart/form-data" {
        from_multipart(request).await?
    } else {
        let bytes = Bytes::from_request(request, &())
            .await
            .map_err(|e| AppError::new(e.status(), e.body_text()))?;
        from_bytes(bytes, &mime).await?
    };

    store(&state, payload).await
}

async fn get_text(State(state): State<Arc<AppState>>) -> Result<Response, AppError> {
    let text = state.clipboard.get_text().await.map_err(|e| {
        AppError::new(
            StatusCode::NOT_FOUND,
            format!("clipboard holds no text: {e}"),
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

async fn get_image(State(state): State<Arc<AppState>>) -> Result<Response, AppError> {
    let image = state.clipboard.get_image().await.map_err(|e| {
        AppError::new(
            StatusCode::NOT_FOUND,
            format!("clipboard holds no image: {e}"),
        )
    })?;
    png_response(image).await
}

/// Text if there is any, otherwise the image, in one request. Shortcuts has no
/// error-handling action, so a client that wants "whatever is on the
/// clipboard" cannot branch on a 404 — it needs a single route that answers
/// with a usable `Content-Type` either way.
async fn get_auto(State(state): State<Arc<AppState>>) -> Result<Response, AppError> {
    if let Ok(text) = state.clipboard.get_text().await {
        return Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response());
    }

    match state.clipboard.get_image().await {
        Ok(image) => png_response(image).await,
        Err(e) => Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("clipboard holds neither text nor an image: {e}"),
        )),
    }
}

async fn png_response(image: crate::clipboard::Image) -> Result<Response, AppError> {
    let png = task::spawn_blocking(move || imaging::encode_png(&image))
        .await
        .map_err(AppError::internal)?
        .map_err(AppError::internal)?;

    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// Content type with any parameters (`; boundary=...`) stripped, lowercased.
fn content_type(request: &Request) -> String {
    strip_params(
        request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    )
}

fn strip_params(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

async fn from_bytes(bytes: Bytes, mime: &str) -> Result<Payload, AppError> {
    if bytes.is_empty() {
        return Err(AppError::bad_request("empty request body"));
    }

    if mime.starts_with("image/") {
        return Ok(Payload::Image(decode_image(bytes).await?));
    }

    if mime == "application/x-www-form-urlencoded" {
        return form_field(&bytes);
    }

    if mime.starts_with("text/") || mime == "application/json" {
        return as_text(&bytes).map(Payload::Text);
    }

    // Unlabelled or `application/octet-stream`: Shortcuts' "file" body type
    // sends whatever the share sheet handed it, so sniff before giving up.
    if imaging::looks_like_image(&bytes) {
        return Ok(Payload::Image(decode_image(bytes).await?));
    }
    as_text(&bytes).map(Payload::Text)
}

async fn from_multipart(request: Request) -> Result<Payload, AppError> {
    let mut multipart = Multipart::from_request(request, &())
        .await
        .map_err(|e| AppError::new(e.status(), e.body_text()))?;

    let mut fallback: Option<Payload> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::new(e.status(), e.body_text()))?
    {
        let mime = strip_params(field.content_type().unwrap_or_default());
        // Shortcuts attaches a filename to plain text just as readily as to a
        // photo, so a file part still goes through the full sniff ladder
        // rather than straight to the image decoder.
        let is_file = field.file_name().is_some() || mime.starts_with("image/");

        let bytes = field
            .bytes()
            .await
            .map_err(|e| AppError::new(e.status(), e.body_text()))?;
        if bytes.is_empty() {
            continue;
        }

        let payload = from_bytes(bytes, &mime).await?;

        // A file part wins outright; other parts are only a fallback.
        if is_file {
            return Ok(payload);
        }
        fallback.get_or_insert(payload);
    }

    fallback.ok_or_else(|| AppError::bad_request("multipart body had no usable field"))
}

fn as_text(bytes: &[u8]) -> Result<String, AppError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| AppError::bad_request("request body is neither a known image nor valid UTF-8"))
}

fn form_field(bytes: &[u8]) -> Result<Payload, AppError> {
    #[derive(Deserialize)]
    struct Form {
        text: String,
    }

    serde_urlencoded::from_bytes::<Form>(bytes)
        .map(|form| Payload::Text(form.text))
        .map_err(|e| AppError::bad_request(format!("form body needs a `text` field: {e}")))
}

async fn decode_image(bytes: Bytes) -> Result<crate::clipboard::Image, AppError> {
    task::spawn_blocking(move || imaging::decode(&bytes))
        .await
        .map_err(AppError::internal)?
        .map_err(|e| AppError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, e.to_string()))
}

async fn store(state: &AppState, payload: Payload) -> Result<Response, AppError> {
    let summary = match &payload {
        Payload::Text(text) => format!("text ({} chars)", text.chars().count()),
        Payload::Image(image) => format!("image ({}x{})", image.width, image.height),
    };

    state
        .clipboard
        .set(payload)
        .await
        .map_err(AppError::internal)?;

    info!("clipboard set: {summary}");
    if state.notify {
        post_notification(summary.clone());
    }

    Ok((StatusCode::OK, format!("ok: {summary}\n")).into_response())
}

/// Fire-and-forget macOS banner. `summary` is generated by us, never echoed
/// user content, so it is safe to drop into an AppleScript string literal.
fn post_notification(summary: String) {
    task::spawn_blocking(move || {
        let script = format!(
            r#"display notification "{summary}" with title "clipd" subtitle "clipboard updated""#
        );
        match std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => warn!("osascript exited with {status}"),
            Err(e) => warn!("could not post notification: {e}"),
        }
    });
}

async fn authorize(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(next.run(request).await);
    };

    let presented = request
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| query_param(request.uri(), "token"));

    match presented {
        Some(token) if tokens_match(&token, expected) => Ok(next.run(request).await),
        _ => Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "bad or missing token",
        )),
    }
}

fn query_param(uri: &Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_owned())
    })
}

/// Length still leaks, which is fine for a LAN shared secret.
fn tokens_match(presented: &str, expected: &str) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(err: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        warn!("{}: {}", self.status, self.message);
        (self.status, format!("{}\n", self.message)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_only_on_equality() {
        assert!(tokens_match("secret", "secret"));
        assert!(!tokens_match("secret", "secreT"));
        assert!(!tokens_match("secret", "secretly"));
        assert!(!tokens_match("", "secret"));
    }

    #[test]
    fn query_param_finds_key_anywhere() {
        let uri: Uri = "/set_clipboard?text=hi&token=abc".parse().unwrap();
        assert_eq!(query_param(&uri, "token").as_deref(), Some("abc"));
        assert_eq!(query_param(&uri, "text").as_deref(), Some("hi"));
        assert_eq!(query_param(&uri, "missing"), None);

        let bare: Uri = "/health".parse().unwrap();
        assert_eq!(query_param(&bare, "token"), None);
    }

    #[test]
    fn content_type_drops_parameters() {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "Multipart/Form-Data; boundary=xyz")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(content_type(&request), "multipart/form-data");

        let bare = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert_eq!(content_type(&bare), "");
    }

    #[tokio::test]
    async fn form_body_needs_a_text_field() {
        let ok = from_bytes(
            Bytes::from_static(b"text=hello+world"),
            "application/x-www-form-urlencoded",
        )
        .await;
        assert!(matches!(ok, Ok(Payload::Text(t)) if t == "hello world"));

        let bad = from_bytes(
            Bytes::from_static(b"other=hello"),
            "application/x-www-form-urlencoded",
        )
        .await;
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn unlabelled_utf8_body_becomes_text() {
        let payload = from_bytes(Bytes::from_static("你好 & bye".as_bytes()), "")
            .await
            .unwrap();
        assert!(matches!(payload, Payload::Text(t) if t == "你好 & bye"));
    }

    #[tokio::test]
    async fn empty_body_is_rejected() {
        assert!(from_bytes(Bytes::new(), "text/plain").await.is_err());
    }
}
