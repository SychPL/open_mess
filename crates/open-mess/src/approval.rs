//! Approval of waiting writes in Windows notifications (spec 8.6). One action at a time: the next
//! toast appears only once the shown action is decided or expires, so a burst of writes never piles
//! up toasts to approve blindly. Nothing is approved that was not on screen in full: a toast offers
//! `Approve` only when every participant is named and all of it fits the lines Windows shows,
//! otherwise only `Show all`, which opens a dialog with everything. No toast or dialog decides more
//! than one action, and only the user's click reaches [`Pending::decide`]: no tool does. Other
//! platforms leave the actions waiting until they expire.

use std::sync::Arc;

use tokio::runtime::Runtime;

use crate::pending::{Kind, Pending, Preview};

/// The AppUserModelID the toasts show under, registered for the current user at daemon start.
pub const APP_ID: &str = "open_mess.daemon";
/// Participants a toast names; the others are counted.
const NAMED_PARTICIPANTS: usize = 5;
/// Windows shows at most four lines of a toast's two text elements (the title has its own two).
/// Counted at 24 narrow characters a line (see [`lines`]), which holds even for the widest letters
/// at the default text size, so what a toast can approve is never cut off.
const TOAST_LINES: usize = 4;
const TOAST_WIDTH: usize = 24;
/// The dialog has no scroll bar: what it approves must fit a small screen. Longer content can only
/// be rejected there; approving it waits for the configuration window (milestone E).
const DIALOG_LINES: usize = 25;
const DIALOG_WIDTH: usize = 40;
const APPROVE: &[(&str, &str)] = &[("Approve", "approve"), ("Reject", "reject")];
const SHOW_ALL: &[(&str, &str)] = &[("Show all", "show")];

/// What the approval of one action shows: the account, the conversation, the participants and the
/// whole content (spec 8.6), cleaned of control characters and direction marks.
#[derive(Debug, PartialEq, Eq)]
pub struct Prompt {
    pub title: String,
    /// The account and the conversation.
    pub conversation: String,
    /// Every participant's name; none for a channel.
    pub participants: Vec<String>,
    /// The text; an edit's text before and after; a reaction and the message it is under.
    pub content: String,
}

/// What a toast shows.
#[derive(Debug, PartialEq, Eq)]
pub struct ToastText {
    /// The conversation and the first participants.
    pub context: String,
    /// The content, or how to read it.
    pub line: String,
    /// Label and argument of each button.
    pub buttons: &'static [(&'static str, &'static str)],
}

impl Prompt {
    pub fn new(preview: &Preview) -> Prompt {
        let what = match preview.kind {
            Kind::Send => "send",
            Kind::Edit => "edit",
            Kind::React => "reaction",
        };
        let conversation = format!("{} - {}", clean(&preview.account), clean(&preview.conversation_title));
        let participants = preview.participants.iter().map(|name| clean(name)).collect();
        let field = |text: &Option<String>| clean(text.as_deref().unwrap_or_default());
        let content = match (preview.kind, &preview.target_text) {
            (Kind::Send, None) => field(&preview.text),
            (Kind::Send, Some(_)) => format!("{}\n\nIn reply to: {}", field(&preview.text), field(&preview.target_text)),
            (Kind::Edit, _) => format!("Before: {}\nAfter: {}", field(&preview.target_text), field(&preview.text)),
            (Kind::React, _) => format!("{} on: {}", field(&preview.reaction), field(&preview.target_text)),
        };
        Prompt { title: format!("open_mess: approve {what}"), conversation, participants, content }
    }

    /// The conversation and the first `named` participants.
    fn context(&self, named: usize) -> String {
        let mut context = self.conversation.clone();
        if !self.participants.is_empty() {
            context.push_str("\nWith ");
            context.push_str(&participants(&self.participants, named));
        }
        context
    }

