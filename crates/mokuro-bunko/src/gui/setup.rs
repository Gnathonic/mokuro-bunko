//! The processor pairing page's "write the configuration" step, over the same code the
//! CLI uses: `processor setup` (bunko-processor checks the account against the
//! library, renders and writes processor.yaml mode 600), plus the processor settings
//! the CLI wizard leaves at their defaults. (A library server sets itself up on its
//! own `/setup` page.)

use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The processor wizard's answers (and the processor settings page's).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProcessorForm {
    pub url: String,
    pub username: String,
    /// Empty on the settings page: keep the one in the file.
    pub password: String,
    /// `true`, `false` or a certificate path.
    pub tls_verify: String,
    pub name: String,
    pub public_name: String,
    pub max_sessions: Option<u32>,
    pub storage: String,
    pub archive_memory_mb: Option<u64>,
    /// `processor.auto_update`: install the library's updates (opt-in).
    pub auto_update: bool,
    pub overwrite: bool,
}

#[cfg(feature = "ocr")]
pub use full::*;

#[cfg(feature = "ocr")]
mod full {
    use super::*;
    use bunko_processor::config::{DEFAULT_ARCHIVE_MEMORY_MB, TlsVerify, default_name};
    use bunko_processor::setup as ps;
    use serde_yaml_ng::{Mapping, Value as Yaml};

    fn tls(raw: &str) -> Result<TlsVerify, String> {
        ps::parse_tls_verify(if raw.is_empty() { "true" } else { raw }).map_err(|e| e.0)
    }

    /// Log in with the answers (registers nothing). The library's answer, or why not.
    pub async fn test_connection(form: &ProcessorForm) -> Result<Value, String> {
        let (url, note) = ps::normalize_url(&form.url).map_err(|e| e.0)?;
        if form.username.trim().is_empty() || form.password.is_empty() {
            return Err("the processor account's username and password are needed".into());
        }
        let v = ps::verify_account(
            &url,
            form.username.trim(),
            &form.password,
            &tls(&form.tls_verify)?,
        )
        .await
        .map_err(|e| e.0)?;
        let mut notes: Vec<String> = note.into_iter().collect();
        notes.extend(v.notes);
        Ok(json!({
            "url": url,
            "username": v.username,
            "role": v.role,
            "protocol": v.protocol,
            "library_version": v.library_version,
            "notes": notes,
        }))
    }

    /// The processor keys that differ from their defaults, added to `data`.
    fn put_processor_keys(data: &mut Mapping, form: &ProcessorForm) -> Result<(), String> {
        let mut p = match data.remove("processor") {
            Some(Yaml::Mapping(m)) => m,
            _ => Mapping::new(),
        };
        let hostname = default_name();
        let name = form.name.trim();
        if name.is_empty() || name == hostname {
            p.remove("name");
        } else {
            p.insert("name".into(), name.into());
        }
        let public = form.public_name.trim();
        if public.is_empty() {
            p.remove("public_name");
        } else {
            if public.chars().count() > bunko_processor::config::MAX_PUBLIC_NAME {
                return Err("public name: at most 64 characters".into());
            }
            p.insert("public_name".into(), public.into());
        }
        match form.max_sessions {
            Some(0) => return Err("max sessions: at least 1".into()),
            Some(1) | None => {
                p.remove("max_sessions");
            }
            Some(n) => {
                p.insert("max_sessions".into(), Yaml::Number(n.into()));
            }
        }
        let storage = form.storage.trim();
        let default_storage = bunko_processor::config::default_storage_path();
        if storage.is_empty() || Path::new(storage) == default_storage {
            p.remove("storage");
        } else {
            p.insert("storage".into(), storage.into());
        }
        match form.archive_memory_mb {
            Some(n) if n != DEFAULT_ARCHIVE_MEMORY_MB => {
                p.insert("archive_memory_mb".into(), Yaml::Number(n.into()));
            }
            _ => {
                p.remove("archive_memory_mb");
            }
        }
        if form.auto_update {
            p.insert("auto_update".into(), Yaml::Bool(true));
        } else {
            p.remove("auto_update");
        }
        if !p.is_empty() {
            data.insert("processor".into(), Yaml::Mapping(p));
        }
        Ok(())
    }

