//! Approval of waiting writes in Windows notifications (spec 8.6). One action at a time: the next
//! toast appears only once the shown action is decided or expires, so a burst of writes never piles
//! up toasts to approve blindly. Content too long for a toast gets only a `Show all` button, which
//! opens a dialog with all of it. No toast or dialog decides more than one action, and only the
//! user's click reaches [`Pending::decide`]: no tool does. Other platforms leave the actions waiting
//! until they expire.

use std::sync::Arc;

use tokio::runtime::Runtime;

use crate::pending::{Kind, Pending, Preview};

/// The AppUserModelID the toasts show under, registered for the current user at daemon start.
pub const APP_ID: &str = "open_mess.daemon";
/// The most content a toast shows, in characters; longer content only gets `Show all`.
pub const MAX_TOAST_TEXT: usize = 280;
/// Participants a toast names; the others are counted.
const NAMED_PARTICIPANTS: usize = 5;

/// What the approval of one action shows: the account, the conversation, the participants and the
/// whole content (spec 8.6), cleaned of control characters and direction marks.
#[derive(Debug, PartialEq, Eq)]
pub struct Prompt {
    pub title: String,
    /// The account, the conversation and its participants.
    pub context: String,
    /// The text; an edit's text before and after; a reaction and the message it is under.
    pub content: String,
}

impl Prompt {
    pub fn new(preview: &Preview) -> Prompt {
        let what = match preview.kind {
            Kind::Send => "send",
            Kind::Edit => "edit",
            Kind::React => "reaction",
        };
        let mut context = format!("{} - {}", clean(&preview.account), clean(&preview.conversation_title));
        if !preview.participants.is_empty() {
            context.push_str("\nWith ");
            context.push_str(&participants(&preview.participants));
        }
        let field = |text: &Option<String>| clean(text.as_deref().unwrap_or_default());
        let content = match (preview.kind, &preview.target_text) {
            (Kind::Send, None) => field(&preview.text),
            (Kind::Send, Some(_)) => format!("{}\n\nIn reply to: {}", field(&preview.text), field(&preview.target_text)),
            (Kind::Edit, _) => format!("Before: {}\nAfter: {}", field(&preview.target_text), field(&preview.text)),
            (Kind::React, _) => format!("{} on: {}", field(&preview.reaction), field(&preview.target_text)),
        };
        Prompt { title: format!("open_mess: approve {what}"), context, content }
    }

    /// The toast's content line and its buttons (label, argument): the content with `Approve` and
    /// `Reject`, or, when it is too long to read there, only `Show all`.
    pub fn toast(&self) -> (&str, &'static [(&'static str, &'static str)]) {
        if self.content.chars().count() <= MAX_TOAST_TEXT {
            (&self.content, &[("Approve", "approve"), ("Reject", "reject")])
        } else {
            ("Too long for a notification: open it to read all of it before you decide.", &[("Show all", "show")])
        }
    }
}

/// The first names, then how many more.
pub fn participants(names: &[String]) -> String {
    let mut shown: Vec<String> = names.iter().take(NAMED_PARTICIPANTS).map(|name| clean(name)).collect();
    if names.len() > NAMED_PARTICIPANTS {
        shown.push(format!("+{}", names.len() - NAMED_PARTICIPANTS));
    }
    shown.join(", ")
}

/// Without control characters (line breaks stay) and the direction marks that would make a text
/// read differently than it is.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            c == '\n' || !(c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        })
        .collect()
}

/// Which waiting action is on screen.
#[derive(Default)]
pub struct Queue {
    shown: Option<String>,
}

impl Queue {
    /// The action to show now: the oldest waiting one, once the one on screen is no longer waiting.
    pub fn next<'a>(&mut self, waiting: &'a [Preview]) -> Option<&'a Preview> {
        if self.shown.as_ref().is_some_and(|shown| waiting.iter().any(|preview| &preview.action == shown)) {
            return None;
        }
        let next = waiting.first();
        self.shown = next.map(|preview| preview.action.clone());
        next
    }
}