    /// The content with `Approve` and `Reject` when all of it fits the toast, else only `Show all`.
    pub fn toast(&self) -> ToastText {
        let context = self.context(NAMED_PARTICIPANTS);
        let fits = lines(&context, TOAST_WIDTH) + lines(&self.content, TOAST_WIDTH) <= TOAST_LINES;
        if fits && self.participants.len() <= NAMED_PARTICIPANTS {
            ToastText { context, line: self.content.clone(), buttons: APPROVE }
        } else {
            ToastText { context, line: "Open it to read all of it before you decide.".into(), buttons: SHOW_ALL }
        }
    }

    /// Everything, for the dialog: every participant and the whole content; None when that is too
    /// long to be read there in full.
    pub fn dialog(&self) -> Option<String> {
        let text = format!("{}\n\n{}", self.context(usize::MAX), self.content);
        (lines(&text, DIALOG_WIDTH) <= DIALOG_LINES).then_some(text)
    }
}

/// The first `named` names, then how many more.
pub fn participants(names: &[String], named: usize) -> String {
    let mut shown: Vec<&str> = names.iter().take(named).map(String::as_str).collect();
    let more = names.len().saturating_sub(named);
    let more = format!("+{more}");
    if names.len() > named {
        shown.push(&more);
    }
    shown.join(", ")
}

/// About how many lines `text` takes at `width` narrow characters a line, on the safe side: the font
/// is not known here, so a wide character (CJK, emoji and the like) counts as two.
fn lines(text: &str, width: usize) -> usize {
    let units = |line: &str| line.chars().map(|c| if u32::from(c) < 0x1100 { 1 } else { 2 }).sum::<usize>();
    text.split('\n').map(|line| units(line).div_ceil(width).max(1)).sum()
}

/// Without control characters (line breaks stay) and the direction marks that would make a text
/// read differently than it is.
pub(crate) fn clean(text: &str) -> String {
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

    /// Whether the action on screen is no longer waiting: decided elsewhere, expired or removed
    /// with its account.
    pub fn shown_is_gone(&self, waiting: &[Preview]) -> bool {
        self.shown.as_ref().is_some_and(|shown| !waiting.iter().any(|preview| &preview.action == shown))
    }
}