    fn render(data: &Mapping) -> String {
        format!(
            "{}{}",
            ps::HEADER,
            serde_yaml_ng::to_string(&Yaml::Mapping(data.clone())).unwrap_or_default()
        )
    }

    /// Check the account, then write processor.yaml. Nothing is written when the
    /// library refuses.
    pub async fn write_processor(form: &ProcessorForm, path: &Path) -> Result<Value, String> {
        if path.exists() && !form.overwrite {
            return Err(format!(
                "{} already exists: tick \"Replace it\" to write a new one",
                path.display()
            ));
        }
        let checked = test_connection(form).await?;
        let url = checked["url"].as_str().unwrap_or_default().to_string();
        let username = checked["username"].as_str().unwrap_or_default().to_string();
        let text = ps::render_config(
            &url,
            &username,
            &form.password,
            None,
            &tls(&form.tls_verify)?,
            &default_name(),
            false,
        );
        let mut data: Mapping = serde_yaml_ng::from_str(&text).map_err(|e| e.to_string())?;
        put_processor_keys(&mut data, form)?;
        let warning = ps::write_config(path, &render(&data), true).map_err(|e| e.0)?;
        Ok(json!({"connection": checked, "config": path, "warning": warning}))
    }

    /// processor.yaml as the settings page shows it (no password).
    pub fn read_processor(path: &Path) -> Result<Value, String> {
        if !path.is_file() {
            return Ok(json!({"exists": false, "config": path,
                "defaults": {"name": default_name(), "max_sessions": 1,
                    "storage": bunko_processor::config::default_storage_path(),
                    "archive_memory_mb": DEFAULT_ARCHIVE_MEMORY_MB}}));
        }
        let c = bunko_processor::load_processor_config(path).map_err(|e| e.0)?;
        let tls = match &c.library.tls_verify {
            TlsVerify::Yes => "true".to_string(),
            TlsVerify::No => "false".to_string(),
            TlsVerify::Cert(p) => p.display().to_string(),
        };
        Ok(json!({
            "exists": true,
            "config": path,
            "url": c.library.url,
            "username": c.library.username,
            "password_set": !c.library.password.is_empty(),
            "tls_verify": tls,
            "name": c.processor.name,
            "public_name": c.processor.public_name,
            "max_sessions": c.processor.max_sessions,
            "storage": c.processor.storage,
            "archive_memory_mb": c.processor.archive_memory_mb,
            "auto_update": c.processor.auto_update,
            "status": bunko_processor::status::read_status(&c.processor.storage),
        }))
    }

