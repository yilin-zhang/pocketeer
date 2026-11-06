//! Covers are decoded, scaled and encoded for the terminal on their own
//! thread: that takes far longer than a frame (about 100 ms per cover in a
//! debug build), and the UI thread only ever draws what is ready. The
//! covers next to the selected game are prepared ahead, so moving through
//! the list usually finds them done. Each frame tells the thread which
//! covers it wants, in order; work for rows already scrolled past is
//! dropped instead of making the selected one wait behind it.
//!
//! Terminals that can show images get the picture itself. The others get
//! a dot-matrix picture: braille characters give 2×4 dots per cell, eight
//! times what half blocks give, and one tone of dots dithered on the dark
//! background reads like the Pocket's own screen instead of a mosaic.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::SystemTime,
};

use image::{DynamicImage, GrayImage, imageops::FilterType};
use ratatui::{
    layout::Size,
    style::{Color, Style},
    text::{Line, Span},
};
use ratatui_image::{
    Resize, ResizeEncodeRender,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};

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
        cover: Art,
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

/// A decoded cover, ready to draw at the size it was made for.
enum Art {
    /// For terminals that show images (kitty, iTerm2, sixel).
    Image(Box<StatefulProtocol>),
    /// For the others.
    Dots(Dots),
}

impl Art {
    fn new(picker: &Picker, image: DynamicImage, size: Size) -> Self {
        match picker.protocol_type() {
            ProtocolType::Halfblocks => Art::Dots(Dots::new(&image, size)),
            _ => Art::Image(Box::new(picker.new_resize_protocol(image))),
        }
    }

    fn needs_resize(&self, size: Size) -> bool {
        match self {
            Art::Image(p) => p.needs_resize(&RESIZE, size).is_some(),
            Art::Dots(d) => d.size != size,
        }
    }

    fn resize(&mut self, size: Size) {
        match self {
            Art::Image(p) => {
                if let Some(target) = p.needs_resize(&RESIZE, size) {
                    p.resize_encode(&RESIZE, target);
                }
            }
            Art::Dots(d) => d.resize(size),
        }
    }
}

enum Outcome {
    Ready(Art),
    Unreadable,
    /// Nobody wanted it any more by the time the thread got to it.
    Dropped,
}

enum State {
    Working,
    Unreadable,
    Ready(Art),
}

pub enum Cover<'a> {
    Loading,
    Unreadable,
    Image(&'a mut StatefulProtocol),
    /// The dot-matrix picture, one line per row, centered in the panel.
    Dots(&'a [Line<'static>]),
}

pub struct Covers {
    tx: Sender<Work>,
    rx: Receiver<Done>,
    cache: HashMap<PathBuf, (Stamp, State)>,
    /// What the panel needs now, most wanted first; shared with the thread.
    wanted: Arc<Mutex<Vec<PathBuf>>>,
}

