//! Bounds accepted data-plane sockets, including keepalive and blocked writes.
//! Body buffers alone cannot account for transport/kernel buffers left after
//! the application has finished producing an SSE response.

use crate::error::{Error, Result};
use axum::{Router, serve::Listener};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use std::{
    future::Future,
    io,
    net::{Shutdown, SocketAddr},
    os::fd::AsFd,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Serve the data plane over HTTP/1 so one socket cannot retain multiplexed
/// response buffers after application delivery permits have been released.
/// A TLS edge may terminate HTTP/2 and forward requests using HTTP/1.1.
pub async fn serve_h1(
    mut listener: BoundedListener,
    router: Router,
    shutdown: impl Future<Output = ()>,
) {
    let connections = TaskTracker::new();
    let stopping = CancellationToken::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            (io, _) = listener.accept() => {
                let service = TowerToHyperService::new(router.clone());
                let stopping = stopping.clone();
                connections.spawn(async move {
                    let connection = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(io), service);
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = stopping.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        },
                    };
                    if let Err(error) = result {
                        tracing::trace!(%error, "data-plane HTTP connection ended");
                    }
                });
            },
        }
    }
    drop(listener);
    stopping.cancel();
    connections.close();
    connections.wait().await;
}

pub struct BoundedListener {
    listener: TcpListener,
    capacity: Arc<Semaphore>,
    lifetime: Duration,
    limit: usize,
    rejected: u64,
}

impl BoundedListener {
    pub fn new(listener: TcpListener, limit: usize, lifetime: Duration) -> Result<Self> {
        if !(1..=1024).contains(&limit) || lifetime.is_zero() {
            return Err(Error::Configuration);
        }
        Ok(Self {
            listener,
            capacity: Arc::new(Semaphore::new(limit)),
            lifetime,
            limit,
            rejected: 0,
        })
    }

    pub fn from_env(listener: TcpListener) -> Result<Self> {
        let number = |name: &str, default: u64| -> Result<u64> {
            std::env::var(name)
                .map(|v| v.parse().map_err(|_| Error::Configuration))
                .unwrap_or(Ok(default))
        };
        let request_timeout = number("BROKERROUTER_REQUEST_TIMEOUT_SECONDS", 120)?;
        let lifetime = number(
            "BROKERROUTER_HTTP_CONNECTION_TIMEOUT_SECONDS",
            request_timeout.saturating_add(60),
        )?;
        if !(1..=900).contains(&lifetime) {
            return Err(Error::Configuration);
        }
        Self::new(
            listener,
            usize::try_from(number("BROKERROUTER_MAX_HTTP_CONNECTIONS", 64)?)
                .map_err(|_| Error::Configuration)?,
            Duration::from_secs(lifetime),
        )
    }
}

impl Listener for BoundedListener {
    type Io = LifetimeIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = Listener::accept(&mut self.listener).await;
            let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
                self.rejected = self.rejected.saturating_add(1);
                // Count every refusal but avoid a log flood under overload.
                if self.rejected.is_power_of_two() {
                    tracing::warn!(
                        error_code = "http_connection_capacity_full",
                        limit = self.limit,
                        rejected_connections = self.rejected,
                        "connection refused before HTTP admission"
                    );
                }
                drop(stream);
                continue;
            };
            let Ok(fd) = stream.as_fd().try_clone_to_owned() else {
                tracing::error!(error_code = "http_connection_setup_failed");
                continue;
            };
            let deadline_socket = std::net::TcpStream::from(fd);
            let permit = Arc::new(permit);
            let deadline_permit = permit.clone();
            let closed = CancellationToken::new();
            let deadline_closed = closed.clone();
            let lifetime = self.lifetime;
            // Shut down the kernel socket even if the service/body future is
            // not polling IO. Both handles retain the same admission permit.
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(lifetime) => {
                        let _ = deadline_socket.shutdown(Shutdown::Both);
                        tracing::info!(error_code = "http_connection_deadline", "connection lifetime ended");
                    },
                    _ = deadline_closed.cancelled() => {},
                }
                drop(deadline_socket);
                drop(deadline_permit);
            });
            return (
                LifetimeIo {
                    stream,
                    closed,
                    _permit: permit,
                },
                address,
            );
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

