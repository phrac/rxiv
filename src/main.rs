use image::{AnimationDecoder, DynamicImage, ImageDecoder};
use serde::Deserialize;
use softbuffer::{Context, Surface};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "tiff", "tif", "webp", "ico", "pnm", "pbm", "pgm", "ppm",
    "qoi",
];

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    keys: HashMap<String, String>,
}

impl Config {
    fn load() -> Self {
        let config_path = dirs_config_path().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&config_path) else {
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: failed to parse {}: {}", config_path.display(), e);
                Self::default()
            }
        }
    }

    fn command_for_key(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(|s| s.as_str())
    }
}

fn dirs_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("rxiv")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/rxiv")
    } else {
        PathBuf::from(".config/rxiv")
    }
}

fn collect_images(path: &Path) -> Vec<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(Path::new(".")).to_path_buf()
    };

    let mut images: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        })
        .collect();

    images.sort();
    images
}

fn find_index(images: &[PathBuf], target: &Path) -> usize {
    images.iter().position(|p| p == target).unwrap_or(0)
}

const BAR_HEIGHT: u32 = 20;
const BAR_BG: u32 = 0x222222;
const BAR_FG: u32 = 0xcccccc;

/// canvas background; images are composited over this at decode time
const BG: u32 = 0x1a1a1a;

const FONT_W: u32 = 6;
const FONT_H: u32 = 10;

#[rustfmt::skip]
const FONT_DATA: &[u8] = include_bytes!("font6x10.bin");

fn draw_char(buf: &mut [u32], buf_w: u32, x: u32, y: u32, ch: char, fg: u32) {
    let idx = ch as usize;
    if !(32..128).contains(&idx) {
        return;
    }
    let glyph = &FONT_DATA[(idx - 32) * FONT_H as usize..][..FONT_H as usize];
    for (row, &bits) in glyph.iter().enumerate() {
        let py = y + row as u32;
        for col in 0..FONT_W {
            if bits & (0x80 >> col) != 0 {
                let px = x + col;
                if px >= buf_w {
                    break; // clip at the row edge instead of wrapping into it
                }
                let off = (py * buf_w + px) as usize;
                if off < buf.len() {
                    buf[off] = fg;
                }
            }
        }
    }
}

