//! Covers are decoded, scaled and encoded for the terminal on their own
//! thread: that takes far longer than a frame (about 100 ms per cover in a
//! debug build), and the UI thread only ever draws what is ready. The
//! covers next to the selected game are prepared ahead, so moving through
//! the list usually finds them done. Each frame tells the thread which
//! covers it wants, in order; work for rows already scrolled past is
//! dropped instead of making the selected one wait behind it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::SystemTime,
};

use ratatui::layout::Size;
use ratatui_image::{Resize, ResizeEncodeRender, picker::Picker, protocol::StatefulProtocol};

/// How the cover panel fits an image; the widget must be drawn with the same.
pub const RESIZE: Resize = Resize::Fit(None);

/// How many decoded covers to keep. One is a few hundred KB to a few MB.
const KEEP: usize = 24;

/// When a cover file was last written, and its size. A cover fetched or
/// imported again replaces the file in place, so the path alone can't tell.
type Stamp = (Option<SystemTime>, u64);

enum Work {
    /// Read the file and encode it for `size` cells.
    Decode {
        path: PathBuf,
        stamp: Stamp,
        size: Size,
    },
    /// The panel changed size: encode an already decoded cover again.
    Resize {
        path: PathBuf,
        stamp: Stamp,
        cover: Box<StatefulProtocol>,
        size: Size,
    },
}

impl Work {
    fn path(&self) -> &Path {
        match self {
            Work::Decode { path, .. } | Work::Resize { path, .. } => path,
        }
    }
}

struct Done {
    path: PathBuf,
    stamp: Stamp,
    outcome: Outcome,
}

enum Outcome {
    Ready(Box<StatefulProtocol>),
    Unreadable,
    /// Nobody wanted it any more by the time the thread got to it.
    Dropped,
}

enum State {
    Working,
    Unreadable,
    Ready(Box<StatefulProtocol>),
}

pub enum Cover<'a> {
    Loading,
    Unreadable,
    Ready(&'a mut StatefulProtocol),
}

pub struct Covers {
    tx: Sender<Work>,
    rx: Receiver<Done>,
    cache: HashMap<PathBuf, (Stamp, State)>,
    /// What the panel needs now, most wanted first; shared with the thread.
    wanted: Arc<Mutex<Vec<PathBuf>>>,
}

impl Covers {
    pub fn spawn(picker: Picker) -> Self {
        let (tx, work) = mpsc::channel::<Work>();
        let (done, rx) = mpsc::channel();
        let wanted = Arc::new(Mutex::new(Vec::new()));
        let shared = wanted.clone();
        thread::spawn(move || {
            let mut queue: Vec<Work> = Vec::new();
            loop {
                if queue.is_empty() {
                    match work.recv() {
                        Ok(job) => queue.push(job),
                        Err(_) => return,
                    }
                }
                queue.extend(work.try_iter());
                let wanted = shared.lock().map(|w| w.clone()).unwrap_or_default();
                let next = wanted
                    .iter()
                    .find_map(|p| queue.iter().position(|job| job.path() == p));
                let results: Vec<Done> = match next {
                    Some(i) => vec![run(&picker, queue.swap_remove(i))],
                    // Everything queued is for rows the user has left.
                    None => queue.drain(..).map(drop_work).collect(),
                };
                for result in results {
                    if done.send(result).is_err() {
                        return;
                    }
                }
            }
        });
        Covers {
            tx,
            rx,
            cache: HashMap::new(),
            wanted,
        }
    }

