use axum::{body::Body, response::Response};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::OwnedSemaphorePermit,
    time::{Instant, Sleep},
};

struct GuardedBody {
    inner: Body,
    timer: Pin<Box<Sleep>>,
    idle: Duration,
    permit: Option<OwnedSemaphorePermit>,
    done: bool,
}
impl HttpBody for GuardedBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                let deadline = Instant::now() + self.idle;
                self.timer.as_mut().reset(deadline);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(result) => {
                self.done = true;
                self.permit.take();
                Poll::Ready(result)
            }
            Poll::Pending => {
                if self.timer.as_mut().poll(cx).is_ready() {
                    self.done = true;
                    self.inner = Body::empty();
                    self.permit.take();
                    Poll::Ready(Some(Err(axum::Error::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "response body idle timeout",
                    )))))
                } else {
                    Poll::Pending
                }
            }
        }
    }
    fn is_end_stream(&self) -> bool {
        self.done || self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
pub fn guard(response: Response, permit: OwnedSemaphorePermit, idle: Duration) -> Response {
    let (parts, inner) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(GuardedBody {
            inner,
            timer: Box::pin(tokio::time::sleep(idle)),
            idle,
            permit: Some(permit),
            done: false,
        }),
    )
}
