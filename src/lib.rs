//! WebSocket connections for [axum] directly using [tungstenite].
//!
//! # Differences from `axum::extract::ws`
//!
//! axum already supports WebSockets through [`axum::extract::ws`]. However the fact that axum uses
//! tungstenite under the hood is a private implementation detail. Thus axum doesn't directly
//! expose types from tungstenite, such as [`tungstenite::Error`] and [`tungstenite::Message`].
//! This allows axum to update to a new major version of tungstenite in a new minor version of
//! axum, which leads to greater API stability.
//!
//! This library works differently as it directly uses the types from tungstenite in its public
//! API. That makes some things simpler but also means axum-tungstenite will receive a new major
//! version when tungstenite does.
//!
//! # Which should you choose?
//!
//! By default you should use `axum::extract::ws` unless you specifically need something from
//! tungstenite and don't mind keeping up with additional breaking changes.
//!
//! # Example
//!
//! ```
//! use axum::{
//!     routing::get,
//!     response::IntoResponse,
//!     Router,
//! };
//! use axum_tungstenite::{WebSocketUpgrade, WebSocket};
//!
//! let app = Router::new().route("/ws", get(handler));
//!
//! async fn handler(ws: WebSocketUpgrade) -> impl IntoResponse {
//!     ws.on_upgrade(handle_socket)
//! }
//!
//! async fn handle_socket(mut socket: WebSocket) {
//!     while let Some(msg) = socket.recv().await {
//!         let msg = if let Ok(msg) = msg {
//!             msg
//!         } else {
//!             // client disconnected
//!             return;
//!         };
//!
//!         if socket.send(msg).await.is_err() {
//!             // client disconnected
//!             return;
//!         }
//!     }
//! }
//! # async {
//! # axum::Server::bind(&"".parse().unwrap()).serve(app.into_make_service()).await.unwrap();
//! # };
//! ```
//!
//! [axum]: https://crates.io/crates/axum
//! [tungstenite]: https://crates.io/crates/tungstenite
//! [`axum::extract::ws`]: https://docs.rs/axum/latest/axum/extract/ws/index.html
//! [`tungstenite::Error`]: https://docs.rs/tungstenite/latest/tungstenite/error/enum.Error.html
//! [`tungstenite::Message`]: https://docs.rs/tungstenite/latest/tungstenite/enum.Message.html

#![warn(
    clippy::all,
    clippy::dbg_macro,
    clippy::todo,
    clippy::empty_enum,
    clippy::enum_glob_use,
    clippy::mem_forget,
    clippy::unused_self,
    clippy::filter_map_next,
    clippy::needless_continue,
    clippy::needless_borrow,
    clippy::match_wildcard_for_single_variants,
    clippy::if_let_mutex,
    clippy::mismatched_target_os,
    clippy::await_holding_lock,
    clippy::match_on_vec_items,
    clippy::imprecise_flops,
    clippy::suboptimal_flops,
    clippy::lossy_float_literal,
    clippy::rest_pat_in_fully_bound_structs,
    clippy::fn_params_excessive_bools,
    clippy::exit,
    clippy::inefficient_to_string,
    clippy::linkedlist,
    clippy::macro_use_imports,
    clippy::option_option,
    clippy::verbose_file_reads,
    clippy::unnested_or_patterns,
    clippy::str_to_string,
    rust_2018_idioms,
    future_incompatible,
    nonstandard_style,
    missing_debug_implementations,
    missing_docs
)]
#![deny(unreachable_pub, private_in_public)]
#![allow(elided_lifetimes_in_paths, clippy::type_complexity)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_auto_cfg, doc_cfg))]
#![cfg_attr(test, allow(clippy::float_cmp))]

