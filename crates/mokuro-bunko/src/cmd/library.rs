//! `library status`, `library connect` and `library disconnect` (0.8 §7, §11): where
//! the library lives, how its store is doing, and changing it.
//!
//! A server running on this storage is asked through its local control API, the way
//! the tray asks it: `.control.json`'s token as a bearer, over plain http to
//! 127.0.0.1 only (the URL is built from the file's port; the file's own `url` is never
//! followed, so nothing leaves this machine). Its `status.library.storage` is the
//! admin panel's block (health, sync state, caches; never secrets). Without a running
//! server the configuration is read instead, with no network: backend, folder, store
//! id and whether a sign-in is on file. No credential is taken on the command line.

use super::Ctx;
use crate::cfgfile;
use crate::cli::LibraryCmd;
use crate::out::{CmdResult, Fail, runtime};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::time::Duration;

pub fn run(ctx: &Ctx, cmd: LibraryCmd) -> CmdResult {
    match cmd {
        LibraryCmd::Status { json } => status(ctx, json),
        LibraryCmd::Connect(a) => connect(ctx, a),
        LibraryCmd::Disconnect(a) => disconnect(ctx, a),
    }
}

fn status(ctx: &Ctx, json: bool) -> CmdResult {
    let config = cfgfile::load_effective(&ctx.config_path)?;
    let live = bunko_control::file::live_instance(&config.storage.base_path).and_then(|file| {
        match ask_control(&file) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Could not ask the running server ({e}); showing the configuration");
                None
            }
        }
    });
    let (block, source) = match live {
        Some(v) => (v, "running server"),
        None => (
            bunko_server::storage::describe_offline(&config),
            "configuration",
        ),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&block).map_err(Fail::msg)?
        );
        return Ok(());
    }
    for line in describe(&block, source) {
        println!("{line}");
    }
    Ok(())
}

/// The control API's status URL: loopback http on the file's port, whatever else the
/// file says.
pub fn control_status_url(file: &bunko_control::ControlFile) -> String {
    format!("http://127.0.0.1:{}/control/status", file.port)
}

/// `status.library.storage` of the running instance (`None`: it reports none, e.g. a
/// processor or an older version). Errors never carry the token.
fn ask_control(file: &bunko_control::ControlFile) -> Result<Option<Value>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let url = control_status_url(file);
    let rt = runtime().map_err(|e| e.to_string())?;
    let body: Value = rt.block_on(async {
        let resp = client
            .get(&url)
            .bearer_auth(&file.token)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| e.without_url().to_string())
    })?;
    Ok(body
        .get("library")
        .and_then(|l| l.get("storage"))
        .filter(|s| s.is_object())
        .cloned())
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut x = n as f64;
    let mut u = 0;
    while x >= 1024.0 && u + 1 < UNITS.len() {
        x /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{x:.1} {}", UNITS[u])
    }
}

/// The lines `library status` prints for a `library.storage` block.
pub fn describe(v: &Value, source: &str) -> Vec<String> {
    let backend = text(&v["backend"]).unwrap_or_else(|| "?".into());
    let mut out = Vec::new();
    if backend == "local" {
        out.push("Library: on this disk".to_string());
        if let Some(folder) = text(&v["folder"]) {
            out.push(format!("Folder:  {folder}"));
        }
        out.push(format!("(from the {source})"));
        return out;
    }
    let mut first = format!(
        "Library: {backend}, folder {}",
        text(&v["folder"]).unwrap_or_default()
    );
    if let Some(p) = text(&v["pairing"]) {
        first.push_str(&format!(" (pairing {p})"));
    }
    out.push(first);
    if let Some(a) = text(&v["account"]) {
        out.push(format!("Account: {a}"));
    }
    match text(&v["store_id"]) {
        Some(id) => {
            let dir = text(&v["store_dir"])
                .map(|d| format!(" in {d}"))
                .unwrap_or_default();
            out.push(format!("Store:   {id}{dir}"));
        }
        None => out.push("Store:   not connected".to_string()),
    }
    if let Some(p) = text(&v["problem"]) {
        out.push(format!("Problem: {p}"));
    }
    if let Some(state) = text(&v["health"]["state"]) {
        let mut h = format!("Health:  {state}");
        if let Some(engine) = text(&v["engine"]) {
            h.push_str(&format!(" (feed: {engine})"));
        }
        if let (Some(done), total) = (v["sync"]["done"].as_u64(), v["sync"]["total"].as_u64()) {
            match total {
                Some(t) => h.push_str(&format!(", {done}/{t} synced")),
                None => h.push_str(&format!(", {done} synced")),
            }
        }
        out.push(h);
    } else if let Some(indexed) = v["indexed"].as_bool() {
        out.push(format!(
            "Index:   {}",
            if indexed {
                "present (a sync ran before)"
            } else {
                "none yet (the first sync runs when the server starts)"
            }
        ));
    }
    let lease = &v["lease"];
    if let Some(state) = text(&lease["state"]) {
        // This Bunko's own claim (with the time left), or the other Bunko's name.
        let line = match (text(&lease["detail"]), text(&lease["holder"]["name"])) {
            (Some(d), _) => format!("Lease:   {state}: {d}"),
            (None, Some(by)) if lease["holder"]["ours"] != true => {
                format!("Lease:   {state} (held by {by})")
            }
            _ => format!("Lease:   {state}"),
        };
        out.push(line);
    }
    let c = &v["cache"];
    if c.is_object() {
        let n = |k: &str| c[k].as_u64().unwrap_or(0);
        out.push(format!(
            "Cache:   {} small files ({}), {} archive directories, {} spooled archives ({})",
            n("blobs"),
            bytes(n("blob_bytes")),
            n("zipdirs"),
            n("spools"),
            bytes(n("spool_bytes"))
        ));
    }
    out.push(format!("(from the {source})"));
    out
}