/// Hands the waiting actions to `present` one at a time, for as long as `pending` lives.
pub async fn run(pending: Arc<Pending>, present: impl Fn(&Preview)) {
    let mut changed = pending.subscribe();
    let mut queue = Queue::default();
    loop {
        if let Some(preview) = queue.next(&pending.waiting()) {
            present(preview);
        }
        if changed.changed().await.is_err() {
            return;
        }
    }
}

/// Registers the toasts' AppUserModelID and shows the waiting actions for the daemon's lifetime.
#[cfg(windows)]
pub fn start(runtime: &Runtime, pending: Arc<Pending>) {
    // ponytail: without the registration Windows may show no toast; reported once a log sink exists.
    let _ = toast::register();
    let (handle, shown) = (runtime.handle().clone(), Arc::clone(&pending));
    runtime.spawn(run(pending, move |preview| toast::show(&shown, &handle, Prompt::new(preview), preview.action.clone())));
}

/// No notifications here yet: actions wait until they expire (the configuration window of
/// milestone E lists them).
#[cfg(not(windows))]
pub fn start(_runtime: &Runtime, _pending: Arc<Pending>) {}

#[cfg(windows)]
mod toast {
    use std::sync::Arc;

    use tauri_winrt_notification::{Scenario, Toast};
    use tokio::runtime::Handle;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    use super::{APP_ID, Prompt};
    use crate::pending::Pending;

