//! Providers, accounts and the tokens the simulator issues. There is no sign-in flow:
//! a test (or the control API) asks for a token for a named account and gets one, with
//! a refresh token. Revoking either kills the grant (API calls 401, refresh answers
//! `invalid_grant`); expiring an access token makes it 401 while its refresh token
//! still works.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How long an issued access token claims to live.
pub const EXPIRES_IN: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Google,
    Microsoft,
    Mega,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Google, Provider::Microsoft, Provider::Mega];

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::Microsoft => "microsoft",
            Provider::Mega => "mega",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// An issued grant, as the token endpoint would answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

struct Grant {
    provider: Provider,
    account: String,
    revoked: bool,
}

struct Access {
    grant: usize,
    expired: bool,
}

/// Why a refresh was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum RefreshError {
    /// Unknown, revoked, or another provider's refresh token.
    InvalidGrant,
}

#[derive(Default)]
pub struct TokenTable {
    grants: Vec<Grant>,
    access: HashMap<String, Access>,
    refresh: HashMap<String, usize>,
    seq: u64,
    refreshes: usize,
}

impl TokenTable {
    fn mint(&mut self, kind: &str, p: Provider) -> String {
        self.seq += 1;
        format!("cloudsim-{}-{kind}-{:06}", p.as_str(), self.seq)
    }

    fn new_access(&mut self, grant: usize) -> String {
        let t = self.mint("at", self.grants[grant].provider);
        self.access.insert(
            t.clone(),
            Access {
                grant,
                expired: false,
            },
        );
        t
    }

    fn new_refresh(&mut self, grant: usize) -> String {
        let t = self.mint("rt", self.grants[grant].provider);
        self.refresh.insert(t.clone(), grant);
        t
    }

    /// A new grant for `account`.
    pub fn issue(&mut self, provider: Provider, account: &str) -> Tokens {
        self.grants.push(Grant {
            provider,
            account: account.to_string(),
            revoked: false,
        });
        let g = self.grants.len() - 1;
        Tokens {
            access_token: self.new_access(g),
            refresh_token: self.new_refresh(g),
            expires_in: EXPIRES_IN,
        }
    }

    /// Whether the simulator issued this access token at all (valid or not).
    pub fn knows(&self, token: &str) -> bool {
        self.access.contains_key(token)
    }

    /// The account a live access token of `provider` belongs to.
    pub fn resolve(&self, provider: Provider, token: &str) -> Option<String> {
        let a = self.access.get(token)?;
        let g = &self.grants[a.grant];
        (g.provider == provider && !g.revoked && !a.expired).then(|| g.account.clone())
    }

    /// A new access token for the grant behind `refresh_token`. Microsoft rotates the
    /// refresh token on each use (the old one keeps working, as Microsoft's does until
    /// it expires); Google answers without one, so the client keeps its own.
    pub fn refresh(
        &mut self,
        provider: Provider,
        refresh_token: &str,
    ) -> Result<(String, Option<String>), RefreshError> {
        let g = *self
            .refresh
            .get(refresh_token)
            .ok_or(RefreshError::InvalidGrant)?;
        if self.grants[g].provider != provider || self.grants[g].revoked {
            return Err(RefreshError::InvalidGrant);
        }
        self.refreshes += 1;
        let at = self.new_access(g);
        let rt = (provider == Provider::Microsoft).then(|| self.new_refresh(g));
        Ok((at, rt))
    }

    fn grant_of(&self, token: &str) -> Option<usize> {
        self.access
            .get(token)
            .map(|a| a.grant)
            .or_else(|| self.refresh.get(token).copied())
    }

    /// Revoke the grant `token` (access or refresh) belongs to. False if unknown.
    pub fn revoke(&mut self, token: &str) -> bool {
        let Some(g) = self.grant_of(token) else {
            return false;
        };
        self.grants[g].revoked = true;
        true
    }

    /// Expire an access token (or, given a refresh token, every access token of its
    /// grant). The refresh token keeps working. False if unknown.
    pub fn expire(&mut self, token: &str) -> bool {
        if let Some(a) = self.access.get_mut(token) {
            a.expired = true;
            return true;
        }
        let Some(&g) = self.refresh.get(token) else {
            return false;
        };
        for a in self.access.values_mut().filter(|a| a.grant == g) {
            a.expired = true;
        }
        true
    }

    /// Refreshes answered so far.
    pub fn refreshes(&self) -> usize {
        self.refreshes
    }
}
