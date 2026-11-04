//! The signed release manifest (`latest.json`) and every check applied before
//! a byte is downloaded.
//!
//! The manifest is authenticated as raw bytes by [`crate::updater::signature`];
//! this module only parses and validates the already-verified document. It
//! refuses documents from another schema or product, assets that could escape
//! the staging directory, stale versions and downgrades.

use semver::Version;
use serde::{Deserialize, Serialize};

/// Manifest schema understood by this build.
pub const SCHEMA: u32 = 1;
/// Product name written by the release tooling.
pub const PRODUCT: &str = "flotilla";
/// Largest `latest.json` this build will read.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Largest signature file this build will read.
pub const MAX_SIGNATURE_BYTES: usize = 512;
/// Largest client asset this build will download.
pub const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

/// The platform key for this build's update asset, if releases publish one.
///
/// The client ships for Windows x64 only; other targets (the headless UI check)
/// have no update feed and the updater stays idle.
pub const fn client_platform() -> Option<&'static str> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some("windows-x64")
    }
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub product: String,
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub signing: SigningInfo,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningInfo {
    #[serde(default)]
    pub algorithm: String,
    #[serde(default)]
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub platform: String,
    pub kind: String,
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// Why a manifest was refused. Every variant means "do not use this document".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("manifest is not valid JSON: {0}")]
    Malformed(String),
    #[error("manifest schema {0} is not supported")]
    Schema(u32),
    #[error("manifest is for {0}, not {PRODUCT}")]
    Product(String),
    #[error("manifest version {0:?} is not a semantic version")]
    BadVersion(String),
    #[error("manifest tag {0:?} is not a plain release tag")]
    UnsafeTag(String),
    #[error("manifest offers {offered}, which is not newer than {current}")]
    NotNewer { offered: String, current: String },
    #[error("manifest has no client asset for {0}")]
    NoClientAsset(String),
    #[error("manifest grows the same {0} client asset more than once")]
    DuplicateAsset(String),
    #[error("asset {name} is not a plain file name")]
    UnsafeName { name: String },
    #[error("asset {name} has no valid sha256")]
    BadHash { name: String },
    #[error("asset {name} is {size} bytes, above the {MAX_ASSET_BYTES} byte limit")]
    TooLarge { name: String, size: u64 },
    #[error("asset {name} is empty")]
    Empty { name: String },
}

