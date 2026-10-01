//! The connection loop: hyper HTTP/1 connections with a header-read timeout, each in a tracked
//! task, so the shutdown deadline really ends them.
//!
//! - **Header-read timeout** (`server.header_read_timeout_secs`): a client must send a request's
//!   headers within it, else the connection is closed (against slow-header attacks); it also
//!   closes idle keep-alive connections after that long. axum's own `serve` sets no timer, so
//!   hyper's default timeout would never fire.
//! - **Shutdown:** on the signal the listener is closed and every connection is told to finish
//!   its current request and close; after the grace period the remaining connection tasks are
//!   aborted, which drops their in-flight handlers (they never complete after the modules and the
//!   pool shut down).
//! - HTTP upgrades stay possible (for the WebSocket hub).

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tower::ServiceExt;

use crate::shutdown::Shutdown;

/// Serve `router` on `listener` until `shutdown` fires, then drain within `grace`.
pub(crate) async fn serve_connections(
    listener: TcpListener,
    router: Router,
    shutdown: Shutdown,
    header_timeout: Duration,
    grace: Duration,
    name: &'static str,
) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    tasks.spawn(serve_one(stream, peer, router.clone(), shutdown.clone(), header_timeout));
                }
                Err(error) => {
                    // E.g. too many open files: back off instead of spinning.
                    tracing::warn!(listener = name, %error, "accepting a connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            _ = shutdown.wait() => break,
        }
    }
    drop(listener);
    let drained = tokio::time::timeout(grace, async { while tasks.join_next().await.is_some() {} }).await;
    if drained.is_err() {
        tracing::warn!(listener = name, remaining = tasks.len(), "shutdown grace period over; the remaining connections are dropped");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

async fn serve_one(stream: TcpStream, peer: SocketAddr, router: Router, shutdown: Shutdown, header_timeout: Duration) {
    let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
        let mut request = request.map(axum::body::Body::new);
        request.extensions_mut().insert(ConnectInfo(peer));
        router.clone().oneshot(request)
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.timer(TokioTimer::new()).header_read_timeout(header_timeout);
    let connection = builder.serve_connection(TokioIo::new(stream), service).with_upgrades();
    let mut connection = std::pin::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(error) = result {
                tracing::debug!(%peer, %error, "connection ended with an error");
            }
        }
        _ = shutdown.wait() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}