    /// Change processor.yaml in place (other keys, comments aside, kept). A changed
    /// login is checked against the library first.
    pub async fn update_processor(form: &ProcessorForm, path: &Path) -> Result<Value, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut data: Mapping = serde_yaml_ng::from_str(&raw).map_err(|e| e.to_string())?;
        let current = bunko_processor::load_processor_config(path).map_err(|e| e.0)?;
        let mut lib = match data.remove("library") {
            Some(Yaml::Mapping(m)) => m,
            _ => Mapping::new(),
        };
        let password = if form.password.is_empty() {
            current.library.password.clone()
        } else {
            form.password.clone()
        };
        let (url, _) = ps::normalize_url(&form.url).map_err(|e| e.0)?;
        let tls_verify = tls(&form.tls_verify)?;
        let login_changed = url != current.library.url
            || form.username.trim() != current.library.username
            || password != current.library.password
            || tls_verify != current.library.tls_verify;
        let mut checked = Value::Null;
        if login_changed {
            let mut f = ProcessorForm {
                url: url.clone(),
                username: form.username.clone(),
                password: password.clone(),
                tls_verify: form.tls_verify.clone(),
                ..ProcessorForm::default()
            };
            checked = test_connection(&f).await?;
            f.password.clear();
        }
        lib.insert("url".into(), url.into());
        lib.insert("username".into(), form.username.trim().into());
        if !form.password.is_empty() {
            lib.remove("password_file");
            lib.insert("password".into(), form.password.clone().into());
        }
        match &tls_verify {
            TlsVerify::Yes => {
                lib.remove("tls_verify");
            }
            TlsVerify::No => {
                lib.insert("tls_verify".into(), Yaml::Bool(false));
            }
            TlsVerify::Cert(p) => {
                lib.insert("tls_verify".into(), p.to_string_lossy().into_owned().into());
            }
        }
        data.insert("library".into(), Yaml::Mapping(lib));
        put_processor_keys(&mut data, form)?;
        let warning = ps::write_config(path, &render(&data), true).map_err(|e| e.0)?;
        Ok(
            json!({"connection": checked, "config": path, "warning": warning,
                  "restart_needed": true}),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn processor_keys_only_when_not_default() {
            let mut data = Mapping::new();
            let form = ProcessorForm {
                name: default_name(),
                max_sessions: Some(1),
                archive_memory_mb: Some(DEFAULT_ARCHIVE_MEMORY_MB),
                ..ProcessorForm::default()
            };
            put_processor_keys(&mut data, &form).unwrap();
            assert!(data.get("processor").is_none());
            let form = ProcessorForm {
                name: "tower-x".into(),
                public_name: "Tower".into(),
                max_sessions: Some(3),
                storage: "/srv/p".into(),
                archive_memory_mb: Some(512),
                auto_update: true,
                ..ProcessorForm::default()
            };
            put_processor_keys(&mut data, &form).unwrap();
            let text = serde_yaml_ng::to_string(&Yaml::Mapping(data.clone())).unwrap();
            for k in [
                "name: tower-x",
                "public_name: Tower",
                "max_sessions: 3",
                "storage: /srv/p",
                "archive_memory_mb: 512",
                "auto_update: true",
            ] {
                assert!(text.contains(k), "{text}");
            }
            let bad = ProcessorForm {
                max_sessions: Some(0),
                ..ProcessorForm::default()
            };
            assert!(put_processor_keys(&mut data, &bad).is_err());
        }
    }
}

/// The directories and files a picker may show under `dir`: subdirectories, and
/// with `files`, the files whose extension is in `exts` (empty: none).
pub fn list_dir(dir: &Path, exts: &[String]) -> Result<Value, String> {
    let dir = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in rd.flatten().take(5000) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let is_dir = ft.is_dir() || (ft.is_symlink() && e.path().is_dir());
        if is_dir {
            dirs.push(name);
        } else if !exts.is_empty() {
            let ext = Path::new(&name)
                .extension()
                .map(|x| x.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if exts.iter().any(|x| x.eq_ignore_ascii_case(&ext)) {
                files.push(name);
            }
        }
    }
    dirs.sort_by_key(|a| a.to_lowercase());
    files.sort_by_key(|a| a.to_lowercase());
    let parent: Option<PathBuf> = dir.parent().map(Path::to_path_buf);
    Ok(
        json!({"path": dir, "parent": parent, "dirs": dirs, "files": files,
              "sep": std::path::MAIN_SEPARATOR.to_string()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_lists_folders_and_chosen_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Manga")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("cert.PEM"), "x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let v = list_dir(dir.path(), &[]).unwrap();
        assert_eq!(v["dirs"], json!(["Manga"]));
        assert_eq!(v["files"], json!([]));
        let v = list_dir(dir.path(), &["pem".into()]).unwrap();
        assert_eq!(v["files"], json!(["cert.PEM"]));
        assert!(list_dir(&dir.path().join("nope"), &[]).is_err());
    }
}
