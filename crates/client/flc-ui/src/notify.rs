//! Notification delivery policy (spec §8.4).
//!
//! Window-free half of the notification settings: the two event classes keep
//! independent policies, and the focused-window rule is a pure decision so it
//! can be tested without a toast backend. The Windows WinRT delivery lives in
//! the client; the in-app inbox and toasts always stay on.
use serde::{Deserialize, Serialize};

/// What happens to a desktop notification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Show it even while a Flotilla window has focus.
    Always,
    /// Show it only while no Flotilla window has focus.
    #[default]
    Unfocused,
    /// Never show a desktop notification; the in-app toast and inbox remain.
    Never,
}

impl Delivery {
    pub const ALL: [Delivery; 3] = [Delivery::Always, Delivery::Unfocused, Delivery::Never];

    pub fn label(self) -> &'static str {
        match self {
            Delivery::Always => "Always",
            Delivery::Unfocused => "Only when not focused",
            Delivery::Never => "Never",
        }
    }

    /// The next policy in the settings row's cycle.
    pub fn next(self) -> Self {
        match self {
            Delivery::Always => Delivery::Unfocused,
            Delivery::Unfocused => Delivery::Never,
            Delivery::Never => Delivery::Always,
        }
    }

    /// Whether this policy allows a desktop notification right now.
    pub fn allows(self, focused: bool) -> bool {
        match self {
            Delivery::Always => true,
            Delivery::Unfocused => !focused,
            Delivery::Never => false,
        }
    }
}

/// Events are classified by what the user has to do about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    /// A turn or session finished.
    Completion,
    /// An approval or a question is waiting for input.
    Input,
}

/// Per-class desktop notification policy with the shared sound toggle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    pub completion: Delivery,
    pub input: Delivery,
    pub sounds: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            completion: Delivery::Unfocused,
            input: Delivery::Always,
            sounds: true,
        }
    }
}

impl NotificationSettings {
    pub fn delivery(&self, class: EventClass) -> Delivery {
        match class {
            EventClass::Completion => self.completion,
            EventClass::Input => self.input,
        }
    }

    pub fn set_delivery(&mut self, class: EventClass, delivery: Delivery) {
        match class {
            EventClass::Completion => self.completion = delivery,
            EventClass::Input => self.input = delivery,
        }
    }

    /// Whether a desktop notification for `class` is allowed now.
    pub fn should_deliver(&self, class: EventClass, focused: bool) -> bool {
        self.delivery(class).allows(focused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_policy_applies_the_focused_rule() {
        assert!(Delivery::Always.allows(true));
        assert!(Delivery::Always.allows(false));
        assert!(!Delivery::Unfocused.allows(true));
        assert!(Delivery::Unfocused.allows(false));
        assert!(!Delivery::Never.allows(true));
        assert!(!Delivery::Never.allows(false));
    }

    #[test]
    fn the_two_event_classes_are_configured_separately() {
        let mut settings = NotificationSettings::default();
        assert!(settings.should_deliver(EventClass::Input, true));
        assert!(!settings.should_deliver(EventClass::Completion, true));
        assert!(settings.should_deliver(EventClass::Completion, false));

        settings.set_delivery(EventClass::Input, Delivery::Never);
        settings.set_delivery(EventClass::Completion, Delivery::Always);
        assert!(!settings.should_deliver(EventClass::Input, false));
        assert!(settings.should_deliver(EventClass::Completion, true));
    }

    #[test]
    fn the_row_cycle_visits_every_policy_and_returns() {
        let mut delivery = Delivery::Always;
        for _ in 0..Delivery::ALL.len() {
            delivery = delivery.next();
        }
        assert_eq!(delivery, Delivery::Always);
    }

    #[test]
    fn a_settings_file_without_the_section_uses_the_defaults() {
        let settings: NotificationSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, NotificationSettings::default());
        let explicit: NotificationSettings =
            serde_json::from_str(r#"{"completion":"always","input":"ummm","sounds":false}"#)
                .unwrap_or_default();
        // An unknown delivery name is not silently accepted by serde, but a
        // missing section is; the caller sanitizes a corrupt file.
        assert_eq!(explicit, NotificationSettings::default());
        let parsed: NotificationSettings =
            serde_json::from_str(r#"{"completion":"always","input":"never","sounds":false}"#)
                .unwrap();
        assert_eq!(parsed.completion, Delivery::Always);
        assert_eq!(parsed.input, Delivery::Never);
        assert!(!parsed.sounds);
    }
}
