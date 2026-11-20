//! Accepted-connection admission, shared across HTTP and HTTPS listeners.
//! The permit is acquired before a TLS handshake and follows the socket until
//! the connection (including an upgraded tunnel) closes.
use axum_server::accept::Accept;
use std::{future::Future, io, pin::Pin, sync::Arc, task::{Context, Poll}};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore},
};

#[derive(Clone)]
pub struct LimitedAcceptor<A> {
    inner: A,
    slots: Arc<Semaphore>,
}

impl<A> LimitedAcceptor<A> {
    pub fn new(inner: A, slots: Arc<Semaphore>) -> Self {
        Self { inner, slots }
    }
}

pub struct LimitedIo<I> {
    io: I,
    _permit: OwnedSemaphorePermit,
}

impl<I: AsyncRead + Unpin> AsyncRead for LimitedIo<I> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl<I: AsyncWrite + Unpin> AsyncWrite for LimitedIo<I> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
    fn is_write_vectored(&self) -> bool { self.io.is_write_vectored() }
    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }
}

impl<A, S> Accept<TcpStream, S> for LimitedAcceptor<A>
where
    A: Accept<TcpStream, S> + Clone + Send + Sync + 'static,
    A::Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    A::Service: Send + 'static,
    A::Future: Send + 'static,
    S: Send + 'static,
{
    type Stream = LimitedIo<A::Stream>;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let slots = Arc::clone(&self.slots);
        let inner = self.inner.clone();
        Box::pin(async move {
            let permit = slots.try_acquire_owned().map_err(|_| {
                io::Error::new(io::ErrorKind::ConnectionRefused, "connection limit exceeded")
            })?;
            let (io, service) = inner.accept(stream, service).await?;
            Ok((LimitedIo { io, _permit: permit }, service))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_server::accept::DefaultAcceptor;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn socket_permits_release_on_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let acceptor = LimitedAcceptor::new(DefaultAcceptor, Arc::clone(&slots));

        let _client1 = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server1, _) = listener.accept().await.unwrap();
        let (limited, _) = acceptor.accept(server1, ()).await.unwrap();
        assert_eq!(slots.available_permits(), 0);

        let _client2 = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server2, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(server2, ()).await.is_err());
        drop(limited);
        assert_eq!(slots.available_permits(), 1);
    }
}
