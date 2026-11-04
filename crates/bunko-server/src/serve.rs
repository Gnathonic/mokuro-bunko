//! The HTTP(S) listener: hyper 1 with HTTP/1.1, optional rustls, graceful shutdown on
//! SIGINT/SIGTERM (0.5.2 only handled Ctrl+C, so `docker stop` skipped all cleanup).

use axum::Router;
use axum::extract::ConnectInfo;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::Service;
use tracing::{debug, info, warn};

/// Peer address of the connection, inserted into every request's extensions.
pub type PeerAddr = ConnectInfo<SocketAddr>;

/// Resolve when SIGINT or SIGTERM arrives.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// Most connections served at once.
pub const MAX_CONNECTIONS: usize = 1024;

/// Serve `app` on `listener` until `stop` is cancelled; open connections get
/// `grace` to finish. Long-lived connections (processor WebSockets, downloads) are
/// cut at the end of the grace period.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    stop: CancellationToken,
    grace: Duration,
) -> std::io::Result<()> {
    serve_with(listener, app, tls, stop, grace, None).await
}

/// [`serve`], counting open connections in `gauges` (the analytics' host view).
pub async fn serve_with(
    listener: TcpListener,
    app: Router,
    tls: Option<Arc<rustls::ServerConfig>>,
    stop: CancellationToken,
    grace: Duration,
    gauges: Option<Arc<crate::analytics::Gauges>>,
) -> std::io::Result<()> {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let tracker = tokio_util::task::TaskTracker::new();
    let conn_stop = CancellationToken::new();
    // Last resort: the connections still serving when the shutdown's waits run out
    // are dropped (their responses with them) before this returns, so every response
    // records (as aborted) before the analytics write their last window.
    let conn_abort = CancellationToken::new();
    // Bounded concurrency: past this many open connections, accepting waits (the
    // kernel backlog queues new clients) instead of growing memory without limit.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let permit = tokio::select! {
            p = slots.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
            _ = stop.cancelled() => break,
        };
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    // EMFILE and friends: back off rather than spin.
                    warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = stop.cancelled() => break,
        };
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        let acceptor = acceptor.clone();
        let conn_stop = conn_stop.clone();
        let conn_abort = conn_abort.clone();
        let open = gauges.as_ref().map(|g| g.connection());
        tracker.spawn(async move {
            let _permit = permit;
            let _open = open;
            let svc = hyper::service::service_fn(
                move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(ConnectInfo(peer));
                    let mut app = app.clone();
                    async move { app.call(req.map(axum::body::Body::new)).await }
                },
            );
            // HTTP/1.1 only (as 0.5.2): no h2c surface with its own limits to tune.
            let mut builder = auto::Builder::new(TokioExecutor::new()).http1_only();
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30))
                .keep_alive(true);
            let result = match acceptor {
                Some(acceptor) => {
                    match tokio::time::timeout(Duration::from_secs(15), acceptor.accept(stream))
                        .await
                    {
                        Ok(Ok(tls)) => {
                            serve_conn(&builder, TokioIo::new(tls), svc, &conn_stop, &conn_abort)
                                .await
                        }
                        Ok(Err(e)) => {
                            debug!("TLS handshake with {peer} failed: {e}");
                            return;
                        }
                        Err(_) => return,
                    }
                }
                None => {
                    serve_conn(&builder, TokioIo::new(stream), svc, &conn_stop, &conn_abort).await
                }
            };
            if let Err(e) = result {
                debug!("connection {peer}: {e}");
            }
        });
    }
    info!("Shutting down...");
    tracker.close();
    if tokio::time::timeout(grace, tracker.wait()).await.is_err() {
        conn_stop.cancel();
        if tokio::time::timeout(Duration::from_secs(2), tracker.wait())
            .await
            .is_err()
        {
            conn_abort.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(1), tracker.wait()).await;
        }
    }
    Ok(())
}

async fn serve_conn<I, S>(
    builder: &auto::Builder<TokioExecutor>,
    io: I,
    svc: S,
    stop: &CancellationToken,
    abort: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    S: hyper::service::Service<
            hyper::Request<hyper::body::Incoming>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    let conn = builder.serve_connection_with_upgrades(io, svc);
    tokio::pin!(conn);
    tokio::select! {
        r = conn.as_mut() => r,
        _ = stop.cancelled() => {
            conn.as_mut().graceful_shutdown();
            tokio::select! {
                r = conn.as_mut() => r,
                // Dropping the connection drops the response it was sending.
                _ = abort.cancelled() => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A response still streaming when the shutdown's waits run out is dropped before
    /// `serve_with` returns: its body's end (the request accounting's record) happens
    /// before the analytics write their last window, not at process exit after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_response_still_streaming_at_shutdown_is_dropped_before_serve_returns() {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let app = Router::new().route(
            "/",
            axum::routing::get(move || {
                let guard = Dropped(flag.clone());
                async move {
                    let chunks = futures_util::stream::unfold(guard, |g| async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        Some((
                            Ok::<_, std::io::Error>(bytes::Bytes::from_static(&[0u8; 1024])),
                            g,
                        ))
                    });
                    axum::body::Body::from_stream(chunks)
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let server = tokio::spawn(serve_with(
            listener,
            app,
            None,
            stop.clone(),
            Duration::from_millis(100),
            None,
        ));
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0);
        stop.cancel();
        // The client keeps reading slowly; the download never ends by itself.
        let reader = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = client.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        server.await.unwrap().unwrap();
        assert!(
            dropped.load(Ordering::SeqCst),
            "the streaming response outlived serve_with"
        );
        reader.abort();
    }
}