pub struct LifetimeIo {
    stream: TcpStream,
    closed: CancellationToken,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl Drop for LifetimeIo {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl AsyncRead for LifetimeIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LifetimeIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn server(
        limit: usize,
        lifetime: Duration,
    ) -> (SocketAddr, Arc<Semaphore>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let listener = BoundedListener::new(listener, limit, lifetime).unwrap();
        let capacity = listener.capacity.clone();
        let task = tokio::spawn(async move {
            serve_h1(
                listener,
                Router::new().route("/", get(|| async { "ok" })),
                std::future::pending(),
            )
            .await;
        });
        (address, capacity, task)
    }

    #[tokio::test]
    async fn completed_keepalive_response_keeps_socket_quota_until_client_closes() {
        let (address, capacity, task) = server(1, Duration::from_secs(5)).await;
        let mut first = TcpStream::connect(address).await.unwrap();
        first
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut data = [0; 1024];
        assert!(first.read(&mut data).await.unwrap() > 0);
        assert_eq!(capacity.available_permits(), 0);
        let mut excess = TcpStream::connect(address).await.unwrap();
        excess
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .ok();
        let result = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut data))
            .await
            .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), async {
            while capacity.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut next = TcpStream::connect(address).await.unwrap();
        next.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        assert!(next.read(&mut data).await.unwrap() > 0);
        task.abort();
    }

    #[tokio::test]
    async fn idle_socket_deadline_closes_transport_and_restores_capacity() {
        let (address, capacity, task) = server(1, Duration::from_millis(40)).await;
        let mut idle = TcpStream::connect(address).await.unwrap();
        let mut data = [0; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), idle.read(&mut data))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
        tokio::time::timeout(Duration::from_secs(1), async {
            while capacity.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn h2c_preface_is_rejected_before_http_admission() {
        let (address, _, task) = server(1, Duration::from_secs(5)).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
            .await
            .unwrap();
        let mut data = [0; 128];
        if let Ok(read) = tokio::time::timeout(Duration::from_secs(1), client.read(&mut data))
            .await
            .unwrap()
        {
            assert!(
                read == 0 || data[..read].starts_with(b"HTTP/1."),
                "data-plane accepted an HTTP/2 connection: {:?}",
                &data[..read]
            );
        }
        task.abort();
    }

    #[tokio::test]
    async fn graceful_shutdown_stops_admission_and_finishes_in_flight_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let listener = BoundedListener::new(listener, 1, Duration::from_secs(5)).unwrap();
        let capacity = listener.capacity.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let handler_entered = entered.clone();
        let handler_release = release.clone();
        let router = Router::new().route(
            "/",
            get(move || {
                let entered = handler_entered.clone();
                let release = handler_release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    "settled"
                }
            }),
        );
        let stopping = CancellationToken::new();
        let task = tokio::spawn(serve_h1(
            listener,
            router,
            stopping.clone().cancelled_owned(),
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        stopping.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Ok(peer) = TcpStream::connect(address).await {
                drop(peer);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !task.is_finished(),
            "shutdown cancelled an in-flight response"
        );
        release.notify_one();
        let mut data = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut data))
            .await
            .unwrap()
            .unwrap();
        assert!(data.starts_with(b"HTTP/1.1 200"));
        assert!(data.ends_with(b"settled"));
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while capacity.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn deadline_closes_socket_while_service_is_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let listener = BoundedListener::new(listener, 1, Duration::from_millis(100)).unwrap();
        let capacity = listener.capacity.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let notify = entered.clone();
        let task = tokio::spawn(async move {
            serve_h1(
                listener,
                Router::new().route(
                    "/",
                    get(move || {
                        let notify = notify.clone();
                        async move {
                            notify.notify_one();
                            std::future::pending::<&'static str>().await
                        }
                    }),
                ),
                std::future::pending(),
            )
            .await;
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert_eq!(capacity.available_permits(), 0);
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut [0; 1]))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
        tokio::time::timeout(Duration::from_secs(1), async {
            while capacity.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn blocked_socket_write_hits_deadline_and_releases_its_permit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut listener = BoundedListener::new(listener, 1, Duration::from_millis(50)).unwrap();
        let capacity = listener.capacity.clone();
        let _unread_peer = TcpStream::connect(address).await.unwrap();
        let (mut io, _) = listener.accept().await;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            io.write_all(&vec![b'x'; 16 * 1024 * 1024]),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected
        ));
        drop(io);
        tokio::time::timeout(Duration::from_secs(1), async {
            while capacity.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
