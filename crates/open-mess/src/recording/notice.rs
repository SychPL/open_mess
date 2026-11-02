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
                toast = toast.add_button("Zatrzymaj", &format!("stop:{id}")).add_button("Zatrzymaj i usuń", &format!("delete:{id}"));
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
        let what = if microphone { "(dźwięk urządzenia rozmowy i mikrofon)" } else { "(tylko dźwięk urządzenia rozmowy)" };
        self.show("Nagrywam rozmowę Teams".into(), [format!("{what} na tym komputerze."), "Poinformuj uczestników.".into()], Some(id.to_owned()));
    }

    fn not_recording(&self, why: &str) {
        self.show("Nie nagrywam tej rozmowy".into(), [format!("Przyczyna: {why}."), String::new()], None);
    }

    fn track_lost(&self, microphone: bool) {
        let title = if microphone { "Nie nagrywam mikrofonu" } else { "Nie nagrywam rozmówców" };
        self.show(title.into(), ["Nie da się otworzyć urządzenia.".into(), "Nagranie trwa bez tej ścieżki.".into()], None);
    }

    fn agent_stopped(&self) {
        self.show("Agent zatrzymał nagrywanie".into(), ["Nagranie rozmowy zostało zatrzymane przez agenta.".into(), String::new()], None);
    }

    fn clear(&self) {
        let _ = self.jobs.send(Box::new(|| {
            let _ = ToastNotificationManager::History().and_then(|history| history.ClearWithId(&HSTRING::from(RECORDING_APP_ID)));
        }));
    }
}
