//! tsvg — a tiny SVG viewer.
//!
//! The SVG is parsed once into a `usvg::Tree`. Only the visible viewport is
//! rasterized (window-sized pixmap + transform), so arbitrarily large SVGs
//! never hit bitmap/texture size limits. Rasterization runs on a background
//! thread; while it is busy the previous frame is scaled to follow the view.

#[cfg(target_os = "macos")]
mod macos;

use std::io::{IsTerminal, Read};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, bail};
use eframe::egui;
use resvg::usvg::fontdb;
use resvg::{tiny_skia, usvg};

const USAGE: &str = "usage: tsvg <file.svg>\n       cat file.svg | tsvg [-]";

/// Raw SVG input plus metadata.
struct Source {
    data: Vec<u8>,
    title: String,
    /// Base directory for relative `href`s.
    dir: Option<PathBuf>,
}

impl Source {
    fn from_path(path: &Path) -> Result<Self> {
        let data =
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let dir = path
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        Ok(Self { data, title, dir })
    }

    fn from_stdin() -> Result<Self> {
        let mut data = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut data)
            .context("failed to read stdin")?;
        if data.is_empty() {
            bail!("stdin was empty");
        }
        Ok(Self {
            data,
            title: "stdin".into(),
            dir: std::env::current_dir().ok(),
        })
    }

    fn parse(&self, fontdb: &Arc<fontdb::Database>) -> Result<usvg::Tree> {
        let opt = usvg::Options {
            resources_dir: self.dir.clone(),
            fontdb: fontdb.clone(),
            ..Default::default()
        };
        usvg::Tree::from_data(&self.data, &opt)
            .with_context(|| format!("failed to parse {}", self.title))
    }
}

/// Decides what to show at startup. `None` means "start empty and wait for a
/// file" (launched from Finder/Dock, where stdin is /dev/null).
fn read_input() -> Result<Option<Source>> {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("-psn_")) // legacy Finder process serial number
        .collect();
    match args.as_slice() {
        [a] if a == "-h" || a == "--help" => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        [a] if a == "-" => Source::from_stdin().map(Some),
        [path] => Source::from_path(Path::new(path)).map(Some),
        [] => {
            if std::io::stdin().is_terminal() {
                bail!("no input\n{USAGE}");
            }
            let piped = std::fs::metadata("/dev/fd/0")
                .map(|m| {
                    let t = m.file_type();
                    t.is_fifo() || t.is_file() || t.is_socket()
                })
                .unwrap_or(false);
            if piped {
                Source::from_stdin().map(Some)
            } else {
                Ok(None)
            }
        }
        _ => bail!("too many arguments\n{USAGE}"),
    }
}

/// Hands file-open requests (Finder, Dock) to the UI thread.
pub struct Opener {
    tx: Sender<PathBuf>,
    ctx: Arc<OnceLock<egui::Context>>,
}

impl Opener {
    pub fn open(&self, path: PathBuf) {
        let _ = self.tx.send(path);
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }
}

fn main() -> Result<()> {
    let input = read_input()?;

    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let fontdb = Arc::new(db);

    // Parse CLI input before opening a window so errors go to the terminal.
    let initial = match input {
        Some(src) => Some((src.parse(&fontdb)?, src.title)),
        None => None,
    };

    let (open_tx, open_rx) = channel();
    let ctx_slot = Arc::new(OnceLock::new());
    let opener = Opener {
        tx: open_tx,
        ctx: ctx_slot.clone(),
    };

    let event_loop = winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event()
        .build()
        .context("failed to create event loop")?;

    #[cfg(target_os = "macos")]
    macos::install_open_handler(opener);
    #[cfg(not(target_os = "macos"))]
    drop(opener);

    let title = initial
        .as_ref()
        .map_or("tsvg".to_string(), |(_, t)| format!("tsvg — {t}"));
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    let mut app = eframe::create_native(
        "tsvg",
        native_options,
        Box::new(move |cc| {
            let _ = ctx_slot.set(cc.egui_ctx.clone());
            Ok(Box::new(Viewer::new(cc, fontdb, initial, open_rx)))
        }),
        &event_loop,
    );
    event_loop.run_app(&mut app).context("event loop failed")?;
    Ok(())
}

/// Mapping from SVG user units to screen points:
/// `screen = canvas_min + pan + svg * scale`.
#[derive(Clone, Copy, PartialEq, Debug)]
struct View {
    scale: f32,
    pan: egui::Vec2,
}

