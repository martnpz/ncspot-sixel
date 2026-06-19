//! In-process sixel cover rendering.
//!
//! Decoding, resizing and sixel-encoding happen on a worker thread; the
//! cursive draw call only does a cache lookup and a single write to the
//! terminal. Writing from the cursive thread serializes our escape sequences
//! with crossterm's own output, which prevents tearing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

use cursive::Vec2;
use log::{debug, error, warn};

use crate::events::EventManager;

/// Spotify covers are at most 640x640.
const MAX_COVER_PX: usize = 640;

/// Maximum number of encoded sixels kept in memory. Evicted FIFO; an evicted
/// entry is re-read cheaply from the on-disk `.six` cache if needed again.
/// Encoded covers/thumbnails range from a few KB (thumbnails) to ~tens of KB
/// (640px covers), so this bounds the cache to roughly a few MB.
const MAX_CACHE_ENTRIES: usize = 256;

/// Process-wide handle to the image cache for view-construction sites that
/// dependency injection can't reach (e.g. [`crate::traits::ListItem::open`]).
/// Set once at startup when thumbnails are available.
static SHARED: std::sync::OnceLock<Arc<SixelImageCache>> = std::sync::OnceLock::new();

pub fn set_shared(images: Arc<SixelImageCache>) {
    SHARED.set(images).ok();
}

pub fn shared() -> Option<Arc<SixelImageCache>> {
    SHARED.get().cloned()
}

/// (cover URL, target width px, target height px)
type CacheKey = (String, usize, usize);

/// Shared service that turns cover URLs into encoded sixel images of a
/// requested pixel size. Downloading, decoding, resizing and encoding all
/// happen on a worker thread; [SixelImageCache::get] never blocks. Used by
/// the cover backend and by track-list thumbnails.
pub struct SixelImageCache {
    jobs: mpsc::Sender<CacheKey>,
    cache: Arc<RwLock<HashMap<CacheKey, Arc<String>>>>,
    pending: Arc<RwLock<HashSet<CacheKey>>>,
    /// Solid-background sixels used to blank image slots, keyed by size.
    blanks: RwLock<HashMap<(usize, usize), Arc<String>>>,
}

impl SixelImageCache {
    pub fn new(events: EventManager) -> Self {
        let (jobs, job_rx) = mpsc::channel::<CacheKey>();
        let cache: Arc<RwLock<HashMap<CacheKey, Arc<String>>>> = Default::default();
        let pending: Arc<RwLock<HashSet<CacheKey>>> = Default::default();

        std::thread::Builder::new()
            .name("sixel-encoder".into())
            .spawn({
                let cache = cache.clone();
                let pending = pending.clone();
                // Insertion order for FIFO eviction; only the encoder thread
                // touches it, so it lives here rather than on the struct.
                let order: RwLock<VecDeque<CacheKey>> = RwLock::new(VecDeque::new());
                move || {
                    while let Ok(key) = job_rx.recv() {
                        match encode(&key) {
                            Ok(sixel) => {
                                Self::store(&cache, &order, key.clone(), Arc::new(sixel));
                                events.trigger();
                            }
                            Err(e) => error!("failed to sixel-encode {}: {e}", key.0),
                        }
                        pending.write().unwrap().remove(&key);
                    }
                }
            })
            .expect("failed to spawn sixel encoder thread");

        Self {
            jobs,
            cache,
            pending,
            blanks: RwLock::new(HashMap::new()),
        }
    }

    /// Insert an encoded sixel into the cache, evicting the oldest entries
    /// (FIFO) once the cache grows past [MAX_CACHE_ENTRIES].
    fn store(
        cache: &RwLock<HashMap<CacheKey, Arc<String>>>,
        order: &RwLock<VecDeque<CacheKey>>,
        key: CacheKey,
        sixel: Arc<String>,
    ) {
        let mut cache = cache.write().unwrap();
        let mut order = order.write().unwrap();
        if cache.insert(key.clone(), sixel).is_none() {
            // Only track newly added keys so re-encodes don't double-count.
            order.push_back(key);
        }
        while order.len() > MAX_CACHE_ENTRIES {
            if let Some(evicted) = order.pop_front() {
                cache.remove(&evicted);
            }
        }
    }

