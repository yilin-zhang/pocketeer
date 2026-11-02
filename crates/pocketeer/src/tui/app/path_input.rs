//! Path lookup has its own worker so neither disk I/O nor queued network work
//! can block typing. A folder is read once (a few levels deep) and every
//! keystroke after that only filters what was read, in memory.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

pub struct Matches {
    pub generation: u64,
    pub value: String,
    /// `None` while the folder is still being read: keep showing the last list.
    pub suggestions: Option<Vec<Suggestion>>,
    pub completion: String,
    pub finished: bool,
}

pub struct PathLookup {
    tx: Sender<(u64, String)>,
    rx: Receiver<Matches>,
    generation: Arc<AtomicU64>,
    /// The value being looked up, so a folder read can stop as soon as the
    /// user types their way into another folder.
    latest: Arc<Mutex<String>>,
}
impl PathLookup {
    pub fn spawn() -> Self {
        let (tx, requests) = mpsc::channel::<(u64, String)>();
        let (results, rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let latest = Arc::new(Mutex::new(String::new()));
        let current = generation.clone();
        let wanted = latest.clone();
        thread::spawn(move || {
            let mut cache: Option<FolderIndex> = None;
            while let Ok(first) = requests.recv() {
                // Only the newest edit matters.
                let (generation, value) = requests.try_iter().last().unwrap_or(first);
                if current.load(Ordering::Relaxed) != generation {
                    continue;
                }
                let completion = complete_path(&value, || false);
                let (dir, _) = split(&value);
                let fresh = cache
                    .as_ref()
                    .is_some_and(|c| c.dir == dir && c.read.elapsed() < CACHE_FOR);
                if !fresh {
                    let interim = Matches {
                        generation,
                        value: value.clone(),
                        suggestions: None,
                        completion: completion.clone(),
                        finished: false,
                    };
                    if results.send(interim).is_err() {
                        return;
                    }
                    // Stop reading once the prompt has moved to another folder.
                    let moved = || wanted.lock().map(|v| split(&v).0 != dir).unwrap_or(true);
                    match FolderIndex::read(&dir, moved) {
                        Some(index) => cache = Some(index),
                        None => continue,
                    }
                }
                let index = cache.as_ref().expect("read above");
                let done = Matches {
                    generation,
                    suggestions: Some(index.suggestions(&value)),
                    value,
                    completion,
                    finished: true,
                };
                if results.send(done).is_err() {
                    return;
                }
            }
        });
        Self {
            tx,
            rx,
            generation,
            latest,
        }
    }
    pub fn request(&self, value: String) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut latest) = self.latest.lock() {
            latest.clone_from(&value);
        }
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

/// A byte range in a suggestion's name.
type Span = (usize, usize);

/// How long a folder's listing is reused before it is read again.
const CACHE_FOR: Duration = Duration::from_secs(15);

/// `value` → (the folder it is in, what is typed after the last `/`).
fn split(value: &str) -> (String, String) {
    let expanded = expand_home(value);
    match expanded.rsplit_once('/') {
        Some((d, p)) => (
            if d.is_empty() { "/" } else { d }.to_string(),
            p.to_string(),
        ),
        None => (".".to_string(), expanded),
    }
}

/// Everything under one folder, a few levels deep and capped, read once
/// and then filtered on every keystroke without touching the disk.
struct FolderIndex {
    dir: String,
    read: std::time::Instant,
    entries: Vec<Entry>,
}

struct Entry {
    /// Relative to the folder.
    rel: String,
    lower: String,
    /// Byte offset of the file name in `rel`.
    name_at: usize,
    depth: usize,
    is_dir: bool,
    hidden: bool,
}

impl FolderIndex {
    const MAX_DEPTH: usize = 4;
    const MAX_VISITED: usize = 6000;

    /// `None` when `stop` said to give up part way.
    fn read(dir: &str, stop: impl Fn() -> bool) -> Option<Self> {
        let root = std::path::Path::new(dir);
        let mut entries = Vec::new();
        if root.is_dir() {
            let walker = walkdir::WalkDir::new(root)
                .min_depth(1)
                .max_depth(Self::MAX_DEPTH)
                .into_iter()
                .filter_entry(|e| {
                    let name = e.file_name().to_string_lossy();
                    // Hidden entries are listed at the top only, never entered.
                    !(name.starts_with('.') && e.depth() > 1)
                        && name != "node_modules"
                        && name != "target"
                });
            for (visited, entry) in walker.filter_map(|e| e.ok()).enumerate() {
                if visited % 64 == 0 && stop() {
                    return None;
                }
                if visited >= Self::MAX_VISITED {
                    break;
                }
                let is_dir = entry.file_type().is_dir();
                // Don't descend into a hidden folder at the top either.
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .into_owned();
                entries.push(Entry {
                    lower: rel.to_lowercase(),
                    name_at: rel.len() - name.len(),
                    hidden: name.starts_with('.'),
                    depth: entry.depth(),
                    is_dir,
                    rel,
                });
            }
        }
        Some(FolderIndex {
            dir: dir.to_string(),
            read: std::time::Instant::now(),
            entries,
        })
    }

