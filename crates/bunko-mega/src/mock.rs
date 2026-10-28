//! An in-process mock of the MEGA API (feature `mock`, tests only): the `cs` commands
//! this crate sends, `sc` with a wait URL, and the download/upload URLs, with the
//! node crypto done for real so the client's decryption is exercised. Knobs inject
//! hashcash challenges, EAGAIN, over-quota answers, dropped upload chunks and lost
//! cursors; `reader_*`-style helpers act as another client would.
//!
//! One [`MegaHub`] serves any number of accounts behind one gateway (the cloud
//! simulator, `bunko-cloudsim`, mounts its [`MegaHub::router`] under `/mega/`): `cs`
//! and `sc` pick the account by `sid`, or by the email of a login; the transfer and
//! wait URLs it hands out carry the account's index. [`MockMega::start`] is a hub with
//! one account on its own port.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use cipher::StreamCipher;
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio::sync::Notify;

use crate::crypto::{
    self, Fingerprint, MacStream, NodeAttributes, NodeKey, b64, ctr_at, prepare_key_v2, unb64,
};
use crate::hashcash::Challenge;
use crate::session::{SID_LEN, encode_blob};

/// The cloud drive root, inbox and rubbish bin handles.
pub const ROOT: &str = "ROOT0000";
pub const INBOX: &str = "INBOX000";
pub const RUBBISH: &str = "RUBBISH0";
/// The account's user handle.
pub const USER: &str = "UserHandle1";

// A throwaway 1024-bit RSA key (openssl genrsa), for the login's csid only.
const RSA_P: &str = "fb17206e3111ad5e83376243738710434582e662d3ce40899489a2aab6cd5912420bf06f729fb37338e2ade0abc112240a187ca8d73b611633b3287f3adde7eb";
const RSA_Q: &str = "ca6e4c77cc841f7d8c0f3dc50e4d9bc48bac920f916935197c27d589bc50ebad27ab0426c5b58541729bf99887a5599fa151f820254a0e0ff400d4bbd4ad9379";
const RSA_D: &str = "67d5d590021ba33183ed0b7a6ce4cb321dccdb0faf0d2129a4e056e05b825ab1035643bf0cf076bd0b38feb0bb5a208a397aa29ddc790144ed3e17c1e822b93daa491c0a0cec02e220dd2c7258c0fb8116d4b7165ef9cb8c5838c17765fd16452480ecefee29cd9d9f94cd3f6f8a8eb45c90df7452332d8c2589ccee854ed161";
const RSA_U: &str = "1cb78b674b062469f82325a8c715d3368df58ac920a98a7271eb649f26cd7c2333d591de1bff738104d9058bfbf9fd739ca7df2b9483bb2428d93929d65df445";

#[derive(Debug, Clone)]
pub struct MNode {
    pub parent: String,
    pub kind: i64,
    pub attr: String,
    pub key: String,
    pub ts: i64,
    pub size: u64,
    /// Ciphertext.
    pub data: Vec<u8>,
}

struct Account {
    email: String,
    salt: Vec<u8>,
    user_hash: String,
    login_key: [u8; 16],
    mfa: Option<String>,
}

struct Upload {
    size: u64,
    buf: Vec<u8>,
    token: Option<String>,
}

#[derive(Default)]
struct Faults {
    hashcash: Option<u8>,
    hashcash_answered: usize,
    eagain: usize,
    overquota_g: bool,
    bandwidth_509: bool,
    garbage_download: bool,
    drop_upload_at: Option<u64>,
    command: HashMap<String, VecDeque<i64>>,
}

struct St {
    base: String,
    /// This account's index in the hub (in the transfer and wait URLs).
    idx: usize,
    master: [u8; 16],
    sek: [u8; 16],
    sid: String,
    killed: bool,
    account: Option<Account>,
    nodes: BTreeMap<String, MNode>,
    /// Data keys of the files this mock created itself (for `content`).
    log: Vec<Value>,
    floor: usize,
    next: u64,
    clock: i64,
    uploads: HashMap<String, Upload>,
    faults: Faults,
    calls: Vec<String>,
    /// Commands this mock does not model (answered `-2`).
    unknown: Vec<String>,
    posts: Vec<(u64, usize)>,
    downloads: Vec<String>,
}

/// One account of a hub: its state, knobs and "another client" helpers.
pub struct MockMega {
    /// The gateway (`api_origin`), with a trailing slash.
    pub base: String,
    st: Arc<Mutex<St>>,
    notify: Arc<Notify>,
    _task: Option<tokio::task::JoinHandle<()>>,
}

