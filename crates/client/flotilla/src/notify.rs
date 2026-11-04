//! Desktop notifications (spec §8.4).
//!
//! The in-app inbox, toasts and banner stay the cross-platform path. This
//! module only decides whether a desktop notification is allowed and, on
//! Windows, shows it through WinRT `ToastNotificationManager`. The installer
//! registers the same AppUserModelID on the Start-menu shortcut.
//!
//! Columns in a toast are user-visible text, never markup: they are escaped
//! before they become the toast's XML payload.
use flc_ui::notify::EventClass;
use uuid::Uuid;

/// A toast was clicked. The app opens the source tab for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub host: String,
    pub session: Uuid,
}

/// The AppUserModelID the installer registers on the Start-menu shortcut.
pub const APP_USER_MODEL_ID: &str = "Coaxys.Flotilla";

/// Where the WinRT activation callbacks send their result. `None` on platforms
/// without a desktop toast backend.
pub type ActivationSender = tokio::sync::mpsc::UnboundedSender<Activation>;

/// Give the Windows backend the channel its click callbacks use.
pub fn install(sender: ActivationSender) {
    #[cfg(windows)]
    {
        let _ = activations().set(sender);
    }
    #[cfg(not(windows))]
    {
        let _ = sender;
    }
}

#[cfg(windows)]
fn activations() -> &'static std::sync::OnceLock<ActivationSender> {
    static ACTIVATIONS: std::sync::OnceLock<ActivationSender> = std::sync::OnceLock::new();
    &ACTIVATIONS
}

/// Register the process identity toasts and the taskbar group by. Safe to
/// call more than once.
pub fn init_app_id() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
        use windows::core::HSTRING;
        // A failure only degrades toast attribution; it must never stop the UI.
        if let Err(error) =
            unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_USER_MODEL_ID)) }
        {
            tracing::warn!("cannot register the AppUserModelID: {error}");
        }
    }
}

#[cfg(windows)]
fn class_label(class: EventClass) -> &'static str {
    match class {
        EventClass::Completion => "Finished",
        EventClass::Input => "Needs you",
    }
}

/// Escape text for the toast's XML payload. Bounded by the caller's title,
/// which is already a short fixed string, but every user-visible fragment
/// still goes through this function.
#[cfg_attr(not(windows), allow(dead_code))]
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Build the toast XML. Kept pure so the payload shape is testable on every
/// platform.
#[cfg_attr(not(windows), allow(dead_code))]
fn toast_xml(title: &str, line: &str, sounds: bool) -> String {
    let audio = if sounds {
        r#"<audio src="ms-winsoundevent:Notification.Default"/>"#
    } else {
        r#"<audio silent="true"/>"#
    };
    format!(
        r#"<toast activationType="foreground"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>{audio}</toast>"#,
        escape(title),
        escape(line)
    )
}

/// Show one Windows toast. A failure is logged and never blocks the UI; the
/// in-app toast is already visible either way.
pub fn desktop(host: &str, class: EventClass, title: &str, session: Uuid, sounds: bool) {
    #[cfg(windows)]
    {
        use windows::Data::Xml::Dom::XmlDocument;
        use windows::Foundation::TypedEventHandler;
        use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
        use windows::core::{HSTRING, IInspectable};

        let Some(sender) = activations().get().cloned() else {
            return;
        };
        let xml = toast_xml(
            title,
            &format!("{} · {}", class_label(class), APP_USER_MODEL_ID),
            sounds,
        );
        let shown = (|| -> windows::core::Result<()> {
            let document = XmlDocument::new()?;
            document.LoadXml(&HSTRING::from(xml))?;
            let toast = ToastNotification::CreateToastNotification(&document)?;
            let sender = sender.clone();
            let host = host.to_string();
            let handler = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
                let _ = sender.send(Activation {
                    host: host.clone(),
                    session,
                });
                Ok(())
            });
            toast.Activated(&handler)?;
            let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
                APP_USER_MODEL_ID,
            ))?;
            notifier.Show(&toast)?;
            Ok(())
        })();
        if let Err(error) = shown {
            tracing::warn!("Windows toast could not be shown: {error}");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (host, class, title, session, sounds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_text_is_escaped_and_the_audio_toggle_is_respected() {
        let xml = toast_xml("A & B <c>", "x\"y'z", true);
        assert!(xml.contains("A &amp; B &lt;c&gt;"));
        assert!(xml.contains("x&quot;y&apos;z"));
        assert!(xml.contains(r#"<audio src="ms-winsoundevent:Notification.Default"/>"#));
        assert!(!toast_xml("t", "l", false).contains("Notification.Default"));
        assert!(toast_xml("t", "l", false).contains(r#"<audio silent="true"/>"#));
    }

    #[test]
    fn the_identity_matches_the_installer_shortcut() {
        assert_eq!(APP_USER_MODEL_ID, "Coaxys.Flotilla");
    }
}