fn draw_text(buf: &mut [u32], buf_w: u32, x: u32, y: u32, text: &str, fg: u32) {
    for (i, ch) in text.chars().enumerate() {
        draw_char(buf, buf_w, x + i as u32 * FONT_W, y, ch, fg);
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Convert RGBA to packed 0xRRGGBB composited over the background, so
/// transparent regions show the background rather than black and
/// semi-transparent edges get no dark halo.
fn rgba_to_packed(rgba: &[u8]) -> Vec<u32> {
    let (bgr, bgg, bgb) = ((BG >> 16) & 0xff, (BG >> 8) & 0xff, BG & 0xff);
    rgba.chunks_exact(4)
        .map(|c| {
            let a = c[3] as u32;
            if a == 255 {
                return ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
            }
            if a == 0 {
                return BG;
            }
            let inv = 255 - a;
            let r = (c[0] as u32 * a + bgr * inv) / 255;
            let g = (c[1] as u32 * a + bgg * inv) / 255;
            let b = (c[2] as u32 * a + bgb * inv) / 255;
            r << 16 | g << 8 | b
        })
        .collect()
}

/// Quote a string for safe interpolation into a `sh -c` command line.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A box-filtered halving level of a frame; mips[0] is 1/2 scale.
/// Kept next to the full-res pixels so downscaling samples a level close
/// to the output size instead of nearest-neighbour subsampling (moiré).
#[derive(Clone)]
struct Mip {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

/// 2x2 box average into a halving level; odd edges duplicate the last pixel.
fn box2(src: &[u32], w: u32, h: u32) -> (u32, u32, Vec<u32>) {
    let nw = w.div_ceil(2).max(1);
    let nh = h.div_ceil(2).max(1);
    let mut dst = vec![0u32; (nw * nh) as usize];
    for y in 0..nh {
        let y0 = ((y * 2).min(h - 1) as usize) * w as usize;
        let y1 = ((y * 2 + 1).min(h - 1) as usize) * w as usize;
        for x in 0..nw {
            let x0 = (x * 2).min(w - 1) as usize;
            let x1 = (x * 2 + 1).min(w - 1) as usize;
            let (a, b, c, d) = (src[y0 + x0], src[y0 + x1], src[y1 + x0], src[y1 + x1]);
            let avg = |sh: u32| {
                ((a >> sh) & 0xff) + ((b >> sh) & 0xff) + ((c >> sh) & 0xff) + ((d >> sh) & 0xff)
            };
            dst[(y * nw + x) as usize] = (avg(16) / 4) << 16 | (avg(8) / 4) << 8 | avg(0) / 4;
        }
    }
    (nw, nh, dst)
}

/// Halving chain from 1/2 scale down; stops once both dims reach 8.
fn build_mips(width: u32, height: u32, pixels: &[u32]) -> Vec<Mip> {
    if width <= 16 && height <= 16 {
        return Vec::new(); // sampler can bilinear from full-res fine at this size
    }
    let (w, h, p) = box2(pixels, width, height);
    let mut level = Mip {
        width: w,
        height: h,
        pixels: p,
    };
    let mut mips = Vec::new();
    loop {
        let (w, h, p) = box2(&level.pixels, level.width, level.height);
        mips.push(std::mem::replace(
            &mut level,
            Mip {
                width: w,
                height: h,
                pixels: p,
            },
        ));
        if level.width <= 8 && level.height <= 8 {
            mips.push(level);
            break;
        }
    }
    mips
}

/// deepest level (full-res counts as level 0) still covering target size
fn pick_level<'a>(
    width: u32,
    height: u32,
    pixels: &'a [u32],
    mips: &'a [Mip],
    target_w: i64,
    target_h: i64,
) -> (usize, usize, &'a [u32]) {
    let mut best = (width as usize, height as usize, pixels);
    for m in mips {
        if m.width as i64 >= target_w && m.height as i64 >= target_h {
            best = (m.width as usize, m.height as usize, &m.pixels);
        } else {
            break;
        }
    }
    best
}

/// Draw one frame of an image into the framebuffer, choosing the best
/// sampler for the scale: nearest at ~1:1 and deliberate big upscale,
/// bilinear from the deepest covering mip otherwise.
#[allow(clippy::too_many_arguments)]
fn blit_scaled(
    buf: &mut [u32],
    buf_w: usize,
    x_start: u32,
    y_start: u32,
    x_end: u32,
    y_end: u32,
    img_w: u32,
    img_h: u32,
    pixels: &[u32],
    mips: &[Mip],
    scale: f64,
    scaled_w: i64,
    scaled_h: i64,
    ox: i64,
    oy: i64,
) {
    let x_span = x_end.saturating_sub(x_start) as usize;
    let y_span = y_end.saturating_sub(y_start) as usize;
    if x_span == 0 || y_span == 0 || !(scale.is_finite() && scale > 0.0) {
        return;
    }

    // deepest mip level still covering the target size: sampling from
    // there means at most ~2x reduction per output pixel, where bilinear
    // behaves like a box filter and kills the moiré a full-res nearest
    // subsample would show
    let (lv_w, lv_h, lv) = pick_level(img_w, img_h, pixels, mips, scaled_w, scaled_h);

    // level-space -> screen fixed-point inverse scale (16.16); sampling at
    // dest pixel centres adds half a source pixel, removes half a dest pixel
    let level_scale = scaled_w.max(1) as f64 / lv_w.max(1) as f64;
    let inv_scale = (lv_w.max(1) as f64 / (img_w as f64 * scale) * 65536.0)
        .clamp(1.0, (u32::MAX >> 1) as f64) as i64;
    let half_px = (inv_scale >> 1) - 32768;

    // nearest is crisp at ~1:1 and deliberately blocky at big zoom-in;
    // everything else bilinear
    let nearest = (0.98..=1.02).contains(&level_scale) || level_scale >= 2.0;

    if nearest {
        let src_x_table: Vec<usize> = (x_start..x_end)
            .map(|wx| (((wx as i64 - ox) * inv_scale) >> 16).clamp(0, lv_w as i64 - 1) as usize)
            .collect();
        for wy in y_start..y_end {
            let src_y = (((wy as i64 - oy) * inv_scale) >> 16).clamp(0, lv_h as i64 - 1) as usize;
            let row_offset = src_y * lv_w;
            let buf_row = wy as usize * buf_w;

            // SAFETY: x_start..x_end is clamped to 0..buf_w by the caller,
            // so buf_row + x < buf.len(); src_x/src_y are clamped into the
            // level, so row_offset + src_x < lv.len()
            unsafe {
                for (i, &src_x) in src_x_table.iter().enumerate() {
                    let px = *lv.get_unchecked(row_offset + src_x);
                    *buf.get_unchecked_mut(buf_row + x_start as usize + i) = px;
                }
            }
        }
    } else {
        // x taps/weights depend only on the x span, so compute them once
        // per blit, not per row
        let taps: Vec<(usize, usize, u32)> = (x_start..x_end)
            .map(|wx| {
                let v = ((wx as i64 - ox) * inv_scale + half_px).max(0);
                let x0 = ((v >> 16) as usize).min(lv_w - 1);
                (x0, (x0 + 1).min(lv_w - 1), ((v >> 8) & 0xff) as u32)
            })
            .collect();
        let lerp = |p: u32, q: u32, t: u32| -> u32 {
            let r = (((p >> 16) & 0xff) * (255 - t) + ((q >> 16) & 0xff) * t) / 255;
            let g = (((p >> 8) & 0xff) * (255 - t) + ((q >> 8) & 0xff) * t) / 255;
            let b = ((p & 0xff) * (255 - t) + (q & 0xff) * t) / 255;
            r << 16 | g << 8 | b
        };
        for wy in y_start..y_end {
            let v = ((wy as i64 - oy) * inv_scale + half_px).max(0);
            let y0 = ((v >> 16) as usize).min(lv_h - 1);
            let yw = ((v >> 8) & 0xff) as u32;
            let y1 = (y0 + 1).min(lv_h - 1);
            let r0 = y0 * lv_w;
            let r1 = y1 * lv_w;
            let dst = wy as usize * buf_w + x_start as usize;
            for (i, &(x0, x1, xwt)) in taps.iter().enumerate() {
                let top = lerp(lv[r0 + x0], lv[r0 + x1], xwt);
                let bot = lerp(lv[r1 + x0], lv[r1 + x1], xwt);
                buf[dst + i] = lerp(top, bot, yw);
            }
        }
    }
}

struct Frame {
    pixels: Vec<u32>,
    mips: Vec<Mip>,
    delay: Duration,
}

struct DecodedImage {
    width: u32,
    height: u32,
    frames: Vec<Frame>,
    file_size: u64,
}

impl DecodedImage {
    fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }
}

fn try_load_animated(path: &Path) -> Option<DecodedImage> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let data = std::fs::read(path).ok()?;
    let cursor = std::io::Cursor::new(&data);

    match ext.as_str() {
        "gif" => {
            let decoder = image::codecs::gif::GifDecoder::new(cursor).ok()?;
            let raw_frames: Vec<_> = decoder.into_frames().collect_frames().ok()?;
            if raw_frames.len() <= 1 {
                return None; // not animated, fall back to normal loader
            }
            let width = raw_frames[0].buffer().width();
            let height = raw_frames[0].buffer().height();
            let frames = raw_frames
                .into_iter()
                .map(|f| {
                    let (numer, denom) = f.delay().numer_denom_ms();
                    let ms = if denom == 0 {
                        100
                    } else {
                        (numer / denom).max(10)
                    };
                    let pixels = rgba_to_packed(f.buffer().as_raw());
                    let mips = build_mips(width, height, &pixels);
                    Frame {
                        pixels,
                        mips,
                        delay: Duration::from_millis(ms as u64),
                    }
                })
                .collect();
            Some(DecodedImage {
                width,
                height,
                frames,
                file_size,
            })
        }
        "png" => {
            let decoder = image::codecs::png::PngDecoder::new(cursor).ok()?;
            if !decoder.is_apng().unwrap_or(false) {
                return None;
            }
            let raw_frames: Vec<_> = decoder.apng().ok()?.into_frames().collect_frames().ok()?;
            if raw_frames.len() <= 1 {
                return None;
            }
            let width = raw_frames[0].buffer().width();
            let height = raw_frames[0].buffer().height();
            let frames = raw_frames
                .into_iter()
                .map(|f| {
                    let (numer, denom) = f.delay().numer_denom_ms();
                    let ms = if denom == 0 {
                        100
                    } else {
                        (numer / denom).max(10)
                    };
                    let pixels = rgba_to_packed(f.buffer().as_raw());
                    let mips = build_mips(width, height, &pixels);
                    Frame {
                        pixels,
                        mips,
                        delay: Duration::from_millis(ms as u64),
                    }
                })
                .collect();
            Some(DecodedImage {
                width,
                height,
                frames,
                file_size,
            })
        }
        _ => None,
    }
}