/// What a hub request was, for a caller's request log (a response extension).
#[derive(Debug, Clone)]
pub struct MegaTag {
    /// The account's name in the hub ("" when no account matched).
    pub account: String,
    /// `list`, `metadata`, `download`, `upload`, `write`, `delete`, `changes` or `auth`.
    pub class: &'static str,
    /// Request body bytes.
    pub bytes_in: u64,
}

struct Hub {
    base: String,
    accounts: Mutex<Vec<(String, Arc<Mutex<St>>)>>,
    notify: Arc<Notify>,
    violations: Mutex<Vec<String>>,
}

/// Any number of mock accounts behind one gateway.
#[derive(Clone)]
pub struct MegaHub(Arc<Hub>);

type App = MegaHub;

fn hexb(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn mpi(bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes;
    while let [0, rest @ ..] = b {
        b = rest;
    }
    let bits = if b.is_empty() {
        0
    } else {
        (b.len() - 1) * 8 + (8 - b[0].leading_zeros() as usize)
    };
    let mut out = vec![(bits >> 8) as u8, bits as u8];
    out.extend_from_slice(b);
    out
}

fn node_json(h: &str, n: &MNode) -> Value {
    let mut v = json!({"h": h, "p": n.parent, "t": n.kind, "a": n.attr, "ts": n.ts, "u": USER});
    if n.kind <= 1 {
        v["k"] = json!(n.key);
    }
    if n.kind == 0 {
        v["s"] = json!(n.size);
    }
    v
}

impl St {
    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn handle(&mut self) -> String {
        self.next += 1;
        format!("N{:07}", self.next)
    }

    fn wrap(&self, key: &NodeKey) -> String {
        let mut k = key.packed();
        crypto::ecb_encrypt(&self.master, &mut k).expect("blocks");
        format!("{USER}:{}", b64(&k))
    }

    fn unwrap(&self, n: &MNode) -> NodeKey {
        let (_, k) = n.key.split_once(':').expect("owner:key");
        let mut raw = unb64(k).expect("b64");
        crypto::ecb_decrypt(&self.master, &mut raw).expect("blocks");
        NodeKey::from_unwrapped(n.kind == 0, &raw).expect("key")
    }

    fn insert(&mut self, h: String, n: MNode, idem: Option<&str>) -> Value {
        let j = node_json(&h, &n);
        self.nodes.insert(h, n);
        let mut p = json!({"a": "t", "t": {"f": [j.clone()]}, "ou": USER});
        if let Some(i) = idem {
            p["i"] = json!(i);
        }
        self.log.push(p);
        j
    }

    fn subtree(&self, h: &str) -> Vec<String> {
        let mut out = vec![h.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            out.extend(
                self.nodes
                    .iter()
                    .filter(|(_, n)| n.parent == cur)
                    .map(|(k, _)| k.clone()),
            );
            i += 1;
        }
        out
    }

    fn is_container(&self, h: &str) -> bool {
        self.nodes.get(h).is_some_and(|n| n.kind != 0)
    }

    fn command(&mut self, c: &Value, query_sid: Option<&str>) -> Value {
        let a = c.get("a").and_then(Value::as_str).unwrap_or("").to_string();
        self.calls.push(a.clone());
        if let Some(code) = self
            .faults
            .command
            .get_mut(&a)
            .and_then(VecDeque::pop_front)
        {
            return json!(code);
        }
        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let authed = query_sid == Some(self.sid.as_str()) && !self.killed;
        let login = a == "us0" || (a == "us" && c.get("user").is_some());
        if !login && !authed {
            return json!(-15);
        }
        match a.as_str() {
            "us0" => match &self.account {
                Some(acc) => json!({"v": 2, "s": b64(&acc.salt)}),
                None => json!(-9),
            },
            "us" if c.get("user").is_some() => {
                let Some(acc) = &self.account else {
                    return json!(-9);
                };
                if s("user") != acc.email || s("uh") != acc.user_hash {
                    return json!(-9);
                }
                if let Some(code) = &acc.mfa
                    && s("mfa") != *code
                {
                    return json!(-26);
                }
                let login_key = acc.login_key;
                let sek: [u8; 16] = unb64(&s("sek"))
                    .ok()
                    .and_then(|v| v.try_into().ok())
                    .unwrap_or([0; 16]);
                self.sek = sek;
                let mut k = self.master.to_vec();
                crypto::ecb_encrypt(&login_key, &mut k).expect("blocks");
                // csid = RSA(sid || padding) with e = 65537.
                let sid = unb64(&self.sid).expect("sid");
                let mut m = sid.clone();
                m.extend((0..(127 - SID_LEN)).map(|i| i as u8));
                let n = rsa::BigUint::from_bytes_be(&hexb(RSA_P))
                    * rsa::BigUint::from_bytes_be(&hexb(RSA_Q));
                let c = rsa::BigUint::from_bytes_be(&m).modpow(&rsa::BigUint::from(65537u32), &n);
                json!({"k": b64(&k), "privk": self.privk(), "csid": b64(&mpi(&c.to_bytes_be())), "u": USER, "sek": b64(&sek), "ach": 1})
            }
            "us" => json!({"sek": b64(&self.sek), "u": USER, "privk": self.privk(), "ach": 1}),
            "sml" => {
                self.killed = true;
                json!(0)
            }
            "ug" => {
                json!({"u": USER, "email": self.account.as_ref().map_or("user@example.com".to_string(), |a| a.email.clone()), "s": 0})
            }
            "uq" => {
                let used: u64 = self.nodes.values().map(|n| n.size).sum();
                json!({"mstrg": 20u64 << 30, "cstrg": used})
            }
            "f" => {
                let f: Vec<Value> = self.nodes.iter().map(|(h, n)| node_json(h, n)).collect();
                json!({"f": f, "ok": [], "s": [], "sn": self.log.len().to_string(), "noc": 1})
            }
            "g" => match self.nodes.get(&s("n")) {
                Some(n) if n.kind == 0 => {
                    if self.faults.overquota_g {
                        return json!({"e": -17, "tl": 600, "s": n.size});
                    }
                    json!({"g": format!("{}dl/{}/{}", self.base, self.idx, s("n")), "s": n.size, "at": n.attr})
                }
                _ => json!(-9),
            },
            "u" => {
                let size = c.get("s").and_then(Value::as_u64).unwrap_or(0);
                self.next += 1;
                let id = format!("up{}", self.next);
                self.uploads.insert(
                    id.clone(),
                    Upload {
                        size,
                        buf: Vec::new(),
                        token: None,
                    },
                );
                json!({"p": format!("{}ul/{}/{id}", self.base, self.idx)})
            }
            "p" => {
                let t = s("t");
                if !self.is_container(&t) {
                    return json!(-9);
                }
                let Some(n) = c.get("n").and_then(|n| n.get(0)) else {
                    return json!(-2);
                };
                let g = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let kind = n.get("t").and_then(Value::as_i64).unwrap_or(-1);
                let (data, size) = if kind == 0 {
                    let tok = g("h");
                    let Some((_, up)) = self
                        .uploads
                        .iter()
                        .find(|(_, u)| u.token.as_deref() == Some(tok.as_str()))
                    else {
                        return json!(-9);
                    };
                    (up.buf.clone(), up.size)
                } else {
                    (Vec::new(), 0)
                };
                let h = self.handle();
                let ts = self.tick();
                let node = MNode {
                    parent: t,
                    kind,
                    attr: g("a"),
                    key: format!("{USER}:{}", g("k")),
                    ts,
                    size,
                    data,
                };
                let idem = s("i");
                let j = self.insert(h, node, Some(&idem));
                json!({"f": [j]})
            }
            "a" => {
                let n = s("n");
                let ts = self.tick();
                let Some(node) = self.nodes.get_mut(&n) else {
                    return json!(-9);
                };
                node.attr = s("attr");
                let at = node.attr.clone();
                self.log
                    .push(json!({"a": "u", "n": n, "u": USER, "at": at, "ts": ts, "i": s("i")}));
                json!(0)
            }
            "m" => self.do_move(&s("n"), &s("t"), Some(&s("i"))),
            "d" => self.do_delete(&s("n"), Some(&s("i"))),
            _ => {
                self.unknown.push(a.clone());
                json!(-2)
            }
        }
    }

    fn privk(&self) -> String {
        let mut p = Vec::new();
        for h in [RSA_P, RSA_Q, RSA_D, RSA_U] {
            p.extend(mpi(&hexb(h)));
        }
        p.resize(p.len().div_ceil(16) * 16, 0);
        crypto::ecb_encrypt(&self.master, &mut p).expect("blocks");
        b64(&p)
    }

    fn do_move(&mut self, n: &str, t: &str, idem: Option<&str>) -> Value {
        if !self.nodes.contains_key(n) || !self.is_container(t) {
            return json!(-9);
        }
        if self.subtree(n).iter().any(|h| h == t) {
            return json!(-10);
        }
        let node = self.nodes.get_mut(n).expect("node");
        node.parent = t.to_string();
        let j = node_json(n, node);
        let mut d = json!({"a": "d", "n": n, "m": 1, "ou": USER});
        let mut tp = json!({"a": "t", "t": {"f": [j]}, "ou": USER});
        if let Some(i) = idem {
            d["i"] = json!(i);
            tp["i"] = json!(i);
        }
        self.log.push(d);
        self.log.push(tp);
        json!(0)
    }

    fn do_delete(&mut self, n: &str, idem: Option<&str>) -> Value {
        if !self.nodes.contains_key(n) {
            return json!(-9);
        }
        for h in self.subtree(n) {
            self.nodes.remove(&h);
        }
        let mut d = json!({"a": "d", "n": n, "ou": USER});
        if let Some(i) = idem {
            d["i"] = json!(i);
        }
        self.log.push(d);
        json!(0)
    }
}

fn lock_st(st: &Mutex<St>) -> std::sync::MutexGuard<'_, St> {
    st.lock().unwrap_or_else(|e| e.into_inner())
}