    /// What a path prompt lists for `value`: with nothing typed after the
    /// last `/`, the folder's entries; otherwise everything under it whose
    /// path contains the typed text, best matches first: name starts with
    /// it, name contains it, path contains it. Hidden entries only show up
    /// once a `.` is typed.
    fn suggestions(&self, value: &str) -> Vec<Suggestion> {
        const MAX_RESULTS: usize = 60;
        let (_, partial) = split(value);
        let needle = partial.to_lowercase();
        let show_hidden = partial.starts_with('.');
        let mut ranked: Vec<(u8, &Entry, Option<Span>)> = Vec::new();
        for e in &self.entries {
            if (e.hidden && !show_hidden) || (needle.is_empty() && e.depth > 1) {
                continue;
            }
            // Offsets are only trusted when lowercasing kept the length.
            let same = e.lower.len() == e.rel.len();
            let span = |at: usize| same.then_some((at, at + needle.len()));
            let name = &e.lower[e.name_at..];
            let (rank, at) = if needle.is_empty() {
                (0, None)
            } else if name.starts_with(&needle) {
                (0, span(e.name_at))
            } else if let Some(i) = name.find(&needle) {
                (1, span(e.name_at + i))
            } else if let Some(i) = e.lower.find(&needle) {
                (2, span(i))
            } else {
                continue;
            };
            ranked.push((rank, e, at));
        }
        ranked.sort_by(|a, b| {
            (a.0, a.1.depth, !a.1.is_dir, &a.1.lower).cmp(&(
                b.0,
                b.1.depth,
                !b.1.is_dir,
                &b.1.lower,
            ))
        });
        ranked
            .into_iter()
            .take(MAX_RESULTS)
            .map(|(_, e, matched)| Suggestion {
                path: if self.dir == "/" {
                    format!("/{}", e.rel)
                } else {
                    format!("{}/{}", self.dir, e.rel)
                },
                name: e.rel.clone(),
                is_dir: e.is_dir,
                matched,
            })
            .collect()
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
    /// Where the typed text is in `name` (byte range), to highlight it.
    pub matched: Option<Span>,
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

/// The suggestions for `value`, reading its folder right away.
#[cfg(test)]
pub(super) fn path_suggestions(value: &str) -> Vec<Suggestion> {
    let (dir, _) = split(value);
    FolderIndex::read(&dir, || false)
        .map(|i| i.suggestions(value))
        .unwrap_or_default()
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
    use super::{complete_path, path_suggestions};

    #[test]
    fn suggestions_rank_names_first_and_mark_the_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Downloads/gba roms")).unwrap();
        std::fs::write(tmp.path().join("Downloads/gba roms/Metroid.gba"), "").unwrap();
        std::fs::write(tmp.path().join("GBA.txt"), "").unwrap();
        std::fs::write(tmp.path().join(".gba-hidden"), "").unwrap();
        let p = |s: &str| format!("{}/{s}", tmp.path().display());
        let found = path_suggestions(&p("gba"));
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        // Name starts with it, then name contains it; hidden only with a dot.
        assert_eq!(
            names,
            [
                "GBA.txt",
                "Downloads/gba roms",
                "Downloads/gba roms/Metroid.gba"
            ]
        );
        assert_eq!(found[0].matched, Some((0, 3)));
        assert_eq!(found[1].matched, Some((10, 13)));
        assert_eq!(&found[1].name[10..13], "gba");
        assert_eq!(found[2].matched, Some((27, 30)));
        let in_name = path_suggestions(&p("metroid"));
        assert_eq!(in_name[0].matched, Some((19, 26)));
        assert_eq!(path_suggestions(&p(".gba-")).len(), 1);
        // Nothing typed: the folder's own entries only.
        assert_eq!(path_suggestions(&p("")).len(), 2);
    }

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
