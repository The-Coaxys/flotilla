//! The update prompt: what the client shows, and when a restart is allowed.
//!
//! Pure data, so the rules are unit-tested: an update never interrupts a
//! running turn, a restart only happens from an explicit user action, and a
//! failure leaves the current build in place.

/// What the updater is doing, as far as the UI is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// Nothing to say (no feed, up to date, or dismissed).
    Idle,
    /// Asking the release feed.
    Checking,
    /// A newer version exists and is being fetched.
    Available { version: String },
    /// The asset is downloading.
    Downloading {
        version: String,
        received: u64,
        total: u64,
    },
    /// The verified update is staged; it applies on the next restart.
    Staged { version: String },
    /// The update failed; the running build is untouched.
    Failed {
        version: Option<String>,
        message: String,
    },
    /// The replacement happened and the new build was started: quit now.
    Restarting { version: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerAction {
    /// Apply the staged update and restart.
    Restart,
    /// Ask the feed again after a failure.
    Retry,
}

/// One line of banner text plus the actions it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub tone: Tone,
    pub title: String,
    pub detail: String,
    pub action: Option<BannerAction>,
    /// Whether the banner can be dismissed for this session.
    pub dismissible: bool,
}

/// A restart is only ever allowed while no turn is running.
pub fn restart_allowed(busy: bool) -> bool {
    !busy
}

pub fn percent(received: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    ((received.min(total) * 100) / total) as u32
}

/// The banner for `state`, or `None` when there is nothing to show.
pub fn banner(state: &UpdateState, busy: bool) -> Option<Banner> {
    let banner = match state {
        UpdateState::Idle | UpdateState::Checking | UpdateState::Restarting { .. } => return None,
        UpdateState::Available { version } => Banner {
            tone: Tone::Info,
            title: format!("Flotilla {version} is available"),
            detail: "Downloading in the background; nothing changes until you restart.".to_string(),
            action: None,
            dismissible: false,
        },
        UpdateState::Downloading {
            version,
            received,
            total,
        } => Banner {
            tone: Tone::Info,
            title: format!("Downloading Flotilla {version}"),
            detail: format!(
                "{}% of {} MiB",
                percent(*received, *total),
                total.div_ceil(1 << 20)
            ),
            action: None,
            dismissible: false,
        },
        UpdateState::Staged { version } if restart_allowed(busy) => Banner {
            tone: Tone::Success,
            title: format!("Flotilla {version} is ready to install"),
            detail: "Restart to switch versions. Nothing has been replaced yet.".to_string(),
            action: Some(BannerAction::Restart),
            dismissible: true,
        },
        UpdateState::Staged { version } => Banner {
            tone: Tone::Success,
            title: format!("Flotilla {version} is ready to install"),
            detail: "Restart is deferred while a turn is running; nothing will interrupt it."
                .to_string(),
            action: None,
            dismissible: true,
        },
        UpdateState::Failed { version, message } => Banner {
            tone: Tone::Danger,
            title: match version {
                Some(version) => format!("Update to {version} failed"),
                None => "Update check failed".to_string(),
            },
            detail: format!("{message} The current build keeps running."),
            action: Some(BannerAction::Retry),
            dismissible: true,
        },
    };
    Some(banner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_update_offers_restart_only_when_idle() {
        let state = UpdateState::Staged {
            version: "0.5.0".into(),
        };
        let idle = banner(&state, false).expect("banner");
        assert_eq!(idle.action, Some(BannerAction::Restart));
        assert_eq!(idle.tone, Tone::Success);
        assert!(idle.title.contains("0.5.0"));

        let busy = banner(&state, true).expect("banner");
        assert_eq!(busy.action, None);
        assert!(busy.detail.to_lowercase().contains("turn"));
        assert!(busy.detail.contains("nothing will interrupt it"));
    }

    #[test]
    fn restart_is_never_allowed_during_a_turn() {
        assert!(!restart_allowed(true));
        assert!(restart_allowed(false));
    }

    #[test]
    fn downloading_never_offers_a_restart() {
        let state = UpdateState::Downloading {
            version: "0.5.0".into(),
            received: 512,
            total: 2048,
        };
        let view = banner(&state, false).expect("banner");
        assert_eq!(view.action, None);
        assert!(!view.dismissible);
        assert_eq!(view.detail, "25% of 1 MiB");
    }

    #[test]
    fn a_failure_offers_retry_and_says_the_current_build_keeps_running() {
        let state = UpdateState::Failed {
            version: Some("0.5.0".into()),
            message: "signature does not match the document.".into(),
        };
        let view = banner(&state, false).expect("banner");
        assert_eq!(view.tone, Tone::Danger);
        assert_eq!(view.action, Some(BannerAction::Retry));
        assert!(view.detail.contains("keeps running"));
        assert!(view.title.contains("0.5.0"));

        let no_version = UpdateState::Failed {
            version: None,
            message: "offline".into(),
        };
        assert!(banner(&no_version, true).unwrap().title.contains("check"));
    }

    #[test]
    fn quiet_states_show_nothing() {
        assert!(banner(&UpdateState::Idle, false).is_none());
        assert!(banner(&UpdateState::Checking, false).is_none());
        assert!(
            banner(
                &UpdateState::Restarting {
                    version: "0.5.0".into()
                },
                false
            )
            .is_none()
        );
    }

    #[test]
    fn percent_is_bounded() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(1, 2), 50);
        assert_eq!(percent(5, 4), 100);
    }
}
