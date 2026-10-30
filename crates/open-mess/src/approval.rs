//! Approval of waiting writes in Windows notifications (spec 8.6). One notification at a time: a
//! single waiting action gets its own toast; while several wait, one toast says how many and only
//! opens the configuration window, whose list shows each of them, so a burst of writes never piles
//! up toasts to approve blindly and none waits unannounced. Nothing is approved that was not on
//! screen in full: an action's toast offers `Approve` only when every participant is named and all
//! of it fits the lines Windows shows, otherwise only `Show all`, which opens a dialog with
//! everything. No toast or dialog decides more than one action, and only the user's click reaches
//! [`Pending::decide`]: no tool does. Other platforms leave the actions waiting until they expire.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;

use crate::i18n::{Lang, plural_pl};
use crate::pending::{Kind, Pending, Preview};
use crate::ui::Opener;
use crate::update::release::Version;
use crate::update::service::UpdateService;

/// The AppUserModelID the toasts show under, registered for the current user at daemon start.
pub const APP_ID: &str = "open_mess.daemon";
/// The AppUserModelID of the notice that an update is ready: its own, so that taking the approvals'
/// toasts away (`ClearWithId(APP_ID)`) leaves it on screen. tauri-winrt-notification gives a toast a
/// tag only with a progress bar, and a group never.
pub const UPDATE_APP_ID: &str = "open_mess.update";
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

/// The buttons of a toast that can approve: label and argument; the arguments are what is matched.
fn approve(lang: Lang) -> &'static [(&'static str, &'static str)] {
    match lang {
        Lang::Pl => &[("Zatwierdź", "approve"), ("Odrzuć", "reject")],
        Lang::En => &[("Approve", "approve"), ("Reject", "reject")],
    }
}

/// The button of a toast that can't.
fn show_all(lang: Lang) -> &'static [(&'static str, &'static str)] {
    match lang {
        Lang::Pl => &[("Pokaż całość", "show")],
        Lang::En => &[("Show all", "show")],
    }
}

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
    /// The language of the toast and the dialog.
    pub lang: Lang,
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
    pub fn new(preview: &Preview, lang: Lang) -> Prompt {
        let title = match (preview.kind, lang) {
            (Kind::Send, Lang::Pl) => "open_mess: zatwierdź wysłanie",
            (Kind::Edit, Lang::Pl) => "open_mess: zatwierdź edycję",
            (Kind::React, Lang::Pl) => "open_mess: zatwierdź reakcję",
            (Kind::Send, Lang::En) => "open_mess: approve send",
            (Kind::Edit, Lang::En) => "open_mess: approve edit",
            (Kind::React, Lang::En) => "open_mess: approve reaction",
        };
        let conversation = format!("{} - {}", clean(&preview.account), clean(&preview.conversation_title));
        let participants = preview.participants.iter().map(|name| clean(name)).collect();
        let field = |text: &Option<String>| clean(text.as_deref().unwrap_or_default());
        let (text, target, reaction) = (field(&preview.text), field(&preview.target_text), field(&preview.reaction));
        let content = match (preview.kind, &preview.target_text, lang) {
            (Kind::Send, None, _) => text,
            (Kind::Send, Some(_), Lang::Pl) => format!("{text}\n\nW odpowiedzi na: {target}"),
            (Kind::Send, Some(_), Lang::En) => format!("{text}\n\nIn reply to: {target}"),
            (Kind::Edit, _, Lang::Pl) => format!("Przed: {target}\nPo: {text}"),
            (Kind::Edit, _, Lang::En) => format!("Before: {target}\nAfter: {text}"),
            (Kind::React, _, Lang::Pl) => format!("{reaction} pod: {target}"),
            (Kind::React, _, Lang::En) => format!("{reaction} on: {target}"),
        };
        Prompt { title: title.into(), conversation, participants, content, lang }
    }

    /// The conversation and the first `named` participants.
    fn context(&self, named: usize) -> String {
        let mut context = self.conversation.clone();
        if !self.participants.is_empty() {
            context.push_str(self.lang.pick("\nZ: ", "\nWith "));
            context.push_str(&participants(&self.participants, named));
        }
        context
    }

    /// The content with `Approve` and `Reject` when all of it fits the toast, else only `Show all`.
    pub fn toast(&self) -> ToastText {
        let context = self.context(NAMED_PARTICIPANTS);
        let fits = lines(&context, TOAST_WIDTH) + lines(&self.content, TOAST_WIDTH) <= TOAST_LINES;
        if fits && self.participants.len() <= NAMED_PARTICIPANTS {
            ToastText { context, line: self.content.clone(), buttons: approve(self.lang) }
        } else {
            let line = self.lang.pick("Otwórz, żeby przeczytać całość przed decyzją.", "Open it to read all of it before you decide.");
            ToastText { context, line: line.into(), buttons: show_all(self.lang) }
        }
    }

    /// Everything, for the dialog: every participant and the whole content; None when that is too
    /// long to be read there in full.
    pub fn dialog(&self) -> Option<String> {
        let text = format!("{}\n\n{}", self.context(usize::MAX), self.content);
        (lines(&text, DIALOG_WIDTH) <= DIALOG_LINES).then_some(text)
    }

    /// What the dialog says under [`Prompt::dialog`]. Windows labels its Yes and No buttons in its
    /// own language, which a language picked in the window may not be.
    pub fn question(&self) -> &'static str {
        self.lang.pick("Zatwierdzić? Tak zatwierdza, Nie odrzuca.", "Approve this? Yes approves it, No rejects it.")
    }

    /// What the dialog says when the content is too long for it.
    pub fn too_long(&self) -> &'static str {
        self.lang.pick(
            "To jest za długie, żeby pokazać całość tutaj, więc tu nie można tego zatwierdzić.\n\nOdrzucić? Tak odrzuca, Nie zostawia do wygaśnięcia.",
            "This is too long to show here in full, so it can't be approved here.\n\nReject it? Yes rejects it, No leaves it waiting until it expires.",
        )
    }
}

