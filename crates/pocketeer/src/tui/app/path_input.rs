//! Path lookup has its own worker so neither disk I/O nor queued network work
//! can block typing. Each edit cancels obsolete work and restarts a short debounce.
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

pub struct PathLookup {
    tx: Sender<(u64, String)>,
    rx: Receiver<Matches>,
    generation: Arc<AtomicU64>,
}
impl PathLookup {
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
impl Drop for PathLookup {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

/// One entry a path prompt offers.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// Full path, as the prompt's value becomes when accepted.
    pub path: String,
    /// Shown in the list.
    pub name: String,
    pub is_dir: bool,
}
pub(super) fn start_path() -> String {
    dirs::home_dir()
        .map(|home| format!("{}/", home.display()))
        .unwrap_or_default()
}

/// `~/x` → `/Users/me/x`.
pub(super) fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest).display().to_string())
            .unwrap_or_else(|| path.to_string()),
        None if path == "~" => dirs::home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_else(|| path.to_string()),
        None => path.to_string(),
    }
}

/// What a path prompt lists for `value`: with nothing typed after the last
/// `/`, the folder's entries; otherwise everything under that folder (a
/// few levels deep, capped) whose path contains the typed text, best
/// matches first: name starts with it, name contains it, path contains it.
/// Hidden entries only show up once a `.` is typed.
pub(super) fn path_suggestions(value: &str, cancelled: impl Fn() -> bool) -> Vec<Suggestion> {
    const MAX_DEPTH: usize = 4;
    const MAX_VISITED: usize = 6000;
    const MAX_RESULTS: usize = 60;

    let expanded = expand_home(value);
    let (dir, partial) = match expanded.rsplit_once('/') {
        Some((d, p)) => (if d.is_empty() { "/" } else { d }, p),
        None => (".", expanded.as_str()),
    };
    let root = std::path::Path::new(dir);
    if !root.is_dir() {
        return Vec::new();
    }
    let needle = partial.to_lowercase();
    let show_hidden = partial.starts_with('.');
    let depth = if needle.is_empty() { 1 } else { MAX_DEPTH };
    let mut ranked: Vec<(u8, usize, String, Suggestion)> = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .max_depth(depth)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Never descend into hidden folders; a hidden name only matches
            // at the top level when the user typed the dot.
            !(name.starts_with('.') && !(show_hidden && e.depth() == 1))
                && name != "node_modules"
                && name != "target"
        });
    for (visited, entry) in walker.filter_map(|e| e.ok()).enumerate() {
        if cancelled() || visited >= MAX_VISITED {
            break;
        }
        let is_dir = entry.file_type().is_dir();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let rank = if needle.is_empty() || name.starts_with(&needle) {
            0
        } else if name.contains(&needle) {
            1
        } else if rel.to_lowercase().contains(&needle) {
            2
        } else {
            continue;
        };
        let path = if dir == "/" {
            format!("/{rel}")
        } else {
            format!("{dir}/{rel}")
        };
        ranked.push((
            rank,
            entry.depth(),
            rel.to_lowercase(),
            Suggestion {
                path,
                name: rel,
                is_dir,
            },
        ));
    }
    ranked.sort_by(|a, b| (a.0, a.1, !a.3.is_dir, &a.2).cmp(&(b.0, b.1, !b.3.is_dir, &b.2)));
    ranked
        .into_iter()
        .map(|(_, _, _, s)| s)
        .take(MAX_RESULTS)
        .collect()
}

/// Shell-style Tab completion: extends `value` to the longest common prefix
/// of the entries it matches; a lone directory match gets a trailing `/`.
pub(super) fn complete_path(value: &str, cancelled: impl Fn() -> bool) -> String {
    let expanded = expand_home(value);
    let (dir, partial) = match expanded.rsplit_once('/') {
        Some((d, p)) => (if d.is_empty() { "/" } else { d }, p),
        None => (".", expanded.as_str()),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return value.to_string();
    };
    let mut names: Vec<String> = entries
        .take_while(|_| !cancelled())
        .filter_map(|e| e.ok())
        .map(|e| {
            let mut n = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                n.push('/');
            }
            n
        })
        .filter(|n| n.starts_with(partial) && (partial.starts_with('.') || !n.starts_with('.')))
        .collect();
    names.sort();
    let Some(first) = names.first() else {
        return value.to_string();
    };
    let mut common = first.clone();
    for n in &names[1..] {
        let len = common
            .chars()
            .zip(n.chars())
            .take_while(|(a, b)| a == b)
            .count();
        common = common.chars().take(len).collect();
    }
    if common.len() <= partial.len() {
        return value.to_string();
    }
    let base = if dir == "/" {
        String::new()
    } else {
        dir.to_string()
    };
    match expanded.rsplit_once('/') {
        Some(_) => format!("{base}/{common}"),
        None => common,
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::complete_path;

    #[test]
    fn completes_unique_prefixes_and_marks_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("roms-gba")).unwrap();
        std::fs::create_dir(tmp.path().join("roms-gbc")).unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "x").unwrap();
        let p = |s: &str| format!("{}/{s}", tmp.path().display());
        assert_eq!(complete_path(&p("n"), || false), p("notes.txt"));
        assert_eq!(complete_path(&p("r"), || false), p("roms-gb"));
        assert_eq!(complete_path(&p("roms-gba"), || false), p("roms-gba/"));
        assert_eq!(complete_path(&p("zzz"), || false), p("zzz"));
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
                ..Default::default()
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
        let worker = PathLookup::spawn();
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
        let obsolete = PathLookup {
            tx: worker.tx.clone(),
            rx,
            generation: worker.generation.clone(),
        };
        obsolete.cancel();
        assert!(obsolete.latest().is_none());
    }
}
