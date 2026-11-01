//! tsvg — a tiny SVG viewer.
//!
//! The SVG is parsed once into a `usvg::Tree`. Only the visible viewport is
//! rasterized (window-sized pixmap + transform), so arbitrarily large SVGs
//! never hit bitmap/texture size limits. Rasterization runs on a background
//! thread; while it is busy the previous frame is scaled to follow the view.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use anyhow::{Context as _, Result, bail};
use eframe::egui;
use resvg::{tiny_skia, usvg};

const USAGE: &str = "usage: tsvg <file.svg>\n       cat file.svg | tsvg [-]";

fn main() -> Result<()> {
    let (data, title, resources_dir) = read_input()?;

    let mut opt = usvg::Options {
        resources_dir,
        ..Default::default()
    };
    opt.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(&data, &opt).context("failed to parse SVG")?;
    drop(data);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("tsvg — {title}"))
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "tsvg",
        native_options,
        Box::new(|cc| Ok(Box::new(Viewer::new(cc, tree)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns (svg bytes, window title, directory for relative resources).
fn read_input() -> Result<(Vec<u8>, String, Option<PathBuf>)> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [a] if a == "-h" || a == "--help" => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        [path] if path != "-" => {
            let path = PathBuf::from(path);
            let data = std::fs::read(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let title = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let dir = path
                .canonicalize()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            Ok((data, title, dir))
        }
        [] | [_] => {
            let stdin = std::io::stdin();
            if args.is_empty() && stdin.is_terminal() {
                bail!("no input\n{USAGE}");
            }
            let mut data = Vec::new();
            stdin
                .lock()
                .read_to_end(&mut data)
                .context("failed to read stdin")?;
            if data.is_empty() {
                bail!("stdin was empty");
            }
            Ok((data, "stdin".into(), std::env::current_dir().ok()))
        }
        _ => bail!("too many arguments\n{USAGE}"),
    }
}

/// Mapping from SVG user units to screen points:
/// `screen = canvas_min + pan + svg * scale`.
#[derive(Clone, Copy, PartialEq, Debug)]
struct View {
    scale: f32,
    pan: egui::Vec2,
}

impl View {
    fn to_screen(&self, origin: egui::Pos2, svg: egui::Vec2) -> egui::Pos2 {
        origin + self.pan + svg * self.scale
    }
    fn to_svg(&self, origin: egui::Pos2, screen: egui::Pos2) -> egui::Vec2 {
        (screen - origin - self.pan) / self.scale
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

fn spawn_renderer(
    tree: Arc<usvg::Tree>,
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

struct Viewer {
    svg_size: egui::Vec2,
    view: Option<View>,
    req_tx: Sender<RenderRequest>,
    res_rx: Receiver<RenderResult>,
    last_sent: Option<RenderRequest>,
    texture: Option<(egui::TextureHandle, RenderRequest)>,
}

impl Viewer {
    fn new(cc: &eframe::CreationContext<'_>, tree: usvg::Tree) -> Self {
        // Cmd +/- must not rescale the egui UI itself.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let size = tree.size();
        let (req_tx, res_rx) = spawn_renderer(Arc::new(tree), cc.egui_ctx.clone());
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
        let scale = 1.0;
        View {
            scale,
            pan: (canvas - self.svg_size * scale) / 2.0,
        }
    }
}

fn zoom_about(view: &mut View, origin: egui::Pos2, anchor: egui::Pos2, factor: f32) {
    let new_scale = (view.scale * factor).clamp(1e-5, 1e5);
    let factor = new_scale / view.scale;
    let a = anchor - origin;
    view.pan = a - (a - view.pan) * factor;
    view.scale = new_scale;
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let origin = rect.min;
        let ppp = ctx.pixels_per_point();
        let response = ui.interact(rect, egui::Id::unique("canvas"), egui::Sense::drag());

        // --- input ---
        let mut view = self.view.unwrap_or_else(|| self.fit_view(rect.size()));
        let (scroll, zoom, hover, keys) = ctx.input(|i| {
            (
                i.smooth_scroll_delta,
                i.zoom_delta(),
                i.pointer.hover_pos(),
                (
                    i.key_pressed(egui::Key::Num0),
                    i.key_pressed(egui::Key::Num1),
                    i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                    i.key_pressed(egui::Key::Minus),
                    i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape),
                ),
            )
        });
        let (fit, actual, zoom_in, zoom_out, quit) = keys;
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if fit {
            view = self.fit_view(rect.size());
        }
        if actual {
            view = self.actual_size_view(rect.size());
        }
        view.pan += response.drag_delta() + scroll;
        let anchor = hover.filter(|p| rect.contains(*p)).unwrap_or(rect.center());
        if zoom != 1.0 {
            zoom_about(&mut view, origin, anchor, zoom);
        }
        if zoom_in {
            zoom_about(&mut view, origin, rect.center(), 1.25);
        }
        if zoom_out {
            zoom_about(&mut view, origin, rect.center(), 0.8);
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
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(60));
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
