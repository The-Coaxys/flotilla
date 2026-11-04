//! `fl-sign`: Flotilla release signing and verification (see `docs/release-signing.md`).
//!
//! Ed25519 detached signatures over raw file bytes. The private key is a
//! 32-byte seed in base64 and must only ever live in the
//! `FLOTILLA_SIGNING_KEY` GitHub Actions secret; this tool never prints it.
//!
//! ```text
//! fl-sign generate --key-out flotilla-signing.key
//! fl-sign pubkey                  (FLOTILLA_SIGNING_KEY in the environment)
//! fl-sign sign <file>...          writes <file>.sig (base64 signature + newline)
//! fl-sign verify <public-key-b64> <file>...
//! fl-sign manifest --version 0.4.0 --tag v0.4.0 --dir dist/upload --out dist/upload/latest.json
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Manifest schema written by this tool and understood by the client updater.
pub const MANIFEST_SCHEMA: u32 = 1;
/// Algorithm tag written into the manifest.
pub const ALGORITHM: &str = "ed25519";
/// Largest artifact the manifest tool will describe (the client enforces its own copy).
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

fn main() {
    if let Err(e) = run() {
        eprintln!("fl-sign: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        bail!("missing command");
    };
    match command {
        "generate" => generate(&args[1..]),
        "pubkey" => pubkey(&args[1..]),
        "sign" => sign(&args[1..]),
        "verify" => verify(&args[1..]),
        "manifest" => manifest(&args[1..]),
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => {
            usage();
            bail!("unknown command {other:?}")
        }
    }
}

fn usage() {
    eprintln!(
        "usage:
  fl-sign generate [--key-out <path>]      create a key pair; prints the public key and key id
  fl-sign pubkey                           print public key (base64) and key id for the configured secret
  fl-sign sign <file>...                   write <file>.sig next to each file
  fl-sign verify <public-key-b64> <file>...  verify <file>.sig for each file
  fl-sign manifest --version <v> --tag <tag> --dir <dir> --out <latest.json>

The signing key is read from FLOTILLA_SIGNING_KEY (base64 seed) or
--key-file <path>. Never commit it."
    );
}

fn generate(args: &[String]) -> Result<()> {
    let key_out = flag_value(args, "--key-out").map(PathBuf::from);
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    let public = key.verifying_key();
    let secret_b64 = B64.encode(seed);

    match key_out {
        Some(path) => {
            write_secret_file(&path, &secret_b64)?;
            eprintln!("wrote the private key to {}", path.display());
            eprintln!(
                "store it with: gh secret set FLOTILLA_SIGNING_KEY -R The-Coaxys/flotilla < {}",
                path.display()
            );
            eprintln!(
                "then delete the file: the private key must exist only in the Actions secret."
            );
        }
        None => {
            eprintln!("no --key-out: the private key is NOT stored anywhere; copy it now:");
            println!("{secret_b64}");
        }
    }
    println!("public_key={}", B64.encode(public.to_bytes()));
    println!("key_id={}", key_id(&public));
    println!(
        "const line: TrustedKey {{ key_id: \"{}\", public_key: \"{}\" }}",
        key_id(&public),
        B64.encode(public.to_bytes())
    );
    Ok(())
}

fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        bail!(
            "{} already exists; refusing to overwrite a signing key",
            path.display()
        );
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    writeln!(file, "{contents}")?;
    file.sync_all()?;
    Ok(())
}

fn pubkey(args: &[String]) -> Result<()> {
    let key = load_key(args)?;
    let public = key.verifying_key();
    println!("public_key={}", B64.encode(public.to_bytes()));
    println!("key_id={}", key_id(&public));
    Ok(())
}

fn sign(args: &[String]) -> Result<()> {
    let files = positionals(args, &["--key-file"]);
    let key = load_key(args)?;
    if files.is_empty() {
        bail!("sign: no files");
    }
    for file in files {
        if file.ends_with(".sig") {
            bail!("sign: refusing to sign a signature file {file}");
        }
        let bytes = std::fs::read(file).with_context(|| format!("read {file}"))?;
        let signature = key.sign(&bytes);
        let mut out = PathBuf::from(file);
        out.set_file_name(format!("{}.sig", file_name(file)?));
        std::fs::write(&out, format!("{}\n", B64.encode(signature.to_bytes())))
            .with_context(|| format!("write {}", out.display()))?;
        eprintln!("signed {file} -> {}", out.display());
    }
    Ok(())
}

fn verify(args: &[String]) -> Result<()> {
    let positional = positionals(args, &["--key-file"]);
    let Some((public_b64, files)) = positional.split_first() else {
        bail!("verify: needs <public-key-b64> <file>...");
    };
    let key = parse_public_key(public_b64)?;
    if files.is_empty() {
        bail!("verify: no files");
    }
    for file in files {
        let bytes = std::fs::read(file).with_context(|| format!("read {file}"))?;
        let sig_path = format!("{file}.sig");
        let sig_text =
            std::fs::read_to_string(&sig_path).with_context(|| format!("read {sig_path}"))?;
        verify_bytes(&key, &bytes, sig_text.trim())
            .with_context(|| format!("signature check failed for {file}"))?;
        eprintln!("verified {file} ({} bytes)", bytes.len());
    }
    Ok(())
}

fn verify_bytes(key: &VerifyingKey, bytes: &[u8], signature_b64: &str) -> Result<()> {
    let raw = B64
        .decode(signature_b64)
        .context("signature is not base64")?;
    let signature = Signature::from_slice(&raw).context("signature is not 64 bytes")?;
    key.verify_strict(bytes, &signature)
        .context("signature does not match")
}

