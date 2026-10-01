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
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let tracker = tokio_util::task::TaskTracker::new();
    let conn_stop = CancellationToken::new();
    loop {
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
        tracker.spawn(async move {
            let svc = hyper::service::service_fn(
                move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(ConnectInfo(peer));
                    let mut app = app.clone();
                    async move { app.call(req.map(axum::body::Body::new)).await }
                },
            );
            let mut builder = auto::Builder::new(TokioExecutor::new());
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
                            serve_conn(&builder, TokioIo::new(tls), svc, &conn_stop).await
                        }
                        Ok(Err(e)) => {
                            debug!("TLS handshake with {peer} failed: {e}");
                            return;
                        }
                        Err(_) => return,
                    }
                }
                None => serve_conn(&builder, TokioIo::new(stream), svc, &conn_stop).await,
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
        let _ = tokio::time::timeout(Duration::from_secs(2), tracker.wait()).await;
    }
    Ok(())
}

async fn serve_conn<I, S>(
    builder: &auto::Builder<TokioExecutor>,
    io: I,
    svc: S,
    stop: &CancellationToken,
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
            conn.await
        }
    }
}