// ---- library connect / disconnect (0.8 §7) -----------------------------------------
//
// With a server running on this storage, both ask it through its local control API
// (`/app/api/library/...`, the control token as a bearer, loopback http only): the
// server runs the sign-in, holds the store, and restarts onto the result. Otherwise
// they run here, on the same code. Either way only the device flow's code and the
// sign-in's state are printed: no token, password or session blob ever is.

/// The request `library connect` sends, from its flags, the configuration and what
/// was asked on the terminal. `secret` is the WebDAV app password or the MEGA
/// password; `totp` the MEGA two-factor code.
pub fn connect_request(
    a: &crate::cli::LibraryConnectArgs,
    config: &bunko_core::Config,
    secret: Option<String>,
    totp: Option<String>,
) -> Result<bunko_server::storage::connect::ConnectRequest, Fail> {
    use crate::cli::{CloudProvider, ReaderPairing};
    use bunko_server::storage::connect::{ConnectRequest, MegaForm, WebDavForm};
    if a.reader.is_some() && a.provider != CloudProvider::Gdrive {
        return Err(Fail::msg("--reader applies to gdrive only"));
    }
    let webdav_only =
        a.url.is_some() || a.username.is_some() || a.password_file.is_some() || a.allow_http;
    if webdav_only && a.provider != CloudProvider::Webdav {
        return Err(Fail::msg(
            "--url, --username, --password-file and --allow-http apply to webdav only",
        ));
    }
    if a.email.is_some() && a.provider != CloudProvider::Mega {
        return Err(Fail::msg("--email applies to mega only"));
    }
    let lib = &config.storage.library;
    let mut req = ConnectRequest {
        provider: a.provider.as_str().into(),
        reader: a.reader.map(|r| match r {
            ReaderPairing::Release => "release".into(),
            ReaderPairing::Beta => "beta".into(),
        }),
        folder: a.folder.clone(),
        flow: Some("device".into()),
        use_this_account: a.use_this_account,
        ..ConnectRequest::default()
    };
    match a.provider {
        CloudProvider::Webdav => {
            let url = a
                .url
                .clone()
                .or(lib.webdav.url.clone())
                .ok_or_else(|| Fail::msg("webdav needs --url (the server's address)"))?;
            let username = a
                .username
                .clone()
                .or(lib.webdav.username.clone())
                .ok_or_else(|| Fail::msg("webdav needs --username"))?;
            req.webdav = Some(WebDavForm {
                url,
                username,
                password: secret
                    .ok_or_else(|| Fail::msg("webdav needs its app password (--password-file)"))?,
                allow_http: a.allow_http || lib.webdav.allow_http,
            });
        }
        CloudProvider::Mega => {
            req.mega = Some(MegaForm {
                email: a
                    .email
                    .clone()
                    .ok_or_else(|| Fail::msg("mega needs --email"))?,
                password: secret.ok_or_else(|| Fail::msg("mega needs the account's password"))?,
                totp: totp.filter(|t| !t.trim().is_empty()),
            });
        }
        _ => {}
    }
    Ok(req)
}

/// The WebDAV app password: `--password-file`, `MOKURO_WEBDAV_PASSWORD_FILE`, or the
/// terminal.
fn webdav_secret(a: &crate::cli::LibraryConnectArgs) -> Result<String, Fail> {
    let file = a.password_file.clone().or_else(|| {
        std::env::var_os(bunko_server::storage::WEBDAV_PASSWORD_FILE_ENV)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    });
    if let Some(f) = file {
        let text =
            std::fs::read_to_string(&f).map_err(|e| Fail::msg(format!("{}: {e}", f.display())))?;
        let pw = text.trim_end_matches(['\r', '\n']).to_string();
        if pw.is_empty() {
            return Err(Fail::msg(format!("{} is empty", f.display())));
        }
        return Ok(pw);
    }
    if !std::io::stdin().is_terminal() {
        return Err(Fail::msg(
            "webdav needs its app password: --password-file, MOKURO_WEBDAV_PASSWORD_FILE, or run on a terminal",
        ));
    }
    crate::prompt::hidden("App password", false)
}