/// Hands the waiting actions to `present` one at a time, for as long as `pending` lives, and calls
/// `withdraw` when the one on screen is no longer waiting, so its toast goes away.
pub async fn run(pending: Arc<Pending>, present: impl Fn(&Preview), withdraw: impl Fn()) {
    let mut changed = pending.subscribe();
    let mut queue = Queue::default();
    loop {
        let waiting = pending.waiting();
        if queue.shown_is_gone(&waiting) {
            withdraw();
        }
        if let Some(preview) = queue.next(&waiting) {
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
    let withdrawn = runtime.handle().clone();
    runtime.spawn(run(
        pending,
        move |preview| toast::show(&shown, &handle, Prompt::new(preview), preview.action.clone()),
        move || toast::withdraw(&withdrawn),
    ));
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
    use windows::UI::Notifications::ToastNotificationManager;
    use windows::core::HSTRING;
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
            let shown = prompt.toast();
            let mut toast = Toast::new(APP_ID).title(&prompt.title).text1(&shown.context).text2(&shown.line).scenario(Scenario::Reminder);
            for (label, argument) in shown.buttons {
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

    /// Takes open_mess's toasts off the screen and out of the notification center. One action is
    /// on screen at a time, so this is the toast of the action that is no longer waiting.
    pub(super) fn withdraw(runtime: &Handle) {
        runtime.spawn_blocking(|| {
            // ponytail: a toast Windows keeps stays until the user dismisses it (clicking it decides
            // nothing then); reported once a log sink exists.
            let _ = ToastNotificationManager::History().and_then(|history| history.ClearWithId(&HSTRING::from(APP_ID)));
        });
    }

    /// Everything in a modal dialog: Yes approves, No rejects, closing decides nothing. Content too
    /// long to show in full can only be rejected. (Buttons named Approve and Reject would need
    /// common controls 6 and an application manifest.)
    fn ask(prompt: &Prompt) -> Option<bool> {
        let dialog = rfd::MessageDialog::new().set_level(rfd::MessageLevel::Warning).set_title(&prompt.title);
        let Some(text) = prompt.dialog() else {
            let why = "This is too long to show here in full, so it can't be approved here.\n\n\
                       Reject it? Yes rejects it, No leaves it waiting until it expires.";
            let answer = dialog.set_description(why).set_buttons(rfd::MessageButtons::YesNo).show();
            return (answer == rfd::MessageDialogResult::Yes).then_some(false);
        };
        let answer =
            dialog.set_description(format!("{text}\n\nApprove this? Yes approves it, No rejects it.")).set_buttons(rfd::MessageButtons::YesNo).show();
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
        let toast = prompt.toast();
        assert_eq!(toast.context, "Ann (teams:ann) - Project\nWith Pat, Sam");
        assert_eq!((toast.line.as_str(), toast.buttons), ("on my way", APPROVE));

        // The context takes three of the four lines Windows shows: one line of content fits.
        send.text = Some("x".repeat(TOAST_WIDTH));
        assert_eq!(Prompt::new(&send).toast().buttons, APPROVE);
        // One character more and the toast shows none of it, only the way to all of it.
        send.text = Some("x".repeat(TOAST_WIDTH + 1));
        let prompt = Prompt::new(&send);
        let toast = prompt.toast();
        assert!(!toast.line.contains('x'));
        assert_eq!(toast.buttons, SHOW_ALL);
        assert_eq!(prompt.content, "x".repeat(TOAST_WIDTH + 1));
        // Short lines count as lines, wide characters as two.
        send.text = Some("a\nb".into());
        assert_eq!(Prompt::new(&send).toast().buttons, SHOW_ALL);
        send.text = Some("\u{4f60}".repeat(TOAST_WIDTH / 2 + 1));
        assert_eq!(Prompt::new(&send).toast().buttons, SHOW_ALL);
        // A reply's target counts too.
        send.text = Some("ok".into());
        send.target_text = Some("fine".into());
        assert_eq!(Prompt::new(&send).toast().buttons, SHOW_ALL);

        // The dialog shows everything, up to what fits a screen.
        send.target_text = None;
        send.text = Some("x".repeat(DIALOG_WIDTH * 20));
        assert!(Prompt::new(&send).dialog().unwrap().ends_with(&"x".repeat(DIALOG_WIDTH * 20)));
        send.text = Some("x".repeat(DIALOG_WIDTH * DIALOG_LINES));
        assert_eq!(Prompt::new(&send).dialog(), None);

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
        assert_eq!(participants(&names(1), NAMED_PARTICIPANTS), "P1");
        assert_eq!(participants(&names(5), NAMED_PARTICIPANTS), "P1, P2, P3, P4, P5");
        assert_eq!(participants(&names(8), NAMED_PARTICIPANTS), "P1, P2, P3, P4, P5, +3");
        assert_eq!(participants(&[], NAMED_PARTICIPANTS), "");
        // A sixth person is not named in the toast, so it can't approve: the dialog names everyone.
        let mut group = preview("a1", Kind::Send);
        group.text = Some("ok".into());
        group.participants = names(6);
        let prompt = Prompt::new(&group);
        assert_eq!(prompt.toast().context, "Ann (teams:ann) - Project\nWith P1, P2, P3, P4, P5, +1");
        assert_eq!(prompt.toast().buttons, SHOW_ALL);
        assert!(prompt.dialog().unwrap().contains("P1, P2, P3, P4, P5, P6\n"));
        let mut channel = preview("a1", Kind::Send);
        channel.participants.clear();
        assert_eq!(Prompt::new(&channel).toast().context, "Ann (teams:ann) - Project");
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

        // The toast on screen goes when its action does, however that happened.
        assert!(!queue.shown_is_gone(std::slice::from_ref(&d)));
        assert!(queue.shown_is_gone(&[a.clone(), b.clone()]));
        assert!(queue.shown_is_gone(&[]));
        assert!(!Queue::default().shown_is_gone(&[]));
    }
}
