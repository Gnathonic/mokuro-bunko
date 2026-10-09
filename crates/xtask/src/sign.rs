//! `xtask sign`, `xtask verify`, `xtask keygen`: the ed25519 signature over
//! `release.json` that `bunko-update` checks before trusting any sha256 in it.
//!
//! The secret key is the base64 of the 32-byte ed25519 seed (64-byte seed‖public
//! keypairs are accepted too). It is read from `--key <file>` or `BUNKO_SIGNING_KEY`
//! and never printed.

use crate::util;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use rand::RngCore;
use std::path::{Path, PathBuf};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

#[derive(Debug, clap::Args)]
pub struct SignArgs {
    /// The file to sign (writes `<file>.sig`).
    #[arg(default_value = "dist/release.json")]
    pub file: PathBuf,
    /// File holding the base64 secret key (default: env `BUNKO_SIGNING_KEY`).
    #[arg(long)]
    pub key: Option<PathBuf>,
    /// Expected public key (base64). Default: the key compiled into bunko-update; a
    /// signing key that does not match it is refused (the release could never verify).
    #[arg(long)]
    pub public_key: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct VerifyArgs {
    /// The manifest (its signature is `<file>.sig` unless `--sig`).
    #[arg(default_value = "dist/release.json")]
    pub file: PathBuf,
    #[arg(long)]
    pub sig: Option<PathBuf>,
    /// Public key (base64). Default: the one compiled into bunko-update.
    #[arg(long)]
    pub public_key: Option<String>,
    /// Also check every artifact found in this directory against the manifest's
    /// sha256 and size.
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// With `--dir`: fail when an artifact listed in the manifest is not there.
    #[arg(long)]
    pub require_all: bool,
}

#[derive(Debug, clap::Args)]
pub struct KeygenArgs {
    /// Where to write the new secret key (mode 0600; never overwritten).
    #[arg(long)]
    pub out: PathBuf,
}

fn load_key(path: Option<&Path>) -> Result<SigningKey> {
    let text = match path {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("reading the signing key {}", p.display()))?,
        None => std::env::var("BUNKO_SIGNING_KEY")
            .context("no signing key: pass --key <file> or set BUNKO_SIGNING_KEY")?,
    };
    // Never echo the key material in errors.
    let bytes = B64
        .decode(text.trim())
        .map_err(|_| anyhow::anyhow!("the signing key is not valid base64"))?;
    let seed: [u8; 32] = match bytes.len() {
        32 | 64 => bytes[..32].try_into().context("seed")?,
        n => bail!("the signing key decodes to {n} bytes; expected a 32-byte ed25519 seed"),
    };
    Ok(SigningKey::from_bytes(&seed))
}

pub fn public_key_b64(key: &SigningKey) -> String {
    B64.encode(key.verifying_key().to_bytes())
}

pub fn sign(args: &SignArgs) -> Result<PathBuf> {
    let key = load_key(args.key.as_deref())?;
    let expected = args
        .public_key
        .clone()
        .unwrap_or_else(|| bunko_update::RELEASE_PUBLIC_KEY.to_string());
    let actual = public_key_b64(&key);
    if actual != expected.trim() {
        bail!(
            "the signing key's public key is {actual}, but the expected key is {}; \
             a release signed with it would never verify",
            expected.trim()
        );
    }
    let bytes =
        std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let sig = B64.encode(key.sign(&bytes).to_bytes());
    let sig_path = sig_path(&args.file);
    std::fs::write(&sig_path, format!("{sig}\n"))?;
    // Check it the way the updater will.
    bunko_update::verify_signature(&bytes, &sig, &expected).map_err(|e| anyhow::anyhow!("{e}"))?;
    if serde_json::from_slice::<bunko_update::Manifest>(&bytes).is_ok() {
        bunko_update::parse_manifest(&bytes, &sig, &expected)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    println!("{}", sig_path.display());
    Ok(sig_path)
}

fn sig_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(".sig");
    PathBuf::from(s)
}

