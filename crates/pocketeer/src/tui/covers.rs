//! Prepares covers for the panel on a background thread, as images or as
//! dot-matrix pictures.

use std::{
    collections::{HashMap, hash_map::Entry},
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Instant, SystemTime},
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

use super::Event;

/// A cover file's last write time and size, or None if there is no file to
/// read. A cover fetched or imported again replaces the file in place, so the
/// path alone can't identify it.
type CoverFileStamp = Option<(Option<SystemTime>, u64)>;

/// Which cover, in which version of its file.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CoverKey {
    path: PathBuf,
    stamp: CoverFileStamp,
}

impl CoverKey {
    /// The key for the cover at `path`, in the version of the file there now.
    fn from_path(path: &Path) -> Self {
        let stamp = fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.modified().ok(), metadata.len()));
        Self {
            path: path.to_path_buf(),
            stamp,
        }
    }
}

enum CoverJob {
    /// Reads the file and encodes it for `size` cells.
    Decode { key: CoverKey, size: Size },
    /// The panel changed size, so an already decoded cover is encoded again.
    Resize {
        key: CoverKey,
        cover: DecodedCover,
        size: Size,
    },
}

impl CoverJob {
    fn key(&self) -> &CoverKey {
        match self {
            CoverJob::Decode { key, .. } | CoverJob::Resize { key, .. } => key,
        }
    }

    fn run(self, picker: &Picker) -> CoverJobReport {
        let (key, cover) = match self {
            CoverJob::Decode { key, size } => {
                let cover = image::ImageReader::open(&key.path)
                    .ok()
                    .and_then(|reader| reader.with_guessed_format().ok())
                    .and_then(|reader| reader.decode().ok())
                    .map(|image| {
                        let mut cover = DecodedCover::new(picker, image, size);
                        cover.resize(size);
                        cover
                    });
                (key, cover)
            }
            CoverJob::Resize {
                key,
                mut cover,
                size,
            } => {
                cover.resize(size);
                (key, Some(cover))
            }
        };
        CoverJobReport {
            key,
            cover: cover.map_or(LoadedCover::Unreadable, LoadedCover::Ready),
        }
    }
}

struct CoverJobReport {
    key: CoverKey,
    cover: LoadedCover,
}

/// A decoded cover, ready to draw at the size it was made for.
pub enum DecodedCover {
    /// For terminals that show images (kitty, iTerm2, sixel).
    Image(Box<StatefulProtocol>),
    /// For terminals that can't show images, drawn in braille dots.
    Dots(BrailleCover),
}

impl DecodedCover {
    fn new(picker: &Picker, image: DynamicImage, size: Size) -> Self {
        match picker.protocol_type() {
            ProtocolType::Halfblocks => DecodedCover::Dots(BrailleCover::new(&image, size)),
            _ => DecodedCover::Image(Box::new(picker.new_resize_protocol(image))),
        }
    }

    fn needs_resize(&self, size: Size) -> bool {
        match self {
            DecodedCover::Image(protocol) => protocol
                .needs_resize(&CoverLoader::RESIZE_MODE, size)
                .is_some(),
            DecodedCover::Dots(dots) => dots.size != size,
        }
    }

    fn resize(&mut self, size: Size) {
        match self {
            DecodedCover::Image(protocol) => {
                if let Some(target_area) = protocol.needs_resize(&CoverLoader::RESIZE_MODE, size) {
                    protocol.resize_encode(&CoverLoader::RESIZE_MODE, target_area);
                }
            }
            DecodedCover::Dots(dots) => dots.resize(size),
        }
    }
}

/// What the thread made of a cover file.
pub enum LoadedCover {
    Ready(DecodedCover),
    Unreadable,
}

struct CachedCover {
    cover: LoadedCover,
    /// When it was last asked for.
    last_shown: Instant,
}

/// The loaded covers. It keeps a limited number, and drops those shown
/// longest ago first.
struct CoverCache {
    entries: HashMap<CoverKey, CachedCover>,
}

