//! Path lookup has its own worker so neither disk I/O nor queued network work
//! can block typing. Each edit cancels obsolete work and restarts a short debounce.
use super::{
    Suggestion,
    dialogs::{complete_path, path_suggestions},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

pub struct Matches {
    pub generation: u64,
    pub value: String,
    pub suggestions: Vec<Suggestion>,
    pub completion: String,
    pub finished: bool,
}

pub struct PathWorker {
    tx: Sender<(u64, String)>,
    rx: Receiver<Matches>,
    generation: Arc<AtomicU64>,
}
impl PathWorker {
    pub fn spawn() -> Self {
        let (tx, requests) = mpsc::channel::<(u64, String)>();
        let (results, rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        thread::spawn(move || {
            while let Ok(mut request) = requests.recv() {
                loop {
                    match requests.recv_timeout(Duration::from_millis(80)) {
                        Ok(newer) => request = newer,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                let (generation, value) = request;
                let cancelled = || current.load(Ordering::Relaxed) != generation;
                if cancelled() {
                    continue;
                }
                let completion = complete_path(&value, cancelled);
                if cancelled() {
                    continue;
                }
                if results
                    .send(Matches {
                        generation,
                        value: value.clone(),
                        suggestions: Vec::new(),
                        completion: completion.clone(),
                        finished: false,
                    })
                    .is_err()
                {
                    return;
                }
                let suggestions = path_suggestions(&value, cancelled);
                if cancelled() {
                    continue;
                }
                if !cancelled()
                    && results
                        .send(Matches {
                            generation,
                            value,
                            suggestions,
                            completion,
                            finished: true,
                        })
                        .is_err()
                {
                    return;
                }
            }
        });
        Self { tx, rx, generation }
    }
    pub fn request(&self, value: String) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send((generation, value));
    }
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
    pub fn latest(&self) -> Option<Matches> {
        self.rx
            .try_iter()
            .filter(|r| r.generation == self.generation.load(Ordering::Relaxed))
            .last()
    }
}
impl Drop for PathWorker {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tab_pressed_before_results_arrive_is_applied_when_ready() {
        use super::super::{App, InputKind, Modal};
        use crate::cli::Global;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("roms")).unwrap();
        let mut app = App::new(
            Global {
                json: false,
                root: Some(tmp.path().join("no-card")),
                offline: true,
                refresh: false,
                cache_dir: Some(tmp.path().join("cache")),
                library: Some(tmp.path().join("lib")),
                config: Some(tmp.path().join("config")),
                github_token: None,
            },
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.input(
            "Import",
            vec![],
            format!("{}/ro", tmp.path().display()),
            InputKind::ImportDir,
        );
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let expected = format!("{}/roms/", tmp.path().display());
        loop {
            app.pump();
            if matches!(&app.modal, Some(Modal::Input { value, .. }) if value == &expected) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Tab was lost while lookup was pending"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // All candidates remain selectable even beyond the visible viewport.
        if let Some(Modal::Input {
            suggestions,
            highlight,
            ..
        }) = &mut app.modal
        {
            *suggestions = (0..20)
                .map(|i| Suggestion {
                    path: format!("/image{i}.png"),
                    name: format!("image{i}.png"),
                    is_dir: false,
                })
                .collect();
            *highlight = None;
        }
        for _ in 0..15 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(&app.modal, Some(Modal::Input { value, .. }) if value == "/image14.png"));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.modal.is_none());
    }

    #[test]
    fn rapid_edits_return_only_the_latest_query() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("alpha.png"), "").unwrap();
        std::fs::write(tmp.path().join("beta.webp"), "").unwrap();
        let worker = PathWorker::spawn();
        worker.request(format!("{}/a", tmp.path().display()));
        let query = format!("{}/b", tmp.path().display());
        worker.request(query.clone());
        let completion = worker.rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!completion.finished);
        assert!(completion.completion.ends_with("beta.webp"));
        let result = worker.rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(result.finished);
        assert_eq!(result.value, query);
        assert_eq!(result.suggestions.len(), 1);
        assert_eq!(result.suggestions[0].name, "beta.webp");
        assert!(result.completion.ends_with("beta.webp"));
        let (tx, rx) = mpsc::channel();
        tx.send(result).unwrap();
        let obsolete = PathWorker {
            tx: worker.tx.clone(),
            rx,
            generation: worker.generation.clone(),
        };
        obsolete.cancel();
        assert!(obsolete.latest().is_none());
    }
}