type Acct = (String, Arc<Mutex<St>>);

impl MegaHub {
    /// An empty hub whose transfer URLs start with `base` (the gateway, with a
    /// trailing slash), e.g. `http://127.0.0.1:PORT/mega/`.
    pub fn new(base: impl Into<String>) -> MegaHub {
        let mut base = base.into();
        if !base.ends_with('/') {
            base.push('/');
        }
        MegaHub(Arc::new(Hub {
            base,
            accounts: Mutex::new(Vec::new()),
            notify: Arc::new(Notify::new()),
            violations: Mutex::new(Vec::new()),
        }))
    }

    pub fn base(&self) -> &str {
        &self.0.base
    }

    /// The routes, relative to the gateway: `/cs`, `/sc`, `/wait/..`, `/dl/..`, `/ul/..`.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/cs", post(cs))
            .route("/sc", post(sc).get(sc))
            .route("/wait/{a}/{sn}", post(wait).get(wait))
            .route("/dl/{a}/{h}/{range}", get(download))
            .route("/ul/{a}/{id}/{offset}", post(upload))
            .with_state(self.clone())
    }

    fn accounts(&self) -> std::sync::MutexGuard<'_, Vec<Acct>> {
        self.0.accounts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The account `name` (an empty drive, no password login yet), created on first
    /// use.
    pub fn account(&self, name: &str) -> MockMega {
        let st = {
            let mut accounts = self.accounts();
            match accounts.iter().find(|(n, _)| n == name) {
                Some((_, st)) => st.clone(),
                None => {
                    let st = Arc::new(Mutex::new(St::new(self.0.base.clone(), accounts.len())));
                    accounts.push((name.to_string(), st.clone()));
                    st
                }
            }
        };
        MockMega {
            base: self.0.base.clone(),
            st,
            notify: self.0.notify.clone(),
            _task: None,
        }
    }

    /// The account names, oldest first.
    pub fn names(&self) -> Vec<String> {
        self.accounts().iter().map(|(n, _)| n.clone()).collect()
    }

    /// Drop every account.
    pub fn reset(&self) {
        self.accounts().clear();
        self.take_violations();
        self.0.notify.notify_waiters();
    }

    /// Requests this mock could not answer (unknown commands, unknown accounts in a
    /// URL), as `mega <what>` lines; cleared.
    pub fn take_violations(&self) -> Vec<String> {
        let mut out =
            std::mem::take(&mut *self.0.violations.lock().unwrap_or_else(|e| e.into_inner()));
        for (name, st) in self.accounts().iter() {
            for a in std::mem::take(&mut lock_st(st).unknown) {
                out.push(format!("mega POST /cs ({name}): unmodelled command {a:?}"));
            }
        }
        out
    }

    fn violation(&self, line: String) {
        self.0
            .violations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line);
    }

    fn by_idx(&self, idx: &str) -> Option<Acct> {
        let i: usize = idx.parse().ok()?;
        self.accounts().get(i).cloned()
    }

    /// The account a `cs`/`sc` request is for: by session id, else by the email of a
    /// login command, else the first account (whose answers then refuse it).
    fn resolve(&self, sid: Option<&str>, cmds: &[Value]) -> Option<Acct> {
        let accounts = self.accounts().clone();
        if let Some(sid) = sid
            && let Some(a) = accounts.iter().find(|(_, st)| lock_st(st).sid == sid)
        {
            return Some(a.clone());
        }
        let email = cmds
            .iter()
            .find_map(|c| c.get("user").and_then(Value::as_str))
            .map(str::to_lowercase);
        if let Some(email) = email
            && let Some(a) = accounts.iter().find(|(_, st)| {
                lock_st(st)
                    .account
                    .as_ref()
                    .is_some_and(|acc| acc.email == email)
            })
        {
            return Some(a.clone());
        }
        accounts.first().cloned()
    }
}