/// The title of the notice that `count` actions (2 or more) wait.
pub fn several_title(count: usize, lang: Lang) -> String {
    match lang {
        Lang::Pl => format!("open_mess: {count} {} na zatwierdzenie", plural_pl(count, "akcja czeka", "akcje czekają", "akcji czeka")),
        Lang::En => format!("open_mess: {count} actions wait for your approval"),
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

/// Actions that join the notice of several within this time of it are merged into it; ones that
/// come later bring it back, with the count then, so a later burst is announced too.
pub const MERGE: Duration = Duration::from_secs(60);

/// What a notification shows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shown {
    /// One action, by id.
    Action(String),
    /// That several wait: those announced, and when.
    Several { announced: Vec<String>, at: Instant },
}

/// What is to change on screen.
#[derive(Debug, PartialEq, Eq)]
pub struct Change<'a> {
    /// Take the notification on screen away.
    pub withdraw: bool,
    pub show: Option<Notice<'a>>,
}

#[derive(PartialEq, Eq)]
pub enum Notice<'a> {
    Action(&'a Preview),
    /// This many actions wait.
    Several(usize),
}

// Hand-written: a preview carries message text, which never reaches a log.
impl std::fmt::Debug for Notice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Notice::Action(preview) => f.debug_tuple("Action").field(&preview.action).finish(),
            Notice::Several(count) => f.debug_tuple("Several").field(count).finish(),
        }
    }
}

/// Which notification is on screen.
#[derive(Default)]
pub struct Queue {
    shown: Option<Shown>,
}

impl Queue {
    /// What changes on screen for these waiting actions at `now`: one action's toast for a single
    /// one, one notice for several, none for none. Nothing changes while the same one still holds;
    /// actions that join a notice within [`MERGE`] are merged into it, later ones show it again.
    pub fn update<'a>(&mut self, waiting: &'a [Preview], now: Instant) -> Change<'a> {
        let unchanged = Change { withdraw: false, show: None };
        let (shown, show) = match waiting {
            [] if self.shown.is_none() => return unchanged,
            [] => (None, None),
            [one] if self.shown.as_ref().is_some_and(|shown| *shown == Shown::Action(one.action.clone())) => return unchanged,
            [one] => (Some(Shown::Action(one.action.clone())), Some(Notice::Action(one))),
            several => {
                let ids = || several.iter().map(|preview| preview.action.clone()).collect();
                if let Some(Shown::Several { announced, at }) = &mut self.shown {
                    if several.iter().all(|preview| announced.contains(&preview.action)) {
                        return unchanged;
                    }
                    if now.saturating_duration_since(*at) < MERGE {
                        *announced = ids();
                        return unchanged;
                    }
                }
                (Some(Shown::Several { announced: ids(), at: now }), Some(Notice::Several(several.len())))
            }
        };
        let withdraw = self.shown.is_some();
        self.shown = shown;
        Change { withdraw, show }
    }
}