fn load_image(path: &Path) -> Option<DecodedImage> {
    // try animated first
    if let Some(animated) = try_load_animated(path) {
        return Some(animated);
    }

    // static image — guess format from content, not extension, so
    // misnamed files (e.g. webp saved as .jpg) still load
    let reader = image::ImageReader::open(path).ok()?;
    let reader = reader.with_guessed_format().ok()?;
    // decode through the decoder so EXIF orientation can be applied —
    // decode() alone leaves phone photos sideways
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    let (width, height) = (img.width(), img.height());
    let rgba = img.to_rgba8();
    let pixels = rgba_to_packed(rgba.as_raw());
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mips = build_mips(width, height, &pixels);
    Some(DecodedImage {
        width,
        height,
        frames: vec![Frame {
            pixels,
            mips,
            delay: Duration::ZERO,
        }],
        file_size,
    })
}

/// Cache key: the file plus its mtime, so an image modified in place by an
/// external command can never return a stale hit, and list changes (delete)
/// never shift meaning between key and file.
type CacheKey = (PathBuf, Option<SystemTime>);

fn cache_key(path: &Path) -> CacheKey {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    (path.to_path_buf(), mtime)
}

/// decoded images kept warm while browsing; entries are Arc'd so the
/// on-screen image and the cache share pixels
const CACHE_CAP: usize = 6;
/// how many positions around the current image to keep preloaded
const PRELOAD_AHEAD: usize = 2;

struct ImageCache {
    entries: HashMap<CacheKey, Arc<DecodedImage>>,
    last_use: HashMap<CacheKey, u64>,
    tick: u64,
    tx: mpsc::Sender<(usize, CacheKey)>,
    rx: mpsc::Receiver<(CacheKey, Arc<DecodedImage>)>,
    /// index currently on screen — the decode thread uses it to prioritise
    /// and to drop requests that navigation has left behind
    current: Arc<Mutex<usize>>,
}

impl ImageCache {
    fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<(usize, CacheKey)>();
        let (res_tx, res_rx) = mpsc::channel::<(CacheKey, Arc<DecodedImage>)>();
        let current = Arc::new(Mutex::new(usize::MAX / 2));
        let bg_current = Arc::clone(&current);