pub fn verify(args: &VerifyArgs) -> Result<()> {
    let bytes =
        std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let sig_file = args.sig.clone().unwrap_or_else(|| sig_path(&args.file));
    let sig = std::fs::read_to_string(&sig_file)
        .with_context(|| format!("reading {}", sig_file.display()))?;
    let key = args
        .public_key
        .clone()
        .unwrap_or_else(|| bunko_update::RELEASE_PUBLIC_KEY.to_string());
    let m = bunko_update::parse_manifest(&bytes, &sig, &key)
        .map_err(|e| anyhow::anyhow!("{}: {e}", args.file.display()))?;
    eprintln!(
        "signature OK: release {} ({} targets)",
        m.version,
        m.artifacts.len()
    );
    let mut problems = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let files = m
        .artifacts
        .iter()
        .flat_map(|(t, f)| f.iter().map(move |(k, a)| (t, k.clone(), a.clone())))
        .chain(m.bundles.iter().flat_map(|(t, f)| {
            f.iter().map(move |(k, d)| {
                (
                    t,
                    format!("{k} (dmg)"),
                    bunko_update::Artifact {
                        url: d.url.clone(),
                        sha256: d.sha256.clone(),
                        size: d.size,
                        binary: String::new(),
                    },
                )
            })
        }))
        .collect::<Vec<_>>();
    for (target, flavor, a) in &files {
        {
            let name = a.url.rsplit('/').next().unwrap_or(&a.url);
            if !seen.insert(name.to_string()) {
                problems.push(format!("{name} is listed twice"));
            }
            if a.sha256.len() != 64 || !a.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                problems.push(format!(
                    "{target} {flavor}: sha256 {:?} is malformed",
                    a.sha256
                ));
            }
            let Some(dir) = &args.dir else { continue };
            let path = dir.join(name);
            if !path.is_file() {
                if args.require_all {
                    problems.push(format!("{name} is missing from {}", dir.display()));
                }
                continue;
            }
            let (sha, size) = util::sha256_file(&path)?;
            if !sha.eq_ignore_ascii_case(&a.sha256) || size != a.size {
                problems.push(format!(
                    "{name}: local sha256 {sha} / {size} bytes, manifest {} / {}",
                    a.sha256, a.size
                ));
            } else {
                eprintln!("    {target:<28} {flavor:<10} OK");
            }
        }
    }
    for (target, variants) in &m.backends {
        for (variant, b) in variants {
            let mut whole = <sha2::Sha256 as sha2::Digest>::new();
            let mut complete = true;
            let mut total = 0u64;
            for p in &b.parts {
                let name = p.url.rsplit('/').next().unwrap_or(&p.url);
                if !seen.insert(name.to_string()) {
                    problems.push(format!("{name} is listed twice"));
                }
                let Some(dir) = &args.dir else {
                    complete = false;
                    continue;
                };
                let path = dir.join(name);
                if !path.is_file() {
                    complete = false;
                    if args.require_all {
                        problems.push(format!("{name} is missing from {}", dir.display()));
                    }
                    continue;
                }
                let bytes_ok = {
                    use std::io::Read;
                    let mut f = std::fs::File::open(&path)?;
                    let mut h = <sha2::Sha256 as sha2::Digest>::new();
                    let mut buf = vec![0u8; 1 << 20];
                    let mut n_all = 0u64;
                    loop {
                        let n = f.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        sha2::Digest::update(&mut h, &buf[..n]);
                        sha2::Digest::update(&mut whole, &buf[..n]);
                        n_all += n as u64;
                    }
                    total += n_all;
                    hex::encode(sha2::Digest::finalize(h)) == p.sha256 && n_all == p.size
                };
                if !bytes_ok {
                    problems.push(format!("{name}: does not match the manifest"));
                }
            }
            if complete && args.dir.is_some() {
                let got = hex::encode(sha2::Digest::finalize(whole));
                if got != b.sha256 || total != b.size {
                    problems.push(format!("{target} torch-{variant}: the parts do not add up to the manifest's sha256/size"));
                } else {
                    eprintln!("    {target:<28} torch-{variant:<8} OK");
                }
            }
        }
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("\n"));
    }
    Ok(())
}

pub fn keygen(args: &KeygenArgs) -> Result<()> {
    if args.out.exists() {
        bail!("{} exists; refusing to overwrite a key", args.out.display());
    }
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let key = SigningKey::from_bytes(&seed);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&args.out)?
        .write_all(format!("{}\n", B64.encode(seed)).as_bytes())?;
    // Only the public half is printed.
    println!("{}", public_key_b64(&key));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_with_a_foreign_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("k");
        keygen(&KeygenArgs {
            out: key_file.clone(),
        })
        .unwrap();
        let key = load_key(Some(&key_file)).unwrap();
        let pk = public_key_b64(&key);
        assert!(
            keygen(&KeygenArgs {
                out: key_file.clone()
            })
            .is_err(),
            "never overwrites"
        );

        let manifest = dir.path().join("release.json");
        std::fs::write(
            &manifest,
            br#"{"version":"0.7.0","artifacts":{"x86_64-unknown-linux-musl":{"lite":{"url":"https://x/mokuro-bunko-0.7.0-x86_64-unknown-linux-musl-lite.tar.gz","sha256":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","size":4}}}}"#,
        )
        .unwrap();
        // Refused against the compiled-in key (a different one).
        assert!(
            sign(&SignArgs {
                file: manifest.clone(),
                key: Some(key_file.clone()),
                public_key: None
            })
            .is_err()
        );
        sign(&SignArgs {
            file: manifest.clone(),
            key: Some(key_file.clone()),
            public_key: Some(pk.clone()),
        })
        .unwrap();
        let v = |dir: Option<PathBuf>, require_all| VerifyArgs {
            file: manifest.clone(),
            sig: None,
            public_key: Some(pk.clone()),
            dir,
            require_all,
        };
        verify(&v(None, false)).unwrap();
        // The artifact on disk must match the manifest.
        let art = dir
            .path()
            .join("mokuro-bunko-0.7.0-x86_64-unknown-linux-musl-lite.tar.gz");
        assert!(
            verify(&v(Some(dir.path().into()), true)).is_err(),
            "missing artifact"
        );
        std::fs::write(&art, b"test").unwrap();
        verify(&v(Some(dir.path().into()), true)).unwrap();
        std::fs::write(&art, b"tesT").unwrap();
        assert!(
            verify(&v(Some(dir.path().into()), true)).is_err(),
            "checksum mismatch"
        );
        // Tampering with the manifest breaks the signature.
        let mut bytes = std::fs::read(&manifest).unwrap();
        bytes[15] ^= 1;
        std::fs::write(&manifest, bytes).unwrap();
        assert!(verify(&v(None, false)).is_err());
        // The default (compiled-in) key does not verify a foreign signature.
        assert!(
            verify(&VerifyArgs {
                public_key: None,
                ..v(None, false)
            })
            .is_err()
        );
    }

    #[test]
    fn key_formats() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("k");
        std::fs::write(&f, B64.encode([7u8; 64])).unwrap();
        let a = load_key(Some(&f)).unwrap();
        std::fs::write(&f, format!("{}\n", B64.encode([7u8; 32]))).unwrap();
        let b = load_key(Some(&f)).unwrap();
        assert_eq!(public_key_b64(&a), public_key_b64(&b));
        std::fs::write(&f, B64.encode([7u8; 16])).unwrap();
        assert!(load_key(Some(&f)).is_err());
    }
}