    /// `HKCU\Software\Classes\AppUserModelId\open_mess.daemon` with the name toasts show.
    pub(super) fn register() -> std::io::Result<()> {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"Software\Classes\AppUserModelId\{APP_ID}"))?;
        key.set_value("DisplayName", &"open_mess")
    }

    /// Shows the toast of one action; it stays on screen until the user acts on it. Off the async
    /// threads: showing waits for Windows.
    pub(super) fn show(pending: &Arc<Pending>, runtime: &Handle, prompt: Prompt, action: String) {
        let (pending, decide_on) = (Arc::clone(pending), runtime.clone());
        // The outcome is for `pending` to report.
        let decide = move |approve: bool| {
            let (pending, action) = (Arc::clone(&pending), action.clone());
            decide_on.spawn(async move { pending.decide(&action, approve).await });
        };
        runtime.spawn_blocking(move || {
            let (line, buttons) = prompt.toast();
            let mut toast = Toast::new(APP_ID).title(&prompt.title).text1(&prompt.context).text2(line).scenario(Scenario::Reminder);
            for (label, argument) in buttons {
                toast = toast.add_button(label, argument);
            }
            let prompt = Arc::new(prompt);
            let toast = toast.on_activated(move |argument| {
                match argument.as_deref() {
                    Some("approve") => decide(true),
                    Some("reject") => decide(false),
                    // `Show all`, or a click on the toast itself.
                    _ => {
                        let (prompt, decide) = (Arc::clone(&prompt), decide.clone());
                        std::thread::spawn(move || {
                            if let Some(approve) = ask(&prompt) {
                                decide(approve);
                            }
                        });
                    }
                }
                Ok(())
            });
            // ponytail: a toast Windows refuses leaves the action waiting until it expires;
            // reported once a log sink exists.
            let _ = toast.show();
        });
    }

    /// The whole content in a modal dialog: Yes approves, No rejects, closing decides nothing.
    /// (Buttons named Approve and Reject would need common controls 6 and an application manifest.)
    fn ask(prompt: &Prompt) -> Option<bool> {
        let answer = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title(&prompt.title)
            .set_description(format!("{}\n\n{}\n\nApprove this? Yes approves it, No rejects it.", prompt.context, prompt.content))
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        match answer {
            rfd::MessageDialogResult::Yes => Some(true),
            rfd::MessageDialogResult::No => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(action: &str, kind: Kind) -> Preview {
        Preview {
            action: action.into(),
            kind,
            account: "Ann (teams:ann)".into(),
            conversation_title: "Project".into(),
            participants: vec!["Pat".into(), "Sam".into()],
            text: Some("on my way".into()),
            reaction: None,
            target_text: None,
        }
    }

    #[test]
    fn toast_text_is_truncated_with_show_all() {
        let mut send = preview("a1", Kind::Send);
        let prompt = Prompt::new(&send);
        assert_eq!(prompt.title, "open_mess: approve send");
        assert_eq!(prompt.context, "Ann (teams:ann) - Project\nWith Pat, Sam");
        assert_eq!(prompt.toast(), ("on my way", &[("Approve", "approve"), ("Reject", "reject")][..]));

        send.text = Some("x".repeat(MAX_TOAST_TEXT));
        assert_eq!(Prompt::new(&send).toast().1, [("Approve", "approve"), ("Reject", "reject")]);
        // One character more and the toast shows none of it, only the way to all of it.
        send.text = Some("x".repeat(MAX_TOAST_TEXT + 1));
        let prompt = Prompt::new(&send);
        let (line, buttons) = prompt.toast();
        assert!(!line.contains('x'));
        assert_eq!(buttons, [("Show all", "show")]);
        assert_eq!(prompt.content, "x".repeat(MAX_TOAST_TEXT + 1));
        // A reply's target counts too.
        send.text = Some("ok".into());
        send.target_text = Some("y".repeat(MAX_TOAST_TEXT));
        assert_eq!(Prompt::new(&send).toast().1, [("Show all", "show")]);

        let mut edit = preview("a2", Kind::Edit);
        edit.target_text = Some("on my wya".into());
        assert_eq!(Prompt::new(&edit).content, "Before: on my wya\nAfter: on my way");
        let mut react = preview("a3", Kind::React);
        react.reaction = Some("like".into());
        react.target_text = Some("lunch?".into());
        assert_eq!(Prompt::new(&react).title, "open_mess: approve reaction");
        assert_eq!(Prompt::new(&react).content, "like on: lunch?");

        // A name or text can't hide or reorder what the toast says.
        send.target_text = None;
        send.text = Some("pay \u{202e}gpj.exe\u{7} now\nplease".into());
        assert_eq!(Prompt::new(&send).content, "pay gpj.exe now\nplease");
    }

    #[test]
    fn participants_are_shortened() {
        let names = |n: usize| (1..=n).map(|i| format!("P{i}")).collect::<Vec<_>>();
        assert_eq!(participants(&names(1)), "P1");
        assert_eq!(participants(&names(5)), "P1, P2, P3, P4, P5");
        assert_eq!(participants(&names(8)), "P1, P2, P3, P4, P5, +3");
        assert_eq!(participants(&[]), "");
        let mut channel = preview("a1", Kind::Send);
        channel.participants.clear();
        assert_eq!(Prompt::new(&channel).context, "Ann (teams:ann) - Project");
    }

    #[test]
    fn queue_shows_one_at_a_time() {
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|id| preview(id, Kind::Send));
        let mut queue = Queue::default();
        let shown = |preview: Option<&Preview>| preview.map(|preview| preview.action.clone());
        assert_eq!(shown(queue.next(&[])), None);
        assert_eq!(shown(queue.next(&[a.clone(), b.clone(), c.clone()])), Some("a".into()));
        // While "a" waits nothing else shows, however many come.
        assert_eq!(shown(queue.next(&[a.clone(), b.clone(), c.clone(), d.clone()])), None);
        // "a" decided or expired: the oldest of the rest.
        assert_eq!(shown(queue.next(&[b.clone(), c.clone(), d.clone()])), Some("b".into()));
        assert_eq!(shown(queue.next(&[b.clone(), c.clone(), d.clone()])), None);
        assert_eq!(shown(queue.next(&[])), None);
        assert_eq!(shown(queue.next(std::slice::from_ref(&d))), Some("d".into()));
    }
}