    /// The encoded sixel for `url` at `width`x`height` pixels if it is ready.
    /// Otherwise schedules download + encode on the worker and returns None;
    /// the [EventManager] is triggered when the image becomes available.
    pub fn get(&self, url: &str, width: usize, height: usize) -> Option<Arc<String>> {
        if width == 0 || height == 0 {
            return None;
        }
        let key: CacheKey = (url.to_string(), width, height);
        if let Some(sixel) = self.cache.read().unwrap().get(&key) {
            return Some(sixel.clone());
        }
        let mut pending = self.pending.write().unwrap();
        if !pending.contains(&key) {
            pending.insert(key.clone());
            let _ = self.jobs.send(key);
        }
        None
    }

    /// A solid background-colored sixel used to blank an image slot.
    /// Encoded on first use per size; cheap (single-color image).
    pub fn blank(&self, width: usize, height: usize) -> Option<Arc<String>> {
        if width == 0 || height == 0 {
            return None;
        }
        if let Some(blank) = self.blanks.read().unwrap().get(&(width, height)) {
            return Some(blank.clone());
        }
        // Opaque black: emitting it overwrites stale pixels (a transparent
        // sixel would leave them in place).
        let pixels = vec![0u8; width * height * 4]
            .chunks_exact(4)
            .flat_map(|_| [0, 0, 0, 255])
            .collect::<Vec<u8>>();
        let sixel = icy_sixel::SixelImage::try_from_rgba(pixels, width, height)
            .and_then(|image| image.encode())
            .ok()?;
        let blank = Arc::new(sixel);
        self.blanks
            .write()
            .unwrap()
            .insert((width, height), blank.clone());
        Some(blank)
    }
}

pub struct SixelBackend {
    images: Arc<SixelImageCache>,
}

impl SixelBackend {
    pub fn new(images: Arc<SixelImageCache>) -> Self {
        Self { images }
    }

    /// Draw the cover for `url` into the cell rectangle described by
    /// `draw_offset`/`draw_size`. Returns whether the image was actually
    /// drawn (false e.g. while an encode is still in flight).
    pub fn draw(
        &self,
        url: &str,
        draw_offset: Vec2,
        draw_size: Vec2,
        font_size: Vec2,
    ) -> bool {
        // Covers are square; fit the largest square (in pixels) into the pane.
        let side_px = (draw_size.x * font_size.x)
            .min(draw_size.y * font_size.y)
            .min(MAX_COVER_PX);
        debug!(
            "sixel draw: cells {draw_size:?} font {font_size:?} -> {side_px}px square at {draw_offset:?}"
        );
        if side_px == 0 {
            return false;
        }

        let Some(sixel) = self.images.get(url, side_px, side_px) else {
            return false;
        };

        // Center the image in the pane (cell-based).
        let cells = Vec2::new(
            (side_px / font_size.x.max(1)).min(draw_size.x),
            (side_px / font_size.y.max(1)).min(draw_size.y),
        );
        let pos = draw_offset + (draw_size - cells) / 2;

        if let Err(e) = emit(&sixel, pos) {
            warn!("failed to write sixel data to terminal: {e}");
            return false;
        }
        true
    }

    /// Remove a previously drawn cover from the terminal by blanking the
    /// `offset`/`size` cell region directly.
    pub fn clear(&self, offset: Vec2, size: Vec2) {
        // Actively write blanks so the sixel pixels are erased even when
        // crossterm's diff optimisation would otherwise skip that area.
        clear_area(offset, size).ok();
    }
}

/// Overwrite `size` cells starting at `pos` with blank characters via a
/// direct `/dev/tty` write, erasing any sixel pixels in that region.
/// Called before showing a modal overlay so the sixel does not bleed through.
pub fn clear_area(pos: Vec2, size: Vec2) -> std::io::Result<()> {
    if size.x == 0 || size.y == 0 {
        return Ok(());
    }
    let mut tty = OpenOptions::new().write(true).open("/dev/tty")?;
    let blank_row = " ".repeat(size.x);
    let mut out = Vec::with_capacity(size.y * (size.x + 16) + 32);
    out.extend_from_slice(b"\x1b[?2026h\x1b7");
    for y in 0..size.y {
        out.extend_from_slice(
            format!("\x1b[{};{}H", pos.y + y + 1, pos.x + 1).as_bytes(),
        );
        out.extend_from_slice(blank_row.as_bytes());
    }
    out.extend_from_slice(b"\x1b8\x1b[?2026l");
    tty.write_all(&out)
}

