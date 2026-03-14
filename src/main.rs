use image::AnimationDecoder;
use softbuffer::{Context, Surface};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "tiff", "tif", "webp", "ico", "pnm", "pbm", "pgm",
    "ppm", "qoi",
];

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

fn rgba_to_packed(rgba: &[u8]) -> Vec<u32> {
    rgba.chunks_exact(4)
        .map(|c| {
            let [r, g, b, a] = [c[0] as u32, c[1] as u32, c[2] as u32, c[3] as u32];
            let r = r * a / 255;
            let g = g * a / 255;
            let b = b * a / 255;
            r << 16 | g << 8 | b
        })
        .collect()
}

struct Frame {
    pixels: Vec<u32>,
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
                    Frame {
                        pixels: rgba_to_packed(f.buffer().as_raw()),
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
                    Frame {
                        pixels: rgba_to_packed(f.buffer().as_raw()),
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
    let img = reader.decode().ok()?;
    let (width, height) = (img.width(), img.height());
    let rgba = img.to_rgba8();
    let pixels = rgba_to_packed(rgba.as_raw());
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Some(DecodedImage {
        width,
        height,
        frames: vec![Frame {
            pixels,
            delay: Duration::ZERO,
        }],
        file_size,
    })
}

struct ImageCache {
    cache: HashMap<usize, DecodedImage>,
    tx: mpsc::Sender<(usize, PathBuf)>,
    rx: mpsc::Receiver<(usize, DecodedImage)>,
    pending: Arc<Mutex<usize>>, // generation counter to discard stale results
    generation: usize,
}

impl ImageCache {
    fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<(usize, PathBuf)>();
        let (res_tx, res_rx) = mpsc::channel::<(usize, DecodedImage)>();
        let pending = Arc::new(Mutex::new(0usize));
        let bg_pending = Arc::clone(&pending);

        std::thread::spawn(move || {
            while let Ok((idx, path)) = req_rx.recv() {
                let gen_before = *bg_pending.lock().unwrap();
                if let Some(img) = load_image(&path) {
                    let gen_after = *bg_pending.lock().unwrap();
                    if gen_before == gen_after {
                        let _ = res_tx.send((idx, img));
                    }
                }
            }
        });

        Self {
            cache: HashMap::new(),
            tx: req_tx,
            rx: res_rx,
            pending,
            generation: 0,
        }
    }

    fn drain_ready(&mut self) {
        while let Ok((idx, img)) = self.rx.try_recv() {
            self.cache.insert(idx, img);
        }
    }

    fn take(&mut self, idx: usize) -> Option<DecodedImage> {
        self.drain_ready();
        self.cache.remove(&idx)
    }

    fn request_preload(&mut self, images: &[PathBuf], center: usize) {
        self.generation += 1;
        *self.pending.lock().unwrap() = self.generation;
        self.cache.clear();
        // drain any stale results
        while self.rx.try_recv().is_ok() {}

        let len = images.len();
        for delta in [1isize, -1] {
            let idx = (center as isize + delta).rem_euclid(len as isize) as usize;
            if idx != center {
                let _ = self.tx.send((idx, images[idx].clone()));
            }
        }
    }
}

struct App {
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    images: Vec<PathBuf>,
    index: usize,
    current: Option<DecodedImage>,
    image_cache: ImageCache,
    zoom: f64,
    pan_x: f64,
    pan_y: f64,
    frame_index: usize,
    frame_start: Instant,
}

impl App {
    fn new(images: Vec<PathBuf>, index: usize) -> Self {
        Self {
            window: None,
            context: None,
            surface: None,
            images,
            index,
            current: None,
            image_cache: ImageCache::new(),
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            frame_index: 0,
            frame_start: Instant::now(),
        }
    }

    fn load_current(&mut self) {
        self.current = self.image_cache.take(self.index).or_else(|| {
            self.images.get(self.index).and_then(|p| load_image(p))
        });
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

        let bg = 0x1a1a1au32;
        buffer.fill(bg);

        let img_area_h = h.saturating_sub(BAR_HEIGHT);

        if let Some(img) = &self.current {
            let frame_pixels = &img.frames[self.frame_index % img.frames.len()].pixels;
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

            // fixed-point inverse scale (16.16) to replace per-pixel f64 division
            let inv_scale = (65536.0 / scale) as u64;
            let img_w = img.width as usize;

            // precompute src_x lookup table for the x span
            let x_span = (x_end - x_start) as usize;
            let mut src_x_table: Vec<u32> = Vec::with_capacity(x_span);
            for wx in x_start..x_end {
                src_x_table.push((((wx as i64 - ox) as u64 * inv_scale) >> 16) as u32);
            }

            let buf = buffer.as_mut();
            for wy in y_start..y_end {
                let src_y = (((wy as i64 - oy) as u64 * inv_scale) >> 16) as u32;
                if src_y >= img.height {
                    continue;
                }
                let row_offset = src_y as usize * img_w;
                let buf_row = wy as usize * w as usize;

                // SAFETY: x_start..x_end is clamped to 0..w, so buf_row + wx < w*h = buf.len()
                // src_x < img.width and src_y < img.height, so row_offset + src_x < frame_pixels.len()
                unsafe {
                    for (i, &src_x) in src_x_table.iter().enumerate() {
                        let px = *frame_pixels.get_unchecked(row_offset + src_x as usize);
                        *buf.get_unchecked_mut(buf_row + x_start as usize + i) = px;
                    }
                }
            }
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
                format!(
                    " {} | {}/{}",
                    name,
                    self.index + 1,
                    self.images.len()
                )
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

        // preload image to determine window size
        if self.current.is_none() {
            self.current = self.images.get(self.index).and_then(|p| load_image(p));
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

        let attrs = Window::default_attributes()
            .with_title("rxiv")
            .with_name("rxiv", "rxiv")
            .with_inner_size(LogicalSize::new(win_w, win_h));

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
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(deadline) = self.advance_animation() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
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
                Key::Named(NamedKey::PageUp) | Key::Named(NamedKey::Backspace) => {
                    self.navigate(-1)
                }
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
                    _ => {}
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

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App::new(images, index);
    event_loop.run_app(&mut app).expect("event loop error");
}
