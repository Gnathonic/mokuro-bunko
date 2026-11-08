//! Byte-counting bodies (design §4.1). The response body counts the data frames it
//! hands hyper and commits the request's record exactly once: at the end of the
//! stream, on a stream error, or when it is dropped first (the client went away). The
//! request body counts what the handler read. `size_hint` and `is_end_stream` pass
//! through, so hyper frames both exactly as before.

use super::collector::RequestRecord;
use super::gauges::InflightGuard;
use axum::body::Body;
use bytes::Bytes;
use hyper::body::{Body as _, Frame, SizeHint};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

/// The request body, counted.
pub(crate) struct CountingReqBody {
    inner: Body,
    read: Arc<AtomicU64>,
}

impl CountingReqBody {
    pub fn new(inner: Body, read: Arc<AtomicU64>) -> Self {
        Self { inner, read }
    }
}

impl hyper::body::Body for CountingReqBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
        {
            self.read.fetch_add(data.len() as u64, Ordering::Relaxed);
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

/// Everything the record needs besides the bytes sent, kept until the body ends.
pub(crate) struct Pending {
    pub analytics: Arc<super::Inner>,
    /// The record, its byte and time fields filled in at the end.
    pub record: RequestRecord,
    pub started: Instant,
    pub bytes_in: Arc<AtomicU64>,
    /// The response's `Content-Length`.
    pub declared: Option<u64>,
    /// HEAD, 1xx, 204, 304: no body was ever due, so nothing can be cut short.
    pub no_body: bool,
    pub _inflight: InflightGuard,
}

/// The response body, counted.
pub(crate) struct CountingBody {
    inner: Body,
    sent: u64,
    pending: Option<Box<Pending>>,
}

enum Ending {
    Complete,
    Error,
    Dropped,
}

impl CountingBody {
    pub fn new(inner: Body, pending: Pending) -> Self {
        Self {
            inner,
            sent: 0,
            pending: Some(Box::new(pending)),
        }
    }

    fn finish(&mut self, ending: Ending) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let Pending {
            analytics,
            mut record,
            started,
            bytes_in,
            declared,
            no_body,
            _inflight,
        } = *p;
        let complete = match ending {
            Ending::Complete => true,
            Ending::Error => false,
            // hyper stops polling once a body says it is at its end, or once it has
            // written the declared length: dropping it then is no abort.
            Ending::Dropped => {
                self.inner.is_end_stream() || declared.is_some_and(|d| self.sent >= d)
            }
        };
        record.bytes_out = self.sent;
        record.bytes_in = bytes_in.load(Ordering::Relaxed);
        record.total_ms = started.elapsed().as_millis() as u64;
        record.aborted = !complete && !no_body;
        analytics.commit(record);
        drop(_inflight);
    }
}

impl hyper::body::Body for CountingBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.sent += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish(Ending::Error);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish(Ending::Complete);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for CountingBody {
    fn drop(&mut self) {
        // Never blocks: the commit takes the collector's lock briefly and returns.
        self.finish(Ending::Dropped);
    }
}