        std::thread::spawn(move || {
            while let Ok(first) = req_rx.recv() {
                // collapse the whole queue, decode what is closest to the
                // visible image first, and skip work navigation outran
                let mut batch = vec![first];
                while let Ok(r) = req_rx.try_recv() {
                    batch.push(r);
                }
                let cur = *bg_current.lock().unwrap();
                batch.sort_by_key(|(idx, _)| idx.abs_diff(cur));
                batch.dedup_by(|(a, _), (b, _)| a == b);
                for (idx, key) in batch {
                    if idx.abs_diff(cur) > PRELOAD_AHEAD + 2 {
                        continue;
                    }
                    if let Some(img) = load_image(&key.0) {
                        let _ = res_tx.send((key, Arc::new(img)));
                    }
                }
            }
        });

        Self {
            entries: HashMap::new(),
            last_use: HashMap::new(),
            tick: 0,
            tx: req_tx,
            rx: res_rx,
            current,
        }
    }

    fn touch(&mut self, key: &CacheKey) {
        self.tick += 1;
        self.last_use.insert(key.clone(), self.tick);
    }

    fn insert(&mut self, key: CacheKey, img: Arc<DecodedImage>) {
        self.touch(&key);
        self.entries.insert(key, img);
        if self.entries.len() > CACHE_CAP {
            // evict the least recently used entry that is not still on
            // screen; if everything is pinned, let the cap slide one
            let victim = self
                .last_use
                .iter()
                .filter(|(k, _)| {
                    self.entries
                        .get(*k)
                        .is_some_and(|v| Arc::strong_count(v) == 1)
                })
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone());
            if let Some(k) = victim {
                self.entries.remove(&k);
                self.last_use.remove(&k);
            }
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<Arc<DecodedImage>> {
        while let Ok((k, img)) = self.rx.try_recv() {
            self.insert(k, img);
        }
        let img = self.entries.get(key).cloned()?;
        self.touch(key);
        Some(img)
    }

    fn request_preload(&mut self, images: &[PathBuf], center: usize) {
        *self.current.lock().unwrap() = center;
        let len = images.len();
        for delta in [1isize, -1, 2, -2] {
            let idx = (center as isize + delta).rem_euclid(len as isize) as usize;
            if idx == center {
                continue;
            }
            let key = cache_key(&images[idx]);
            if self.entries.contains_key(&key) {
                continue;
            }
            let _ = self.tx.send((idx, key));
        }
    }
}

/// A configured key command that is still running in the background.
struct RunningCommand {
    child: std::process::Child,
    path: PathBuf,
    mtime_before: Option<SystemTime>,
    expanded: String,
}

struct App {
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    images: Vec<PathBuf>,
    index: usize,
    current: Option<Arc<DecodedImage>>,
    image_cache: ImageCache,
    config: Config,
    running: Vec<RunningCommand>,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
    frame_index: usize,
    frame_start: Instant,
}

impl App {
    fn new(images: Vec<PathBuf>, index: usize, config: Config) -> Self {
        Self {
            window: None,
            context: None,
            surface: None,
            images,
            index,
            current: None,
            image_cache: ImageCache::new(),
            config,
            running: Vec::new(),
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            frame_index: 0,
            frame_start: Instant::now(),
        }
    }

    fn load_current(&mut self) {
        // tell the decode thread where we are before any (blocking) decode,
        // so it can drop requests for images we sailed past
        *self.image_cache.current.lock().unwrap() = self.index;
        self.current = None;
        if let Some(path) = self.images.get(self.index).cloned() {
            let key = cache_key(&path);
            if let Some(img) = self.image_cache.get(&key) {
                self.current = Some(img);
            } else if let Some(img) = load_image(&path) {
                let img = Arc::new(img);
                self.image_cache.insert(key, img.clone());
                self.current = Some(img);
            }
        }
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.frame_index = 0;
        self.frame_start = Instant::now();
        self.update_title();
        self.request_redraw();
        self.image_cache.request_preload(&self.images, self.index);
    }

