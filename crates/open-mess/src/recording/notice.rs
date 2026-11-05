//! The recording's toasts (spec 2026-10-28-local-transcription, 4.2), under their own
//! AppUserModelID so that taking the approvals' toasts away leaves them. They are shown from a
//! thread of their own, so the recorder never waits for Windows; a button acts on the recording
//! it names only, so a toast left in the action centre stops nothing else.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock, Weak};

use tauri_winrt_notification::Toast;
use windows::UI::Notifications::ToastNotificationManager;
use windows::core::HSTRING;

use super::recorder::{By, Notices, Recorder};
use crate::approval::RECORDING_APP_ID;
use crate::i18n::{self, Lang};

type Job = Box<dyn FnOnce() + Send>;

pub struct Toasts {
    jobs: Sender<Job>,
    recorder: Arc<OnceLock<Weak<Recorder>>>,
}

impl Toasts {
    pub fn new() -> std::io::Result<Toasts> {
        let (jobs, next) = mpsc::channel::<Job>();
        std::thread::Builder::new().name("recording toasts".into()).spawn(move || {
            for job in next {
                job();
            }
        })?;
        Ok(Toasts { jobs, recorder: Arc::default() })
    }

    /// The recorder the buttons stop.
    pub fn attach(&self, recorder: &Arc<Recorder>) {
        let _ = self.recorder.set(Arc::downgrade(recorder));
    }

    fn show(&self, title: String, lines: [String; 2], buttons: Option<String>) {
        let recorder = Arc::clone(&self.recorder);
        // ponytail: a toast Windows refuses is not shown; the tray and `status` still tell.
        let _ = self.jobs.send(Box::new(move || {
            let [first, second] = &lines;
            let mut toast = Toast::new(RECORDING_APP_ID).title(&title).text1(first).text2(second);
            if let Some(id) = buttons {
                let lang = i18n::current();
                let (stop, delete) = (lang.pick("Zatrzymaj", "Stop"), lang.pick("Zatrzymaj i usuń", "Stop and delete"));
                toast = toast.add_button(stop, &format!("stop:{id}")).add_button(delete, &format!("delete:{id}"));
            }
            let toast = toast.on_activated(move |argument| {
                let recorder = recorder.get().and_then(Weak::upgrade);
                if let (Some(recorder), Some((action, id))) = (recorder, argument.as_deref().and_then(|argument| argument.split_once(':'))) {
                    match action {
                        "stop" => drop(recorder.stop(By::Owner, false, Some(id))),
                        "delete" => drop(recorder.stop(By::Owner, true, Some(id))),
                        _ => {}
                    }
                }
                Ok(())
            });
            let _ = toast.show();
        }));
    }
}

impl Notices for Toasts {
    fn started(&self, id: &str, microphone: bool) {
        let lang = i18n::current();
        let what = match (lang, microphone) {
            (Lang::Pl, true) => "(dźwięk urządzenia rozmowy i mikrofon) na tym komputerze.",
            (Lang::Pl, false) => "(tylko dźwięk urządzenia rozmowy) na tym komputerze.",
            (Lang::En, true) => "(the call's device and the microphone) on this computer.",
            (Lang::En, false) => "(only the call's device) on this computer.",
        };
        let lines = [what.into(), lang.pick("Poinformuj uczestników.", "Tell the participants.").into()];
        self.show(lang.pick("Nagrywam rozmowę Teams", "Recording the Teams call").into(), lines, Some(id.to_owned()));
    }

    fn not_recording(&self, why: &str) {
        let lang = i18n::current();
        let reason = match lang {
            Lang::Pl => format!("Przyczyna: {}.", reason(why, lang)),
            Lang::En => format!("Reason: {why}."),
        };
        self.show(lang.pick("Nie nagrywam tej rozmowy", "Not recording this call").into(), [reason, String::new()], None);
    }

    fn track_lost(&self, microphone: bool) {
        let lang = i18n::current();
        let title = match (lang, microphone) {
            (Lang::Pl, true) => "Nie nagrywam mikrofonu",
            (Lang::Pl, false) => "Nie nagrywam rozmówców",
            (Lang::En, true) => "Not recording the microphone",
            (Lang::En, false) => "Not recording the other side",
        };
        let lines = [
            lang.pick("Nie da się otworzyć urządzenia.", "The device cannot be opened.").into(),
            lang.pick("Nagranie trwa bez tej ścieżki.", "The recording goes on without this track.").into(),
        ];
        self.show(title.into(), lines, None);
    }

    fn agent_stopped(&self) {
        let lang = i18n::current();
        let line = lang.pick("Nagranie rozmowy zostało zatrzymane przez agenta.", "An agent stopped the call's recording.");
        self.show(lang.pick("Agent zatrzymał nagrywanie", "An agent stopped recording").into(), [line.into(), String::new()], None);
    }

    fn clear(&self) {
        let _ = self.jobs.send(Box::new(|| {
            let _ = ToastNotificationManager::History().and_then(|history| history.ClearWithId(&HSTRING::from(RECORDING_APP_ID)));
        }));
    }
}

/// Why a call is not recorded (the recorder's reasons, English as `status` gives them), in `lang`.
fn reason(why: &str, lang: Lang) -> &str {
    match (lang, why) {
        (Lang::Pl, "cannot open the call's devices") => "nie da się otworzyć urządzeń rozmowy",
        (Lang::Pl, "no tray icon") => "brak ikony w zasobniku",
        (Lang::Pl, "no model") => "brak modelu",
        (Lang::Pl, "low disk") => "mało miejsca na dysku",
        (Lang::Pl, "audio limit") => "limit dźwięku",
        (Lang::Pl, "text limit") => "limit transkrypcji",
        (Lang::Pl, "update in progress") => "trwa aktualizacja",
        (Lang::Pl, "cannot create the recording") => "błąd tworzenia nagrania",
        _ => why,
    }
}