impl CoverCache {
    /// How many loaded covers to keep. A decoded one is a few hundred KB to
    /// a few MB.
    const LIMIT: usize = 24;

    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn contains(&self, key: &CoverKey) -> bool {
        self.entries.contains_key(key)
    }

    /// The cover for `key`, marked as shown at `now`.
    fn get(&mut self, key: &CoverKey, now: Instant) -> Option<&mut LoadedCover> {
        let cached_cover = self.entries.get_mut(key)?;
        cached_cover.last_shown = now;
        Some(&mut cached_cover.cover)
    }

    /// Adds a cover last asked for at `last_shown`, and drops the one shown
    /// longest ago if there are more than the limit.
    fn insert(&mut self, key: CoverKey, cover: LoadedCover, last_shown: Instant) {
        self.entries.insert(key, CachedCover { cover, last_shown });
        if self.entries.len() > Self::LIMIT
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, cached_cover)| cached_cover.last_shown)
                .map(|(cached_key, _)| cached_key.clone())
        {
            self.entries.remove(&oldest_key);
        }
    }

    /// Takes out the cover for `key` if it is ready but made for another size,
    /// to be encoded again.
    fn take_to_resize(&mut self, key: &CoverKey, size: Size) -> Option<DecodedCover> {
        let needs_resize = matches!(
            self.entries.get(key),
            Some(CachedCover { cover: LoadedCover::Ready(cover), .. }) if cover.needs_resize(size)
        );
        if !needs_resize {
            return None;
        }
        match self.entries.remove(key)?.cover {
            LoadedCover::Ready(cover) => Some(cover),
            LoadedCover::Unreadable => None,
        }
    }
}

/// A job not sent to the thread yet.
struct WaitingJob {
    job: CoverJob,
    /// When its cover was last asked for.
    last_shown: Instant,
}

/// The jobs not sent to the thread yet, and the one it is on. The thread runs
/// one at a time, and the cover asked for most recently goes next.
struct CoverJobQueue {
    waiting_jobs: HashMap<CoverKey, WaitingJob>,
    /// The cover the thread is on, and when it was last asked for.
    job_in_flight: Option<(CoverKey, Instant)>,
}

impl CoverJobQueue {
    fn new() -> Self {
        Self {
            waiting_jobs: HashMap::new(),
            job_in_flight: None,
        }
    }

    /// Adds `job`, asked for at `now`, or marks the one already there for its
    /// cover as asked for then. The one there may be a resize, which holds the
    /// decoded cover.
    ///
    /// No more jobs wait than the cache holds, so the one asked for longest
    /// ago is dropped past that. Its cover would come back older than all
    /// those in the cache and be dropped from it at once.
    fn add(&mut self, job: CoverJob, now: Instant) {
        if let Some((key_in_flight, last_shown)) = &mut self.job_in_flight
            && key_in_flight == job.key()
        {
            *last_shown = now;
            return;
        }
        match self.waiting_jobs.entry(job.key().clone()) {
            Entry::Occupied(mut waiting) => waiting.get_mut().last_shown = now,
            Entry::Vacant(vacant) => {
                vacant.insert(WaitingJob {
                    job,
                    last_shown: now,
                });
            }
        }
        if self.waiting_jobs.len() > CoverCache::LIMIT
            && let Some(oldest_key) = self
                .waiting_jobs
                .iter()
                .min_by_key(|(_, waiting)| waiting.last_shown)
                .map(|(waiting_key, _)| waiting_key.clone())
        {
            self.waiting_jobs.remove(&oldest_key);
        }
    }

    /// The job asked for most recently, now in flight, unless the thread is
    /// still on one.
    fn take_next(&mut self) -> Option<CoverJob> {
        if self.job_in_flight.is_some() {
            return None;
        }
        let next_key = self
            .waiting_jobs
            .iter()
            .max_by_key(|(_, waiting)| waiting.last_shown)
            .map(|(waiting_key, _)| waiting_key.clone())?;
        let waiting = self.waiting_jobs.remove(&next_key)?;
        self.job_in_flight = Some((next_key, waiting.last_shown));
        Some(waiting.job)
    }

