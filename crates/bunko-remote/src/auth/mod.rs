//! Signing in to the OAuth providers and keeping their tokens (0.8 design §7).
//!
//! - [`oauth`]: the loopback + PKCE flow, the device flow and refresh.
//! - [`clients`]: the installed-app clients embedded in every build, and the
//!   Google project pairing (D25).
//! - [`credentials`]: `<base>/cloud/credentials.json`, optionally sealed (D12).
//! - [`token`]: [`token::TokenSource`], what a provider backend asks for an access token.
//! - [`flow`]: which sign-in flow a Connect click runs.
//!
//! Bunko never calls Google's `/revoke`: revocation is project-wide and would sign
//! the reader out too. Nothing in this module calls a revoke endpoint of any provider.

pub mod clients;
pub mod credentials;
pub mod flow;
pub mod oauth;
pub mod token;

pub use clients::{GoogleClients, google_clients, microsoft_client};
pub use credentials::{CredError, CredentialStore, SecretKey, StoredCredential};
pub use flow::{Flow, choose_flow};
pub use oauth::{
    AuthError, DeviceCode, DeviceFlow, LoopbackFlow, OAuthClient, Pending, Provider, Tokens,
    refresh,
};
pub use token::{RefreshingToken, StaticToken, TokenSource};