pub fn connect(ctx: &Ctx, a: crate::cli::LibraryConnectArgs) -> CmdResult {
    use crate::cli::CloudProvider;
    let config = cfgfile::load_effective(&ctx.config_path)?;
    let (secret, totp) = match a.provider {
        CloudProvider::Webdav => (Some(webdav_secret(&a)?), None),
        CloudProvider::Mega => {
            if a.email.is_none() {
                return Err(Fail::msg("mega needs --email"));
            }
            if !std::io::stdin().is_terminal() {
                return Err(Fail::msg(
                    "mega asks for the password and two-factor code on a terminal (docker exec -it)",
                ));
            }
            let pw = crate::prompt::hidden("MEGA password", false)?;
            let totp = crate::prompt::text_opts(
                "Two-factor code (empty if the account has none)",
                Some(""),
                false,
            )?;
            (Some(pw), Some(totp))
        }
        _ => (None, None),
    };
    let req = connect_request(&a, &config, secret, totp)?;
    let interactive = std::io::stdin().is_terminal();
    match bunko_control::file::live_instance(&config.storage.base_path) {
        Some(file) => connect_through_server(&file, req, a.create, interactive),
        None => connect_here(ctx, req, a.create, interactive),
    }
}

/// One status line per change of the sign-in, as the terminal shows it.
#[derive(Default)]
struct Printer {
    code: Option<String>,
    state: String,
}

impl Printer {
    fn show(&mut self, v: &Value) {
        for l in self.lines(v) {
            println!("{l}");
        }
    }

    /// What to print for this state (nothing when it did not change).
    fn lines(&mut self, v: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let state = v["state"].as_str().unwrap_or("").to_string();
        let provider = v["provider"].as_str().unwrap_or("");
        let name = bunko_core::config::LibraryBackend::parse(provider)
            .map(bunko_server::storage::connect::provider_name)
            .unwrap_or("the provider");
        if state == "code" {
            let code = v["user_code"].as_str().map(str::to_string);
            if code != self.code {
                let expires = v["expires_at"]
                    .as_str()
                    .and_then(|t| t.get(11..16))
                    .map(|t| format!("; it expires at {t} UTC"))
                    .unwrap_or_default();
                out.push(String::new());
                out.push(format!(
                    "To sign in to {name}, open {} on any device and enter the code:",
                    v["verification_url"].as_str().unwrap_or("?")
                ));
                out.push(String::new());
                out.push(format!("    {}", code.as_deref().unwrap_or("?")));
                out.push(String::new());
                out.push(format!(
                    "(code {} of {}{expires}; a new one follows when it expires)",
                    v["code_number"].as_u64().unwrap_or(1),
                    v["max_codes"].as_u64().unwrap_or(5)
                ));
                self.code = code;
            }
        } else if state != self.state {
            match state.as_str() {
                "checking" => out.push(format!("Checking {name}…")),
                "browser" => out.push(format!(
                    "Open this address in a browser on this computer to sign in: {}",
                    v["auth_url"].as_str().unwrap_or("?")
                )),
                _ => {}
            }
        }
        self.state = state;
        out
    }
}

/// The Check's lines; with `create` (`--create`), a missing folder is a statement of
/// what happens next, not the wizard's question.
pub fn check_lines(v: &Value, create: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(a) = v["account"].as_str() {
        out.push(format!("Signed in as {a}."));
    }
    let c = &v["check"];
    if let Some(p) = v["pairing"].as_str() {
        out.push(format!(
            "Pairing: {}",
            bunko_server::storage::connect::pairing_label(p)
        ));
    }
    if create && c["found"].as_bool() == Some(false) {
        let folder = c["folder"].as_str().unwrap_or("mokuro-reader");
        out.push(format!("Creating {folder}/…"));
    } else if let Some(s) = c["summary"].as_str() {
        out.push(s.to_string());
    }
    for k in ["warning", "lease"] {
        if let Some(w) = c[k].as_str() {
            out.push(format!("Note: {w}"));
        }
    }
    if c["offer_switch"].as_bool() == Some(true)
        && let Some(other) = c["other_pairing"].as_str()
    {
        out.push(format!(
            "To try the other pairing: mokuro-bunko library connect gdrive --reader {other}"
        ));
    }
    out
}

/// Whether to create a missing folder: `--create`, else asked on a terminal.
fn want_create(v: &Value, create: bool, interactive: bool) -> Result<bool, Fail> {
    if v["check"]["found"].as_bool() != Some(false) {
        return Ok(false);
    }
    if create {
        return Ok(true);
    }
    if !interactive {
        println!("Run again with --create to make the folder, or create it in the cloud first.");
        return Ok(false);
    }
    let folder = v["check"]["folder"].as_str().unwrap_or("mokuro-reader");
    crate::prompt::confirm(&format!("Create {folder}/ now?"), Some(true))
}