    /// Ends the job in flight, and returns when its cover was last asked for.
    fn finish(&mut self) -> Option<Instant> {
        self.job_in_flight.take().map(|(_, last_shown)| last_shown)
    }
}

/// Finds covers for the panel. Loaded ones come from the cache, and the
/// others are queued as jobs for the thread.
pub struct CoverLoader {
    job_tx: Sender<CoverJob>,
    report_rx: Receiver<CoverJobReport>,
    cache: CoverCache,
    job_queue: CoverJobQueue,
}

impl CoverLoader {
    /// How the cover panel fits an image. The widget must be drawn with the
    /// same value.
    pub const RESIZE_MODE: Resize = Resize::Fit(None);

    /// Starts the thread that decodes, scales and encodes covers, which takes
    /// far longer than a frame. `events` hears when a cover is ready.
    pub fn spawn(picker: Picker, events: Sender<Event>) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<CoverJob>();
        let (report_tx, report_rx) = mpsc::channel();
        thread::spawn(move || {
            for job in job_rx {
                if report_tx.send(job.run(&picker)).is_err() {
                    return;
                }
                let _ = events.send(Event::CoverReady);
            }
        });
        Self {
            job_tx,
            report_rx,
            cache: CoverCache::new(),
            job_queue: CoverJobQueue::new(),
        }
    }

    /// The cover at `path` for a panel of `size` cells, or None while it is
    /// being made. A cover whose file is gone comes back unreadable.
    pub fn show(&mut self, path: &Path, size: Size) -> Option<&mut LoadedCover> {
        let now = Instant::now();
        let key = CoverKey::from_path(path);
        // A cover is encoded for the panel's size, which changes with the
        // terminal and the layout.
        if let Some(cover) = self.cache.take_to_resize(&key, size) {
            let job = CoverJob::Resize {
                key: key.clone(),
                cover,
                size,
            };
            self.job_queue.add(job, now);
        } else if !self.cache.contains(&key) {
            let job = CoverJob::Decode {
                key: key.clone(),
                size,
            };
            self.job_queue.add(job, now);
        }
        // The thread may be idle, with nothing to start it.
        self.send_next_job();
        self.cache.get(&key, now)
    }

    pub fn collect_reports(&mut self) {
        for report in self.report_rx.try_iter() {
            let last_shown = self.job_queue.finish().unwrap_or_else(Instant::now);
            self.cache.insert(report.key, report.cover, last_shown);
        }
        // The thread is free again. This runs whatever page is open, unlike
        // `show`, so the queue keeps going while no cover is on screen.
        self.send_next_job();
    }

    fn send_next_job(&mut self) {
        if let Some(job) = self.job_queue.take_next() {
            let _ = self.job_tx.send(job);
        }
    }
}

// ---------------------------------------------------------------------------
// The dot-matrix picture

/// A cover drawn in colored braille dots, kept with its picture so it can
/// be drawn again when the panel changes size.
pub struct BrailleCover {
    source: DynamicImage,
    /// The size the lines were drawn for, in cells.
    size: Size,
    lines: Vec<Line<'static>>,
}

impl BrailleCover {
    fn new(image: &DynamicImage, size: Size) -> Self {
        let source = DynamicImage::ImageRgb8(image.to_rgb8());
        let lines = render_dots(&source, size);
        Self {
            source,
            size,
            lines,
        }
    }

    /// One line per row, centered in the panel.
    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    fn resize(&mut self, size: Size) {
        self.size = size;
        self.lines = render_dots(&self.source, size);
    }
}

