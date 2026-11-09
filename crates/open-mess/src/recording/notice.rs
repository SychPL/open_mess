//! The recording's toasts (spec 2026-10-28-local-transcription, 4.2), under their own
//! AppUserModelID so that taking the approvals' toasts away leaves them. They are shown from a
//! thread of their own, so the recorder never waits for Windows; a button acts on the recording
//! it names only, so a toast left in the action centre stops nothing else. The captions archive
//! shows its own toast with them (spec 2026-11-04-captions-archive, 4.7).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock, Weak};

use tauri_winrt_notification::Toast;
use windows::UI::Notifications::ToastNotificationManager;
use windows::core::HSTRING;

use super::captions::{self, Archiver};
use super::recorder::{By, Notices, Recorder};
use crate::approval::RECORDING_APP_ID;
use crate::i18n::{self, Lang};

type Job = Box<dyn FnOnce() + Send>;

pub struct Toasts {
    jobs: Sender<Job>,
    recorder: Arc<OnceLock<Weak<Recorder>>>,
    archiver: Arc<OnceLock<Weak<Archiver>>>,
    /// The toasts Windows refused, for `status`.
    failed: Arc<AtomicUsize>,
}

impl Toasts {
    pub fn new() -> std::io::Result<Toasts> {
        let (jobs, next) = mpsc::channel::<Job>();
        std::thread::Builder::new().name("recording toasts".into()).spawn(move || {
            for job in next {
                job();
            }
        })?;
        Ok(Toasts { jobs, recorder: Arc::default(), archiver: Arc::default(), failed: Arc::default() })
    }

    /// The recorder the buttons stop.
    pub fn attach(&self, recorder: &Arc<Recorder>) {
        let _ = self.recorder.set(Arc::downgrade(recorder));
    }

    /// The captions archive whose recordings the buttons stop.
    pub fn attach_archiver(&self, archiver: &Arc<Archiver>) {
        let _ = self.archiver.set(Arc::downgrade(archiver));
    }

    /// Shows a toast whose buttons, label and argument each, act on one recording.
    fn show(&self, title: String, lines: [String; 2], buttons: Vec<(&'static str, String)>) {
        let (recorder, archiver, failed) = (Arc::clone(&self.recorder), Arc::clone(&self.archiver), Arc::clone(&self.failed));
        let _ = self.jobs.send(Box::new(move || {
            let [first, second] = &lines;
            let mut toast = Toast::new(RECORDING_APP_ID).title(&title).text1(first).text2(second);
            for (label, argument) in &buttons {
                toast = toast.add_button(label, argument);
            }
            let toast = toast.on_activated(move |argument| {
                let Some((action, id)) = argument.as_deref().and_then(|argument| argument.split_once(':')) else { return Ok(()) };
                let recorder = recorder.get().and_then(Weak::upgrade);
                match (action, recorder) {
                    ("stop", Some(recorder)) => drop(recorder.stop(By::Owner, false, Some(id))),
                    ("delete", Some(recorder)) => drop(recorder.stop(By::Owner, true, Some(id))),
                    ("captions", _) => {
                        if let Some(archiver) = archiver.get().and_then(Weak::upgrade) {
                            archiver.stop_soon(id.to_owned());
                        }
                    }
                    _ => {}
                }
                Ok(())
            });
            if toast.show().is_err() {
                failed.fetch_add(1, Ordering::SeqCst);
            }
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
        let buttons =
            vec![(lang.pick("Zatrzymaj", "Stop"), format!("stop:{id}")), (lang.pick("Zatrzymaj i usuń", "Stop and delete"), format!("delete:{id}"))];
        self.show(lang.pick("Nagrywam rozmowę Teams", "Recording the Teams call").into(), lines, buttons);
    }

    fn not_recording(&self, why: &str) {
        let lang = i18n::current();
        let reason = match lang {
            Lang::Pl => format!("Przyczyna: {}.", reason(why, lang)),
            Lang::En => format!("Reason: {why}."),
        };
        self.show(lang.pick("Nie nagrywam tej rozmowy", "Not recording this call").into(), [reason, String::new()], Vec::new());
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
        self.show(title.into(), lines, Vec::new());
    }

    fn agent_stopped(&self) {
        let lang = i18n::current();
        let line = lang.pick("Nagranie rozmowy zostało zatrzymane przez agenta.", "An agent stopped the call's recording.");
        self.show(lang.pick("Agent zatrzymał nagrywanie", "An agent stopped recording").into(), [line.into(), String::new()], Vec::new());
    }

    fn clear(&self) {
        let _ = self.jobs.send(Box::new(|| {
            let _ = ToastNotificationManager::History().and_then(|history| history.ClearWithId(&HSTRING::from(RECORDING_APP_ID)));
        }));
    }
}

impl captions::Notices for Toasts {
    fn started(&self, id: &str) {
        let lang = i18n::current();
        let lines = [
            lang.pick("Tekst z imionami mówców zostaje na tym komputerze.", "The text with the speakers' names stays on this computer.").into(),
            lang.pick("Uczestników informujesz Ty.", "Telling the participants is up to you.").into(),
        ];
        let title = lang.pick("open_mess zapisuje napisy tej rozmowy", "open_mess keeps this call's captions");
        self.show(title.into(), lines, vec![(lang.pick("Zatrzymaj", "Stop"), format!("captions:{id}"))]);
    }

    fn failed(&self) -> usize {
        self.failed.load(Ordering::SeqCst)
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