fn finished(v: &Value, use_this_account: bool) -> Result<bool, Fail> {
    match v["state"].as_str().unwrap_or("") {
        "connected" => Ok(true),
        "account-mismatch" if use_this_account => Ok(false),
        "account-mismatch" => Err(Fail::msg(format!(
            "{} Run again with --use-this-account to switch.",
            v["message"].as_str().unwrap_or("another account")
        ))),
        "failed" => Err(Fail::msg(
            v["error"]
                .as_str()
                .unwrap_or("the sign-in failed")
                .to_string(),
        )),
        "stopped" | "cancelled" => Err(Fail::msg(
            v["message"]
                .as_str()
                .unwrap_or("the sign-in stopped")
                .to_string(),
        )),
        _ => Ok(false),
    }
}

/// A loopback client for this machine's control API (no proxy, no redirects).
fn control_client() -> Result<reqwest::Client, Fail> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Fail::msg)
}

/// The control API's base URL: loopback http on the file's port, whatever else the
/// file says.
pub fn control_base(file: &bunko_control::ControlFile) -> String {
    format!("http://127.0.0.1:{}", file.port)
}

async fn call(
    client: &reqwest::Client,
    file: &bunko_control::ControlFile,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), Fail> {
    let mut r = client
        .request(method, format!("{}{path}", control_base(file)))
        .bearer_auth(&file.token);
    if let Some(b) = body {
        r = r.json(b);
    }
    let resp = r
        .send()
        .await
        .map_err(|e| Fail::msg(format!("the running server: {}", e.without_url())))?;
    let status = resp.status().as_u16();
    let v = resp.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, v))
}