impl Manifest {
    /// Parse and structurally validate a manifest whose bytes already passed
    /// the signature check.
    pub fn parse(bytes: &[u8]) -> Result<Self, Refusal> {
        let manifest: Manifest =
            serde_json::from_slice(bytes).map_err(|e| Refusal::Malformed(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), Refusal> {
        if self.schema != SCHEMA {
            return Err(Refusal::Schema(self.schema));
        }
        if self.product != PRODUCT {
            return Err(Refusal::Product(self.product.clone()));
        }
        Version::parse(&self.version).map_err(|_| Refusal::BadVersion(self.version.clone()))?;
        // The tag names a staging directory and a release-asset URL segment.
        let plain_tag = !self.tag.is_empty()
            && self
                .tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !plain_tag {
            return Err(Refusal::UnsafeTag(self.tag.clone()));
        }
        // `version` is the product version the binary reports (the ordering key
        // and the self-check expectation); `tag` only names where the assets
        // were published, so it is deliberately not tied to the version.
        for asset in &self.assets {
            validate_asset(asset)?;
        }
        Ok(())
    }

    /// The version this manifest offers, refusing anything not strictly newer.
    pub fn newer_than(&self, current: &str) -> Result<Version, Refusal> {
        let offered =
            Version::parse(&self.version).map_err(|_| Refusal::BadVersion(self.version.clone()))?;
        let current =
            Version::parse(current).map_err(|_| Refusal::BadVersion(current.to_string()))?;
        if offered <= current {
            return Err(Refusal::NotNewer {
                offered: offered.to_string(),
                current: current.to_string(),
            });
        }
        Ok(offered)
    }

    /// The client (installed-binary) asset for `platform`.
    pub fn client_asset(&self, platform: &str) -> Result<&Asset, Refusal> {
        let mut found: Option<&Asset> = None;
        for asset in &self.assets {
            if asset.kind == "client" && asset.platform == platform {
                if found.is_some() {
                    return Err(Refusal::DuplicateAsset(platform.to_string()));
                }
                found = Some(asset);
            }
        }
        found.ok_or_else(|| Refusal::NoClientAsset(platform.to_string()))
    }
}

fn validate_asset(asset: &Asset) -> Result<(), Refusal> {
    let name = &asset.name;
    // Published artifact names are plain: no separators, no control characters,
    // nothing that could climb out of a staging directory.
    let plain = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !plain {
        return Err(Refusal::UnsafeName { name: name.clone() });
    }
    if asset.sha256.len() != 64
        || !asset
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Refusal::BadHash { name: name.clone() });
    }
    if asset.size == 0 {
        return Err(Refusal::Empty { name: name.clone() });
    }
    if asset.size > MAX_ASSET_BYTES {
        return Err(Refusal::TooLarge {
            name: name.clone(),
            size: asset.size,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const SHA_A: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub(crate) const SHA_B: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    pub(crate) fn manifest_json(version: &str, tag: &str, client_version_sha: &str) -> String {
        format!(
            r#"{{"schema":1,"product":"flotilla","version":"{version}","tag":"{tag}","published_at":"2026-11-04T00:00:00Z","signing":{{"algorithm":"ed25519","key_id":"d901808076693bdc"}},"assets":[
  {{"platform":"windows-x64","kind":"client","name":"flotilla-x64.exe","sha256":"{client_version_sha}","size":11}},
  {{"platform":"windows-x64","kind":"installer","name":"Flotilla-Setup-x64.exe","sha256":"{SHA_B}","size":14}}]}}"#
        )
    }

    #[test]
    fn parses_a_release_manifest() {
        let bytes = manifest_json("0.5.0", "v0.5.0", SHA_A);
        let manifest = Manifest::parse(bytes.as_bytes()).expect("valid manifest");
        assert_eq!(manifest.version, "0.5.0");
        assert_eq!(manifest.signing.key_id, "d901808076693bdc");
        let asset = manifest.client_asset("windows-x64").expect("client asset");
        assert_eq!(asset.name, "flotilla-x64.exe");
        assert_eq!(asset.size, 11);
        assert_eq!(
            manifest.newer_than("0.4.0").unwrap(),
            Version::parse("0.5.0").unwrap()
        );
    }

    #[test]
    fn refuses_a_downgrade_and_the_same_version() {
        let bytes = manifest_json("0.4.0", "v0.4.0", SHA_A);
        let manifest = Manifest::parse(bytes.as_bytes()).unwrap();
        assert_eq!(
            manifest.newer_than("0.5.0").unwrap_err(),
            Refusal::NotNewer {
                offered: "0.4.0".into(),
                current: "0.5.0".into()
            }
        );
        assert!(matches!(
            manifest.newer_than("0.4.0"),
            Err(Refusal::NotNewer { .. })
        ));
        // Releasing 0.4.0 over an installed 0.4.0-rc.1 is an upgrade.
        assert!(manifest.newer_than("0.4.0-rc.1").is_ok());
        // ... and offering a prerelease over a stable build is a downgrade.
        let prerelease = manifest_json("0.4.0-rc.1", "v0.4.0-rc.1", SHA_A);
        let prerelease = Manifest::parse(prerelease.as_bytes()).unwrap();
        assert!(matches!(
            prerelease.newer_than("0.4.0"),
            Err(Refusal::NotNewer { .. })
        ));
    }

