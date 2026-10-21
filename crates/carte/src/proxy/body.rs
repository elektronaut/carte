use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::body::{Body, Frame, SizeHint};

use super::{ActivityGuard, BoxFuture};

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

pub(crate) fn boxed<B>(body: B) -> ProxyBody
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    body.map_err(Into::into).boxed_unsync()
}

/// A body that holds an activity guard until it has been streamed to the end
/// or dropped.
pub(crate) struct GuardedBody<B> {
    inner: B,
    guard: Option<Box<dyn ActivityGuard>>,
}

impl<B> GuardedBody<B> {
    pub(crate) fn new(inner: B, guard: Box<dyn ActivityGuard>) -> Self {
        Self {
            inner,
            guard: Some(guard),
        }
    }
}

impl<B: Body + Unpin> Body for GuardedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            self.guard = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A body that ends with an error once `closed` resolves, for a response whose
/// route closed while it streamed.
pub(crate) struct ClosingBody<T> {
    inner: ProxyBody,
    closed: Option<BoxFuture<'static, T>>,
}

impl<T> ClosingBody<T> {
    pub(crate) fn new(inner: ProxyBody, closed: BoxFuture<'static, T>) -> Self {
        Self {
            inner,
            closed: Some(closed),
        }
    }
}

impl<T> Body for ClosingBody<T> {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let Some(closed) = self.closed.as_mut() else {
            return Poll::Ready(None);
        };
        if closed.as_mut().poll(cx).is_ready() {
            self.closed = None;
            return Poll::Ready(Some(Err("the route closed".into())));
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.closed.is_none() || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
