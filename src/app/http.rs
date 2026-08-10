//! The HTTP client PIKU installs: one that refuses everything.
//!
//! PIKU is a local file manager. It opens no sockets, and the only code that
//! could make it try is gpui's own image loader: `img()` fetches an
//! [`ImageSource::Resource(Resource::Uri(..))`] through `App::http_client`, and
//! gpui-component's markdown renderer turns **every** image in a document into
//! exactly that. `impl From<SharedUri> for ImageSource` has no scheme check, so
//! `![](./local.png)` and `![](https://attacker.example/pixel.png)` take the
//! same path. Selecting a `.md` file in the inspector is enough to trigger it.
//!
//! That has never actually fired, but only by accident: gpui defaults to its
//! own `NullHttpClient` and PIKU never replaced it. An accident is not a
//! security property. Installing this makes "PIKU makes no network requests"
//! something a test can assert and a reviewer can see, and means the day
//! someone adds an update check — a perfectly reasonable thing to add — they
//! have to make a deliberate decision about this client rather than silently
//! arming a beacon that fires on file selection.
//!
//! This is the outer of two gates. The inner one strips the URL sinks out of
//! the document before it is ever rendered
//! (`backend::services::preview::markdown_safe`); either alone closes the hole,
//! and they fail in different ways, which is the point of having both.
//!
//! Scope note: git's network access is unaffected. `gix` fetches over its own
//! reqwest-based transport and never goes through gpui's client.

use futures::future::BoxFuture;
use gpui::http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};

/// An [`HttpClient`] that fails every request.
pub struct DenyAllHttpClient;

impl HttpClient for DenyAllHttpClient {
    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        // Log rather than stay silent: a request reaching here is either a bug
        // or a hostile document, and both are worth seeing. The URI is
        // attacker-controlled, so it goes through the same sanitizer as any
        // other untrusted string that reaches a log or a toast.
        let uri = crate::security::text::sanitize_display(&req.uri().to_string(), 200, false);
        tracing::warn!(target: "piku::http", %uri, "refused an outbound HTTP request");
        Box::pin(async move { anyhow::bail!("PIKU makes no network requests") })
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: Future>(f: F) -> F::Output {
        crate::backend::runtime::get()
            .expect("runtime")
            .handle()
            .block_on(f)
    }

    #[test]
    fn the_http_client_refuses_every_request() {
        let client = DenyAllHttpClient;
        let result = block_on(client.get("https://example.invalid/pixel.png", ().into(), true));
        assert!(result.is_err(), "an outbound request was allowed");
    }

    /// `get` is a provided method that routes through `send`, and it is the one
    /// gpui's image loader calls. Asserting both means overriding `send` is
    /// enough and no other entry point leaks past.
    #[test]
    fn the_refusal_covers_send_and_post_as_well_as_get() {
        let client = DenyAllHttpClient;
        assert!(block_on(client.post_json("https://example.invalid/", ().into())).is_err());
        let request = gpui::http_client::http::Request::builder()
            .uri("https://example.invalid/")
            .body(AsyncBody::empty())
            .expect("request");
        assert!(block_on(client.send(request)).is_err());
    }
}