fn connect_through_server(
    file: &bunko_control::ControlFile,
    req: bunko_server::storage::connect::ConnectRequest,
    create: bool,
    interactive: bool,
) -> CmdResult {
    let use_this = req.use_this_account;
    let body = connect_body(&req);
    let rt = runtime()?;
    rt.block_on(async {
        let client = control_client()?;
        println!(
            "Asking the running server (its sign-in shows in the admin panel and the tray too)…"
        );
        let (status, v) = call(
            &client,
            file,
            reqwest::Method::POST,
            "/app/api/library/connect",
            Some(&body),
        )
        .await?;
        if status == 404 {
            return Err(Fail::msg("the running instance is not a library server"));
        }
        if status != 200 {
            return Err(Fail::msg(
                v["error"]
                    .as_str()
                    .unwrap_or("the server refused the request")
                    .to_string(),
            ));
        }
        let session = v["session"].as_u64();
        let mut p = Printer::default();
        let mut asked_account = false;
        let v = loop {
            let (_, v) = call(
                &client,
                file,
                reqwest::Method::GET,
                "/app/api/library/connect",
                None,
            )
            .await?;
            if v["session"].as_u64() != session {
                return Err(Fail::msg("another sign-in replaced this one"));
            }
            p.show(&v);
            if finished(&v, use_this)? {
                break v;
            }
            if v["state"] == "account-mismatch" && !asked_account {
                asked_account = true;
                let (status, r) = call(
                    &client,
                    file,
                    reqwest::Method::POST,
                    "/app/api/library/connect/account",
                    Some(&json!({"use_this_account": true})),
                )
                .await?;
                if status != 200 {
                    return Err(Fail::msg(
                        r["error"]
                            .as_str()
                            .unwrap_or("switching the account failed")
                            .to_string(),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        };
        for l in check_lines(&v, create) {
            println!("{l}");
        }
        if want_create(&v, create, interactive)? {
            let (status, r) = call(
                &client,
                file,
                reqwest::Method::POST,
                "/app/api/library/connect/create",
                None,
            )
            .await?;
            if status != 200 {
                return Err(Fail::msg(
                    r["error"]
                        .as_str()
                        .unwrap_or("creating the folder failed")
                        .to_string(),
                ));
            }
            if let Some(s) = r["check"]["summary"].as_str() {
                println!("{s}");
            }
        }
        println!("Connected. The server restarts to open the library.");
        Ok(())
    })
}

fn connect_body(req: &bunko_server::storage::connect::ConnectRequest) -> Value {
    let mut v = json!({
        "provider": req.provider,
        "reader": req.reader,
        "folder": req.folder,
        "flow": req.flow,
        "use_this_account": req.use_this_account,
    });
    if let Some(w) = &req.webdav {
        v["webdav"] = json!({"url": w.url, "username": w.username, "password": w.password, "allow_http": w.allow_http});
    }
    if let Some(m) = &req.mega {
        v["mega"] = json!({"email": m.email, "password": m.password, "totp": m.totp});
    }
    v
}

/// The configuration file as `library connect` and `disconnect` change it here.
fn local_control(ctx: &Ctx) -> Result<std::sync::Arc<bunko_server::storage::StorageControl>, Fail> {
    let config = cfgfile::load_effective(&ctx.config_path)?;
    let (rp, wp) = (ctx.config_path.clone(), ctx.config_path.clone());
    let fallback = config.clone();
    Ok(bunko_server::storage::StorageControl::new(
        config.storage.layout().cloud_dir(),
        None,
        std::sync::Arc::new(move || {
            cfgfile::load_effective_quiet(&rp).unwrap_or_else(|_| fallback.clone())
        }),
        std::sync::Arc::new(move |f: &mut dyn FnMut(&mut bunko_core::Config)| {
            let mut c = cfgfile::load_for_write(&wp).map_err(|e| e.to_string())?;
            f(&mut c);
            cfgfile::save(&c, &wp).map_err(|e| e.to_string())
        }),
        None,
    ))
}

fn connect_here(
    ctx: &Ctx,
    req: bunko_server::storage::connect::ConnectRequest,
    create: bool,
    interactive: bool,
) -> CmdResult {
    let control = local_control(ctx)?;
    let use_this = req.use_this_account;
    let rt = runtime()?;
    rt.block_on(async {
        let first = control
            .connect(
                req,
                bunko_server::storage::connect::RequestOrigin::cli(),
                false,
            )
            .map_err(Fail::msg)?;
        let session = first.session;
        let mut p = Printer::default();
        let mut asked_account = false;
        let v = loop {
            let s = control
                .connector()
                .wait_for(
                    |s| s.session != session || !s.in_progress() || s.state == "code",
                    Duration::from_secs(1),
                )
                .await;
            let v = s.as_json();
            p.show(&v);
            if finished(&v, use_this)? {
                break v;
            }
            if s.state == "account-mismatch" && !asked_account {
                asked_account = true;
                control
                    .decide_account(true, false)
                    .await
                    .map_err(Fail::msg)?;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        for l in check_lines(&v, create) {
            println!("{l}");
        }
        if want_create(&v, create, interactive)? {
            let check = control
                .connector()
                .create_folder()
                .await
                .map_err(Fail::msg)?;
            println!("{}", check.summary);
        }
        println!(
            "Connected; {} now names this library. Start the server to open it: mokuro-bunko serve",
            ctx.config_path.display()
        );
        Ok(())
    })
}

pub fn disconnect(ctx: &Ctx, a: crate::cli::LibraryDisconnectArgs) -> CmdResult {
    use bunko_server::storage::disconnect::{DisconnectMode, copy_export, prepare_export_dir};
    let config = cfgfile::load_effective(&ctx.config_path)?;
    // `--export DIR` is this user's own folder: written here, in-process. A running
    // server is never told a folder (no HTTP API takes one): it exports into its own
    // `<base>/exports/...`, and the files are copied from there into DIR.
    let export_to = match &a.export {
        Some(dir) => Some(std::path::absolute(dir).map_err(Fail::msg)?),
        None => None,
    };
    let mode = match (&export_to, a.discard) {
        (Some(_), _) => json!({"mode": "export"}),
        (None, true) => json!({"mode": "discard"}),
        (None, false) => json!({"mode": "wait", "wait_s": a.wait}),
    };
    if !a.yes {
        if !std::io::stdin().is_terminal() {
            return Err(Fail::msg(
                "library disconnect asks first: run it on a terminal, or pass --yes",
            ));
        }
        let what = if a.discard {
            "Disconnect the cloud library and DISCARD the results not yet uploaded?"
        } else {
            "Disconnect the cloud library? (Nothing in the cloud is deleted.)"
        };
        if !crate::prompt::confirm(what, Some(false))? {
            return Err(Fail::Exit(1));
        }
    }
    let live = bunko_control::file::live_instance(&config.storage.base_path);
    if let (Some(_), Some(dir)) = (&live, &export_to) {
        // Refused before anything is disconnected: a new or empty plain folder.
        prepare_export_dir(dir).map_err(Fail::msg)?;
    }
    let answer = match live {
        Some(file) => {
            let rt = runtime()?;
            rt.block_on(async {
                let client = control_client()?;
                let (status, v) = call(
                    &client,
                    &file,
                    reqwest::Method::POST,
                    "/app/api/library/disconnect",
                    Some(&mode),
                )
                .await?;
                Ok::<_, Fail>(match status {
                    200 => Ok(v),
                    404 => Err(json!({"error": "the running instance is not a library server"})),
                    _ => Err(v),
                })
            })?
        }
        None => {
            let control = local_control(ctx)?;
            let m = match &export_to {
                Some(dir) => DisconnectMode::ExportTo(dir.clone()),
                None => DisconnectMode::from_json(&mode).map_err(Fail::msg)?,
            };
            match control.disconnect(&m) {
                Ok(report) => Ok(json!({"report": report, "restarting": false})),
                Err(e) => Err(e.as_json()),
            }
        }
    };
    match answer {
        Ok(v) => {
            let r = &v["report"];
            if let Some(n) = r["exported"].as_array().filter(|a| !a.is_empty()) {
                let written = r["export_dir"].as_str().map(std::path::PathBuf::from);
                match (&written, &export_to) {
                    // The server wrote them into its own folder: copy them into DIR.
                    (Some(from), Some(to)) if from != to => match copy_export(from, to) {
                        Ok(_) => println!(
                            "Exported {} result(s) to {} (the server's copy: {}).",
                            n.len(),
                            to.display(),
                            from.display()
                        ),
                        Err(e) => eprintln!(
                            "Exported {} result(s) to {}; copying them into {} failed: {e}",
                            n.len(),
                            from.display(),
                            to.display()
                        ),
                    },
                    (Some(dir), _) => {
                        println!("Exported {} result(s) to {}.", n.len(), dir.display())
                    }
                    (None, _) => println!("Exported {} result(s).", n.len()),
                }
            }
            if r["discarded"].as_u64().unwrap_or(0) > 0 {
                println!("Discarded {} result(s).", r["discarded"]);
            }
            if r["released"] == true {
                println!(
                    "Wrote \"released\" into the folder's marker: readers write their own metadata again."
                );
            }
            println!(
                "Disconnected. The local index and caches go at the next start; nothing in the cloud was deleted.{}",
                if v["restarting"] == true {
                    " The server restarts now."
                } else {
                    ""
                }
            );
            Ok(())
        }
        Err(e) => {
            if e.get("owed").is_some() {
                eprintln!(
                    "Error: {}",
                    e["error"].as_str().unwrap_or("results are still owed")
                );
                eprintln!(
                    "Run again with --wait <SECS> (wait longer), --export <DIR> or --discard."
                );
                return Err(Fail::Exit(1));
            }
            Err(Fail::msg(
                e["error"]
                    .as_str()
                    .unwrap_or("the disconnect failed")
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The control file's own `url` is never followed: the request goes to loopback
    /// http on its port, and the token is not part of the URL.
    #[test]
    fn the_control_api_is_only_asked_on_loopback() {
        let file = bunko_control::ControlFile {
            role: bunko_control::Role::Server,
            pid: 1,
            port: 4321,
            token: "sekrit-token".into(),
            version: "0.8.0".into(),
            started_at: "2026-10-12T00:00:00Z".into(),
            url: "https://evil.example:4321".into(),
            managed: false,
        };
        let url = control_status_url(&file);
        assert_eq!(url, "http://127.0.0.1:4321/control/status");
        assert!(!url.contains("sekrit"));
        assert!(bunko_control::http::is_loopback_host("127.0.0.1:4321"));
        // Nothing listens there: the error names no token.
        let err = ask_control(&bunko_control::ControlFile { port: 1, ..file }).unwrap_err();
        assert!(!err.contains("sekrit"), "{err}");
    }

    /// A store block never carries the credentials it was opened with.
    #[test]
    fn printed_lines_carry_no_secret() {
        let v = json!({"backend": "webdav", "folder": "mokuro-reader", "store_id": "x",
            "account": "me@https://dav.example/", "problem": null});
        let all = describe(&v, "configuration").join("\n");
        assert!(!all.contains("password"));
    }

    #[test]
    fn live_blocks_read_as_lines() {
        let v = json!({
            "backend": "gdrive", "folder": "mokuro-reader", "store_id": "0123456789abcdef",
            "account": "me@example.com", "pairing": "release", "connected": true, "problem": null,
            "health": {"state": "syncing", "done": 10, "total": 0}, "engine": "full-sync",
            "sync": {"done": 10, "total": null},
            "cache": {"blobs": 2, "blob_bytes": 2048, "zipdirs": 1, "blocks": 0, "block_bytes": 0, "spools": 1, "spool_bytes": 5242880},
        });
        let lines = describe(&v, "running server");
        assert_eq!(
            lines,
            [
                "Library: gdrive, folder mokuro-reader (pairing release)",
                "Account: me@example.com",
                "Store:   0123456789abcdef",
                "Health:  syncing (feed: full-sync), 10 synced",
                "Cache:   2 small files (2.0 KiB), 1 archive directories, 1 spooled archives (5.0 MiB)",
                "(from the running server)",
            ]
        );
        // This Bunko's first claim: what the bridge's 503 says, with the time left.
        let mut v = v;
        v["lease"] = json!({"state": "claiming", "can_take_over": false, "claiming_left_s": 540,
            "detail": "This Bunko is still claiming the library: about 9 min left"});
        assert!(
            describe(&v, "running server").contains(
                &"Lease:   claiming: This Bunko is still claiming the library: about 9 min left"
                    .to_string()
            )
        );
        v["lease"] = json!({"state": "standby", "can_take_over": true,
            "holder": {"name": "nas", "ours": false}});
        assert!(
            describe(&v, "running server").contains(&"Lease:   standby (held by nas)".to_string())
        );
    }

    #[test]
    fn offline_blocks_say_what_is_missing() {
        let v = json!({
            "backend": "onedrive", "folder": "mokuro-reader", "store_id": null, "account": null,
            "pairing": null, "problem": "not signed in to OneDrive: run `mokuro-bunko library connect onedrive`",
            "store_dir": null, "indexed": false,
        });
        let lines = describe(&v, "configuration");
        assert_eq!(lines[1], "Store:   not connected");
        assert!(lines[2].starts_with("Problem: not signed in to OneDrive"));
        assert!(lines[3].starts_with("Index:   none yet"));
        let local = describe(
            &json!({"backend": "local", "folder": "/data/library"}),
            "configuration",
        );
        assert_eq!(local[0], "Library: on this disk");
    }

    fn parse(args: &[&str]) -> Result<crate::cli::Cli, clap::Error> {
        use clap::Parser;
        let mut v = vec!["mokuro-bunko"];
        v.extend_from_slice(args);
        crate::cli::Cli::try_parse_from(v)
    }

    fn connect_args(args: &[&str]) -> crate::cli::LibraryConnectArgs {
        let mut v = vec!["library", "connect"];
        v.extend_from_slice(args);
        match parse(&v).unwrap().command {
            Some(crate::cli::Command::Library(LibraryCmd::Connect(a))) => a,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn connect_and_disconnect_arguments() {
        let a = connect_args(&["gdrive", "--reader", "beta"]);
        assert_eq!(a.provider, crate::cli::CloudProvider::Gdrive);
        assert_eq!(a.reader, Some(crate::cli::ReaderPairing::Beta));
        let a = connect_args(&[
            "webdav",
            "--url",
            "https://dav.example/",
            "--username",
            "me",
            "--password-file",
            "/run/secrets/dav",
        ]);
        assert_eq!(a.url.as_deref(), Some("https://dav.example/"));
        assert_eq!(
            a.password_file.as_deref(),
            Some(std::path::Path::new("/run/secrets/dav"))
        );
        let a = connect_args(&["mega", "--email", "me@example.com"]);
        assert_eq!(a.email.as_deref(), Some("me@example.com"));
        assert!(parse(&["library", "connect", "dropbox"]).is_err());
        assert!(parse(&["library", "connect", "gdrive", "--reader", "nightly"]).is_err());
        // No password on the command line, ever.
        assert!(parse(&["library", "connect", "webdav", "--password", "x"]).is_err());
        assert!(parse(&["library", "disconnect", "--export", "/tmp/x", "--discard"]).is_err());
        match parse(&["library", "disconnect", "--wait", "30", "-y"])
            .unwrap()
            .command
        {
            Some(crate::cli::Command::Library(LibraryCmd::Disconnect(d))) => {
                assert_eq!(d.wait, 30);
                assert!(d.yes && !d.discard && d.export.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn connect_requests_from_flags_and_config() {
        let mut config = bunko_core::Config::default();
        // --reader is for Drive, --email for MEGA, the WebDAV flags for WebDAV.
        let a = connect_args(&["onedrive", "--reader", "beta"]);
        assert!(connect_request(&a, &config, None, None).is_err());
        let a = connect_args(&["gdrive", "--email", "x@y"]);
        assert!(connect_request(&a, &config, None, None).is_err());
        let a = connect_args(&["mega", "--url", "https://x"]);
        assert!(connect_request(&a, &config, None, None).is_err());
        // Drive: always the device flow from a terminal.
        let a = connect_args(&["gdrive", "--reader", "beta", "--folder", "manga"]);
        let r = connect_request(&a, &config, None, None).unwrap();
        assert_eq!(
            (r.provider.as_str(), r.reader.as_deref(), r.flow.as_deref()),
            ("gdrive", Some("beta"), Some("device"))
        );
        assert_eq!(r.folder.as_deref(), Some("manga"));
        // WebDAV: the URL and user may come from config; the password never does.
        config.storage.library.webdav.url = Some("https://nas.example/dav".into());
        config.storage.library.webdav.username = Some("me".into());
        let a = connect_args(&["webdav"]);
        assert!(connect_request(&a, &config, None, None).is_err());
        let r = connect_request(&a, &config, Some("app-secret".into()), None).unwrap();
        let w = r.webdav.as_ref().unwrap();
        assert_eq!(
            (w.url.as_str(), w.username.as_str()),
            ("https://nas.example/dav", "me")
        );
        assert!(!format!("{r:?}").contains("app-secret"));
        // MEGA: the 2FA code is optional.
        let a = connect_args(&["mega", "--email", "me@example.com"]);
        let r = connect_request(&a, &config, Some("hunter2".into()), Some(" ".into())).unwrap();
        assert_eq!(r.mega.as_ref().unwrap().totp, None);
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    /// The device code is printed with where to enter it and its expiry, once per code;
    /// nothing a status could carry beyond that is printed.
    #[test]
    fn the_terminal_shows_each_code_once() {
        let mut p = Printer::default();
        let code = json!({"state": "code", "provider": "gdrive", "user_code": "ABCD-EFGH",
            "verification_url": "https://www.google.com/device", "expires_at": "2026-10-13T14:32:00Z",
            "code_number": 1, "max_codes": 5});
        let lines = p.lines(&code).join("\n");
        assert!(
            lines.contains("open https://www.google.com/device"),
            "{lines}"
        );
        assert!(lines.contains("    ABCD-EFGH"), "{lines}");
        assert!(
            lines.contains("code 1 of 5; it expires at 14:32 UTC"),
            "{lines}"
        );
        assert!(
            p.lines(&code).is_empty(),
            "the same code is not printed again"
        );
        let mut next = code.clone();
        next["user_code"] = json!("WXYZ-1234");
        next["code_number"] = json!(2);
        assert!(p.lines(&next).join("\n").contains("code 2 of 5"));
        assert_eq!(
            p.lines(&json!({"state": "checking", "provider": "gdrive"})),
            ["Checking Google Drive…"]
        );
        // The finished check, with the pairing offer.
        let done = json!({"state": "connected", "account": "me@example.com", "pairing": "release",
            "check": {"summary": "Found mokuro-reader/ — 4 volumes in 2 series", "offer_switch": true,
                      "other_pairing": "beta", "warning": "Only 4 volumes are visible", "found": true}});
        let lines = check_lines(&done, false);
        assert_eq!(lines[0], "Signed in as me@example.com.");
        assert!(
            lines
                .iter()
                .any(|l| l == "Pairing: reader.mokuro.app (release)")
        );
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with("library connect gdrive --reader beta"))
        );
        assert!(finished(&done, false).unwrap());
        // No folder yet: the wizard's question, or with --create what happens next.
        let missing = json!({"state": "connected", "check": {"found": false,
            "folder": "mokuro-reader", "summary": "No mokuro-reader/ folder yet. Create it?"}});
        assert_eq!(
            check_lines(&missing, false),
            ["No mokuro-reader/ folder yet. Create it?"]
        );
        assert_eq!(check_lines(&missing, true), ["Creating mokuro-reader/…"]);
        let mismatch =
            json!({"state": "account-mismatch", "message": "belongs to old@example.com"});
        assert!(
            finished(&mismatch, false)
                .unwrap_err()
                .to_string()
                .contains("--use-this-account")
        );
        assert!(!finished(&mismatch, true).unwrap());
        assert!(
            finished(
                &json!({"state": "stopped", "message": "5 codes went unused"}),
                false
            )
            .is_err()
        );
    }

    /// With a server running on this storage, `library connect` asks it through the
    /// control API (what `docker exec` does): the token as a bearer, loopback only, the
    /// device flow requested, the server's sign-in followed to its end.
    #[test]
    fn connect_asks_the_running_server() {
        use axum::routing::{get, post};
        use std::sync::{Arc, Mutex};
        type Seen = Arc<Mutex<Vec<(String, Option<String>, Value)>>>;
        let seen: Seen = Arc::default();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let port = {
            let (s1, s2, p2) = (seen.clone(), seen.clone(), polls.clone());
            let app = axum::Router::new()
                .route(
                    "/app/api/library/connect",
                    post(move |h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| {
                        let s = s1.clone();
                        async move {
                            let auth = h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                            s.lock().unwrap().push(("POST".into(), auth, b));
                            axum::Json(json!({"state": "starting", "session": 7}))
                        }
                    })
                    .get(move |h: axum::http::HeaderMap| {
                        let (s, p) = (s2.clone(), p2.clone());
                        async move {
                            let auth = h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                            s.lock().unwrap().push(("GET".into(), auth, Value::Null));
                            let n = p.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            axum::Json(if n == 0 {
                                json!({"state": "code", "session": 7, "provider": "gdrive", "user_code": "ABCD-EFGH",
                                       "verification_url": "https://www.google.com/device", "code_number": 1, "max_codes": 5})
                            } else {
                                json!({"state": "connected", "session": 7, "provider": "gdrive", "account": "me@example.com",
                                       "check": {"found": true, "summary": "Found mokuro-reader/ — 1 volume in 1 series"}})
                            })
                        }
                    }),
                )
                .route("/app/api/library/storage", get(|| async { axum::Json(json!({})) }));
            let l = rt
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
            let port = l.local_addr().unwrap().port();
            rt.spawn(async move { axum::serve(l, app).await.unwrap() });
            port
        };
        let file = bunko_control::ControlFile {
            role: bunko_control::Role::Server,
            pid: 1,
            port,
            token: "the-control-token".into(),
            version: "0.8.0".into(),
            started_at: String::new(),
            url: "https://elsewhere.example".into(),
            managed: false,
        };
        assert_eq!(control_base(&file), format!("http://127.0.0.1:{port}"));
        let a = connect_args(&["gdrive", "--reader", "beta"]);
        let req = connect_request(&a, &bunko_core::Config::default(), None, None).unwrap();
        connect_through_server(&file, req, false, false).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "POST");
        assert_eq!(seen[0].1.as_deref(), Some("Bearer the-control-token"));
        assert_eq!(seen[0].2["provider"], "gdrive");
        assert_eq!(seen[0].2["reader"], "beta");
        assert_eq!(seen[0].2["flow"], "device");
        assert!(seen.iter().filter(|s| s.0 == "GET").count() >= 2);
        drop(seen);
        rt.shutdown_background();
    }
}