/// Write the encoded sixel to the terminal at `pos` (cell coordinates),
/// preserving the cursor and using synchronized updates to avoid tearing.
/// Must only be called from the cursive draw thread, so our bytes serialize
/// with crossterm's output.
pub fn emit(sixel: &str, pos: Vec2) -> std::io::Result<()> {
    let mut tty = OpenOptions::new().write(true).open("/dev/tty")?;
    let mut out = Vec::with_capacity(sixel.len() + 64);
    // begin synchronized update; save cursor; move to cell
    out.extend_from_slice(b"\x1b[?2026h\x1b7");
    out.extend_from_slice(format!("\x1b[{};{}H", pos.y + 1, pos.x + 1).as_bytes());
    out.extend_from_slice(sixel.as_bytes());
    // restore cursor; end synchronized update
    out.extend_from_slice(b"\x1b8\x1b[?2026l");
    tty.write_all(&out)
}

/// Download (if needed), load, resize and encode a cover image. Slow; runs
/// on the encoder thread.
fn encode(key: &CacheKey) -> Result<String, String> {
    let (url, width, height) = (&key.0, key.1, key.2);

    let path = crate::utils::cache_path_for_url(url.clone());
    let cache_file = sixel_cache_file(&path, width, height);
    if let Ok(mut file) = File::open(&cache_file) {
        let mut sixel = String::new();
        if file.read_to_string(&mut sixel).is_ok() && !sixel.is_empty() {
            debug!("sixel cache hit: {cache_file:?}");
            return Ok(sixel);
        }
    }

    if !path.exists() {
        crate::utils::download(url.clone(), path.clone()).map_err(|e| e.to_string())?;
    }

    debug!("sixel-encoding {path:?} at {width}x{height}");
    // Cover cache files have no extension, so sniff the format from content.
    let image = image::ImageReader::open(&path)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let resized = image.resize_exact(
        width as u32,
        height as u32,
        image::imageops::FilterType::Lanczos3,
    );
    let rgba = resized.to_rgba8();
    let sixel = icy_sixel::SixelImage::try_from_rgba(rgba.into_raw(), width, height)
        .and_then(|image| image.encode())
        .map_err(|e| e.to_string())?;

    if let Err(e) = std::fs::create_dir_all(cache_file.parent().unwrap())
        .and_then(|()| std::fs::write(&cache_file, &sixel))
    {
        warn!("failed to write sixel cache {cache_file:?}: {e}");
    }
    Ok(sixel)
}

fn sixel_cache_file(image_path: &Path, width: usize, height: usize) -> PathBuf {
    let stem = image_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::config::cache_path("covers-sixel").join(format!("{stem}-{width}x{height}.six"))
}

/// Ask the terminal whether it supports sixel graphics via a DA1 query.
///
/// Must be called before cursive takes over the terminal, since it reads the
/// response from /dev/tty directly.
pub fn probe_support() -> bool {
    match probe_da1() {
        Ok(supported) => {
            debug!("sixel support detected: {supported}");
            supported
        }
        Err(e) => {
            warn!("sixel detection failed, assuming no support: {e}");
            false
        }
    }
}

fn probe_da1() -> std::io::Result<bool> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let fd = tty.as_raw_fd();

    // Raw mode so the response isn't echoed or line-buffered.
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let saved = termios;
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| {
        tty.write_all(b"\x1b[c")?;
        tty.flush()?;

        let mut response = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pollfd, 1, 250) };
            if ready <= 0 {
                break; // timeout or error: terminal didn't answer
            }
            let n = tty.read(&mut buf)?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            if response.contains(&b'c') {
                break;
            }
        }

        // Response looks like ESC [ ? 64 ; 4 ; ... c — attribute 4 is sixel.
        let response = String::from_utf8_lossy(&response);
        let supported = response
            .trim_start_matches(['\x1b', '[', '?'])
            .trim_end_matches('c')
            .split(';')
            .any(|attribute| attribute == "4");
        Ok(supported)
    })();

    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    result
}
