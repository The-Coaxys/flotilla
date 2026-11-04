//! Offline end-to-end checks for the update pipeline: no network, temp dirs.
//!
//! These cover the refusals the release process depends on: a tampered or
//! unsigned manifest, a downgrade, a download that does not match its signed
//! hash, and a staged replacement whose new build fails its self-check.

use std::path::{Path, PathBuf};

use super::download::DownloadError;
use super::manifest::Refusal;
use super::signature::VerifyError;
use super::testkit::{ASSET_NAME, PLATFORM, Site};
use super::*;

const RUNNING: &[u8] = b"the running build";

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    site: Site,
}

impl Fixture {
    fn new(version: &str) -> Self {
        Self::with(version, "0.4.0")
    }

    fn with(version: &str, current: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let site = Site::new(version);
        let config = Self::config(dir.path(), &site, current);
        Self {
            _dir: dir,
            config,
            site,
        }
    }

    fn config(root: &Path, site: &Site, current: &str) -> Config {
        let install = root.join("install");
        std::fs::create_dir_all(&install).unwrap();
        let installed_exe = install.join("flotilla.exe");
        std::fs::write(&installed_exe, RUNNING).unwrap();
        Config {
            release_base: site.base.clone(),
            staging_dir: root.join("staging"),
            installed_exe,
            current_version: current.to_string(),
            platform: Some(PLATFORM),
            enabled: true,
        }
    }

    fn installed(&self) -> PathBuf {
        self.config.installed_exe.clone()
    }

    fn check(
        &self,
        source: &dyn Source,
    ) -> (Vec<UpdateEvent>, Result<Option<Staged>, UpdateError>) {
        let keys = self.site.key.trusted();
        let mut events = Vec::new();
        let result = check_and_stage(&self.config, source, &keys, &mut |event| events.push(event));
        (events, result)
    }

    fn staging_is_empty(&self) -> bool {
        fn no_files(dir: &Path) -> bool {
            match std::fs::read_dir(dir) {
                Ok(entries) => entries.filter_map(|entry| entry.ok()).all(|entry| {
                    let path = entry.path();
                    if path.is_dir() {
                        no_files(&path)
                    } else {
                        false
                    }
                }),
                Err(_) => true,
            }
        }
        no_files(&self.config.staging_dir)
    }
}

#[test]
fn accepts_a_signed_manifest_and_stages_the_asset() {
    let fixture = Fixture::new("0.5.0");
    let source = fixture.site.source();
    let (events, result) = fixture.check(&source);
    let staged = result
        .expect("signed manifest is accepted")
        .expect("update available");
    assert_eq!(staged.version.to_string(), "0.5.0");
    assert_eq!(staged.tag, "v0.5.0");
    assert_eq!(staged.asset.name, ASSET_NAME);
    assert_eq!(
        std::fs::read(&staged.path).unwrap(),
        fixture.site.asset_bytes
    );
    assert!(matches!(
        events.first(),
        Some(UpdateEvent::Available { .. })
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, UpdateEvent::Downloading { .. }))
    );
    // Nothing is replaced until an explicit restart.
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
}

#[test]
fn refuses_a_tampered_manifest_without_downloading_anything() {
    let fixture = Fixture::new("0.5.0");
    let tampered = String::from_utf8(fixture.site.manifest.clone())
        .unwrap()
        .replace("\"0.5.0\"", "\"9.9.9\"");
    let source = fixture.site.source();
    source.route(&fixture.site.manifest_url(), tampered.into_bytes());
    let (_, result) = fixture.check(&source);
    assert!(
        matches!(result, Err(UpdateError::Signature(VerifyError::Mismatch))),
        "{result:?}"
    );
    let requested: Vec<String> = source.requested().into_iter().map(|(url, _)| url).collect();
    assert!(
        !requested.iter().any(|url| url.contains(ASSET_NAME)),
        "the asset must never be fetched for an unverified manifest: {requested:?}"
    );
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
    assert!(fixture.staging_is_empty());
}

#[test]
fn refuses_a_manifest_signed_by_an_untrusted_key() {
    let fixture = Fixture::new("0.5.0");
    // A different signature over the same bytes: the key id in the manifest is
    // not one this build trusts.
    let mut manifest: serde_json::Value = serde_json::from_slice(&fixture.site.manifest).unwrap();
    manifest["signing"]["key_id"] = serde_json::Value::String("0000000000000000".into());
    let source = fixture.site.source();
    source.route(
        &fixture.site.manifest_url(),
        serde_json::to_vec(&manifest).unwrap(),
    );
    let (_, result) = fixture.check(&source);
    assert!(
        matches!(
            result,
            Err(UpdateError::Signature(VerifyError::UnknownKey(_)))
        ),
        "{result:?}"
    );
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
}

#[test]
fn refuses_an_unsigned_manifest() {
    let fixture = Fixture::new("0.5.0");
    let source = testkit::FakeSource::new();
    source.route(&fixture.site.manifest_url(), fixture.site.manifest.clone());
    // The manifest is there but no signature was published next to it.
    source.route_status(&fixture.site.signature_url(), 404);
    let (_, result) = fixture.check(&source);
    assert!(matches!(result, Err(UpdateError::Unsigned)), "{result:?}");
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
}

#[test]
fn refuses_a_downgrade() {
    let fixture = Fixture::with("0.3.0", "0.4.0");
    let source = fixture.site.source();
    let (_, result) = fixture.check(&source);
    assert!(
        matches!(
            result,
            Err(UpdateError::Refusal(Refusal::NotNewer { ref offered, .. })) if offered == "0.3.0"
        ),
        "{result:?}"
    );
    let requested: Vec<String> = source.requested().into_iter().map(|(url, _)| url).collect();
    assert!(!requested.iter().any(|url| url.contains(ASSET_NAME)));
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
}