    fn update_title(&self) {
        if let Some(window) = &self.window {
            if let Some(path) = self.images.get(self.index) {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                window.set_title(&format!(
                    "rxiv - {} [{}/{}]",
                    name,
                    self.index + 1,
                    self.images.len()
                ));
            } else {
                window.set_title("rxiv - no images");
            }
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn advance_animation(&mut self) -> Option<Instant> {
        let img = self.current.as_ref()?;
        if !img.is_animated() {
            return None;
        }
        let frame = &img.frames[self.frame_index % img.frames.len()];
        let deadline = self.frame_start + frame.delay;
        let now = Instant::now();
        if now >= deadline {
            self.frame_index = (self.frame_index + 1) % img.frames.len();
            self.frame_start = now;
            self.request_redraw();
            let next_delay = img.frames[self.frame_index].delay;
            Some(now + next_delay)
        } else {
            Some(deadline)
        }
    }

    fn do_render(&mut self) {
        let Some(window) = &self.window else { return };
        let Some(surface) = &mut self.surface else {
            return;
        };

        let size = window.inner_size();
        let w = size.width;
        let h = size.height;
        if w == 0 || h == 0 {
            return;
        }

        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .ok();

        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };

        buffer.fill(BG);

        let img_area_h = h.saturating_sub(BAR_HEIGHT);

        if let Some(img) = &self.current {
            let frame = &img.frames[self.frame_index % img.frames.len()];
            let iw = img.width as f64;
            let ih = img.height as f64;
            let ww = w as f64;
            let wh = img_area_h as f64;

            let base_scale = (ww / iw).min(wh / ih).min(1.0);
            let scale = base_scale * self.zoom;

            let scaled_w = (iw * scale) as i64;
            let scaled_h = (ih * scale) as i64;

            let ox = ((ww - scaled_w as f64) / 2.0 + self.pan_x) as i64;
            let oy = ((wh - scaled_h as f64) / 2.0 + self.pan_y) as i64;

            let x_start = ox.max(0) as u32;
            let y_start = oy.max(0) as u32;
            let x_end = (ox + scaled_w).max(0).min(w as i64) as u32;
            let y_end = (oy + scaled_h).max(0).min(img_area_h as i64) as u32;

            blit_scaled(
                buffer.as_mut(),
                w as usize,
                x_start,
                y_start,
                x_end,
                y_end,
                img.width,
                img.height,
                &frame.pixels,
                &frame.mips,
                scale,
                scaled_w,
                scaled_h,
                ox,
                oy,
            );
        }

        // draw status bar background
        let bar_start = img_area_h as usize * w as usize;
        let bar_end = h as usize * w as usize;
        buffer[bar_start..bar_end].fill(BAR_BG);

        // draw status bar text
        if let Some(path) = self.images.get(self.index) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let status = if let Some(img) = &self.current {
                let anim_info = if img.is_animated() {
                    format!(" | {} frames", img.frames.len())
                } else {
                    String::new()
                };
                format!(
                    " {} | {} | {}x{}{} | {}/{}",
                    name,
                    format_size(img.file_size),
                    img.width,
                    img.height,
                    anim_info,
                    self.index + 1,
                    self.images.len()
                )
            } else {
                format!(" {} | {}/{}", name, self.index + 1, self.images.len())
            };
            let text_y = img_area_h + (BAR_HEIGHT.saturating_sub(FONT_H)) / 2;
            draw_text(&mut buffer, w, 4, text_y, &status, BAR_FG);
        }

        buffer.present().ok();
    }

    fn navigate(&mut self, delta: isize) {
        if self.images.is_empty() {
            return;
        }
        let len = self.images.len() as isize;
        let new = (self.index as isize + delta).rem_euclid(len) as usize;
        if new != self.index {
            self.index = new;
            self.load_current();
        }
    }

    fn delete_current(&mut self) {
        if self.images.is_empty() {
            return;
        }
        let path = self.images[self.index].clone();
        if std::fs::remove_file(&path).is_err() {
            return;
        }
        self.images.remove(self.index);
        if self.images.is_empty() {
            self.current = None;
            self.update_title();
            self.request_redraw();
            return;
        }
        if self.index >= self.images.len() {
            self.index = self.images.len() - 1;
        }
        self.load_current();
    }

    fn run_key_command(&mut self, cmd: &str) {
        if self.images.is_empty() {
            return;
        }
        let path = self.images[self.index].clone();
        // quote the path so names with spaces survive shell expansion
        let expanded = cmd.replace("$file", &shell_quote(&path.to_string_lossy()));
        let mtime_before = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        // spawn without waiting so launching e.g. gimp doesn't freeze the UI;
        // reap_commands() picks up the result later
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(&expanded)
            .spawn()
        {
            Ok(child) => self.running.push(RunningCommand {
                child,
                path,
                mtime_before,
                expanded,
            }),
            Err(e) => eprintln!("failed to run command: {}: {}", expanded, e),
        }
    }