/// Hands what the waiting actions need on screen to `present` (one action) and `several` (how many
/// wait), for as long as `pending` lives, and calls `withdraw` first when what is on screen no
/// longer holds, so its toast goes away.
pub async fn run(pending: Arc<Pending>, present: impl Fn(&Preview), several: impl Fn(usize), withdraw: impl Fn()) {
    let mut changed = pending.subscribe();
    let mut queue = Queue::default();
    loop {
        let waiting = pending.waiting();
        let change = queue.update(&waiting, Instant::now());
        if change.withdraw {
            withdraw();
        }
        match change.show {
            Some(Notice::Action(preview)) => present(preview),
            Some(Notice::Several(count)) => several(count),
            None => {}
        }
        if changed.changed().await.is_err() {
            return;
        }
    }
}

/// Set once [`start`] registered the AppUserModelIDs: only that process writes their names again.
#[cfg(windows)]
static REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Registers the toasts' AppUserModelID and shows the waiting actions for the daemon's lifetime;
/// the notice of several opens the configuration window through `window`.
#[cfg(windows)]
pub fn start(runtime: &Runtime, pending: Arc<Pending>, window: Opener) {
    REGISTERED.store(true, std::sync::atomic::Ordering::Relaxed);
    // ponytail: without the registration Windows may show no toast; reported once a log sink exists.
    let _ = toast::register(crate::i18n::current());
    // Toasts are shown and taken away one after the other on one thread (showing waits for
    // Windows), so taking one away never takes the next one with it.
    let (jobs, queue) = std::sync::mpsc::channel::<toast::Job>();
    // ponytail: without the thread no toast shows (the window still lists the actions); reported
    // once a log sink exists.
    let _ = std::thread::Builder::new().name("open-mess-toasts".into()).spawn(move || queue.into_iter().for_each(|job| job()));
    let (handle, shown, several, withdrawn) = (runtime.handle().clone(), Arc::clone(&pending), jobs.clone(), jobs.clone());
    let open_on = runtime.handle().clone();
    runtime.spawn(run(
        pending,
        move |preview| drop(jobs.send(toast::show(&shown, &handle, Prompt::new(preview, crate::i18n::current()), preview.action.clone()))),
        move |count| drop(several.send(toast::several(count, window.clone(), open_on.clone()))),
        move || drop(withdrawn.send(Box::new(toast::withdraw))),
    ));
}

/// No notifications here: actions wait until they expire.
#[cfg(not(windows))]
pub fn start(_runtime: &Runtime, _pending: Arc<Pending>, _window: Opener) {}

/// The notifications' names in `lang`, once the language changed: written again only by the
/// process that registered them at its start, so the tests never write to the registry.
#[cfg(windows)]
pub fn rename(lang: crate::i18n::Lang) {
    if REGISTERED.load(std::sync::atomic::Ordering::Relaxed) {
        // ponytail: a name that fails to change keeps the old language until the next change.
        let _ = toast::register(lang);
    }
}

/// "open_mess <wersja> jest gotowy" with Zainstaluj and Później (spec 2026-10-27-self-update, 6);
/// Zainstaluj is the user's consent, as in the window.
#[cfg(windows)]
pub fn update_ready(service: &UpdateService, version: &Version, manifest_sha256: &str) {
    let (service, version, manifest_sha256) = (service.clone(), version.to_string(), manifest_sha256.to_owned());
    // Showing waits for Windows: not on the runtime. ponytail: without the thread no toast shows; the
    // window still offers the release.
    let _ = std::thread::Builder::new().name("open-mess-update-toast".into()).spawn(move || toast::update(service, &version, manifest_sha256));
}

/// No notifications here; there is no installation either.
#[cfg(not(windows))]
pub fn update_ready(_service: &UpdateService, _version: &Version, _manifest_sha256: &str) {}

#[cfg(windows)]
mod toast {
    use std::sync::Arc;

    use tauri_winrt_notification::{Scenario, Toast};
    use tokio::runtime::Handle;
    use windows::UI::Notifications::ToastNotificationManager;
    use windows::core::HSTRING;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    use super::{APP_ID, Prompt, UPDATE_APP_ID, several_title};
    use crate::i18n::{self, Lang};
    use crate::pending::Pending;
    use crate::ui::Opener;
    use crate::update::service::UpdateService;