    #[test]
    fn refuses_a_manifest_from_another_schema_or_product() {
        let mut value: serde_json::Value =
            serde_json::from_str(&manifest_json("0.5.0", "v0.5.0", SHA_A)).unwrap();
        value["schema"] = 2.into();
        assert_eq!(
            Manifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            Refusal::Schema(2)
        );
        value["schema"] = 1.into();
        value["product"] = "something-else".into();
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            Refusal::Product(_)
        ));
    }

    #[test]
    fn refuses_a_tag_that_is_not_a_plain_name() {
        for tag in ["v1/../../etc", "../v1", "v 1", "", "v1/2", "v1:2"] {
            let bytes = manifest_json("0.5.0", tag, SHA_A);
            assert!(
                matches!(
                    Manifest::parse(bytes.as_bytes()),
                    Err(Refusal::UnsafeTag(_))
                ),
                "{tag:?} must be refused"
            );
        }
        // The assets live under a milestone tag while the product version is
        // the binary's own version, so the two need not match.
        let bytes = manifest_json("0.2.0", "v0.4.0", SHA_A);
        assert!(Manifest::parse(bytes.as_bytes()).is_ok());
    }

    #[test]
    fn refuses_unsafe_asset_names_and_bad_hashes() {
        for bad in [
            "../flotilla.exe",
            "sub/flotilla.exe",
            "sub\\flotilla.exe",
            "..",
            "",
        ] {
            let json = format!(
                r#"{{"schema":1,"product":"flotilla","version":"0.5.0","tag":"v0.5.0","assets":[{{"platform":"windows-x64","kind":"client","name":"{bad}","sha256":"{SHA_A}","size":1}}]}}"#
            );
            assert!(
                matches!(
                    Manifest::parse(json.as_bytes()),
                    Err(Refusal::UnsafeName { .. })
                ),
                "{bad:?} must be refused"
            );
        }
        for bad in [
            "short",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            let json = format!(
                r#"{{"schema":1,"product":"flotilla","version":"0.5.0","tag":"v0.5.0","assets":[{{"platform":"windows-x64","kind":"client","name":"flotilla-x64.exe","sha256":"{bad}","size":1}}]}}"#
            );
            assert!(matches!(
                Manifest::parse(json.as_bytes()),
                Err(Refusal::BadHash { .. })
            ));
        }
    }

    #[test]
    fn refuses_oversized_and_empty_assets() {
        let oversize = MAX_ASSET_BYTES + 1;
        let json = format!(
            r#"{{"schema":1,"product":"flotilla","version":"0.5.0","tag":"v0.5.0","assets":[{{"platform":"windows-x64","kind":"client","name":"flotilla-x64.exe","sha256":"{SHA_A}","size":{oversize}}}]}}"#
        );
        assert!(matches!(
            Manifest::parse(json.as_bytes()),
            Err(Refusal::TooLarge { .. })
        ));
        let json = format!(
            r#"{{"schema":1,"product":"flotilla","version":"0.5.0","tag":"v0.5.0","assets":[{{"platform":"windows-x64","kind":"client","name":"flotilla-x64.exe","sha256":"{SHA_A}","size":0}}]}}"#
        );
        assert!(matches!(
            Manifest::parse(json.as_bytes()),
            Err(Refusal::Empty { .. })
        ));
    }

    #[test]
    fn refuses_a_manifest_without_a_client_asset_for_this_platform() {
        let bytes = manifest_json("0.5.0", "v0.5.0", SHA_A);
        let manifest = Manifest::parse(bytes.as_bytes()).unwrap();
        assert!(matches!(
            manifest.client_asset("linux-x86_64"),
            Err(Refusal::NoClientAsset(_))
        ));
    }

    #[test]
    fn refuses_two_client_assets_for_one_platform() {
        let json = manifest_json("0.5.0", "v0.5.0", SHA_A)
            .replace(r#""kind":"installer""#, r#""kind":"client""#);
        let manifest = Manifest::parse(json.as_bytes()).unwrap();
        assert!(matches!(
            manifest.client_asset("windows-x64"),
            Err(Refusal::DuplicateAsset(_))
        ));
    }

    #[test]
    fn the_manifest_is_refused_before_any_field_is_trusted() {
        assert!(matches!(
            Manifest::parse(b"not json"),
            Err(Refusal::Malformed(_))
        ));
        assert!(matches!(Manifest::parse(b"{}"), Err(Refusal::Malformed(_))));
    }
}