    /// Check finished background commands: drop files the command
    /// removed/moved, and reload files it modified in place (e.g. rotate).
    fn reap_commands(&mut self) {
        if self.running.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.running.len() {
            let done = match self.running[i].child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        eprintln!(
                            "command exited with {}: {}",
                            status, self.running[i].expanded
                        );
                    }
                    true
                }
                Ok(None) => false,
                Err(_) => true,
            };
            if !done {
                i += 1;
                continue;
            }
            let mut rc = self.running.swap_remove(i);
            let _ = rc.child.wait(); // reap
            if !rc.path.exists() {
                // file was removed/moved — drop it from the list
                if let Some(pos) = self.images.iter().position(|p| p == &rc.path) {
                    self.images.remove(pos);
                    if self.images.is_empty() {
                        self.current = None;
                        self.update_title();
                        self.request_redraw();
                        continue;
                    }
                    if pos == self.index {
                        if self.index >= self.images.len() {
                            self.index = self.images.len() - 1;
                        }
                        self.load_current();
                    } else if pos < self.index {
                        self.index -= 1;
                    }
                }
            } else {
                // file may have been modified in place (rotate, mogrify, ...)
                let now = std::fs::metadata(&rc.path).and_then(|m| m.modified()).ok();
                let is_current = self.images.get(self.index).is_some_and(|p| p == &rc.path);
                if is_current && now != rc.mtime_before {
                    self.reload_current();
                }
            }
        }
    }

    /// Reload the current image in place, keeping zoom/pan — used when an
    /// external command has modified the file. The cache is keyed by mtime,
    /// so the old decode can no longer produce a hit.
    fn reload_current(&mut self) {
        self.current = None;
        if let Some(path) = self.images.get(self.index).cloned() {
            let key = cache_key(&path);
            if let Some(img) = self.image_cache.get(&key) {
                self.current = Some(img);
            } else if let Some(img) = load_image(&path) {
                let img = Arc::new(img);
                self.image_cache.insert(key, img.clone());
                self.current = Some(img);
            }
            self.frame_index = 0;
            self.frame_start = Instant::now();
        }
        self.request_redraw();
    }

    fn toggle_fullscreen(&self) {
        if let Some(window) = &self.window {
            if window.fullscreen().is_some() {
                window.set_fullscreen(None);
            } else {
                window.set_fullscreen(Some(Fullscreen::Borderless(None)));
            }
        }
    }

    fn clamp_pan(&mut self) {
        let Some(window) = &self.window else { return };
        let Some(img) = &self.current else { return };

        let size = window.inner_size();
        let ww = size.width as f64;
        let wh = size.height.saturating_sub(BAR_HEIGHT) as f64;
        let iw = img.width as f64;
        let ih = img.height as f64;

        let base_scale = (ww / iw).min(wh / ih).min(1.0);
        let scale = base_scale * self.zoom;
        let scaled_w = iw * scale;
        let scaled_h = ih * scale;

        let max_pan_x = ((scaled_w - ww) / 2.0).max(0.0);
        let max_pan_y = ((scaled_h - wh) / 2.0).max(0.0);
        self.pan_x = self.pan_x.clamp(-max_pan_x, max_pan_x);
        self.pan_y = self.pan_y.clamp(-max_pan_y, max_pan_y);
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        // preload image to determine window size (and warm the cache so the
        // first navigation doesn't re-decode it)
        if self.current.is_none() {
            if let Some(path) = self.images.get(self.index) {
                let key = cache_key(path);
                self.current = load_image(path).map(|img| {
                    let img = Arc::new(img);
                    self.image_cache.insert(key, img.clone());
                    img
                });
            }
        }

        // determine window size: image dims clamped to monitor, or 800x600 fallback
        let monitor_size = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .map(|m| m.size());

        let (win_w, win_h) = if let Some(img) = &self.current {
            let iw = img.width as f64;
            let ih = img.height as f64 + BAR_HEIGHT as f64;
            if let Some(mon) = monitor_size {
                // leave some margin (90% of screen)
                let max_w = mon.width as f64 * 0.9;
                let max_h = mon.height as f64 * 0.9;
                let scale = (max_w / iw).min(max_h / ih).min(1.0);
                ((iw * scale) as u32, (ih * scale) as u32)
            } else {
                ((iw as u32).min(1920), (ih as u32).min(1080))
            }
        } else {
            (800, 600)
        };

        // win_w/win_h are physical pixels (monitor size is physical), so pass
        // them as a PhysicalSize — LogicalSize makes the window scale-factor
        // times too big on HiDPI displays
        let attrs = Window::default_attributes()
            .with_title("rxiv")
            .with_name("rxiv", "rxiv")
            .with_inner_size(PhysicalSize::new(win_w, win_h));

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        let context =
            Context::new(Arc::clone(&window)).expect("failed to create softbuffer context");
        let surface =
            Surface::new(&context, Arc::clone(&window)).expect("failed to create surface");

        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
        self.update_title();
        self.request_redraw();
        // start preloading neighbours of the initial image too
        self.image_cache.request_preload(&self.images, self.index);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.reap_commands();
        let anim = self.advance_animation();
        // poll soon while background commands run, so their file changes
        // show up without waiting for user input
        let poll = if self.running.is_empty() {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(300))
        };
        match (anim, poll) {
            (Some(a), Some(p)) => event_loop.set_control_flow(ControlFlow::WaitUntil(a.min(p))),
            (Some(a), None) => event_loop.set_control_flow(ControlFlow::WaitUntil(a)),
            (None, Some(p)) => event_loop.set_control_flow(ControlFlow::WaitUntil(p)),
            (None, None) => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::RedrawRequested => self.do_render(),

            WindowEvent::MouseWheel { delta, .. } => {
                let y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(pos) => pos.y / 40.0,
                };
                if y > 0.0 {
                    self.navigate(-1);
                } else if y < 0.0 {
                    self.navigate(1);
                }
            }

            WindowEvent::Resized(_) => {
                self.clamp_pan();
                self.request_redraw();
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => match logical_key {
                Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => self.navigate(1),
                Key::Named(NamedKey::PageUp) | Key::Named(NamedKey::Backspace) => self.navigate(-1),
                Key::Named(NamedKey::Delete) => self.delete_current(),
                Key::Named(NamedKey::Escape) => {
                    if self
                        .window
                        .as_ref()
                        .is_some_and(|w| w.fullscreen().is_some())
                    {
                        self.toggle_fullscreen();
                    } else {
                        event_loop.exit();
                    }
                }
                Key::Named(NamedKey::ArrowUp) => {
                    self.pan_y += 50.0;
                    self.clamp_pan();
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowDown) => {
                    self.pan_y -= 50.0;
                    self.clamp_pan();
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    self.pan_x += 50.0;
                    self.clamp_pan();
                    self.request_redraw();
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.pan_x -= 50.0;
                    self.clamp_pan();
                    self.request_redraw();
                }
                Key::Character(ref ch) => match ch.as_str() {
                    "f" => self.toggle_fullscreen(),
                    "q" => event_loop.exit(),
                    "+" | "=" => {
                        self.zoom = (self.zoom * 1.25).min(32.0);
                        self.clamp_pan();
                        self.request_redraw();
                    }
                    "-" => {
                        self.zoom = (self.zoom / 1.25).max(0.1);
                        self.clamp_pan();
                        self.request_redraw();
                    }
                    "0" => {
                        self.zoom = 1.0;
                        self.pan_x = 0.0;
                        self.pan_y = 0.0;
                        self.request_redraw();
                    }
                    key => {
                        if let Some(cmd) = self.config.command_for_key(key) {
                            let cmd = cmd.to_owned();
                            self.run_key_command(&cmd);
                        }
                    }
                },
                _ => {}
            },

            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let target = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let target = std::fs::canonicalize(&target).unwrap_or(target);

    let images = collect_images(&target);
    let index = if target.is_file() {
        find_index(&images, &target)
    } else {
        0
    };

    if images.is_empty() {
        eprintln!("no images found");
        std::process::exit(1);
    }

    let config = Config::load();

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App::new(images, index, config);
    event_loop.run_app(&mut app).expect("event loop error");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn exif_orientation_is_applied() {
        // stored as 6x2 with orientation "Rotate 90 CW" -> must load as 2x6
        let img = load_image(&fixture("oriented.jpg")).expect("fixture loads");
        assert_eq!((img.width, img.height), (2, 6));
    }

    #[test]
    fn shell_quotes_paths() {
        assert_eq!(shell_quote("/a/b c.png"), "'/a/b c.png'");
        assert_eq!(shell_quote("it's.png"), r"'it'\''s.png'");
    }

    #[test]
    fn alpha_composites_over_background() {
        let out = rgba_to_packed(&[255, 0, 0, 128, 9, 9, 9, 0, 1, 2, 3, 255]);
        // red toward BG at a=128: (255*128 + 0x1a*127) / 255 = 140
        let r = (out[0] >> 16) & 0xff;
        assert!((138..=142).contains(&r), "r={r}");
        assert_eq!(out[1], BG, "fully transparent maps to background");
        assert_eq!(out[2], 0x010203, "opaque pixels pass through");
    }
    fn dummy_image() -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: 1,
            height: 1,
            frames: vec![Frame {
                pixels: vec![0],
                mips: Vec::new(),
                delay: Duration::ZERO,
            }],
            file_size: 0,
        })
    }

    #[test]
    fn cache_evicts_least_recently_used_beyond_cap() {
        let mut c = ImageCache::new();
        let keys: Vec<CacheKey> = (0..CACHE_CAP + 2)
            .map(|i| (PathBuf::from(format!("img{i}")), None))
            .collect();
        for k in &keys {
            c.insert(k.clone(), dummy_image());
        }
        assert_eq!(c.entries.len(), CACHE_CAP);
        assert!(!c.entries.contains_key(&keys[0]), "oldest evicted");
        assert!(c.entries.contains_key(&keys[keys.len() - 1]));
        // touching an entry protects it from the next eviction
        let protect = keys[2].clone();
        assert!(c.get(&protect).is_some());
        c.insert((PathBuf::from("another"), None), dummy_image());
        assert!(c.entries.contains_key(&protect), "recently used survives");
        assert!(
            !c.entries.contains_key(&keys[1]),
            "next-oldest evicted instead"
        );
    }

    #[test]
    fn cache_key_changes_when_file_modified() {
        let path = fixture("alpha.png");
        let before = cache_key(&path);
        let orig = before.1.expect("fixture has mtime");
        let later = std::time::SystemTime::UNIX_EPOCH
            + orig.duration_since(std::time::UNIX_EPOCH).unwrap()
            + Duration::from_secs(10);
        let times = std::fs::FileTimes::new().set_modified(later);
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_times(times).unwrap();
        drop(f);
        assert_ne!(cache_key(&path), before, "modified file must miss cache");
        // restore so the fixture stays stable
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(orig))
            .unwrap();
    }
    #[test]
    fn box2_averages_2x2_and_handles_odd_dims() {
        // 3x3 -> 2x2
        let src = vec![
            0x000000, 0x000000, 0xffffff, 0x000000, 0x000000, 0xffffff, 0x808080, 0x808080,
            0x000000,
        ];
        let (w, h, dst) = box2(&src, 3, 3);
        assert_eq!((w, h), (2, 2));
        assert_eq!(dst[0], 0x000000);
        // odd edges duplicate the last pixel, so the right/bottom cells
        // sample the doubled column/row
        assert_eq!(dst[1], 0xffffff);
        assert_eq!(dst[2], 0x808080);
        assert_eq!(dst[3], 0x000000);
        // even dims get true 2x2 averaging
        let (_, _, dst) = box2(&[0x000000, 0xffffff, 0x000000, 0xffffff], 2, 2);
        assert_eq!(dst[0], 0x7f7f7f, "(0+255+0+255)/4");
    }

    #[test]
    fn mips_halve_until_small() {
        let mips = build_mips(64, 32, &vec![0x123456; 64 * 32]);
        assert_eq!((mips[0].width, mips[0].height), (32, 16));
        let last = mips.last().unwrap();
        assert!(
            last.width <= 8 && last.height <= 8,
            "chain terminates small"
        );
        assert!(
            build_mips(16, 16, &vec![0u32; 256]).is_empty(),
            "tiny images skip the chain"
        );
    }

    #[test]
    fn pick_level_selects_deepest_covering_level() {
        let pixels = vec![0u32; 64 * 32];
        let mips = build_mips(64, 32, &pixels);
        // upscale -> full res
        let (w, h, _) = pick_level(64, 32, &pixels, &mips, 100, 50);
        assert_eq!((w, h), (64, 32));
        // fit-downscale to 30x15 -> the 32x16 level, not full res
        let (w, h, _) = pick_level(64, 32, &pixels, &mips, 30, 15);
        assert_eq!((w, h), (32, 16));
    }
    #[test]
    fn blit_1_to_1_copies_exactly() {
        let img: Vec<u32> = (0..8).collect(); // 4x2: 0..3 / 4..7
        let mut buf = vec![BG; 6 * 4];
        blit_scaled(&mut buf, 6, 2, 1, 6, 3, 4, 2, &img, &[], 1.0, 4, 2, 2, 1);
        assert_eq!(&buf[8..12], &[0, 1, 2, 3]);
        assert_eq!(&buf[14..18], &[4, 5, 6, 7]);
        assert!(
            buf.iter()
                .enumerate()
                .all(|(i, &p)| (8..12).contains(&i) || (14..18).contains(&i) || p == BG)
        );
    }

    #[test]
    fn blit_bilinear_upscale_stays_in_source_range() {
        let img = vec![0x000000, 0xffffff, 0xffffff, 0x000000]; // 2x2 checker
        let mut buf = vec![BG; 3 * 3];
        // 1.5x -> 3x3 bilinear upscale (level_scale 1.5 selects bilinear)
        blit_scaled(&mut buf, 3, 0, 0, 3, 3, 2, 2, &img, &[], 1.5, 3, 3, 0, 0);
        for &p in &buf {
            for sh in [16, 8, 0] {
                let c = (p >> sh) & 0xff;
                assert!(c <= 255, "no overshoot");
            }
        }
        // interior pixel must be a real blend of black and white
        let mid = buf[4];
        assert!(mid != BG && mid != 0 && mid != 0xffffff, "mid={mid:#x}");
    }

    #[test]
    fn blit_survives_full_zoom_and_pan_sweep() {
        let base: Vec<u32> = (0..64 * 32)
            .map(|i| (i as u32).wrapping_mul(2654435761) & 0xffffff)
            .collect();
        let mips = build_mips(64, 32, &base);
        let (bw, bh) = (90usize, 70usize);
        let mut buf = vec![BG; bw * bh];
        let area_h = 60i64; // leave a bar
        for &zoom in &[
            0.1, 0.2, 0.5, 0.7, 0.97, 1.0, 1.02, 1.3, 1.9, 2.0, 3.7, 8.0, 32.0,
        ] {
            let scale = (bw as f64 / 64.0).min(area_h as f64 / 32.0).min(1.0) * zoom;
            let sw = (64.0 * scale) as i64;
            let sh = (32.0 * scale) as i64;
            for &(pan_x, pan_y) in &[(0.0, 0.0), (500.0, 500.0), (-500.0, -500.0), (47.3, -12.9)] {
                let ox = ((bw as f64 - sw as f64) / 2.0 + pan_x) as i64;
                let oy = ((area_h as f64 - sh as f64) / 2.0 + pan_y) as i64;
                let x0 = ox.max(0) as u32;
                let y0 = oy.max(0) as u32;
                let x1 = (ox + sw).max(0).min(bw as i64) as u32;
                let y1 = (oy + sh).max(0).min(area_h) as u32;
                buf.fill(BG);
                blit_scaled(
                    &mut buf, bw, x0, y0, x1, y1, 64, 32, &base, &mips, scale, sw, sh, ox, oy,
                );
                // a pan that pushes the image fully off-canvas (beyond what
                // clamp_pan allows in the app) must draw nothing, not wrap
                let on_screen = x1 > x0 && y1 > y0;
                let visible = buf[..area_h as usize * bw]
                    .iter()
                    .filter(|&&p| p != BG)
                    .count();
                assert_eq!(visible > 0, on_screen, "zoom {zoom} pan ({pan_x},{pan_y})");
            }
        }
    }
    #[test]
    #[ignore = "manual perf check: cargo test --release -- --ignored --nocapture"]
    fn bench_fullscreen_bilinear_blit() {
        let base: Vec<u32> = (0..4000 * 3000).map(|i| i as u32 & 0xffffff).collect();
        let t = Instant::now();
        let mips = build_mips(4000, 3000, &base);
        eprintln!("build_mips: {:?}", t.elapsed());
        let mut buf = vec![BG; 1920 * 1050];
        let scale: f64 = (1920.0f64 / 4000.0).min(1050.0 / 3000.0);
        let (sw, sh) = ((4000.0 * scale) as i64, (3000.0 * scale) as i64);
        let (ox, oy) = (((1920.0 - sw as f64) / 2.0) as i64, 0);
        let t = Instant::now();
        for _ in 0..10 {
            blit_scaled(
                &mut buf, 1920, 0, 0, sw as u32, sh as u32, 4000, 3000, &base, &mips, scale, sw,
                sh, ox, oy,
            );
        }
        eprintln!("blit 1920x{:?}: {:?}/frame", sh, t.elapsed() / 10);
    }
}