    /// `HKCU\Software\Classes\AppUserModelId\open_mess.daemon` and `...\open_mess.update` with the
    /// names toasts and the notification settings show.
    pub(super) fn register(lang: Lang) -> std::io::Result<()> {
        for (id, name) in [(APP_ID, "open_mess"), (UPDATE_APP_ID, lang.pick("open_mess - aktualizacje", "open_mess - updates"))] {
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(format!(r"Software\Classes\AppUserModelId\{id}"))?;
            key.set_value("DisplayName", &name)?;
        }
        Ok(())
    }

    /// Shows that `version` is ready, in place of an earlier notice of a release. Zainstaluj installs
    /// that release, the one with `manifest_sha256`, and nothing else.
    pub(super) fn update(service: UpdateService, version: &str, manifest_sha256: String) {
        let _ = ToastNotificationManager::History().and_then(|history| history.ClearWithId(&HSTRING::from(UPDATE_APP_ID)));
        let lang = i18n::current();
        let title = match lang {
            Lang::Pl => format!("open_mess {version} jest gotowy"),
            Lang::En => format!("open_mess {version} is ready"),
        };
        let toast = Toast::new(UPDATE_APP_ID)
            .title(&title)
            .add_button(lang.pick("Zainstaluj", "Install"), "install")
            .add_button(lang.pick("Później", "Later"), "later")
            .on_activated(move |argument| {
                if argument.as_deref() == Some("install") {
                    // ponytail: a refusal (a check or an installation running, a new version being
                    // checked, another release ready by now) is shown by the window only.
                    let _ = service.install_announced(&manifest_sha256);
                }
                Ok(())
            });
        // ponytail: a toast Windows refuses leaves the release to the window; reported once a log
        // sink exists.
        let _ = toast.show();
    }

    /// Work for the toast thread.
    pub(super) type Job = Box<dyn FnOnce() + Send>;

    /// Shows the toast of one action; it stays on screen until the user acts on it.
    pub(super) fn show(pending: &Arc<Pending>, runtime: &Handle, prompt: Prompt, action: String) -> Job {
        let (pending, decide_on) = (Arc::clone(pending), runtime.clone());
        // The outcome is for `pending` to report.
        let decide = move |approve: bool| {
            let (pending, action) = (Arc::clone(&pending), action.clone());
            decide_on.spawn(async move { pending.decide(&action, approve).await });
        };
        Box::new(move || {
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
        })
    }

    /// Shows that `count` actions wait; it only opens the configuration window, where each is shown
    /// in full and decided on its own.
    pub(super) fn several(count: usize, window: Opener, runtime: Handle) -> Job {
        let lang = i18n::current();
        Box::new(move || {
            let toast = Toast::new(APP_ID)
                .title(&several_title(count, lang))
                .text1(lang.pick("Otwórz listę, żeby przeczytać każdą przed decyzją.", "Open the list to read each one before you decide."))
                .scenario(Scenario::Reminder)
                .add_button(lang.pick("Otwórz listę", "Open the list"), "open")
                .on_activated(move |_| {
                    let window = window.clone();
                    // ponytail: a window that fails to open says nothing here; reported once a log
                    // sink exists (the tray opens it too).
                    runtime.spawn(async move { window.open_waiting().await });
                    Ok(())
                });
            // ponytail: a toast Windows refuses leaves the actions to the window's list; reported
            // once a log sink exists.
            let _ = toast.show();
        })
    }

    /// Takes open_mess's toasts off the screen and out of the notification center. One
    /// notification is on screen at a time, so this is the one that no longer holds.
    pub(super) fn withdraw() {
        // ponytail: a toast Windows keeps stays until the user dismisses it (clicking it decides
        // nothing then); reported once a log sink exists.
        let _ = ToastNotificationManager::History().and_then(|history| history.ClearWithId(&HSTRING::from(APP_ID)));
    }

