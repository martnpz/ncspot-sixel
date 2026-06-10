//! In-process sixel cover rendering.
//!
//! Decoding, resizing and sixel-encoding happen on a worker thread; the
//! cursive draw call only does a cache lookup and a single write to the
//! terminal. Writing from the cursive thread serializes our escape sequences
//! with crossterm's own output, which prevents tearing.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

use cursive::Vec2;
use log::{debug, error, warn};

use crate::events::EventManager;

use super::CoverBackend;

/// Spotify covers are at most 640x640.
const MAX_COVER_PX: usize = 640;

/// (cover URL, target width px, target height px)
type CacheKey = (String, usize, usize);

struct EncodeJob {
    key: CacheKey,
    path: PathBuf,
}

pub struct SixelBackend {
    jobs: mpsc::Sender<EncodeJob>,
    cache: Arc<RwLock<HashMap<CacheKey, Arc<String>>>>,
    pending: Arc<RwLock<HashSet<CacheKey>>>,
}

impl SixelBackend {
    pub fn new(events: EventManager) -> Self {
        let (jobs, job_rx) = mpsc::channel::<EncodeJob>();
        let cache: Arc<RwLock<HashMap<CacheKey, Arc<String>>>> = Default::default();
        let pending: Arc<RwLock<HashSet<CacheKey>>> = Default::default();

        std::thread::Builder::new()
            .name("sixel-encoder".into())
            .spawn({
                let cache = cache.clone();
                let pending = pending.clone();
                move || {
                    while let Ok(job) = job_rx.recv() {
                        match encode(&job) {
                            Ok(sixel) => {
                                cache.write().unwrap().insert(job.key.clone(), Arc::new(sixel));
                                events.trigger();
                            }
                            Err(e) => error!("failed to sixel-encode {:?}: {e}", job.path),
                        }
                        pending.write().unwrap().remove(&job.key);
                    }
                }
            })
            .expect("failed to spawn sixel encoder thread");

        Self {
            jobs,
            cache,
            pending,
        }
    }
}

impl CoverBackend for SixelBackend {
    fn draw(
        &self,
        url: &str,
        path: &Path,
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

        let key: CacheKey = (url.to_string(), side_px, side_px);
        let sixel = self.cache.read().unwrap().get(&key).cloned();
        let Some(sixel) = sixel else {
            let mut pending = self.pending.write().unwrap();
            if !pending.contains(&key) {
                pending.insert(key.clone());
                let _ = self.jobs.send(EncodeJob {
                    key,
                    path: path.to_path_buf(),
                });
            }
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

    fn clear(&self) {
        // Nothing to do: when cursive repaints the cells underneath, the
        // terminal drops the sixel pixels in that region.
    }
}

/// Write the encoded sixel to the terminal at `pos` (cell coordinates),
/// preserving the cursor and using synchronized updates to avoid tearing.
fn emit(sixel: &str, pos: Vec2) -> std::io::Result<()> {
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

/// Load, resize and encode a cover image. Slow; runs on the encoder thread.
fn encode(job: &EncodeJob) -> Result<String, String> {
    let (_, width, height) = (&job.key.0, job.key.1, job.key.2);

    let cache_file = sixel_cache_file(&job.path, width, height);
    if let Ok(mut file) = File::open(&cache_file) {
        let mut sixel = String::new();
        if file.read_to_string(&mut sixel).is_ok() && !sixel.is_empty() {
            debug!("sixel cache hit: {cache_file:?}");
            return Ok(sixel);
        }
    }

    debug!("sixel-encoding {:?} at {width}x{height}", job.path);
    // Cover cache files have no extension, so sniff the format from content.
    let image = image::ImageReader::open(&job.path)
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
