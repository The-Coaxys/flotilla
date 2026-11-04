//! HTTP fetching for the updater: small trusted documents under a hard byte
//! cap, and resumable asset downloads that are verified before they can be
//! installed.
//!
//! Everything goes through [`Source`] so the whole pipeline runs offline in
//! tests; [`HttpSource`] is the only implementation that touches the network.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::manifest::{Asset, MAX_ASSET_BYTES};

/// A response body plus what the transport learned about it.
pub struct Fetched {
    /// HTTP status code.
    pub status: u16,
    /// `Content-Length` when the server sent one (for a `Range` request this is
    /// the remainder, not the whole asset).
    pub content_length: Option<u64>,
    /// Body stream.
    pub body: Box<dyn Read>,
}

/// Where the updater gets bytes. Errors are transport-level strings; document
/// and asset problems are typed in [`DownloadError`] and the signature module.
pub trait Source {
    fn get(&self, url: &str, range: Option<u64>) -> Result<Fetched, String>;
}

/// The real network transport (blocking; the updater owns a worker thread).
pub struct HttpSource {
    agent: ureq::Agent,
}

impl HttpSource {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .timeout_connect(Some(Duration::from_secs(20)))
            .user_agent(concat!("Flotilla/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for HttpSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for HttpSource {
    fn get(&self, url: &str, range: Option<u64>) -> Result<Fetched, String> {
        let mut request = self.agent.get(url);
        if let Some(from) = range {
            request = request.header("Range", format!("bytes={from}-"));
        }
        let response = match request.call() {
            Ok(response) => response,
            // A 404 before the first signed release is a normal answer, not a
            // transport failure: let the caller classify it.
            Err(ureq::Error::StatusCode(status)) => {
                return Ok(Fetched {
                    status,
                    content_length: None,
                    body: Box::new(std::io::empty()),
                });
            }
            Err(e) => return Err(e.to_string()),
        };
        let status = response.status().as_u16();
        let content_length = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        Ok(Fetched {
            status,
            content_length,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("HTTP {status} for {url}")]
    Status { url: String, status: u16 },
    #[error("{url}: {message}")]
    Network { url: String, message: String },
    #[error("{url} is larger than the {limit} byte limit")]
    TooLarge { url: String, limit: u64 },
    #[error("{name}: sha256 mismatch (manifest {expected}, downloaded {actual})")]
    HashMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("{name}: manifest says {expected} bytes, downloaded {actual}")]
    SizeMismatch {
        name: String,
        expected: u64,
        actual: u64,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Fetch one small document (`latest.json`, `latest.json.sig`) under `limit`.
pub fn fetch_small(source: &dyn Source, url: &str, limit: usize) -> Result<Vec<u8>, DownloadError> {
    let mut fetched = source
        .get(url, None)
        .map_err(|message| DownloadError::Network {
            url: url.to_string(),
            message,
        })?;
    if !(200..300).contains(&fetched.status) {
        return Err(DownloadError::Status {
            url: url.to_string(),
            status: fetched.status,
        });
    }
    let limit_u64 = limit as u64;
    if let Some(length) = fetched.content_length
        && length > limit_u64
    {
        return Err(DownloadError::TooLarge {
            url: url.to_string(),
            limit: limit_u64,
        });
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = fetched
            .body
            .read(&mut buf)
            .map_err(|e| DownloadError::Network {
                url: url.to_string(),
                message: e.to_string(),
            })?;
        if read == 0 {
            return Ok(out);
        }
        if out.len() as u64 + read as u64 > limit_u64 {
            return Err(DownloadError::TooLarge {
                url: url.to_string(),
                limit: limit_u64,
            });
        }
        out.extend_from_slice(&buf[..read]);
    }
}

/// Where a partially downloaded asset lives while it is being fetched, and
/// where the verified bytes end up.
pub fn asset_paths(staging: &Path, asset: &Asset) -> (PathBuf, PathBuf) {
    let final_path = staging.join(&asset.name);
    let mut part = final_path.clone().into_os_string();
    part.push(".part");
    (PathBuf::from(part), final_path)
}

/// Download `asset` to `staging`, resuming a previous partial file.
///
/// The bytes only become the staging asset after their size and SHA-256 match
/// the signed manifest; a mismatch deletes the partial file and refuses, so a
/// truncated or tampered download can never reach the installer.
pub fn fetch_asset(
    source: &dyn Source,
    url: &str,
    staging: &Path,
    asset: &Asset,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, DownloadError> {
    if asset.size > MAX_ASSET_BYTES {
        return Err(DownloadError::TooLarge {
            url: url.to_string(),
            limit: MAX_ASSET_BYTES,
        });
    }
    std::fs::create_dir_all(staging)?;
    let (part, final_path) = asset_paths(staging, asset);
    let mut resume = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if resume > asset.size {
        // Not a prefix of what we expect: start over.
        let _ = std::fs::remove_file(&part);
        resume = 0;
    }
    if resume == asset.size {
        if hash_matches(&part, asset)? {
            std::fs::rename(&part, &final_path)?;
            progress(asset.size, asset.size);
            return Ok(final_path);
        }
        let _ = std::fs::remove_file(&part);
        resume = 0;
    }

    let mut fetched = source
        .get(url, (resume > 0).then_some(resume))
        .map_err(|message| DownloadError::Network {
            url: url.to_string(),
            message,
        })?;
    let mut written;
    match fetched.status {
        // Range honoured: append to what we already have.
        206 => written = resume,
        // Range ignored (or not asked for): the body is the whole asset.
        200 => {
            let _ = std::fs::remove_file(&part);
            written = 0;
        }
        // Nothing left to send: the partial file must already be complete.
        416 => {
            return if hash_matches(&part, asset)? {
                std::fs::rename(&part, &final_path)?;
                Ok(final_path)
            } else {
                let _ = std::fs::remove_file(&part);
                Err(DownloadError::SizeMismatch {
                    name: asset.name.clone(),
                    expected: asset.size,
                    actual: resume,
                })
            };
        }
        status => {
            return Err(DownloadError::Status {
                url: url.to_string(),
                status,
            });
        }
    }
    if let Some(remaining) = fetched.content_length
        && written + remaining > asset.size
    {
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::SizeMismatch {
            name: asset.name.clone(),
            expected: asset.size,
            actual: written + remaining,
        });
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(written > 0)
        .write(true)
        .truncate(written == 0)
        .open(&part)?;
    progress(written, asset.size);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = fetched
            .body
            .read(&mut buf)
            .map_err(|e| DownloadError::Network {
                url: url.to_string(),
                message: e.to_string(),
            })?;
        if read == 0 {
            break;
        }
        if written + read as u64 > asset.size {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(DownloadError::SizeMismatch {
                name: asset.name.clone(),
                expected: asset.size,
                actual: written + read as u64,
            });
        }
        file.write_all(&buf[..read])?;
        written += read as u64;
        progress(written, asset.size);
    }
    file.sync_all()?;
    drop(file);
    if written != asset.size {
        return Err(DownloadError::SizeMismatch {
            name: asset.name.clone(),
            expected: asset.size,
            actual: written,
        });
    }
    if !hash_matches(&part, asset)? {
        let actual = file_hash(&part).unwrap_or_default();
        let _ = std::fs::remove_file(&part);
        return Err(DownloadError::HashMismatch {
            name: asset.name.clone(),
            expected: asset.sha256.clone(),
            actual,
        });
    }
    std::fs::rename(&part, &final_path)?;
    Ok(final_path)
}

/// Re-hash an already staged asset; the installer runs this immediately before
/// touching the installed binary, and the staged bytes are never trusted from
/// the earlier download alone.
pub fn verify_staged(path: &Path, asset: &Asset) -> Result<(), DownloadError> {
    if hash_matches(path, asset)? {
        return Ok(());
    }
    Err(DownloadError::HashMismatch {
        name: asset.name.clone(),
        expected: asset.sha256.clone(),
        actual: file_hash(path)?,
    })
}

fn hash_matches(path: &Path, asset: &Asset) -> Result<bool, DownloadError> {
    Ok(file_hash(path)? == asset.sha256)
}

fn file_hash(path: &Path) -> Result<String, DownloadError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        url: String,
        body: Vec<u8>,
        status: u16,
    }

    impl Fake {
        fn ok(url: &str, body: &[u8]) -> Self {
            Self {
                url: url.to_string(),
                body: body.to_vec(),
                status: 200,
            }
        }
    }

    struct One(Fake);

    impl Source for One {
        fn get(&self, url: &str, _range: Option<u64>) -> Result<Fetched, String> {
            assert_eq!(url, self.0.url, "unexpected url");
            Ok(Fetched {
                status: self.0.status,
                content_length: Some(self.0.body.len() as u64),
                body: Box::new(std::io::Cursor::new(self.0.body.clone())),
            })
        }
    }

    fn asset_for(bytes: &[u8]) -> Asset {
        Asset {
            platform: "windows-x64".into(),
            kind: "client".into(),
            name: "flotilla-x64.exe".into(),
            sha256: hex::encode(Sha256::digest(bytes)),
            size: bytes.len() as u64,
        }
    }

    #[test]
    fn fetch_small_reads_the_body_and_enforces_the_limit() {
        let source = One(Fake::ok("https://example/latest.json", b"{}"));
        assert_eq!(
            fetch_small(&source, "https://example/latest.json", 10).unwrap(),
            b"{}"
        );
        assert!(matches!(
            fetch_small(&source, "https://example/latest.json", 1),
            Err(DownloadError::TooLarge { .. })
        ));
    }

    #[test]
    fn fetch_small_reports_a_missing_release() {
        let source = One(Fake {
            url: "u".into(),
            body: Vec::new(),
            status: 404,
        });
        assert!(matches!(
            fetch_small(&source, "u", 10),
            Err(DownloadError::Status { status: 404, .. })
        ));
    }

    #[test]
    fn downloads_and_stages_an_asset() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"new-client-binary";
        let asset = asset_for(bytes);
        let source = One(Fake::ok("https://example/client", bytes));
        let mut seen = Vec::new();
        let path = fetch_asset(
            &source,
            "https://example/client",
            dir.path(),
            &asset,
            &mut |done, total| {
                seen.push((done, total));
            },
        )
        .expect("download");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            *seen.last().unwrap(),
            (bytes.len() as u64, bytes.len() as u64)
        );
        assert!(
            !asset_paths(dir.path(), &asset).0.exists(),
            "part file is consumed"
        );
    }

    #[test]
    fn refuses_bytes_that_do_not_match_the_signed_hash() {
        let dir = tempfile::tempdir().unwrap();
        let asset = asset_for(b"the real client");
        // Same length, different bytes: the size check cannot catch this.
        let source = One(Fake::ok("https://example/client", b"the fake client"));
        let err = fetch_asset(
            &source,
            "https://example/client",
            dir.path(),
            &asset,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::HashMismatch { .. }), "{err:?}");
        // Nothing installable is left behind.
        assert!(!dir.path().join("flotilla-x64.exe").exists());
        assert!(!asset_paths(dir.path(), &asset).0.exists());
    }

    #[test]
    fn refuses_a_body_longer_than_the_manifest_says() {
        let dir = tempfile::tempdir().unwrap();
        let mut asset = asset_for(b"short");
        asset.size = 3;
        let source = One(Fake::ok(
            "https://example/client",
            b"much longer than three",
        ));
        let err = fetch_asset(
            &source,
            "https://example/client",
            dir.path(),
            &asset,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DownloadError::SizeMismatch { .. } | DownloadError::HashMismatch { .. }
        ));
        assert!(!dir.path().join("flotilla-x64.exe").exists());
    }
}
