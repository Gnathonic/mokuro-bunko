//! The object-safe token seam the HTTP drivers (Google Drive, OneDrive) take, so they
//! do not depend on the `auth` module; its token sources implement it.

use std::future::Future;
use std::pin::Pin;

use crate::backend::BErr;

/// A source of OAuth access tokens.
pub trait TokenSourceDyn: Send + Sync {
    /// A currently valid access token (refreshing it if due).
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>>;
    /// The last token was refused (401): the next `access_token` must refresh.
    fn invalidate(&self);
}

/// A fixed access token (tests, the live tests, short scripts).
#[derive(Debug, Clone)]
pub struct StaticToken(String);

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        StaticToken(token.into())
    }
}

impl TokenSourceDyn for StaticToken {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, BErr>> + Send + '_>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
    fn invalidate(&self) {}
}