#[test]
fn refuses_to_offer_the_running_version_again() {
    let fixture = Fixture::with("0.4.0", "0.4.0");
    let source = fixture.site.source();
    let (_, result) = fixture.check(&source);
    assert!(matches!(
        result,
        Err(UpdateError::Refusal(Refusal::NotNewer { .. }))
    ));
}

#[test]
fn reports_no_release_rather_than_failing() {
    let fixture = Fixture::new("0.5.0");
    let source = testkit::FakeSource::new();
    source.route_status(&fixture.site.manifest_url(), 404);
    let (_, result) = fixture.check(&source);
    assert!(matches!(result, Err(UpdateError::NoRelease)), "{result:?}");
}

#[test]
fn a_download_that_does_not_match_the_signed_hash_keeps_the_old_binary() {
    let fixture = Fixture::new("0.5.0");
    let mut wrong = fixture.site.asset_bytes.clone();
    wrong[0] ^= 0xff;
    let source = fixture.site.source_with_asset(wrong);
    let (_, result) = fixture.check(&source);
    assert!(
        matches!(
            result,
            Err(UpdateError::Download(DownloadError::HashMismatch { .. }))
        ),
        "{result:?}"
    );
    assert!(
        !fixture
            .config
            .staging_dir
            .join("v0.5.0")
            .join(ASSET_NAME)
            .exists()
    );
    // No marker, no backup: the install directory is exactly as it was.
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
    assert!(
        !fixture
            .config
            .installed_exe
            .with_file_name("update-pending.json")
            .exists()
    );
    assert!(
        !fixture
            .config
            .installed_exe
            .with_file_name("flotilla.exe.previous")
            .exists()
    );
}

#[test]
fn a_failing_self_check_rolls_back_the_staged_replacement() {
    let fixture = Fixture::new("0.5.0");
    let source = fixture.site.source();
    let (_, result) = fixture.check(&source);
    let staged = result.unwrap().expect("staged");

    let err = apply(&fixture.config, &staged, |_| false).unwrap_err();
    assert!(
        matches!(err, UpdateError::Install(install::InstallError::SelfCheck)),
        "{err:?}"
    );
    // The previous build is back in place and would start normally.
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
    assert!(
        !fixture
            .config
            .installed_exe
            .with_file_name("flotilla.exe.previous")
            .exists()
    );
    assert!(
        !fixture
            .config
            .installed_exe
            .with_file_name("update-pending.json")
            .exists(),
        "a rolled-back replacement leaves no marker behind"
    );
}

#[test]
fn a_healthy_replacement_is_confirmed_at_the_next_start() {
    let fixture = Fixture::new("0.5.0");
    let source = fixture.site.source();
    let (_, result) = fixture.check(&source);
    let staged = result.unwrap().expect("staged");

    let replacement = apply(&fixture.config, &staged, |path| {
        std::fs::read(path).unwrap() == fixture.site.asset_bytes
    })
    .expect("replacement");
    assert_eq!(
        std::fs::read(fixture.installed()).unwrap(),
        fixture.site.asset_bytes
    );
    assert!(
        replacement.backup.exists(),
        "the backup is kept until the new build starts"
    );
    assert!(
        fixture
            .config
            .installed_exe
            .with_file_name("update-pending.json")
            .exists()
    );

    let dir = fixture.config.installed_exe.parent().unwrap();
    let recovery = install::recover(dir, "0.5.0", |_| true, |_| true).unwrap();
    assert_eq!(
        recovery,
        install::Recovery::Confirmed {
            version: "0.5.0".into()
        }
    );
    assert!(!replacement.backup.exists());
    assert_eq!(
        std::fs::read(fixture.installed()).unwrap(),
        fixture.site.asset_bytes
    );
}

#[test]
fn the_replacement_refuses_a_staged_asset_whose_bytes_changed() {
    let fixture = Fixture::new("0.5.0");
    let source = fixture.site.source();
    let (_, result) = fixture.check(&source);
    let staged = result.unwrap().expect("staged");
    std::fs::write(&staged.path, b"swapped after verification").unwrap();

    let err = apply(&fixture.config, &staged, |_| true).unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::Download(DownloadError::HashMismatch { .. })
        ),
        "{err:?}"
    );
    assert_eq!(std::fs::read(fixture.installed()).unwrap(), RUNNING);
}

#[test]
fn urls_point_at_the_published_release_assets() {
    let config = Fixture::new("0.5.0").config;
    let base = "https://example.test/releases";
    assert_eq!(
        config.manifest_url(),
        format!("{base}/latest/download/latest.json")
    );
    assert_eq!(
        config.signature_url(),
        format!("{base}/latest/download/latest.json.sig")
    );
    assert_eq!(
        config.asset_url("v0.5.0", ASSET_NAME),
        format!("{base}/download/v0.5.0/{ASSET_NAME}")
    );
    // A trailing slash in the configured feed never doubles up.
    let mut trailing = config.clone();
    trailing.release_base = format!("{base}/");
    assert_eq!(trailing.manifest_url(), config.manifest_url());
}

#[test]
fn a_platform_without_releases_stages_nothing() {
    let mut config = Fixture::new("0.5.0").config;
    config.platform = None;
    let site = Site::new("0.5.0");
    let source = site.source();
    let keys = site.key.trusted();
    let result = check_and_stage(&config, &source, &keys, &mut |_| {});
    assert!(matches!(result, Err(UpdateError::NoPlatform)));
}
