//! Offline fixtures for the updater tests: a fake release site and a test key.
//!
//! The shipped signing key never signs test data; tests sign with
//! [`TEST_SEED`] and pass that key set to the pipeline explicitly.

use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::download::{Fetched, Source};
use super::signature::key_id_of;

pub(crate) const TEST_SEED: [u8; 32] = [7u8; 32];
pub(crate) const PLATFORM: &str = "windows-x64";
pub(crate) const ASSET_NAME: &str = "flotilla-x64.exe";

pub(crate) struct TestKey {
    pub key_id: String,
    pub public_key: String,
    signing: SigningKey,
}

impl TestKey {
    pub fn new() -> Self {
        let signing = SigningKey::from_bytes(&TEST_SEED);
        let public_key = B64.encode(signing.verifying_key().to_bytes());
        let key_id = key_id_of(&public_key).expect("test key id");
        Self {
            key_id,
            public_key,
            signing,
        }
    }

    /// The key set the pipeline should trust for this test.
    pub fn trusted(&self) -> [(&str, &str); 1] {
        [(self.key_id.as_str(), self.public_key.as_str())]
    }

    pub fn sign(&self, bytes: &[u8]) -> String {
        B64.encode(self.signing.sign(bytes).to_bytes())
    }
}

#[derive(Serialize)]
struct WireManifest<'a> {
    schema: u32,
    product: &'a str,
    version: &'a str,
    tag: &'a str,
    published_at: &'a str,
    signing: WireSigning<'a>,
    assets: Vec<WireAsset<'a>>,
}

#[derive(Serialize)]
struct WireSigning<'a> {
    algorithm: &'a str,
    key_id: &'a str,
}

#[derive(Serialize)]
struct WireAsset<'a> {
    platform: &'a str,
    kind: &'a str,
    name: &'a str,
    sha256: String,
    size: u64,
}

/// A release site for one version: signed manifest, signature and one client
/// asset, served over URLs shaped exactly like GitHub Releases.
pub(crate) struct Site {
    pub key: TestKey,
    pub base: String,
    pub version: String,
    pub tag: String,
    pub asset_bytes: Vec<u8>,
    pub manifest: Vec<u8>,
    pub signature: String,
}

impl Site {
    pub fn new(version: &str) -> Self {
        let mut site = Self {
            key: TestKey::new(),
            base: "https://example.test/releases".to_string(),
            version: version.to_string(),
            tag: format!("v{version}"),
            asset_bytes: b"the new client build".to_vec(),
            manifest: Vec::new(),
            signature: String::new(),
        };
        site.sign();
        site
    }

    /// Re-sign the manifest from the current fields.
    pub fn sign(&mut self) {
        let doc = WireManifest {
            schema: 1,
            product: "flotilla",
            version: &self.version,
            tag: &self.tag,
            published_at: "2026-11-04T00:00:00Z",
            signing: WireSigning {
                algorithm: "ed25519",
                key_id: &self.key.key_id,
            },
            assets: vec![WireAsset {
                platform: PLATFORM,
                kind: "client",
                name: ASSET_NAME,
                sha256: hex::encode(Sha256::digest(&self.asset_bytes)),
                size: self.asset_bytes.len() as u64,
            }],
        };
        self.manifest = serde_json::to_vec_pretty(&doc).expect("manifest json");
        self.signature = self.key.sign(&self.manifest);
    }

    pub fn manifest_url(&self) -> String {
        format!("{}/latest/download/latest.json", self.base)
    }

    pub fn signature_url(&self) -> String {
        format!("{}.sig", self.manifest_url())
    }

    pub fn asset_url(&self) -> String {
        format!("{}/download/{}/{}", self.base, self.tag, ASSET_NAME)
    }

    /// A source serving this site as published.
    pub fn source(&self) -> FakeSource {
        self.source_with_asset(self.asset_bytes.clone())
    }

    /// A source serving the signed manifest but different asset bytes (a
    /// tampered or truncated download).
    pub fn source_with_asset(&self, asset_bytes: Vec<u8>) -> FakeSource {
        let source = FakeSource::new();
        source.route(&self.manifest_url(), self.manifest.clone());
        source.route(
            &self.signature_url(),
            format!("{}\n", self.signature).into_bytes(),
        );
        source.route(&self.asset_url(), asset_bytes);
        source
    }
}

pub(crate) struct FakeSource {
    routes: Mutex<Vec<(String, Vec<u8>, u16)>>,
    pub requests: Mutex<Vec<(String, Option<u64>)>>,
}

impl FakeSource {
    pub fn new() -> Self {
        Self {
            routes: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// The most recently added route for a url wins, so a test can override
    /// one entry of a published site.
    pub fn route(&self, url: &str, body: Vec<u8>) {
        self.routes
            .lock()
            .unwrap()
            .insert(0, (url.to_string(), body, 200));
    }

    pub fn route_status(&self, url: &str, status: u16) {
        self.routes
            .lock()
            .unwrap()
            .insert(0, (url.to_string(), Vec::new(), status));
    }

    pub fn requested(&self) -> Vec<(String, Option<u64>)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Source for FakeSource {
    fn get(&self, url: &str, range: Option<u64>) -> Result<Fetched, String> {
        self.requests.lock().unwrap().push((url.to_string(), range));
        let routes = self.routes.lock().unwrap();
        let Some((_, body, status)) = routes.iter().find(|(route, _, _)| route == url) else {
            return Err(format!("no route for {url}"));
        };
        let mut body = body.clone();
        let mut status = *status;
        if let Some(from) = range
            && from as usize <= body.len()
        {
            body = body.split_off(from as usize);
            status = 206;
        }
        let length = body.len() as u64;
        drop(routes);
        Ok(Fetched {
            status,
            content_length: Some(length),
            body: Box::new(std::io::Cursor::new(body)),
        })
    }
}