fn tagged(r: impl IntoResponse, account: &str, class: &'static str, bytes_in: usize) -> Response {
    let mut r = r.into_response();
    r.extensions_mut().insert(MegaTag {
        account: account.to_string(),
        class,
        bytes_in: bytes_in as u64,
    });
    r
}

/// The class of a `cs` batch, by its first command.
fn cs_class(cmds: &[Value]) -> &'static str {
    match cmds
        .first()
        .and_then(|c| c.get("a"))
        .and_then(Value::as_str)
    {
        Some("us0" | "us" | "sml") => "auth",
        Some("f") => "list",
        Some("p" | "a" | "m") => "write",
        Some("d") => "delete",
        Some("u") => "upload",
        _ => "metadata",
    }
}

async fn cs(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let n = body.len();
    let cmds = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Array(cmds)) => Some(cmds),
        _ => None,
    };
    let class = cmds.as_deref().map_or("metadata", cs_class);
    let sid = q.get("sid").map(String::as_str);
    let Some((name, st)) = app.resolve(sid, cmds.as_deref().unwrap_or_default()) else {
        return tagged("-15", "", class, n);
    };
    let out = {
        let mut st = lock_st(&st);
        if let Some(e) = st.faults.hashcash {
            let challenge = format!("1:{e}:1700000000:{}", b64(&[7u8; 48]));
            let answered = headers
                .get("x-hashcash")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|a| Challenge::parse(&challenge).expect("challenge").verify(a));
            if answered {
                st.faults.hashcash = None;
                st.faults.hashcash_answered += 1;
            } else {
                return tagged(
                    (
                        StatusCode::PAYMENT_REQUIRED,
                        [("X-Hashcash", challenge)],
                        "-3",
                    ),
                    &name,
                    class,
                    n,
                );
            }
        }
        if st.faults.eagain > 0 {
            st.faults.eagain -= 1;
            return tagged("-3", &name, class, n);
        }
        let Some(cmds) = cmds else {
            drop(st);
            app.violation(format!("mega POST /cs ({name}): body is not a JSON array"));
            return tagged("-2", &name, class, n);
        };
        let out: Vec<Value> = cmds.iter().map(|c| st.command(c, sid)).collect();
        out
    };
    app.0.notify.notify_waiters();
    tagged(axum::Json(out), &name, class, n)
}