fn manifest(args: &[String]) -> Result<()> {
    let version = require_flag(args, "--version")?;
    let tag = require_flag(args, "--tag")?;
    let dir = PathBuf::from(require_flag(args, "--dir")?);
    let out = PathBuf::from(require_flag(args, "--out")?);
    let published_at = match flag_value(args, "--published-at") {
        Some(v) => v,
        None => rfc3339_now(),
    };
    let key = load_key(args)?;
    let public = key.verifying_key();

    // assets are addressed by their published file name; the table pins which
    // platform/kind each name serves, so a new artifact cannot slip in unlabelled.
    let mut assets = Vec::new();
    let mut seen = BTreeMap::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("read dir {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".sig") || name == "SHA256SUMS.txt" || name == "latest.json" {
            continue;
        }
        let Some((platform, kind)) = asset_role(name) else {
            bail!("manifest: {name} is not in the release asset table; add it to fl-sign's asset_role()")
        };
        let (sha256, size) = digest(&path)?;
        if size > MAX_ASSET_BYTES {
            bail!("manifest: {name} is {size} bytes, above the {MAX_ASSET_BYTES} byte limit");
        }
        if seen.insert(name.to_string(), ()).is_some() {
            bail!("manifest: duplicate asset {name}");
        }
        assets.push(Asset {
            platform: platform.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            sha256,
            size,
        });
    }
    if assets.is_empty() {
        bail!("manifest: no assets in {}", dir.display());
    }
    assets.sort_by(|a, b| (&a.platform, &a.kind, &a.name).cmp(&(&b.platform, &b.kind, &b.name)));
    if !assets.iter().any(|a| a.kind == "client") {
        bail!("manifest: no client asset; the updater needs one");
    }

    let manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        product: "flotilla".to_string(),
        version: version.clone(),
        tag: tag.clone(),
        published_at,
        signing: Signing {
            algorithm: ALGORITHM.to_string(),
            key_id: key_id(&public),
        },
        assets,
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    std::fs::write(&out, &bytes).with_context(|| format!("write {}", out.display()))?;
    let signature = key.sign(&bytes);
    let sig_path = PathBuf::from(format!("{}.sig", out.display()));
    std::fs::write(&sig_path, format!("{}\n", B64.encode(signature.to_bytes())))
        .with_context(|| format!("write {}", sig_path.display()))?;
    eprintln!(
        "manifest {} ({} assets, version {version}, tag {tag}) signed with {}",
        out.display(),
        manifest.assets.len(),
        manifest.signing.key_id
    );
    Ok(())
}

/// Published file name -> (platform, kind). Keep in sync with the client's
/// `updater::manifest::client_platform` and with the workflow's asset list.
fn asset_role(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "Flotilla-Setup-x64.exe" => ("windows-x64", "installer"),
        "flotilla-x64.exe" => ("windows-x64", "client"),
        "flotilla-windows-licenses.zip" => ("windows-x64", "licenses"),
        "flotillad-x86_64-unknown-linux-musl" => ("linux-x86_64", "daemon"),
        "flotillad-aarch64-unknown-linux-musl" => ("linux-aarch64", "daemon"),
        _ => return None,
    })
}

fn digest(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let size = std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("hash {}", path.display()))?;
    Ok((hex_lower(&hasher.finalize()), size))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
        out.push(char::from_digit((byte & 0xf) as u32, 16).expect("nibble"));
    }
    out
}

/// First 8 bytes of SHA-256 over the public key, hex.
fn key_id(public: &VerifyingKey) -> String {
    let digest = Sha256::digest(public.to_bytes());
    hex_lower(&digest[..8])
}

fn parse_public_key(b64: &str) -> Result<VerifyingKey> {
    let raw = B64.decode(b64.trim()).context("public key is not base64")?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key is not 32 bytes"))?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

fn load_key(args: &[String]) -> Result<SigningKey> {
    let secret = match flag_value(args, "--key-file") {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("read signing key from {path}"))?,
        None => match std::env::var("FLOTILLA_SIGNING_KEY") {
            Ok(value) => value,
            Err(_) => bail!(
                "no signing key: set FLOTILLA_SIGNING_KEY (base64 seed) or pass --key-file <path>"
            ),
        },
    };
    let raw = B64
        .decode(secret.trim())
        .context("signing key is not base64")?;
    let seed: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key is not 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Positional arguments: everything that is not a flag, skipping the value of
/// each flag listed in `value_flags` (`--name value`, not `--name=value`).
fn positionals<'a>(args: &'a [String], value_flags: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg.starts_with("--") {
            if !arg.contains('=') && value_flags.contains(&arg.as_str()) {
                iter.next();
            }
            continue;
        }
        out.push(arg.as_str());
    }
    out
}

fn require_flag(args: &[String], name: &str) -> Result<String> {
    flag_value(args, name).with_context(|| format!("missing {name}"))
}

fn file_name(path: &str) -> Result<&str> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{path} has no file name"))
}

/// RFC 3339 UTC timestamp without pulling a date crate.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil inverse.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    product: String,
    version: String,
    tag: String,
    published_at: String,
    signing: Signing,
    assets: Vec<Asset>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Signing {
    algorithm: String,
    key_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Asset {
    platform: String,
    kind: String,
    name: String,
    sha256: String,
    size: u64,
}
