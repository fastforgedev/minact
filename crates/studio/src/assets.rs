//! Serves the embedded front-end.
//!
//! The Studio front-end is a TanStack Start build in SPA mode: a prerendered
//! `index.html` shell plus content-hashed assets. Anything that is not a real
//! file falls back to the shell so the client router owns the URL space.
//!
//! In debug builds `rust-embed` reads from disk, so `npm run build` is picked
//! up without recompiling.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/web/dist/client"]
struct Assets;

/// Cache-Control for content-hashed files, which can never change in place.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// The shell must be revalidated or a rebuilt Studio keeps serving stale JS.
const NO_CACHE: &str = "no-cache";

pub async fn handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    if let Some(response) = serve(path, IMMUTABLE) {
        return response;
    }

    // Unknown path: hand it to the client router via the SPA shell.
    serve("index.html", NO_CACHE).unwrap_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "minact studio: no front-end assets are embedded in this binary.\n\
             Build them with `cd crates/studio/web && npm run build`, then rebuild.",
        )
            .into_response()
    })
}

fn serve(path: &str, cache_control: &str) -> Option<Response> {
    if path.is_empty() {
        return None;
    }

    let file = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();

    Some(
        (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache_control),
            ],
            file.data.into_owned(),
        )
            .into_response(),
    )
}
