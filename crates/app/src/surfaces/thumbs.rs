//! Wallpaper thumbnails.
//!
//! ## Why this exists at all
//!
//! `gpui::img(path)` decodes at **source resolution**. Its decoder is
//! `DynamicImage::from_decoder` followed by `into_rgba8` — there is no
//! downscale anywhere in the path.
//!
//! Measured on this machine, the picker has 29 wallpapers. Decoded at 4K as
//! RGBA8 that is roughly **957 MB** for a shell that otherwise sits at 47 MB.
//! So handing `img()` a wallpaper path is not a shortcut, it is a way to
//! exhaust memory the first time somebody opens the picker.
//!
//! `Frame` is `pub(crate)` in this fork, so a `RenderImage` cannot be built
//! directly. `ImageSource::Image(Arc<Image>)` *is* public and `Image` is just
//! `format + bytes + id`, so: decode here, shrink here, re-encode here, hand
//! over a small PNG.
//!
//! ## Why off the UI thread
//!
//! A 512px JPEG decode is 10–30 ms. Doing twenty-nine of them in `render`
//! would be a second-long freeze on the first open, on the one thread that
//! draws everything. Two worker threads, results into a shared cache, and the
//! view redraws as each lands. Tiles fill in progressively, which is also what
//! a progressive image load should look like.
//!
//! ## Why re-encode to PNG
//!
//! `Image::from_bytes` takes encoded bytes, not raw pixels. PNG is lossless, so
//! nothing is lost by the round trip, and at 512px the encode is a few
//! milliseconds — cheap next to the decode that preceded it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

use anyhow::Context as _;
use tokio::sync::Notify;

/// Longest edge of a thumbnail, in pixels.
///
/// A tile is at most 240 CSS px. At the 2x device scale that is 480 device px,
/// and the renderer uploads at that size whatever we hand it.
///
/// This was 640 and cost **114 MB** for 29 wallpapers, and 384 cost 56 MB.
/// 256 keeps a tile at its natural size on a 1x display and is close enough at
/// 2x that the GPU's filter hides the difference on a picture you are only
/// choosing, not reading.
///
/// The real fix is to decode only what is on screen. `uniform_list` was the
/// right tool and is already a dependency-free call away, but it lays out a
/// grid of tiles wrong — see the note in `picker::render`. Until that is
/// solved the cap is the only lever, and 256 is where the curve flattens.
pub const MAX_EDGE: u32 = 256;

/// Two workers. This machine has 8 threads and the compositor is using them;
/// four decode threads would be felt as stutter in the bar and the dock.
const WORKERS: usize = 2;

type CacheKey = (PathBuf, u64, u32);

struct Inner {
    ready: RwLock<HashMap<CacheKey, Arc<gpui::Image>>>,
    /// In flight. A second request for the same key is dropped, not queued —
    /// the grid re-renders constantly and would otherwise enqueue duplicates.
    pending: Mutex<HashSet<CacheKey>>,
    queue: Mutex<VecDeque<CacheKey>>,
    ready_signal: Condvar,
    shutdown: bool,
}

/// Fires whenever a thumbnail lands, so the view can redraw.
static DONE: OnceLock<Arc<Notify>> = OnceLock::new();

pub fn done_signal() -> Arc<Notify> {
    DONE.get_or_init(|| Arc::new(Notify::new())).clone()
}

fn inner() -> Arc<Inner> {
    static I: OnceLock<Arc<Inner>> = OnceLock::new();
    I.get_or_init(|| {
        let i = Arc::new(Inner {
            ready: RwLock::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            queue: Mutex::new(VecDeque::new()),
            ready_signal: Condvar::new(),
            shutdown: false,
        });
        for _ in 0..WORKERS {
            let i = i.clone();
            std::thread::Builder::new()
                .name("lume-thumb".into())
                .spawn(move || worker(i))
                .expect("spawn thumbnail worker");
        }
        i
    })
    .clone()
}

fn worker(inner: Arc<Inner>) {
    loop {
        let job = {
            let mut q = inner.queue.lock().expect("thumb queue poisoned");
            loop {
                if inner.shutdown {
                    return;
                }
                if let Some(job) = q.pop_front() {
                    break job;
                }
                q = inner
                    .ready_signal
                    .wait(q)
                    .expect("thumb queue poisoned");
            }
        };

        // Split out before the move, for the same reason as everywhere else
        // in this codebase: field access after a partial move is a borrow
        // error, not a subtle ordering bug to find later.
        let path = job.0.clone();
        let result = thumbnail(&path, job.2);
        match result {
            Ok(image) => {
                inner
                    .ready
                    .write()
                    .expect("thumb cache poisoned")
                    .insert(job.clone(), Arc::new(image));
            }
            Err(e) => {
                // Logged and dropped. A wallpaper that will not decode is a
                // fact about that file, not a reason to leave the picker
                // stuck on a placeholder forever.
                tracing::debug!(path = %path.display(), "thumbnail failed: {e:#}");
            }
        }

        inner
            .pending
            .lock()
            .expect("thumb pending poisoned")
            .remove(&job);
        done_signal().notify_one();
    }
}