async fn sc(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Response {
    let sid = q.get("sid").map(String::as_str);
    let Some((name, st)) = app.resolve(sid, &[]) else {
        return tagged("-15", "", "changes", 0);
    };
    let st = lock_st(&st);
    let r = (|| {
        if sid != Some(st.sid.as_str()) || st.killed {
            return "-15".into_response();
        }
        let Some(sn) = q.get("sn").and_then(|s| s.parse::<usize>().ok()) else {
            return "-6".into_response();
        };
        if sn < st.floor || sn > st.log.len() {
            return "-6".into_response();
        }
        if sn == st.log.len() {
            return axum::Json(json!({"w": format!("{}wait/{}/{sn}", st.base, st.idx)}))
                .into_response();
        }
        let end = (sn + 50).min(st.log.len());
        axum::Json(json!({"a": st.log[sn..end], "sn": end.to_string()})).into_response()
    })();
    tagged(r, &name, "changes", 0)
}

async fn wait(State(app): State<App>, Path((a, sn)): Path<(String, usize)>) -> Response {
    let Some((name, st)) = app.by_idx(&a) else {
        app.violation(format!("mega wait/{a}/{sn}: no such account"));
        return tagged(StatusCode::NOT_FOUND, "", "changes", 0);
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let notified = app.0.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if lock_st(&st).log.len() > sn {
            break;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            break;
        }
    }
    tagged("0", &name, "changes", 0)
}

async fn download(
    State(app): State<App>,
    Path((a, h, range)): Path<(String, String, String)>,
) -> Response {
    let Some((name, st)) = app.by_idx(&a) else {
        app.violation(format!("mega dl/{a}/{h}: no such account"));
        return tagged(StatusCode::NOT_FOUND, "", "download", 0);
    };
    let mut st = lock_st(&st);
    st.downloads.push(range.clone());
    let r = (|| {
        if st.faults.bandwidth_509 {
            return StatusCode::from_u16(509).expect("509").into_response();
        }
        if st.faults.garbage_download {
            return "-17".into_response();
        }
        let Some(n) = st.nodes.get(&h) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let Some((a, b)) = range.split_once('-') else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let end = (b + 1).min(n.data.len());
        if a > end {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        n.data[a..end].to_vec().into_response()
    })();
    tagged(r, &name, "download", 0)
}

async fn upload(
    State(app): State<App>,
    Path((a, id, offset)): Path<(String, String, u64)>,
    body: Bytes,
) -> Response {
    let n = body.len();
    let Some((name, st)) = app.by_idx(&a) else {
        app.violation(format!("mega ul/{a}/{id}: no such account"));
        return tagged(StatusCode::NOT_FOUND, "", "upload", n);
    };
    let mut st = lock_st(&st);
    let r = (|| {
        st.posts.push((offset, body.len()));
        if st.faults.drop_upload_at.is_some_and(|at| offset >= at) {
            st.faults.drop_upload_at = None;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let next = st.next + 1;
        let Some(up) = st.uploads.get_mut(&id) else {
            return "-8".into_response();
        };
        if offset != up.buf.len() as u64 {
            // A re-sent chunk the mock already has is accepted again (MEGA does).
            if offset + body.len() as u64 <= up.buf.len() as u64 {
                up.buf[offset as usize..offset as usize + body.len()].copy_from_slice(&body);
            } else {
                return "-7".into_response();
            }
        } else {
            up.buf.extend_from_slice(&body);
        }
        if up.buf.len() as u64 == up.size {
            let tok = format!("TOK{next}x{}", up.size);
            up.token = Some(tok.clone());
            st.next = next;
            return tok.into_response();
        }
        StatusCode::OK.into_response()
    })();
    tagged(r, &name, "upload", n)
}

impl St {
    /// An empty account (cloud root, inbox, rubbish bin) at index `idx` of a hub.
    fn new(base: String, idx: usize) -> St {
        let mut sid = [0x41u8; SID_LEN];
        for (i, b) in sid.iter_mut().enumerate().skip(1) {
            *b = (i * 31 % 251) as u8;
        }
        // Distinct per account; account 0 keeps the session id it always had.
        sid[SID_LEN - 1] = sid[SID_LEN - 1].wrapping_add(idx as u8);
        sid[SID_LEN - 2] = sid[SID_LEN - 2].wrapping_add((idx >> 8) as u8);
        let mut nodes = BTreeMap::new();
        for (h, kind) in [(ROOT, 2), (INBOX, 3), (RUBBISH, 4)] {
            nodes.insert(
                h.to_string(),
                MNode {
                    parent: String::new(),
                    kind,
                    attr: String::new(),
                    key: String::new(),
                    ts: 1_600_000_000,
                    size: 0,
                    data: Vec::new(),
                },
            );
        }
        St {
            base,
            idx,
            master: [0x5a; 16],
            sek: [0x3c; 16],
            sid: b64(&sid),
            killed: false,
            account: None,
            nodes,
            log: Vec::new(),
            floor: 0,
            next: 0,
            clock: 1_700_000_000,
            uploads: HashMap::new(),
            faults: Faults::default(),
            calls: Vec::new(),
            unknown: Vec::new(),
            posts: Vec::new(),
            downloads: Vec::new(),
        }
    }
}

impl MockMega {
    /// Start a mock with an empty account (cloud root, inbox, rubbish bin).
    pub async fn start() -> MockMega {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}/", listener.local_addr().expect("addr"));
        let hub = MegaHub::new(base);
        let mut mock = hub.account("default");
        let router = hub.router();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        mock._task = Some(task);
        mock
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn changed(&self) {
        self.notify.notify_waiters();
    }

    /// A serialised session the client can resume.
    pub fn session_blob(&self) -> String {
        let st = self.lock();
        encode_blob(&st.master, &st.sek, &unb64(&st.sid).expect("sid")).expect("blob")
    }

    /// Enable password login (v2) for `email`, optionally with a 2FA code.
    pub fn set_account(&self, email: &str, password: &str, mfa: Option<&str>) {
        let salt: Vec<u8> = (0..32).map(|i| i * 3 + 1).collect();
        let derived = prepare_key_v2(password.as_bytes(), &salt);
        let mut login_key = [0u8; 16];
        login_key.copy_from_slice(&derived[..16]);
        self.lock().account = Some(Account {
            email: email.to_lowercase(),
            user_hash: b64(&derived[16..]),
            salt,
            login_key,
            mfa: mfa.map(str::to_string),
        });
    }

    // ---- another client ("the reader") --------------------------------------------

    pub fn mkdir(&self, parent: &str, name: &str) -> String {
        let h = {
            let mut st = self.lock();
            let key = NodeKey::Folder {
                aes: rand::random(),
            };
            let attrs = NodeAttributes {
                name: name.into(),
                ..Default::default()
            };
            let attr = b64(&attrs.encrypt(key.aes()).expect("attrs"));
            let node = MNode {
                parent: parent.into(),
                kind: 1,
                attr,
                key: st.wrap(&key),
                ts: st.tick(),
                size: 0,
                data: Vec::new(),
            };
            let h = st.handle();
            st.insert(h.clone(), node, None);
            h
        };
        self.changed();
        h
    }

    pub fn put(&self, parent: &str, name: &str, plain: &[u8]) -> String {
        let h = {
            let mut st = self.lock();
            let aes: [u8; 16] = rand::random();
            let nonce: [u8; 8] = rand::random();
            let mut m = MacStream::new(&aes, &nonce);
            m.update(plain);
            let key = NodeKey::File {
                aes,
                nonce,
                mac: m.finish(),
            };
            let mut data = plain.to_vec();
            ctr_at(&aes, &nonce, 0).apply_keystream(&mut data);
            let checksum =
                crypto::sparse_checksum(&mut std::io::Cursor::new(plain), plain.len() as u64)
                    .expect("checksum");
            let attrs = NodeAttributes {
                name: name.into(),
                fingerprint: Some(
                    Fingerprint {
                        checksum,
                        modified_at: 1_650_000_000,
                    }
                    .serialize(),
                ),
                ..Default::default()
            };
            let node = MNode {
                parent: parent.into(),
                kind: 0,
                attr: b64(&attrs.encrypt(key.aes()).expect("attrs")),
                key: st.wrap(&key),
                ts: st.tick(),
                size: plain.len() as u64,
                data,
            };
            let h = st.handle();
            st.insert(h.clone(), node, None);
            h
        };
        self.changed();
        h
    }

    pub fn rename(&self, h: &str, name: &str) {
        {
            let mut st = self.lock();
            let n = st.nodes.get(h).expect("node").clone();
            let key = st.unwrap(&n);
            let mut attrs = NodeAttributes::decrypt_b64(key.aes(), &n.attr).expect("attrs");
            attrs.name = name.into();
            let at = b64(&attrs.encrypt(key.aes()).expect("attrs"));
            let ts = st.tick();
            st.nodes.get_mut(h).expect("node").attr = at.clone();
            st.log
                .push(json!({"a": "u", "n": h, "u": USER, "at": at, "ts": ts}));
        }
        self.changed();
    }

    pub fn move_to(&self, h: &str, parent: &str) {
        self.lock().do_move(h, parent, None);
        self.changed();
    }

    pub fn delete(&self, h: &str) {
        self.lock().do_delete(h, None);
        self.changed();
    }

    /// Append a raw action packet (an unmodelled one, say).
    pub fn push_packet(&self, p: Value) {
        self.lock().log.push(p);
        self.changed();
    }

    /// Create the folders along `path` under the cloud root and put `plain` at its
    /// end, replacing a file already there.
    pub fn seed(&self, path: &str, plain: &[u8]) -> String {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut parent = ROOT.to_string();
        for dir in &parts[..parts.len().saturating_sub(1)] {
            parent = match self.find(&parent, dir).into_iter().next() {
                Some(h) => h,
                None => self.mkdir(&parent, dir),
            };
        }
        let Some(name) = parts.last() else {
            return parent;
        };
        for old in self.find(&parent, name) {
            self.delete(&old);
        }
        self.put(&parent, name, plain)
    }

    // ---- inspection ---------------------------------------------------------------

    pub fn node(&self, h: &str) -> Option<MNode> {
        self.lock().nodes.get(h).cloned()
    }

    /// The decrypted name and parent of `h`.
    pub fn name_of(&self, h: &str) -> Option<(String, String)> {
        let st = self.lock();
        let n = st.nodes.get(h)?;
        let key = st.unwrap(n);
        let a = NodeAttributes::decrypt_b64(key.aes(), &n.attr).ok()?;
        Some((a.name, n.parent.clone()))
    }

    /// The plaintext of file `h`, and whether its key's condensed MAC matches it.
    pub fn content(&self, h: &str) -> Option<(Vec<u8>, bool)> {
        let st = self.lock();
        let n = st.nodes.get(h)?;
        let NodeKey::File { aes, nonce, mac } = st.unwrap(n) else {
            return None;
        };
        let mut plain = n.data.clone();
        ctr_at(&aes, &nonce, 0).apply_keystream(&mut plain);
        let mut m = MacStream::new(&aes, &nonce);
        m.update(&plain);
        Some((plain, m.finish() == mac))
    }

    /// Handles of the live children of `h` with decrypted name `name`.
    pub fn find(&self, parent: &str, name: &str) -> Vec<String> {
        let hs: Vec<String> = {
            let st = self.lock();
            st.nodes
                .iter()
                .filter(|(_, n)| n.parent == parent)
                .map(|(h, _)| h.clone())
                .collect()
        };
        hs.into_iter()
            .filter(|h| self.name_of(h).is_some_and(|(n, _)| n == name))
            .collect()
    }

    /// Every file under the cloud root: (path, plaintext), sorted by path.
    pub fn files(&self) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![(ROOT.to_string(), String::new())];
        while let Some((h, prefix)) = stack.pop() {
            let kids: Vec<(String, i64)> = {
                let st = self.lock();
                st.nodes
                    .iter()
                    .filter(|(_, n)| n.parent == h)
                    .map(|(k, n)| (k.clone(), n.kind))
                    .collect()
            };
            for (k, kind) in kids {
                let Some((name, _)) = self.name_of(&k) else {
                    continue;
                };
                let path = format!("{prefix}{name}");
                if kind == 0 {
                    if let Some((plain, _)) = self.content(&k) {
                        out.push((path, plain));
                    }
                } else {
                    stack.push((k, format!("{path}/")));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// `cs` command names received, in order.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    /// Upload chunk POSTs: `(offset, len)`.
    pub fn chunk_posts(&self) -> Vec<(u64, usize)> {
        self.lock().posts.clone()
    }

    /// Download ranges requested (`a-b`).
    pub fn download_ranges(&self) -> Vec<String> {
        self.lock().downloads.clone()
    }

    pub fn hashcash_answered(&self) -> usize {
        self.lock().faults.hashcash_answered
    }

    /// The current `sn` (log length).
    pub fn sn(&self) -> String {
        self.lock().log.len().to_string()
    }

    // ---- knobs --------------------------------------------------------------------

    /// The next `cs` request is answered with a hashcash challenge of this easiness.
    pub fn require_hashcash(&self, easiness: u8) {
        self.lock().faults.hashcash = Some(easiness);
    }

    /// The next `n` `cs` requests answer `-3`.
    pub fn eagain(&self, n: usize) {
        self.lock().faults.eagain = n;
    }

    /// `g` answers `-17` (transfer quota).
    pub fn overquota(&self, on: bool) {
        self.lock().faults.overquota_g = on;
    }

    /// Download URLs answer HTTP 509.
    pub fn bandwidth_509(&self, on: bool) {
        self.lock().faults.bandwidth_509 = on;
    }

    /// Download URLs answer a short 200 body (`-17`), as an over-quota server can.
    pub fn garbage_download(&self, on: bool) {
        self.lock().faults.garbage_download = on;
    }

    /// The first chunk POST at or past `offset` fails with HTTP 500 (not stored).
    pub fn drop_upload_at(&self, offset: u64) {
        self.lock().faults.drop_upload_at = Some(offset);
    }

    /// The next `command` answers `code`.
    pub fn fail_command(&self, command: &str, code: i64) {
        self.lock()
            .faults
            .command
            .entry(command.into())
            .or_default()
            .push_back(code);
    }

    /// Every `sn` issued so far becomes stale (`sc` answers `-6`).
    pub fn lose_cursor(&self) {
        let mut st = self.lock();
        st.floor = st.log.len();
    }

    /// The session is killed (`-15` from then on).
    pub fn kill_session(&self) {
        self.lock().killed = true;
    }
}