impl View {
    fn to_screen(self, origin: egui::Pos2, svg: egui::Vec2) -> egui::Pos2 {
        origin + self.pan + svg * self.scale
    }
    fn to_svg(self, origin: egui::Pos2, screen: egui::Pos2) -> egui::Vec2 {
        (screen - origin - self.pan) / self.scale
    }
    fn zoom_about(&mut self, origin: egui::Pos2, anchor: egui::Pos2, factor: f32) {
        let new_scale = (self.scale * factor).clamp(1e-5, 1e5);
        let factor = new_scale / self.scale;
        let a = anchor - origin;
        self.pan = a - (a - self.pan) * factor;
        self.scale = new_scale;
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct RenderRequest {
    view: View,
    /// Canvas size in physical pixels.
    width: u32,
    height: u32,
    pixels_per_point: f32,
}

struct RenderResult {
    req: RenderRequest,
    image: egui::ColorImage,
}

/// The thread exits once the returned `Sender` is dropped.
fn spawn_renderer(
    tree: usvg::Tree,
    ctx: egui::Context,
) -> (Sender<RenderRequest>, Receiver<RenderResult>) {
    let (req_tx, req_rx) = channel::<RenderRequest>();
    let (res_tx, res_rx) = channel::<RenderResult>();
    std::thread::spawn(move || {
        while let Ok(mut req) = req_rx.recv() {
            // Skip stale requests; only the latest view matters.
            while let Ok(newer) = req_rx.try_recv() {
                req = newer;
            }
            let Some(mut pixmap) = tiny_skia::Pixmap::new(req.width, req.height) else {
                continue;
            };
            let s = req.view.scale * req.pixels_per_point;
            let t = req.view.pan * req.pixels_per_point;
            let transform = tiny_skia::Transform::from_row(s, 0.0, 0.0, s, t.x, t.y);
            resvg::render(&tree, transform, &mut pixmap.as_mut());
            let image = egui::ColorImage::from_rgba_premultiplied(
                [req.width as usize, req.height as usize],
                pixmap.data(),
            );
            if res_tx.send(RenderResult { req, image }).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, res_rx)
}

/// One loaded SVG and its view state.
struct Document {
    svg_size: egui::Vec2,
    view: Option<View>,
    req_tx: Sender<RenderRequest>,
    res_rx: Receiver<RenderResult>,
    last_sent: Option<RenderRequest>,
    texture: Option<(egui::TextureHandle, RenderRequest)>,
}

impl Document {
    fn new(tree: usvg::Tree, ctx: &egui::Context) -> Self {
        let size = tree.size();
        let (req_tx, res_rx) = spawn_renderer(tree, ctx.clone());
        Self {
            svg_size: egui::vec2(size.width(), size.height()),
            view: None,
            req_tx,
            res_rx,
            last_sent: None,
            texture: None,
        }
    }

    fn fit_view(&self, canvas: egui::Vec2) -> View {
        let scale = (canvas.x / self.svg_size.x).min(canvas.y / self.svg_size.y) * 0.95;
        View {
            scale,
            pan: (canvas - self.svg_size * scale) / 2.0,
        }
    }

    fn actual_size_view(&self, canvas: egui::Vec2) -> View {
        // 1 SVG unit == 1 point (same as browsers at 100%), centered.
        View {
            scale: 1.0,
            pan: (canvas - self.svg_size) / 2.0,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let ctx = ui.ctx().clone();
        let origin = rect.min;
        let ppp = ctx.pixels_per_point();
        let response = ui.interact(rect, egui::Id::unique("canvas"), egui::Sense::drag());

        // --- input ---
        let mut view = self.view.unwrap_or_else(|| self.fit_view(rect.size()));
        let (scroll, zoom, hover, fit, actual, zoom_in, zoom_out) = ctx.input(|i| {
            (
                i.smooth_scroll_delta,
                i.zoom_delta(),
                i.pointer.hover_pos(),
                i.key_pressed(egui::Key::Num0),
                i.key_pressed(egui::Key::Num1),
                i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Minus),
            )
        });
        if fit {
            view = self.fit_view(rect.size());
        }
        if actual {
            view = self.actual_size_view(rect.size());
        }
        view.pan += response.drag_delta() + scroll;
        let anchor = hover.filter(|p| rect.contains(*p)).unwrap_or(rect.center());
        if zoom != 1.0 {
            view.zoom_about(origin, anchor, zoom);
        }
        if zoom_in {
            view.zoom_about(origin, rect.center(), 1.25);
        }
        if zoom_out {
            view.zoom_about(origin, rect.center(), 0.8);
        }
        self.view = Some(view);

        // --- rendering ---
        let req = RenderRequest {
            view,
            width: (rect.width() * ppp).round().max(1.0) as u32,
            height: (rect.height() * ppp).round().max(1.0) as u32,
            pixels_per_point: ppp,
        };
        if self.last_sent != Some(req) {
            let _ = self.req_tx.send(req);
            self.last_sent = Some(req);
        }
        while let Ok(res) = self.res_rx.try_recv() {
            match &mut self.texture {
                Some((tex, r)) => {
                    tex.set(res.image, egui::TextureOptions::LINEAR);
                    *r = res.req;
                }
                None => {
                    let tex = ctx.load_texture("svg", res.image, egui::TextureOptions::LINEAR);
                    self.texture = Some((tex, res.req));
                }
            }
        }

        // --- painting ---
        let painter = ui.painter_at(rect);
        let page = egui::Rect::from_min_max(
            view.to_screen(origin, egui::Vec2::ZERO),
            view.to_screen(origin, self.svg_size),
        );
        painter.rect_filled(page, 0.0, egui::Color32::WHITE);

        let mut rendering = true;
        if let Some((tex, r)) = &self.texture {
            rendering = r.view != view || r.width != req.width || r.height != req.height;
            // Place the (possibly stale) image where its pixels belong under the current view.
            let size_pt = egui::vec2(r.width as f32, r.height as f32) / r.pixels_per_point;
            let min = view.to_screen(origin, r.view.to_svg(origin, origin));
            let max = view.to_screen(origin, r.view.to_svg(origin, origin + size_pt));
            painter.image(
                tex.id(),
                egui::Rect::from_min_max(min, max),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }

        let status = format!(
            "{:.1}%{}",
            view.scale * 100.0,
            if rendering { "  rendering…" } else { "" }
        );
        painter.text(
            rect.left_bottom() + egui::vec2(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            status,
            egui::FontId::monospace(12.0),
            egui::Color32::from_gray(200),
        );
    }
}

struct Viewer {
    fontdb: Arc<fontdb::Database>,
    doc: Option<Document>,
    error: Option<String>,
    open_rx: Receiver<PathBuf>,
}

impl Viewer {
    fn new(
        cc: &eframe::CreationContext<'_>,
        fontdb: Arc<fontdb::Database>,
        initial: Option<(usvg::Tree, String)>,
        open_rx: Receiver<PathBuf>,
    ) -> Self {
        // Cmd +/- must not rescale the egui UI itself.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        Self {
            fontdb,
            doc: initial.map(|(tree, _)| Document::new(tree, &cc.egui_ctx)),
            error: None,
            open_rx,
        }
    }

    fn open(&mut self, ctx: &egui::Context, path: &Path) {
        let loaded =
            Source::from_path(path).and_then(|src| Ok((src.parse(&self.fontdb)?, src.title)));
        match loaded {
            Ok((tree, title)) => {
                self.doc = Some(Document::new(tree, ctx));
                self.error = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("tsvg — {title}")));
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();

        // Files from Finder/Dock or dropped onto the window; the last one wins.
        let from_finder = self.open_rx.try_iter().last();
        let (dropped, quit) = ctx.input(|i| {
            (
                i.raw.dropped_files.last().map(|f| f.path().to_path_buf()),
                i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape),
            )
        });
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(path) = dropped.or(from_finder) {
            self.open(&ctx, &path);
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(60));

        match &mut self.doc {
            Some(doc) => doc.ui(ui, rect),
            None => {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Drop an SVG here",
                    egui::FontId::proportional(18.0),
                    egui::Color32::from_gray(160),
                );
            }
        }

        if let Some(err) = &self.error {
            let galley = painter.layout(
                err.clone(),
                egui::FontId::monospace(12.0),
                egui::Color32::WHITE,
                rect.width() - 32.0,
            );
            let pos = rect.left_top() + egui::vec2(16.0, 16.0);
            painter.rect_filled(
                egui::Rect::from_min_size(pos, galley.size()).expand(8.0),
                4.0,
                egui::Color32::from_rgb(150, 40, 40),
            );
            painter.galley(pos, galley, egui::Color32::WHITE);
        }
    }
}