impl Covers {
    /// Starts the thread; `wake` is called when a cover is ready.
    pub fn spawn(picker: Picker, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, work) = mpsc::channel::<Work>();
        let (done, rx) = mpsc::channel();
        let wanted = Arc::new(Mutex::new(Vec::new()));
        let shared = wanted.clone();
        thread::spawn(move || {
            let mut queue: Vec<Work> = Vec::new();
            loop {
                if queue.is_empty() {
                    let Ok(job) = work.recv() else {
                        return;
                    };
                    queue.push(job);
                }
                queue.extend(work.try_iter());
                let wanted = shared.lock().map(|w| w.clone()).unwrap_or_default();
                let next = wanted
                    .iter()
                    .find_map(|p| queue.iter().position(|job| job.path() == p));
                let results: Vec<Done> = match next {
                    Some(i) => vec![do_work(&picker, queue.swap_remove(i))],
                    // Everything queued is for rows the user has left.
                    None => queue.drain(..).map(drop_work).collect(),
                };
                for result in results {
                    if done.send(result).is_err() {
                        return;
                    }
                    wake();
                }
            }
        });
        Self {
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
            if let Some(stamp) = file_stamp(p) {
                self.request_unless_cached(p, stamp, size);
            }
        }
        self.cover_at(path, size)
    }

    /// Takes in whatever the thread finished.
    pub fn collect_finished(&mut self) {
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
    fn cover_at(&mut self, path: &Path, size: Size) -> Cover<'_> {
        let Some(stamp) = file_stamp(path) else {
            return Cover::Unreadable;
        };
        if !self.request_unless_cached(path, stamp, size) {
            return Cover::Loading;
        }
        let Some((_, state)) = self.cache.get_mut(path) else {
            return Cover::Loading;
        };
        if let State::Ready(cover) = state
            && cover.needs_resize(size)
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
            State::Ready(Art::Image(p)) => Cover::Image(p),
            State::Ready(Art::Dots(d)) => Cover::Dots(&d.lines),
        }
    }

    /// Sends the cover to the thread unless the cached copy is for this
    /// version of the file. `false` when it was sent.
    fn request_unless_cached(&mut self, path: &Path, stamp: Stamp, size: Size) -> bool {
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

fn file_stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
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

fn do_work(picker: &Picker, work: Work) -> Done {
    let (path, stamp, cover, size) = match work {
        Work::Decode { path, stamp, size } => {
            let cover = image::ImageReader::open(&path)
                .ok()
                .and_then(|r| r.with_guessed_format().ok())
                .and_then(|r| r.decode().ok())
                .map(|img| Art::new(picker, img, size));
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
        cover.resize(size);
        cover
    });
    Done {
        path,
        stamp,
        outcome: cover.map_or(Outcome::Unreadable, Outcome::Ready),
    }
}

// ---------------------------------------------------------------------------
// The dot-matrix picture

/// A cover drawn in colored braille dots, kept with its picture so it can
/// be drawn again when the panel changes size.
struct Dots {
    source: DynamicImage,
    /// The size the lines were drawn for, in cells.
    size: Size,
    lines: Vec<Line<'static>>,
}

impl Dots {
    fn new(image: &DynamicImage, size: Size) -> Self {
        let source = DynamicImage::ImageRgb8(image.to_rgb8());
        let lines = render_dots(&source, size);
        Self {
            source,
            size,
            lines,
        }
    }

    fn resize(&mut self, size: Size) {
        self.size = size;
        self.lines = render_dots(&self.source, size);
    }
}

/// Dots per cell, across and down.
const DOTS_X: u32 = 2;
const DOTS_Y: u32 = 4;

/// The braille bit for the dot at (x, y) in a cell.
const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// Fits `source` into `size` cells (dots are about square, since a cell is
/// twice as tall as it is wide), dithers its brightness and encodes it as
/// braille. Which dots are lit carries the brightness; each cell is drawn
/// in the average color of its lit dots, so the color carries the hue.
fn render_dots(source: &DynamicImage, size: Size) -> Vec<Line<'static>> {
    let (cols, rows) = (u32::from(size.width), u32::from(size.height));
    if cols == 0 || rows == 0 || source.width() == 0 || source.height() == 0 {
        return Vec::new();
    }
    let small = source.resize(cols * DOTS_X, rows * DOTS_Y, FilterType::Triangle);
    let on = dither(&small.to_luma8());
    let small = small.to_rgb8();
    let (w, h) = small.dimensions();

    // Center the picture in whole cells.
    let used_cols = w.div_ceil(DOTS_X);
    let used_rows = h.div_ceil(DOTS_Y);
    let left = ((cols - used_cols) / 2) as usize;
    let top = (rows - used_rows) / 2;
    let mut lines = vec![Line::default(); top as usize];
    for row in 0..used_rows {
        let mut spans = vec![Span::raw(" ".repeat(left))];
        for col in 0..used_cols {
            let mut bits = 0u8;
            let mut sum = [0u32; 3];
            let mut lit = 0u32;
            for (dx, column) in BITS.iter().enumerate() {
                for (dy, bit) in column.iter().enumerate() {
                    let (x, y) = (col * DOTS_X + dx as u32, row * DOTS_Y + dy as u32);
                    if x < w && y < h && on[(y * w + x) as usize] {
                        bits |= bit;
                        let p = small.get_pixel(x, y).0;
                        for (s, c) in sum.iter_mut().zip(p) {
                            *s += u32::from(c);
                        }
                        lit += 1;
                    }
                }
            }
            // An empty cell is a space: some fonts draw U+2800 as a box.
            if bits == 0 {
                spans.push(Span::raw(" "));
                continue;
            }
            let dot = char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ');
            let [r, g, b] = sum.map(|s| s / lit);
            spans.push(Span::styled(
                dot.to_string(),
                Style::new().fg(lifted_color(r, g, b)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// A dot's color, lifted so a dark color still shows on the dark
/// background: a lit dot means "light here", and density already says how
/// light.
fn lifted_color(r: u32, g: u32, b: u32) -> Color {
    const FLOOR: u32 = 130;
    let max = r.max(g).max(b).max(1);
    let lift = |c: u32| (c * max.max(FLOOR) / max).min(255) as u8;
    Color::Rgb(lift(r), lift(g), lift(b))
}

/// Which dots are lit: the contrast is stretched to the picture's own
/// range, then Atkinson dithering (the classic Mac look) spreads three
/// quarters of each error to six neighbors, which keeps highlights and
/// shadows clean instead of speckled.
fn dither(image: &GrayImage) -> Vec<bool> {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let mut px: Vec<f32> = image.pixels().map(|p| f32::from(p.0[0])).collect();
    // Stretch between the 2nd and 98th percentile, so one stray pixel
    // doesn't flatten the rest. A nearly flat picture is left as it is:
    // stretching nothing would turn it all black or all white.
    let mut sorted = px.clone();
    sorted.sort_by(f32::total_cmp);
    let low = sorted[sorted.len() / 50];
    let high = sorted[sorted.len() - 1 - sorted.len() / 50];
    if high - low >= 32.0 {
        for p in &mut px {
            *p = ((*p - low) / (high - low) * 255.0).clamp(0.0, 255.0);
        }
    }
    let mut on = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let lit = px[i] >= 128.0;
            on[i] = lit;
            let error = (px[i] - if lit { 255.0 } else { 0.0 }) / 8.0;
            for (dx, dy) in [(1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)] {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx >= 0 && (nx as usize) < w && (ny as usize) < h {
                    px[ny as usize * w + nx as usize] += error;
                }
            }
        }
    }
    on
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;
    use std::time::{Duration, Instant};

    fn wait_ready(covers: &mut Covers, path: &Path, size: Size) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            covers.collect_finished();
            match covers.show(path, &[], size) {
                Cover::Image(_) | Cover::Dots(_) => return true,
                Cover::Unreadable => return false,
                Cover::Loading => thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("cover never finished");
    }

    #[test]
    fn loads_cover_on_background_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cover.png");
        image::RgbImage::new(8, 8).save(&path).unwrap();
        let mut covers = Covers::spawn(Picker::halfblocks(), || {});
        let size = Size::new(10, 5);
        // The first look only asks for it.
        assert!(matches!(covers.show(&path, &[], size), Cover::Loading));
        assert!(wait_ready(&mut covers, &path, size));
    }

    #[test]
    fn reloads_cover_when_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cover.png");
        image::RgbImage::new(8, 8).save(&path).unwrap();
        let mut covers = Covers::spawn(Picker::halfblocks(), || {});
        let size = Size::new(10, 5);
        assert!(wait_ready(&mut covers, &path, size));
        image::RgbImage::new(16, 16).save(&path).unwrap();
        assert!(matches!(covers.show(&path, &[], size), Cover::Loading));
        assert!(wait_ready(&mut covers, &path, size));
    }

    #[test]
    fn reports_unreadable_cover_without_retrying() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cover.png");
        std::fs::write(&path, b"not an image").unwrap();
        let mut covers = Covers::spawn(Picker::halfblocks(), || {});
        let size = Size::new(10, 5);
        assert!(!wait_ready(&mut covers, &path, size));
        assert!(matches!(covers.show(&path, &[], size), Cover::Unreadable));
    }

    #[test]
    fn loads_cover_scrolled_past_when_user_returns() {
        let tmp = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..6)
            .map(|i| {
                let p = tmp.path().join(format!("{i}.png"));
                image::RgbImage::new(64, 64).save(&p).unwrap();
                p
            })
            .collect();
        let mut covers = Covers::spawn(Picker::halfblocks(), || {});
        let size = Size::new(10, 5);
        // Scroll through all of them without waiting; most get dropped.
        for p in &paths {
            covers.show(p, &[], size);
        }
        assert!(wait_ready(&mut covers, &paths[5], size));
        assert!(wait_ready(&mut covers, &paths[0], size));
    }

    // -- the dot-matrix picture --

    fn picture(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> DynamicImage {
        DynamicImage::ImageLuma8(GrayImage::from_fn(w, h, |x, y| Luma([f(x, y)])))
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn fits_tall_cover_centered_in_whole_cells() {
        // White left half, black right half; 1:2, so 10×5 cells take the
        // full height (20 dots) and 10 dots (5 cells) across.
        let dots = Dots::new(
            &picture(50, 100, |x, _| if x < 25 { 255 } else { 0 }),
            Size::new(10, 5),
        );
        let lines: Vec<String> = dots.lines.iter().map(text).collect();
        assert_eq!(lines.len(), 5);
        for line in &lines {
            assert_eq!(line.chars().count(), 2 + 5);
            assert!(line.starts_with("  "));
        }
        // Lit on the left, dark on the right.
        assert_eq!(lines[2].chars().nth(2), Some('⣿'));
        assert_eq!(lines[2].chars().nth(6), Some(' '));
    }

    #[test]
    fn dithers_mid_gray_to_about_half_the_dots() {
        // A gray ramp: after stretching, the middle is mid-gray.
        let on = dither(&picture(64, 64, |x, _| (x * 4) as u8).to_luma8());
        let middle: Vec<bool> = (0..64)
            .flat_map(|y| (28..36).map(move |x| (x, y)))
            .map(|(x, y)| on[y * 64 + x])
            .collect();
        let lit = middle.iter().filter(|b| **b).count() as f32 / middle.len() as f32;
        assert!((0.35..0.65).contains(&lit), "{lit}");
    }

    #[test]
    fn resizes_dots_from_kept_picture() {
        let dots = Dots::new(&picture(40, 40, |_, _| 200), Size::new(8, 4));
        let art = Art::Dots(dots);
        assert!(!art.needs_resize(Size::new(8, 4)));
        assert!(art.needs_resize(Size::new(20, 10)));
        let Art::Dots(mut dots) = art else {
            unreachable!()
        };
        dots.resize(Size::new(20, 10));
        assert_eq!(dots.lines.len(), 10);
    }
}