    /// The cover at `path` for a panel of `size` cells, if it is ready; the
    /// ones in `next` (the rows around it) are started ahead.
    pub fn show(&mut self, path: &Path, next: &[PathBuf], size: Size) -> Cover<'_> {
        if let Ok(mut wanted) = self.wanted.lock()
            && (wanted.first().map(PathBuf::as_path) != Some(path) || wanted[1..] != *next)
        {
            *wanted = std::iter::once(path.to_path_buf())
                .chain(next.iter().cloned())
                .collect();
        }
        // The selected cover goes to the thread first, so it never waits
        // behind the ones around it.
        for p in std::iter::once(path).chain(next.iter().map(PathBuf::as_path)) {
            if let Some(stamp) = stamp(p) {
                self.request(p, stamp, size);
            }
        }
        self.get(path, size)
    }

    /// Takes in whatever the thread finished.
    pub fn receive(&mut self) {
        for Done {
            path,
            stamp,
            outcome,
        } in self.rx.try_iter()
        {
            // A cover rewritten meanwhile has been asked for again; this
            // result is for the old file.
            let Some((s, state @ State::Working)) = self.cache.get_mut(&path) else {
                continue;
            };
            if *s != stamp {
                continue;
            }
            match outcome {
                Outcome::Ready(cover) => *state = State::Ready(cover),
                Outcome::Unreadable => *state = State::Unreadable,
                // Asked for again if the user comes back to it.
                Outcome::Dropped => {
                    self.cache.remove(&path);
                }
            }
        }
    }

    /// The cover at `path`, encoded for `size` cells, if it is ready. When it
    /// isn't, the thread is asked for it and the panel says it's loading.
    fn get(&mut self, path: &Path, size: Size) -> Cover<'_> {
        let Some(stamp) = stamp(path) else {
            return Cover::Unreadable;
        };
        if !self.request(path, stamp, size) {
            return Cover::Loading;
        }
        let Some((_, state)) = self.cache.get_mut(path) else {
            return Cover::Loading;
        };
        if let State::Ready(cover) = state
            && cover.needs_resize(&RESIZE, size).is_some()
        {
            let State::Ready(cover) = std::mem::replace(state, State::Working) else {
                unreachable!()
            };
            let _ = self.tx.send(Work::Resize {
                path: path.to_path_buf(),
                stamp,
                cover,
                size,
            });
            return Cover::Loading;
        }
        match state {
            State::Working => Cover::Loading,
            State::Unreadable => Cover::Unreadable,
            State::Ready(cover) => Cover::Ready(cover),
        }
    }

    /// Sends the cover to the thread unless the cached copy is for this
    /// version of the file. `false` when it was sent.
    fn request(&mut self, path: &Path, stamp: Stamp, size: Size) -> bool {
        if self.cache.get(path).is_some_and(|(s, _)| *s == stamp) {
            return true;
        }
        if self.cache.len() >= KEEP {
            // Covers being worked on are what's on screen or next to it.
            self.cache
                .retain(|_, (_, state)| matches!(state, State::Working));
        }
        self.cache
            .insert(path.to_path_buf(), (stamp, State::Working));
        let _ = self.tx.send(Work::Decode {
            path: path.to_path_buf(),
            stamp,
            size,
        });
        false
    }
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

/// Gives back unwanted work without doing it. A cover that only needed a
/// new size is still decoded, so it is kept as it is.
fn drop_work(work: Work) -> Done {
    match work {
        Work::Decode { path, stamp, .. } => Done {
            path,
            stamp,
            outcome: Outcome::Dropped,
        },
        Work::Resize {
            path, stamp, cover, ..
        } => Done {
            path,
            stamp,
            outcome: Outcome::Ready(cover),
        },
    }
}

fn run(picker: &Picker, work: Work) -> Done {
    let (path, stamp, cover, size) = match work {
        Work::Decode { path, stamp, size } => {
            let cover = image::ImageReader::open(&path)
                .ok()
                .and_then(|r| r.with_guessed_format().ok())
                .and_then(|r| r.decode().ok())
                .map(|img| Box::new(picker.new_resize_protocol(img)));
            (path, stamp, cover, size)
        }
        Work::Resize {
            path,
            stamp,
            cover,
            size,
        } => (path, stamp, Some(cover), size),
    };
    let cover = cover.map(|mut cover| {
        if let Some(target) = cover.needs_resize(&RESIZE, size) {
            cover.resize_encode(&RESIZE, target);
        }
        cover
    });
    Done {
        path,
        stamp,
        outcome: cover.map_or(Outcome::Unreadable, Outcome::Ready),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_ready(covers: &mut Covers, path: &Path, size: Size) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            covers.receive();
            match covers.show(path, &[], size) {
                Cover::Ready(_) => return true,
                Cover::Unreadable => return false,
                Cover::Loading => thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("cover never finished");
    }

    #[test]
    fn covers_load_off_thread_and_reload_when_the_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cover.png");
        image::RgbImage::new(8, 8).save(&path).unwrap();
        let mut covers = Covers::spawn(Picker::halfblocks());
        let size = Size::new(10, 5);

        // The first look only asks for it.
        assert!(matches!(covers.show(&path, &[], size), Cover::Loading));
        assert!(wait_ready(&mut covers, &path, size));

        // A new file at the same path is decoded again.
        image::RgbImage::new(16, 16).save(&path).unwrap();
        assert!(matches!(covers.show(&path, &[], size), Cover::Loading));
        assert!(wait_ready(&mut covers, &path, size));

        // Garbage is reported, not retried every frame.
        std::fs::write(&path, b"not an image").unwrap();
        assert!(!wait_ready(&mut covers, &path, size));
        assert!(matches!(covers.show(&path, &[], size), Cover::Unreadable));
    }

    #[test]
    fn a_cover_scrolled_past_still_loads_when_the_user_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..6)
            .map(|i| {
                let p = tmp.path().join(format!("{i}.png"));
                image::RgbImage::new(64, 64).save(&p).unwrap();
                p
            })
            .collect();
        let mut covers = Covers::spawn(Picker::halfblocks());
        let size = Size::new(10, 5);
        // Scroll through all of them without waiting; most get dropped.
        for p in &paths {
            covers.show(p, &[], size);
        }
        assert!(wait_ready(&mut covers, &paths[5], size));
        assert!(wait_ready(&mut covers, &paths[0], size));
    }
}