    /// Everything in a modal dialog: Yes approves, No rejects, closing decides nothing. Content too
    /// long to show in full can only be rejected. (Buttons named Approve and Reject would need
    /// common controls 6 and an application manifest.)
    fn ask(prompt: &Prompt) -> Option<bool> {
        let dialog = rfd::MessageDialog::new().set_level(rfd::MessageLevel::Warning).set_title(&prompt.title);
        let Some(text) = prompt.dialog() else {
            let answer = dialog.set_description(prompt.too_long()).set_buttons(rfd::MessageButtons::YesNo).show();
            return (answer == rfd::MessageDialogResult::Yes).then_some(false);
        };
        let answer = dialog.set_description(format!("{text}\n\n{}", prompt.question())).set_buttons(rfd::MessageButtons::YesNo).show();
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
            target_cut: false,
        }
    }

    #[test]
    fn toast_text_is_truncated_with_show_all() {
        let mut send = preview("a1", Kind::Send);
        let prompt = Prompt::new(&send, Lang::En);
        assert_eq!(prompt.title, "open_mess: approve send");
        let toast = prompt.toast();
        assert_eq!(toast.context, "Ann (teams:ann) - Project\nWith Pat, Sam");
        assert_eq!((toast.line.as_str(), toast.buttons), ("on my way", approve(Lang::En)));

        // The context takes three of the four lines Windows shows: one line of content fits.
        send.text = Some("x".repeat(TOAST_WIDTH));
        assert_eq!(Prompt::new(&send, Lang::En).toast().buttons, approve(Lang::En));
        // One character more and the toast shows none of it, only the way to all of it.
        send.text = Some("x".repeat(TOAST_WIDTH + 1));
        let prompt = Prompt::new(&send, Lang::En);
        let toast = prompt.toast();
        assert!(!toast.line.contains('x'));
        assert_eq!(toast.buttons, show_all(Lang::En));
        assert_eq!(prompt.content, "x".repeat(TOAST_WIDTH + 1));
        // Short lines count as lines, wide characters as two.
        send.text = Some("a\nb".into());
        assert_eq!(Prompt::new(&send, Lang::En).toast().buttons, show_all(Lang::En));
        send.text = Some("\u{4f60}".repeat(TOAST_WIDTH / 2 + 1));
        assert_eq!(Prompt::new(&send, Lang::En).toast().buttons, show_all(Lang::En));
        // A reply's target counts too.
        send.text = Some("ok".into());
        send.target_text = Some("fine".into());
        assert_eq!(Prompt::new(&send, Lang::En).toast().buttons, show_all(Lang::En));

        // The dialog shows everything, up to what fits a screen.
        send.target_text = None;
        send.text = Some("x".repeat(DIALOG_WIDTH * 20));
        assert!(Prompt::new(&send, Lang::En).dialog().unwrap().ends_with(&"x".repeat(DIALOG_WIDTH * 20)));
        send.text = Some("x".repeat(DIALOG_WIDTH * DIALOG_LINES));
        assert_eq!(Prompt::new(&send, Lang::En).dialog(), None);

        let mut edit = preview("a2", Kind::Edit);
        edit.target_text = Some("on my wya".into());
        assert_eq!(Prompt::new(&edit, Lang::En).content, "Before: on my wya\nAfter: on my way");
        let mut react = preview("a3", Kind::React);
        react.reaction = Some("like".into());
        react.target_text = Some("lunch?".into());
        assert_eq!(Prompt::new(&react, Lang::En).title, "open_mess: approve reaction");
        assert_eq!(Prompt::new(&react, Lang::En).content, "like on: lunch?");

        // A name or text can't hide or reorder what the toast says.
        send.target_text = None;
        send.text = Some("pay \u{202e}gpj.exe\u{7} now\nplease".into());
        assert_eq!(Prompt::new(&send, Lang::En).content, "pay gpj.exe now\nplease");
    }

    #[test]
    fn prompts_in_both_languages() {
        let mut send = preview("a1", Kind::Send);
        send.target_text = Some("lunch?".into());
        let mut edit = preview("a2", Kind::Edit);
        edit.target_text = Some("on my wya".into());
        let mut react = preview("a3", Kind::React);
        react.reaction = Some("like".into());
        react.target_text = Some("lunch?".into());
        for (preview, lang, title, content) in [
            (&send, Lang::Pl, "open_mess: zatwierdź wysłanie", "on my way\n\nW odpowiedzi na: lunch?"),
            (&send, Lang::En, "open_mess: approve send", "on my way\n\nIn reply to: lunch?"),
            (&edit, Lang::Pl, "open_mess: zatwierdź edycję", "Przed: on my wya\nPo: on my way"),
            (&edit, Lang::En, "open_mess: approve edit", "Before: on my wya\nAfter: on my way"),
            (&react, Lang::Pl, "open_mess: zatwierdź reakcję", "like pod: lunch?"),
            (&react, Lang::En, "open_mess: approve reaction", "like on: lunch?"),
        ] {
            let prompt = Prompt::new(preview, lang);
            assert_eq!((prompt.title.as_str(), prompt.content.as_str()), (title, content));
        }
        // The toast and the dialog speak the prompt's language; the arguments stay.
        send.target_text = None;
        let toast = Prompt::new(&send, Lang::Pl).toast();
        assert_eq!(toast.context, "Ann (teams:ann) - Project\nZ: Pat, Sam");
        assert_eq!(toast.buttons, [("Zatwierdź", "approve"), ("Odrzuć", "reject")]);
        send.text = Some("x".repeat(TOAST_WIDTH * 2));
        let prompt = Prompt::new(&send, Lang::Pl);
        assert_eq!(prompt.toast().line, "Otwórz, żeby przeczytać całość przed decyzją.");
        assert_eq!(prompt.toast().buttons, [("Pokaż całość", "show")]);
        assert!(prompt.question().contains("Tak zatwierdza, Nie odrzuca"));
        assert!(prompt.too_long().contains("Tak odrzuca"));
        let prompt = Prompt::new(&send, Lang::En);
        assert_eq!(prompt.toast().line, "Open it to read all of it before you decide.");
        assert!(prompt.question().contains("Yes approves it, No rejects it"));
        assert!(prompt.too_long().contains("Yes rejects it"));
    }

    #[test]
    fn notice_of_several_counts_in_both_languages() {
        assert_eq!(several_title(2, Lang::Pl), "open_mess: 2 akcje czekają na zatwierdzenie");
        assert_eq!(several_title(5, Lang::Pl), "open_mess: 5 akcji czeka na zatwierdzenie");
        assert_eq!(several_title(22, Lang::Pl), "open_mess: 22 akcje czekają na zatwierdzenie");
        assert_eq!(several_title(12, Lang::Pl), "open_mess: 12 akcji czeka na zatwierdzenie");
        assert_eq!(several_title(2, Lang::En), "open_mess: 2 actions wait for your approval");
        assert_eq!(several_title(12, Lang::En), "open_mess: 12 actions wait for your approval");
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
        let prompt = Prompt::new(&group, Lang::En);
        assert_eq!(prompt.toast().context, "Ann (teams:ann) - Project\nWith P1, P2, P3, P4, P5, +1");
        assert_eq!(prompt.toast().buttons, show_all(Lang::En));
        assert!(prompt.dialog().unwrap().contains("P1, P2, P3, P4, P5, P6\n"));
        let mut channel = preview("a1", Kind::Send);
        channel.participants.clear();
        assert_eq!(Prompt::new(&channel, Lang::En).toast().context, "Ann (teams:ann) - Project");
    }

    #[test]
    fn one_notification_at_a_time() {
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|id| preview(id, Kind::Send));
        let mut queue = Queue::default();
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let nothing = Change { withdraw: false, show: None };
        assert_eq!(queue.update(&[], at(0)), nothing);
        // One waits: its own toast.
        assert_eq!(queue.update(std::slice::from_ref(&a), at(0)), Change { withdraw: false, show: Some(Notice::Action(&a)) });
        assert_eq!(queue.update(std::slice::from_ref(&a), at(0)), nothing);
        // Another joins: one notice instead, not shown again for each one more soon after.
        let two = [a.clone(), b.clone()];
        assert_eq!(queue.update(&two, at(1)), Change { withdraw: true, show: Some(Notice::Several(2)) });
        assert_eq!(queue.update(&[a.clone(), b.clone(), c.clone()], at(30)), nothing);
        // Decided in the window, nothing new: nothing changes, however late.
        assert_eq!(queue.update(&[b.clone(), c.clone()], at(600)), nothing);
        // A new one long after the notice: shown again, with the count now.
        assert_eq!(queue.update(&[b.clone(), c.clone(), d.clone()], at(601)), Change { withdraw: true, show: Some(Notice::Several(3)) });
        // Down to one, whichever: its own toast again; none left: nothing on screen.
        assert_eq!(queue.update(std::slice::from_ref(&c), at(602)), Change { withdraw: true, show: Some(Notice::Action(&c)) });
        assert_eq!(queue.update(std::slice::from_ref(&b), at(603)), Change { withdraw: true, show: Some(Notice::Action(&b)) });
        assert_eq!(queue.update(&[], at(604)), Change { withdraw: true, show: None });
        assert_eq!(queue.update(&[], at(605)), nothing);
        // Several at once from nothing: the notice alone.
        assert_eq!(queue.update(&two, at(606)), Change { withdraw: false, show: Some(Notice::Several(2)) });
    }
}