/// Fits `source` into `size` cells, dithers its brightness and encodes it as
/// braille.
///
/// Braille gives 2×4 dots per cell, eight times what half blocks give, and the
/// dots are about square because a cell is twice as tall as it is wide. Which
/// dots are lit carries the brightness, and each cell is drawn in the average
/// color of its lit dots, so the color carries the hue.
fn render_dots(source: &DynamicImage, size: Size) -> Vec<Line<'static>> {
    // Dots per cell, across and down.
    const DOTS_X: u32 = 2;
    const DOTS_Y: u32 = 4;
    // The braille bit for the dot at (x, y) in a cell.
    const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    let (column_count, row_count) = (u32::from(size.width), u32::from(size.height));
    if column_count == 0 || row_count == 0 || source.width() == 0 || source.height() == 0 {
        return Vec::new();
    }
    let downscaled = source.resize(
        column_count * DOTS_X,
        row_count * DOTS_Y,
        FilterType::Triangle,
    );
    let lit_dots = dither(&downscaled.to_luma8());
    let downscaled = downscaled.to_rgb8();
    let (width, height) = downscaled.dimensions();

    // Center the picture in whole cells.
    let used_column_count = width.div_ceil(DOTS_X);
    let used_row_count = height.div_ceil(DOTS_Y);
    let left_padding = ((column_count - used_column_count) / 2) as usize;
    let top_padding = (row_count - used_row_count) / 2;
    let mut lines = vec![Line::default(); top_padding as usize];
    for cell_row in 0..used_row_count {
        let mut spans = vec![Span::raw(" ".repeat(left_padding))];
        for cell_column in 0..used_column_count {
            let mut braille_bits = 0u8;
            let mut color_sums = [0u32; 3];
            let mut lit_count = 0u32;
            for (dot_column, column_bits) in BITS.iter().enumerate() {
                for (dot_row, dot_bit) in column_bits.iter().enumerate() {
                    let (x, y) = (
                        cell_column * DOTS_X + dot_column as u32,
                        cell_row * DOTS_Y + dot_row as u32,
                    );
                    if x < width && y < height && lit_dots[(y * width + x) as usize] {
                        braille_bits |= dot_bit;
                        let pixel = downscaled.get_pixel(x, y).0;
                        for (channel_sum, channel) in color_sums.iter_mut().zip(pixel) {
                            *channel_sum += u32::from(channel);
                        }
                        lit_count += 1;
                    }
                }
            }
            // An empty cell is a space, because some fonts draw U+2800 as a
            // box.
            if braille_bits == 0 {
                spans.push(Span::raw(" "));
                continue;
            }
            let braille_character = char::from_u32(0x2800 + u32::from(braille_bits)).unwrap_or(' ');
            let [r, g, b] = color_sums.map(|channel_sum| channel_sum / lit_count);
            spans.push(Span::styled(
                braille_character.to_string(),
                Style::new().fg(lifted_color(r, g, b)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// A dot's color, lifted so that a dark color still shows on the dark
/// background. A lit dot means "light here", and the density already says how
/// light.
fn lifted_color(r: u32, g: u32, b: u32) -> Color {
    const FLOOR: u32 = 130;
    let brightest_channel = r.max(g).max(b).max(1);
    let lifted =
        |channel: u32| (channel * brightest_channel.max(FLOOR) / brightest_channel).min(255) as u8;
    Color::Rgb(lifted(r), lifted(g), lifted(b))
}

/// Which dots are lit. The contrast is stretched to the picture's own range,
/// then Atkinson dithering spreads three quarters of each error to six
/// neighbors, which keeps highlights and shadows clean instead of speckled.
fn dither(image: &GrayImage) -> Vec<bool> {
    let (width, height) = (image.width() as usize, image.height() as usize);
    let mut pixels: Vec<f32> = image.pixels().map(|pixel| f32::from(pixel.0[0])).collect();
    // Stretch between the 2nd and 98th percentile, so one stray pixel doesn't
    // flatten the rest. A nearly flat picture is left alone, because stretching
    // it would turn it all black or all white.
    let mut sorted_pixels = pixels.clone();
    sorted_pixels.sort_by(f32::total_cmp);
    let dark_cutoff = sorted_pixels[sorted_pixels.len() / 50];
    let light_cutoff = sorted_pixels[sorted_pixels.len() - 1 - sorted_pixels.len() / 50];
    if light_cutoff - dark_cutoff >= 32.0 {
        for pixel in &mut pixels {
            *pixel =
                ((*pixel - dark_cutoff) / (light_cutoff - dark_cutoff) * 255.0).clamp(0.0, 255.0);
        }
    }
    let mut lit_dots = vec![false; width * height];
    for y in 0..height {
        for x in 0..width {
            let pixel_index = y * width + x;
            let is_lit = pixels[pixel_index] >= 128.0;
            lit_dots[pixel_index] = is_lit;
            let spread_error = (pixels[pixel_index] - if is_lit { 255.0 } else { 0.0 }) / 8.0;
            for (dx, dy) in [(1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)] {
                let (neighbor_x, neighbor_y) = (x as isize + dx, y as isize + dy);
                if neighbor_x >= 0
                    && (neighbor_x as usize) < width
                    && (neighbor_y as usize) < height
                {
                    pixels[neighbor_y as usize * width + neighbor_x as usize] += spread_error;
                }
            }
        }
    }
    lit_dots
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;
    use std::time::Duration;

    fn wait_ready(cover_loader: &mut CoverLoader, path: &Path, size: Size) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            cover_loader.collect_reports();
            match cover_loader.show(path, size) {
                Some(LoadedCover::Ready(_)) => return true,
                Some(LoadedCover::Unreadable) => return false,
                None => thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("cover never finished");
    }

    // -- the dot-matrix picture --

    fn picture(width: u32, height: u32, shade_at: impl Fn(u32, u32) -> u8) -> DynamicImage {
        DynamicImage::ImageLuma8(GrayImage::from_fn(width, height, |x, y| {
            Luma([shade_at(x, y)])
        }))
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    mod loading {
        use super::*;

        #[test]
        fn reloads_cover_when_file_changes() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("cover.png");
            image::RgbImage::new(8, 8).save(&path).unwrap();
            let mut cover_loader = CoverLoader::spawn(Picker::halfblocks(), mpsc::channel().0);
            let size = Size::new(10, 5);
            // The first call only requests it, on the background thread.
            assert!(cover_loader.show(&path, size).is_none());
            assert!(wait_ready(&mut cover_loader, &path, size));
            image::RgbImage::new(16, 16).save(&path).unwrap();
            assert!(cover_loader.show(&path, size).is_none());
            assert!(wait_ready(&mut cover_loader, &path, size));
        }

        #[test]
        fn reports_unreadable_cover_without_retrying() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("cover.png");
            std::fs::write(&path, b"not an image").unwrap();
            let mut cover_loader = CoverLoader::spawn(Picker::halfblocks(), mpsc::channel().0);
            let size = Size::new(10, 5);
            assert!(!wait_ready(&mut cover_loader, &path, size));
            assert!(matches!(
                cover_loader.show(&path, size),
                Some(LoadedCover::Unreadable)
            ));
        }

        #[test]
        fn loads_cover_scrolled_past_when_user_returns() {
            let tmp = tempfile::tempdir().unwrap();
            let paths: Vec<PathBuf> = (0..6)
                .map(|i| {
                    let path = tmp.path().join(format!("{i}.png"));
                    image::RgbImage::new(64, 64).save(&path).unwrap();
                    path
                })
                .collect();
            let mut cover_loader = CoverLoader::spawn(Picker::halfblocks(), mpsc::channel().0);
            let size = Size::new(10, 5);
            // Scroll through all of them without waiting.
            for path in &paths {
                cover_loader.show(path, size);
            }
            assert!(wait_ready(&mut cover_loader, &paths[5], size));
            assert!(wait_ready(&mut cover_loader, &paths[0], size));
        }
    }

    mod dots {
        use super::*;

        /// The picture fits the panel, centered in whole cells, and fits it
        /// again when the panel changes size.
        #[test]
        fn fits_tall_cover_centered_in_whole_cells() {
            // White left half, black right half. At 1:2, 10×5 cells take the full
            // height of 20 dots and 10 dots (5 cells) across.
            let mut dots = BrailleCover::new(
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

            dots.resize(Size::new(20, 10));
            let lines: Vec<String> = dots.lines.iter().map(text).collect();
            assert_eq!(lines.len(), 10);
            assert!(lines.iter().all(|line| line.chars().count() <= 20));
        }
    }
}