/// The cached thumbnail, or `None` if it is not ready yet.
///
/// Never blocks and never decodes. Render calls this and draws a placeholder
/// when it returns `None`.
pub fn get(path: &Path, max_edge: u32) -> Option<Arc<gpui::Image>> {
    let key = key_for(path, max_edge)?;
    inner()
        .ready
        .read()
        .expect("thumb cache poisoned")
        .get(&key)
        .cloned()
}

/// Queue a thumbnail if it is neither cached nor already in flight.
///
/// Cheap and idempotent, so it is safe to call from `render`.
pub fn request(path: &Path, max_edge: u32) {
    let Some(key) = key_for(path, max_edge) else {
        return;
    };
    let inner = inner();

    if inner
        .ready
        .read()
        .expect("thumb cache poisoned")
        .contains_key(&key)
    {
        return;
    }
    if !inner
        .pending
        .lock()
        .expect("thumb pending poisoned")
        .insert(key.clone())
    {
        return; // already queued
    }

    inner.queue.lock().expect("thumb queue poisoned").push_back(key);
    inner.ready_signal.notify_one();
}

/// Path + mtime + size.
///
/// Keyed on mtime as well as path so replacing a wallpaper in place
/// invalidates it. Without that, a user who replaces their wallpaper sees the
/// old thumbnail until lume restarts.
fn key_for(path: &Path, max_edge: u32) -> Option<CacheKey> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)?;
    Some((path.to_path_buf(), mtime, max_edge))
}

/// Decode, shrink, re-encode.
fn thumbnail(path: &Path, max_edge: u32) -> anyhow::Result<gpui::Image> {
    let reader = image::ImageReader::open(path)
        .context("opening image")?
        .with_guessed_format()
        .context("guessing image format")?;
    let decoded = reader.decode().context("decoding image")?;

    // `thumbnail` preserves aspect ratio and only shrinks.
    let small = decoded.thumbnail(max_edge, max_edge);
    if small.width() == 0 || small.height() == 0 {
        anyhow::bail!("image has a zero dimension");
    }

    let mut bytes = Vec::new();
    small
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .context("encoding thumbnail")?;

    tracing::trace!(
        path = %path.display(),
        "{}x{} -> {}x{}, {} KB",
        decoded.width(),
        decoded.height(),
        small.width(),
        small.height(),
        bytes.len() / 1024
    );

    Ok(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes))
}

/// Drop every cached thumbnail.
///
/// Called when the picker closes. Holding them for the life of the process is
/// not acceptable in a shell that sits at ~47 MB: opening the picker once and
/// leaving it grew private dirty to 174 MB and it did not come back down.
///
/// The tradeoff is re-decoding on the next open, which is ~1.5 s of background
/// work spread over two threads. Reopening a picker is rare and the memory is
/// permanent, so the memory wins.
pub fn clear() {
    let inner = inner();
    let dropped = inner
        .ready
        .write()
        .expect("thumb cache poisoned")
        .len();
    if dropped > 0 {
        tracing::debug!(dropped, "thumbnail cache cleared");
    }
    inner
        .pending
        .lock()
        .expect("thumb pending poisoned")
        .clear();
}

/// How many thumbnails are ready. For tests and for the log line; not a hot
/// path.
pub fn ready_count() -> usize {
    inner().ready.read().expect("thumb cache poisoned").len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gpui::Image` is opaque — format, bytes, id — so the produced PNG is
    /// decoded back to check what actually came out.
    fn dimensions(img: &gpui::Image) -> (u32, u32) {
        let decoded = image::load_from_memory(&img.bytes).expect("thumbnail re-decode");
        (decoded.width(), decoded.height())
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([40, 80, 160]));
        img.save(path).unwrap();
    }

    #[test]
    fn thumbnail_shrinks_but_keeps_aspect() {
        let dir = std::env::temp_dir().join("lume-thumb-test");
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.png");
        write_png(&big, 4000, 2000);

        let (w, h) = dimensions(&thumbnail(&big, MAX_EDGE).unwrap());
        assert!(w <= MAX_EDGE && h <= MAX_EDGE, "got {w}x{h}, max {MAX_EDGE}");
        // 2:1 stays 2:1.
        assert!(
            (w as f32 / h as f32 - 2.0).abs() < 0.05,
            "aspect changed: {w}x{h}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason this module exists: a 4K source must not be handed to
    /// the renderer at source size.
    #[test]
    fn a_4k_source_becomes_a_small_png() {
        let dir = std::env::temp_dir().join("lume-thumb-size");
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("4k.png");
        write_png(&big, 3840, 2160);

        let img = thumbnail(&big, MAX_EDGE).unwrap();
        let (w, h) = dimensions(&img);
        assert!(w <= MAX_EDGE && h <= MAX_EDGE);

        // 29 of these at full size is ~957 MB. The point is that it is not.
        let bytes = img.bytes.len();
        assert!(
            bytes < 2_000_000,
            "thumbnail is {bytes} bytes; full-res RGBA8 would be ~33 MB"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        assert!(thumbnail(Path::new("/nonexistent/lume/wall.png"), MAX_EDGE).is_err());
    }

    #[test]
    fn keying_is_by_path_and_mtime() {
        let dir = std::env::temp_dir().join("lume-thumb-key");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.png");
        write_png(&p, 32, 32);
        assert!(key_for(&p, MAX_EDGE).is_some());
        assert!(key_for(Path::new("/nope/none.png"), MAX_EDGE).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