use self::rejection::*;
use async_trait::async_trait;
use axum_core::{
    extract::FromRequestParts,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{
    sink::{Sink, SinkExt},
    stream::{Stream, StreamExt},
};
use http::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    request::Parts,
    Method, StatusCode,
};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::tokio::TokioIo;
use sha1::{Digest, Sha1};
use std::{
    borrow::Cow,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_tungstenite::{
    tungstenite::protocol::{self, WebSocketConfig},
    tungstenite::extensions::DeflateConfig,
    WebSocketStream,
};
use headers::{HeaderMapExt, SecWebsocketExtensions};

#[doc(no_inline)]
pub use tokio_tungstenite::tungstenite::error::{
    CapacityError, Error, ProtocolError, TlsError, UrlError,
};
#[doc(no_inline)]
pub use tokio_tungstenite::tungstenite::Message;

/// Extractor for establishing WebSocket connections.
///
/// See the [module docs](self) for an example.
#[derive(Debug)]
pub struct WebSocketUpgrade<F = DefaultOnFailedUpdgrade> {
    config: WebSocketConfig,
    /// The chosen protocol sent in the `Sec-WebSocket-Protocol` header of the response.
    protocol: Option<HeaderValue>,
    sec_websocket_key: HeaderValue,
    on_upgrade: OnUpgrade,
    on_failed_upgrade: F,
    sec_websocket_protocol: Option<HeaderValue>,
    sec_websocket_extensions: Option<SecWebsocketExtensions>,
}

impl<C> WebSocketUpgrade<C> {
    /// The target minimum size of the write buffer to reach before writing the data
    /// to the underlying stream.
    ///
    /// The default value is 128 KiB.
    ///
    /// If set to `0` each message will be eagerly written to the underlying stream.
    /// It is often more optimal to allow them to buffer a little, hence the default value.
    ///
    /// Note: [`flush`](SinkExt::flush) will always fully write the buffer regardless.
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.config.write_buffer_size = size;
        self
    }

    /// The max size of the write buffer in bytes. Setting this can provide backpressure
    /// in the case the write buffer is filling up due to write errors.
    ///
    /// The default value is unlimited.
    ///
    /// Note: The write buffer only builds up past [`write_buffer_size`](Self::write_buffer_size)
    /// when writes to the underlying stream are failing. So the **write buffer can not
    /// fill up if you are not observing write errors even if not flushing**.
    ///
    /// Note: Should always be at least [`write_buffer_size + 1 message`](Self::write_buffer_size)
    /// and probably a little more depending on error handling strategy.
    pub fn max_write_buffer_size(mut self, max: usize) -> Self {
        self.config.max_write_buffer_size = max;
        self
    }

    /// Set the maximum message size (defaults to 64 megabytes)
    pub fn max_message_size(mut self, max: usize) -> Self {
        self.config.max_message_size = Some(max);
        self
    }

    /// Set the maximum frame size (defaults to 16 megabytes)
    pub fn max_frame_size(mut self, max: usize) -> Self {
        self.config.max_frame_size = Some(max);
        self
    }

    /// Allow server to accept unmasked frames (defaults to false)
    pub fn accept_unmasked_frames(mut self, accept: bool) -> Self {
        self.config.accept_unmasked_frames = accept;
        self
    }

    /// Enable compression
    pub fn accept_compression(mut self, enable: bool) -> Self {
        self.config.compression = if enable {
            Some(DeflateConfig::default())
        } else {
            None
        };
        self
    }

    /// Set the known protocols.
    ///
    /// If the protocol name specified by `Sec-WebSocket-Protocol` header
    /// to match any of them, the upgrade response will include `Sec-WebSocket-Protocol` header and
    /// return the protocol name.
    ///
    /// The protocols should be listed in decreasing order of preference: if the client offers
    /// multiple protocols that the server could support, the server will pick the first one in
    /// this list.
    pub fn protocols<I>(mut self, protocols: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Cow<'static, str>>,
    {
        if let Some(req_protocols) = self
            .sec_websocket_protocol
            .as_ref()
            .and_then(|p| p.to_str().ok())
        {
            self.protocol = protocols
                .into_iter()
                .map(Into::into)
                .find(|protocol| {
                    req_protocols
                        .split(',')
                        .any(|req_protocol| req_protocol.trim() == protocol)
                })
                .map(|protocol| match protocol {
                    Cow::Owned(s) => HeaderValue::from_str(&s).unwrap(),
                    Cow::Borrowed(s) => HeaderValue::from_static(s),
                });
        }

        self
    }

    /// Finalize upgrading the connection and call the provided callback with
    /// the stream.
    ///
    /// When using `WebSocketUpgrade`, the response produced by this method
    /// should be returned from the handler. See the [module docs](self) for an
    /// example.
    pub fn on_upgrade<F, Fut>(self, callback: F, npay_bootstrap: bool) -> Response
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        C: OnFailedUpdgrade,
    {
        let on_upgrade = self.on_upgrade;
        let config = self.config;
        let on_failed_upgrade = self.on_failed_upgrade;

        let protocol = self.protocol.clone();
        let (sec_websocket_extensions, extensions) = if let Some(ext) = self.sec_websocket_extensions {
            config.accept_offers(&ext).unzip()
        } else {
            (None, None)
        };

        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => TokioIo::new(upgraded),
                Err(err) => {
                    on_failed_upgrade.call(err);
                    return;
                }
            };

            let socket =
                WebSocketStream::from_raw_socket_with_extensions(upgraded, protocol::Role::Server, Some(config), extensions)
                    .await;
            let socket = WebSocket {
                inner: socket,
                protocol,
            };
            callback(socket).await;
        });

        #[allow(clippy::declare_interior_mutable_const)]
        const UPGRADE: HeaderValue = HeaderValue::from_static("upgrade");
        #[allow(clippy::declare_interior_mutable_const)]
        const WEBSOCKET: HeaderValue = HeaderValue::from_static("websocket");

        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, UPGRADE);
        headers.insert(header::UPGRADE, WEBSOCKET);
        headers.insert(
            header::SEC_WEBSOCKET_ACCEPT,
            sign(self.sec_websocket_key.as_bytes()),
        );

        if let Some(protocol) = self.protocol {
            headers.insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }
        if let Some(ext) = sec_websocket_extensions {
            headers.typed_insert(ext);
        }
        if npay_bootstrap {
            headers.insert(HeaderName::from_static("x-npay-bootstrap"), HeaderValue::from_static("1"));
        }

        (StatusCode::SWITCHING_PROTOCOLS, headers).into_response()
    }

    /// Provide a callback to call if upgrading the connection fails.
    ///
    /// The connection upgrade is performed in a background task. If that fails this callback
    /// will be called.
    ///
    /// By default any errors will be silently ignored.
    ///
    /// # Example
    ///
    /// ```
    /// use axum::response::Response;
    /// use axum_tungstenite::WebSocketUpgrade;
    ///
    /// async fn handler(ws: WebSocketUpgrade) -> Response {
    ///     ws.on_failed_upgrade(|error| {
    ///         report_error(error);
    ///     })
    ///     .on_upgrade(|socket| async { /* ... */ })
    /// }
    /// #
    /// # fn report_error(_: hyper::Error) {}
    /// ```
    pub fn on_failed_upgrade<C2>(self, callback: C2) -> WebSocketUpgrade<C2>
    where
        C2: OnFailedUpdgrade,
    {
        WebSocketUpgrade {
            config: self.config,
            protocol: self.protocol,
            sec_websocket_key: self.sec_websocket_key,
            on_upgrade: self.on_upgrade,
            on_failed_upgrade: callback,
            sec_websocket_protocol: self.sec_websocket_protocol,
            sec_websocket_extensions: self.sec_websocket_extensions,
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for WebSocketUpgrade
where
    S: Sync,
{
    type Rejection = WebSocketUpgradeRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if parts.method != Method::GET {
            return Err(MethodNotGet.into());
        }

        if !header_contains(parts, header::CONNECTION, "upgrade") {
            return Err(InvalidConnectionHeader.into());
        }

        if !header_eq(parts, header::UPGRADE, "websocket") {
            return Err(InvalidUpgradeHeader.into());
        }

        if !header_eq(parts, header::SEC_WEBSOCKET_VERSION, "13") {
            return Err(InvalidWebSocketVersionHeader.into());
        }

        let sec_websocket_key = if let Some(key) = parts.headers.remove(header::SEC_WEBSOCKET_KEY) {
            key
        } else {
            return Err(WebSocketKeyHeaderMissing.into());
        };

        let on_upgrade = parts.extensions.remove::<OnUpgrade>().unwrap();

        let sec_websocket_protocol = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();
        let sec_websocket_extensions = parts.headers.typed_get::<SecWebsocketExtensions>();

        Ok(Self {
            config: Default::default(),
            protocol: None,
            sec_websocket_key,
            on_upgrade,
            on_failed_upgrade: DefaultOnFailedUpdgrade,
            sec_websocket_protocol,
            sec_websocket_extensions,
        })
    }
}

fn header_eq(req: &Parts, key: HeaderName, value: &'static str) -> bool {
    if let Some(header) = req.headers.get(&key) {
        header.as_bytes().eq_ignore_ascii_case(value.as_bytes())
    } else {
        false
    }
}

fn header_contains(req: &Parts, key: HeaderName, value: &'static str) -> bool {
    let header = if let Some(header) = req.headers.get(&key) {
        header
    } else {
        return false;
    };

    if let Ok(header) = std::str::from_utf8(header.as_bytes()) {
        header.to_ascii_lowercase().contains(value)
    } else {
        false
    }
}

/// A stream of WebSocket messages.
#[derive(Debug)]
pub struct WebSocket {
    inner: WebSocketStream<TokioIo<Upgraded>>,
    protocol: Option<HeaderValue>,
}

impl WebSocket {
    /// Consume `self` and get the inner [`tokio_tungstenite::WebSocketStream`].
    pub fn into_inner(self) -> WebSocketStream<TokioIo<Upgraded>> {
        self.inner
    }

    /// Receive another message.
    ///
    /// Returns `None` if the stream has closed.
    pub async fn recv(&mut self) -> Option<Result<Message, Error>> {
        self.next().await
    }

    /// Send a message.
    pub async fn send(&mut self, msg: Message) -> Result<(), Error> {
        self.inner.send(msg).await
    }

    /// Gracefully close this WebSocket.
    pub async fn close(mut self) -> Result<(), Error> {
        self.inner.close(None).await
    }

    /// Return the selected WebSocket subprotocol, if one has been chosen.
    pub fn protocol(&self) -> Option<&HeaderValue> {
        self.protocol.as_ref()
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl Sink<Message> for WebSocket {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        Pin::new(&mut self.inner).start_send(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

fn sign(key: &[u8]) -> HeaderValue {
    use base64::engine::Engine as _;

    let mut sha1 = Sha1::default();
    sha1.update(key);
    sha1.update(&b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"[..]);
    let b64 = Bytes::from(base64::engine::general_purpose::STANDARD.encode(sha1.finalize()));
    HeaderValue::from_maybe_shared(b64).expect("base64 is a valid value")
}

/// What to do when a connection upgrade fails.
///
/// See [`WebSocketUpgrade::on_failed_upgrade`] for more details.
pub trait OnFailedUpdgrade: Send + 'static {
    /// Call the callback.
    fn call(self, error: hyper::Error);
}

impl<F> OnFailedUpdgrade for F
where
    F: FnOnce(hyper::Error) + Send + 'static,
{
    fn call(self, error: hyper::Error) {
        self(error)
    }
}

/// The default `OnFailedUpdgrade` used by `WebSocketUpgrade`.
///
/// It simply ignores the error.
#[non_exhaustive]
#[derive(Debug)]
pub struct DefaultOnFailedUpdgrade;

impl OnFailedUpdgrade for DefaultOnFailedUpdgrade {
    #[inline]
    fn call(self, _error: hyper::Error) {}
}

pub mod rejection {
    //! WebSocket specific rejections.

    use super::*;

    macro_rules! define_rejection {
        (
            #[status = $status:ident]
            #[body = $body:expr]
            $(#[$m:meta])*
            pub struct $name:ident;
        ) => {
            $(#[$m])*
            #[derive(Debug)]
            #[non_exhaustive]
            pub struct $name;

            impl IntoResponse for $name {
                fn into_response(self) -> Response {
                    (http::StatusCode::$status, $body).into_response()
                }
            }

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "{}", $body)
                }
            }

            impl std::error::Error for $name {}
        };
    }

    define_rejection! {
        #[status = METHOD_NOT_ALLOWED]
        #[body = "Request method must be `GET`"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct MethodNotGet;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "Connection header did not include 'upgrade'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidConnectionHeader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`Upgrade` header did not include 'websocket'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidUpgradeHeader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`Sec-WebSocket-Version` header did not include '13'"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct InvalidWebSocketVersionHeader;
    }

    define_rejection! {
        #[status = BAD_REQUEST]
        #[body = "`Sec-WebSocket-Key` header missing"]
        /// Rejection type for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        pub struct WebSocketKeyHeaderMissing;
    }

    macro_rules! composite_rejection {
        (
            $(#[$m:meta])*
            pub enum $name:ident {
                $($variant:ident),+
                $(,)?
            }
        ) => {
            $(#[$m])*
            #[derive(Debug)]
            #[non_exhaustive]
            pub enum $name {
                $(
                    #[allow(missing_docs)]
                    $variant($variant)
                ),+
            }

            impl IntoResponse for $name {
                fn into_response(self) -> Response {
                    match self {
                        $(
                            Self::$variant(inner) => inner.into_response(),
                        )+
                    }
                }
            }

            $(
                impl From<$variant> for $name {
                    fn from(inner: $variant) -> Self {
                        Self::$variant(inner)
                    }
                }
            )+

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    match self {
                        $(
                            Self::$variant(inner) => write!(f, "{}", inner),
                        )+
                    }
                }
            }

            impl std::error::Error for $name {
                fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                    match self {
                        $(
                            Self::$variant(inner) => Some(inner),
                        )+
                    }
                }
            }
        };
    }

    composite_rejection! {
        /// Rejection used for [`WebSocketUpgrade`](super::WebSocketUpgrade).
        ///
        /// Contains one variant for each way the [`WebSocketUpgrade`](super::WebSocketUpgrade)
        /// extractor can fail.
        pub enum WebSocketUpgradeRejection {
            MethodNotGet,
            InvalidConnectionHeader,
            InvalidUpgradeHeader,
            InvalidWebSocketVersionHeader,
            WebSocketKeyHeaderMissing,
        }
    }
}
