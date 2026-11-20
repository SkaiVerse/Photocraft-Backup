//! PhotoCraft for iPad: the engine and egui interface, drawn with Metal, driven by a Swift shell.
#![allow(clippy::missing_safety_doc)]
#![allow(dead_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_doc::Document;
use photocraft_engine::Session;
use photocraft_ui_egui::stylus::{PenSample, StylusFeed};
use photocraft_ui_egui::theme::ThemeKind;
use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply, FileDialogRequest, Inbox, PhotocraftApp, PlaceInbox, Services};

// ---------------------------------------------------------------------------------------------
// Callbacks into Swift (must match PhotoCraft-Bridging-Header.h, field for field)
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PCCallbacks {
    pub user: *mut c_void,
    /// Show the import chooser (Photos or Files). Arguments: several files allowed (1/0), and
    /// whether pictures make sense here, so Photos is offered (1/0).
    pub open_dialog: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int)>,
    /// Show Apple's Save to Files sheet for a file with this name.
    pub save_dialog: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    /// Write a file that lives outside the app (Files, iCloud Drive) the way iPadOS wants it.
    /// Returns 1 on success.
    pub write_file: Option<unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize) -> c_int>,
    pub open_url: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
    pub copy_text: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn documents_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Documents"))
}

fn working_dir() -> Option<PathBuf> {
    documents_dir().map(|d| d.join("Working"))
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/PhotoCraft"))
}

fn note(msg: &str) {
    use std::io::Write;
    let Some(dir) = documents_dir() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("PhotoCraft-log.txt")) {
        let _ = writeln!(f, "[rust] {msg}");
    }
}

/// Sends the app's own log messages to PhotoCraft-log.txt (Files > On My iPad > PhotoCraft).
struct FileLogger;

impl log::Log for FileLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        let t = m.target();
        if t.starts_with("photocraft") || t.starts_with("craft") {
            m.level() <= log::Level::Info
        } else {
            m.level() <= log::Level::Warn
        }
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            note(&format!("{} {}: {}", r.level(), r.target(), r.args()));
        }
    }
    fn flush(&self) {}
}

static LOGGER: FileLogger = FileLogger;

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

unsafe fn from_c(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

unsafe fn bytes_from(data: *const u8, len: usize) -> Vec<u8> {
    if data.is_null() || len == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(data, len) }.to_vec() }
}

fn guard<R>(default: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

fn to_mods(m: u32) -> egui::Modifiers {
    // Only Apple's modifier rules: Command is the shortcut key, Control stays Control.
    let cmd = m & 8 != 0;
    egui::Modifiers { alt: m & 4 != 0, ctrl: m & 2 != 0, shift: m & 1 != 0, mac_cmd: cmd, command: cmd }
}

fn hid_to_key(hid: u32) -> Option<egui::Key> {
    use egui::Key;
    const LETTERS: [Key; 26] = [
        Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K, Key::L, Key::M,
        Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X, Key::Y, Key::Z,
    ];
    const DIGITS: [Key; 9] =
        [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
    const FKEYS: [Key; 12] =
        [Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10, Key::F11, Key::F12];
    Some(match hid {
        0x04..=0x1D => LETTERS[(hid - 0x04) as usize],
        0x1E..=0x26 => DIGITS[(hid - 0x1E) as usize],
        0x27 => Key::Num0,
        0x28 => Key::Enter,
        0x29 => Key::Escape,
        0x2A => Key::Backspace,
        0x2B => Key::Tab,
        0x2C => Key::Space,
        0x2D => Key::Minus,
        0x2E => Key::Equals,
        0x2F => Key::OpenBracket,
        0x30 => Key::CloseBracket,
        0x31 => Key::Backslash,
        0x33 => Key::Semicolon,
        0x34 => Key::Quote,
        0x35 => Key::Backtick,
        0x36 => Key::Comma,
        0x37 => Key::Period,
        0x38 => Key::Slash,
        0x3A..=0x45 => FKEYS[(hid - 0x3A) as usize],
        0x49 => Key::Insert,
        0x4A => Key::Home,
        0x4B => Key::PageUp,
        0x4C => Key::Delete,
        0x4D => Key::End,
        0x4E => Key::PageDown,
        0x4F => Key::ArrowRight,
        0x50 => Key::ArrowLeft,
        0x51 => Key::ArrowDown,
        0x52 => Key::ArrowUp,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Preferences and the file dialog state
// ---------------------------------------------------------------------------------------------

fn prefs_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("preferences.json"))
}

/// An Open sheet that is on screen: the app is waiting for the files.
struct PendingOpen {
    reply: FileDialogReply,
    multiple: bool,
    files: Vec<(String, Vec<u8>)>,
    paths: Vec<String>,
}

thread_local! {
    static DIALOG: RefCell<Option<PendingOpen>> = const { RefCell::new(None) };
    static SAVE_DIALOG: RefCell<Option<FileDialogReply>> = const { RefCell::new(None) };
}

/// How many key presses are written to the log (to find out why a shortcut does nothing).
static KEY_LOG: AtomicUsize = AtomicUsize::new(0);
static PEN_LOG: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------------------------
// Services: what the app asks the iPad to do
// ---------------------------------------------------------------------------------------------

fn build_services(cb: PCCallbacks, inbox: &Inbox, place_inbox: &PlaceInbox) -> Services {
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
            photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
            photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        file_dialog: Some(Box::new(move |request, _parent, reply| match request {
            FileDialogRequest::Open { multiple, extensions, .. } => {
                // Pictures make sense unless the app asks for specific other kinds of file only.
                let kinds = format!("{extensions:?}").to_lowercase();
                let pictures = kinds == "none" || kinds.contains("png") || kinds.contains("jpg") || kinds.contains("jpeg");
                note(&format!("the app asked to open a file (several: {multiple}, pictures: {pictures})"));
                DIALOG.with(|d| *d.borrow_mut() = Some(PendingOpen { reply, multiple, files: Vec::new(), paths: Vec::new() }));
                if let Some(f) = cb.open_dialog {
                    unsafe { f(cb.user, multiple as c_int, pictures as c_int) }
                }
            }
            FileDialogRequest::Folder { .. } => reply.send(None),
            // Save As, a first save and exports: Apple's Save to Files sheet picks the place, and
            // the app then writes straight to it. No hidden copy is kept inside the app.
            FileDialogRequest::Save { suggested } => {
                let name = std::path::Path::new(&suggested)
                    .file_name()
                    .map_or_else(|| suggested.clone(), |n| n.to_string_lossy().to_string());
                note(&format!("the app asked to save as: {name}"));
                SAVE_DIALOG.with(|d| *d.borrow_mut() = Some(reply));
                if let Some(f) = cb.save_dialog {
                    let c = cstring(&name);
                    unsafe { f(cb.user, c.as_ptr()) }
                }
            }
        })),
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            // Files inside the app are written directly. Files in Files / iCloud Drive go through
            // Swift, which writes them the way iPadOS requires.
            let home = std::env::var("HOME").unwrap_or_default();
            let inside = !home.is_empty() && path.starts_with(&home);
            let via_swift = !inside || path.contains("/PhotoCraftPhotos-");
            if via_swift && let Some(f) = cb.write_file {
                let c = cstring(path);
                let ok = unsafe { f(cb.user, c.as_ptr(), bytes.as_ptr(), bytes.len()) };
                note(&format!("wrote {path} through Files ({} bytes): {}", bytes.len(), ok != 0));
                return if ok != 0 { Ok(()) } else { Err(format!("{path}: iPadOS did not allow writing this file")) };
            }
            let p = std::path::Path::new(path);
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(p, bytes).map_err(|e| format!("{path}: {e}"))?;
            note(&format!("wrote {path} ({} bytes)", bytes.len()));
            Ok(())
        })),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        open_url: Some(Box::new(move |url: &str| {
            if let Some(f) = cb.open_url {
                let c = cstring(url);
                unsafe { f(cb.user, c.as_ptr()) }
            }
            Ok(())
        })),
        inbox: Some(inbox.clone()),
        place_inbox: Some(place_inbox.clone()),
        load_prefs: Some(Box::new(|| prefs_path().and_then(|p| std::fs::read_to_string(p).ok()))),
        save_prefs: Some(Box::new(|text: &str| {
            let Some(path) = prefs_path() else { return Err("no preferences folder".to_string()) };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(path, text).map_err(|e| e.to_string())
        })),
        load_gpu_memo: None,
        save_gpu_memo: None,
        shader_compiler: None,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// The host: one per window
// ---------------------------------------------------------------------------------------------

fn touch_settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("touch.txt"))
}

/// Finger drawing on/off and how firmly the Pencil must press, kept between launches.
fn load_touch_settings() -> f32 {
    let mut divisor = 2.5_f32;
    if let Some(text) = touch_settings_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("pressure_divisor=")
                && let Ok(n) = v.trim().parse::<f32>()
            {
                divisor = n.clamp(1.0, 5.0);
            }
        }
    }
    divisor
}

fn save_touch_settings(divisor: f32) {
    if let Some(path) = touch_settings_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, format!("pressure_divisor={divisor}\n"));
    }
}

/// Buttons drawn by the host inside the app's own interface.
#[derive(Clone, Copy)]
enum TouchAction {
    Undo,
    Redo,
    Delete,
    Softer,
    Firmer,
}

/// State of the touch helpers: the round assist button, its panel, and the held-down keys.
#[derive(Default)]
struct TouchUi {
    /// Where the assist button is (dragged by the person); None until first shown.
    puck: Option<egui::Pos2>,
    open: bool,
    /// Keys held down by touch: Command, Shift, Option, Control.
    sticky: [bool; 4],
    /// The spots covered by these helpers, so touches there are never taken for scrolling.
    rects: Vec<egui::Rect>,
}

#[derive(Clone, Copy)]
enum Icon {
    Undo,
    Redo,
    Trash,
}

/// A square button with a drawn icon (no text, no font needed).
fn icon_button(ui: &mut egui::Ui, size: f32, icon: Icon) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let visuals = ui.style().interact(&resp).clone();
    ui.painter().rect_filled(rect, 8.0, visuals.weak_bg_fill);
    let color = visuals.fg_stroke.color;
    let stroke = egui::Stroke::new(2.4, color);
    let at = |u: f32, v: f32| egui::pos2(rect.min.x + u * size, rect.min.y + v * size);
    let painter = ui.painter();
    match icon {
        Icon::Undo | Icon::Redo => {
            let mirror = matches!(icon, Icon::Redo);
            let x = |u: f32| if mirror { 1.0 - u } else { u };
            // A curve over the top, ending in an arrow head.
            let (cx, cy, r) = (0.5_f32, 0.56_f32, 0.2_f32);
            let mut pts = Vec::new();
            let mut deg = 20.0_f32;
            while deg >= -200.0 {
                let t = deg.to_radians();
                pts.push(at(x(cx + r * t.cos()), cy + r * t.sin()));
                deg -= 10.0;
            }
            painter.add(egui::Shape::line(pts, stroke));
            let t = (-200.0_f32).to_radians();
            let end = (cx + r * t.cos(), cy + r * t.sin());
            let dir = (t.sin(), -t.cos());
            let dir = if mirror { (-dir.0, dir.1) } else { dir };
            let end_p = at(x(end.0), end.1);
            let d = egui::vec2(dir.0, dir.1);
            let perp = egui::vec2(-d.y, d.x);
            let tip = end_p + d * (0.13 * size);
            let b1 = end_p + perp * (0.1 * size);
            let b2 = end_p - perp * (0.1 * size);
            painter.add(egui::Shape::convex_polygon(vec![tip, b1, b2], color, egui::Stroke::NONE));
        }
        Icon::Trash => {
            let line = |a: (f32, f32), b: (f32, f32)| painter.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
            line((0.26, 0.30), (0.74, 0.30));
            line((0.42, 0.30), (0.42, 0.22));
            line((0.42, 0.22), (0.58, 0.22));
            line((0.58, 0.22), (0.58, 0.30));
            line((0.32, 0.30), (0.36, 0.78));
            line((0.36, 0.78), (0.64, 0.78));
            line((0.64, 0.78), (0.68, 0.30));
            line((0.46, 0.42), (0.46, 0.66));
            line((0.54, 0.42), (0.54, 0.66));
        }
    }
    resp
}

/// The touch helpers, drawn inside the app's own interface:
/// - Undo, Redo and Delete as icon buttons in a column near the toolbar, bottom-left of the picture;
/// - a round assist button (drag it anywhere) that opens Command / Shift / Option / Control to hold
///   down by touch (any number together), plus Pencil firmness.
fn touch_ui(
    ctx: &egui::Context,
    screen: egui::Rect,
    canvas: egui::Rect,
    st: &mut TouchUi,
    divisor: f32,
    out: &mut Vec<TouchAction>,
) {
    st.rects.clear();
    let has_canvas = canvas.width() > 10.0 && canvas.height() > 10.0;
    let (left, right, top, bottom) = if has_canvas {
        (canvas.min.x, canvas.max.x, canvas.min.y, canvas.max.y)
    } else {
        (screen.min.x, screen.max.x, screen.min.y, screen.max.y)
    };

    let actions = egui::Area::new(egui::Id::new("pc_actions"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(left + 6.0, bottom - screen.max.y - 6.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);
                if icon_button(ui, 44.0, Icon::Undo).clicked() {
                    out.push(TouchAction::Undo);
                }
                if icon_button(ui, 44.0, Icon::Redo).clicked() {
                    out.push(TouchAction::Redo);
                }
                if icon_button(ui, 44.0, Icon::Trash).clicked() {
                    out.push(TouchAction::Delete);
                }
            });
        });
    st.rects.push(actions.response.rect);

    // The round assist button.
    let size = 56.0_f32;
    let default = egui::pos2(right - size - 12.0, top + (bottom - top) * 0.62);
    let mut pos = st.puck.unwrap_or(default);
    pos.x = pos.x.clamp(screen.min.x, (screen.max.x - size).max(screen.min.x));
    pos.y = pos.y.clamp(screen.min.y, (screen.max.y - size).max(screen.min.y));
    let any_down = st.sticky.iter().any(|b| *b);
    let is_open = st.open;
    let mut new_pos = pos;
    let mut toggled = false;
    let puck = egui::Area::new(egui::Id::new("pc_puck"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click_and_drag());
            let ring = if any_down || is_open { ui.visuals().selection.bg_fill } else { egui::Color32::from_gray(175) };
            let c = rect.center();
            ui.painter().circle_filled(c, size / 2.0, egui::Color32::from_black_alpha(150));
            ui.painter().circle_stroke(c, size / 2.0 - 3.0, egui::Stroke::new(3.0, ring));
            ui.painter().circle_filled(c, 8.0, ring);
            if resp.dragged() {
                new_pos += resp.drag_delta();
            }
            if resp.clicked() {
                toggled = true;
            }
        });
    st.rects.push(puck.response.rect);
    st.puck = Some(new_pos);
    if toggled {
        st.open = !st.open;
    }

    if st.open {
        let panel = egui::Area::new(egui::Id::new("pc_puck_panel"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::RIGHT_TOP)
            .fixed_pos(egui::pos2(new_pos.x - 8.0, new_pos.y))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (i, label) in ["Cmd", "Shift", "Opt", "Ctrl"].iter().enumerate() {
                            if ui.add_sized([62.0, 44.0], egui::Button::new(*label).selected(st.sticky[i])).clicked() {
                                st.sticky[i] = !st.sticky[i];
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized([58.0, 44.0], egui::Button::new("Soft"))
                            .on_hover_text(format!("Pencil: a lighter touch is enough ({divisor:.1})"))
                            .clicked()
                        {
                            out.push(TouchAction::Softer);
                        }
                        if ui
                            .add_sized([58.0, 44.0], egui::Button::new("Firm"))
                            .on_hover_text(format!("Pencil: needs more pressure ({divisor:.1})"))
                            .clicked()
                        {
                            out.push(TouchAction::Firmer);
                        }
                    });
                });
            });
        st.rects.push(panel.response.rect);
    }
}

struct Host {
    app: PhotocraftApp,
    frame: eframe::Frame,
    ctx: egui::Context,
    rs: egui_wgpu::RenderState,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    needs_reconfigure: bool,
    scale: f32,
    events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
    dark: bool,
    focused: bool,
    start: Instant,
    deadline: Arc<Mutex<Option<Instant>>>,
    cb: PCCallbacks,
    inbox: Inbox,
    feed: StylusFeed,
    pen_down: bool,
    wants_keyboard: bool,
    /// The Pencil force that counts as full pressure (1.0 is an average touch). Lower is softer.
    pressure_divisor: f32,
    touch: TouchUi,
    /// Modifier keys as reported by the hardware keyboard (the held-down touch keys are added).
    hw: egui::Modifiers,
    /// A fingertip held on the screen while the Pencil draws: keeps Shift down (constrain).
    touch_shift: bool,
    /// The mouse cursor the app asked for last frame (resize arrows tell us a handle is there).
    cursor: egui::CursorIcon,
    /// A finger or Pencil that has not decided yet what it is doing (tap, scroll, drag, draw).
    gesture: Option<Gesture>,
    /// Events held back a few frames (a click's release, the pointer leaving).
    posts: Vec<(u8, egui::Event)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GMode {
    Pending,
    Pointer,
    Scroll,
    Pan,
}

#[derive(Clone, Copy)]
struct Gesture {
    start: (f32, f32),
    last: (f32, f32),
    t0: Instant,
    mode: GMode,
    pen: bool,
    consumed: bool,
    on_canvas: bool,
    /// Started on one of the host's own buttons (assist button, undo column): always a plain click or drag.
    host_ui: bool,
}

fn wake_at(deadline: &Arc<Mutex<Option<Instant>>>, at: Instant) {
    let mut d = deadline.lock().unwrap_or_else(|e| e.into_inner());
    if d.is_none_or(|cur| at < cur) {
        *d = Some(at);
    }
}

impl Host {
    fn new(layer: *mut c_void, width: u32, height: u32, scale: f32, cb: PCCallbacks) -> Result<Host, String> {
        // Folders that show up in Files > On My iPad > PhotoCraft.
        if let Some(docs) = documents_dir() {
            let _ = std::fs::create_dir_all(docs.join("Fonts"));
            let _ = std::fs::create_dir_all(docs.join("Working"));
        }

        // Graphics: Metal through wgpu. The app's own GPU compositor shares this device.
        let instance = wgpu::Instance::default();
        let surface: wgpu::Surface<'static> =
            unsafe { instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer)) }
                .map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("adapter: {e}"))?;
        let info = adapter.get_info();
        note(&format!("gpu: {} ({:?})", info.name, info.backend));
        let limits = adapter.limits();
        note(&format!("gpu: max texture {} px", limits.max_texture_dimension_2d));
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("photocraft"),
            required_limits: limits,
            ..Default::default()
        }))
        .map_err(|e| format!("device: {e}"))?;
        let caps = surface.get_capabilities(&adapter);
        let format = egui_wgpu::preferred_framebuffer_format(&caps.formats).map_err(|e| format!("format: {e}"))?;
        let mut config = surface
            .get_default_config(&adapter, width.max(1), height.max(1))
            .ok_or_else(|| "this surface is not supported".to_string())?;
        config.format = format;
        config.view_formats = vec![format];
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        let renderer = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let rs = egui_wgpu::RenderState {
            adapter: adapter.clone(),
            available_adapters: Vec::new(),
            instance: instance.clone(),
            device: device.clone(),
            queue: queue.clone(),
            target_format: format,
            renderer: Arc::new(egui::mutex::RwLock::new(renderer)),
        };

        // The egui context and the app.
        let ctx = egui::Context::default();
        // Apple's shortcut set only: the app asks the context which system it runs on.
        ctx.set_os(egui::os::OperatingSystem::Mac);
        // Floating panels stay inside the one window.
        ctx.set_embed_viewports(true);
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(Some(Instant::now())));
        {
            let dl = deadline.clone();
            ctx.set_request_repaint_callback(move |info| {
                let now = Instant::now();
                let at = now.checked_add(info.delay).unwrap_or(now + Duration::from_secs(3600));
                wake_at(&dl, at);
            });
        }
        PhotocraftApp::install_type_reset_hooks(&ctx);
        PhotocraftApp::setup_context(&ctx, ThemeKind::Pro);

        let inbox: Inbox = Arc::default();
        let place_inbox: PlaceInbox = Arc::default();
        let services = build_services(cb, &inbox, &place_inbox);
        let mut app = PhotocraftApp::new(Session::new(), services);
        app.set_theme(&ctx, ThemeKind::Pro);
        app.set_wgpu(rs.clone());
        app.start_compositor(&ctx);
        let feed = app.stylus.feed.clone();

        Ok(Host {
            app,
            frame: eframe::Frame::_new_kittest(),
            ctx,
            rs,
            surface,
            config,
            needs_reconfigure: false,
            scale,
            events: Vec::new(),
            modifiers: egui::Modifiers::NONE,
            dark: true,
            focused: true,
            start: Instant::now(),
            deadline,
            cb,
            inbox,
            feed,
            pen_down: false,
            wants_keyboard: false,
            touch: TouchUi::default(),
            hw: egui::Modifiers::NONE,
            touch_shift: false,
            cursor: egui::CursorIcon::Default,
            pressure_divisor: load_touch_settings(),
            gesture: None,
            posts: Vec::new(),
        })
    }

    fn wake(&self) {
        wake_at(&self.deadline, Instant::now());
    }

    fn push(&mut self, e: egui::Event) {
        self.events.push(e);
        self.wake();
    }

    /// The modifier keys in effect: the hardware keyboard's, plus any held down by touch.
    fn effective_mods(&self) -> egui::Modifiers {
        let k = &self.touch.sticky;
        let cmd = self.hw.command || k[0];
        egui::Modifiers {
            alt: self.hw.alt || k[2],
            ctrl: self.hw.ctrl || k[3],
            shift: self.hw.shift || k[1] || self.touch_shift,
            mac_cmd: cmd,
            command: cmd,
        }
    }

    /// Tells egui which modifier keys (Shift, Command, ...) are down, whenever that changes.
    /// This egui version learns modifier state only from this event.
    fn refresh_mods(&mut self) {
        let m = self.effective_mods();
        if m != self.modifiers {
            self.modifiers = m;
            self.push(egui::Event::ModifiersChanged(m));
        }
    }

    fn set_modifiers(&mut self, hw: egui::Modifiers) {
        self.hw = hw;
        self.refresh_mods();
    }

    /// Points in the UIKit sense, converted to egui points (the interface size zooms).
    fn pt(&self, x: f32, y: f32) -> egui::Pos2 {
        let z = self.ctx.zoom_factor().max(0.1);
        egui::pos2(x / z, y / z)
    }

    /// Is this spot on the picture itself (and not on a panel, a menu or a floating window)?
    fn on_canvas(&self, x: f32, y: f32) -> bool {
        let p = self.pt(x, y);
        self.app.last_canvas_rect.contains(p)
            && self.ctx.layer_id_at(p).is_none_or(|l| l.order == egui::Order::Background)
    }

    /// The Pencil force (1.0 is an average touch) turned into a 0..1 pressure, with the
    /// `pressure_divisor` setting how hard the Pencil must be pressed. Kept simple and stable:
    /// the smoothing lives in the shell, and the stroke never gets a hard start or end.
    fn pressure_curve(&self, force: f32, _phase: i32) -> f32 {
        (force / self.pressure_divisor).clamp(0.0, 1.0)
    }

    /// An event for a few frames from now.
    fn post(&mut self, frames: u8, e: egui::Event) {
        self.posts.push((frames, e));
        self.wake();
    }

    /// A click whose press and release land in different frames, so the app sees a real click.
    fn click(&mut self, x: f32, y: f32, button: egui::PointerButton, keep_pointer: bool) {
        let pos = self.pt(x, y);
        let m = self.modifiers;
        self.push(egui::Event::PointerMoved(pos));
        self.push(egui::Event::PointerButton { pos, button, pressed: true, modifiers: m });
        self.post(1, egui::Event::PointerButton { pos, button, pressed: false, modifiers: m });
        if !keep_pointer {
            self.post(2, egui::Event::PointerGone);
        }
    }

    fn wheel(&mut self, dx: f32, dy: f32) {
        let z = self.ctx.zoom_factor().max(0.1);
        let m = self.modifiers;
        self.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(dx / z, dy / z),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: m,
        });
    }

    fn gesture_begin(&mut self, x: f32, y: f32, pen: bool) {
        let p = self.pt(x, y);
        let host_ui = self.touch.rects.iter().any(|r| r.contains(p));
        let on_canvas = !host_ui && self.on_canvas(x, y);
        // Let the app see what is under the finger now, so by the time it moves we know if it is
        // a handle that can be dragged (panel edge, splitter) rather than something to scroll.
        self.push(egui::Event::PointerMoved(p));
        self.gesture = Some(Gesture { start: (x, y), last: (x, y), t0: Instant::now(), mode: GMode::Pending, pen, consumed: false, on_canvas, host_ui });
        // Wake up in time to notice a press-and-hold (that opens menus and tool trays).
        wake_at(&self.deadline, Instant::now() + Duration::from_millis(480));
    }

    fn gesture_move(&mut self, x: f32, y: f32) {
        let Some(mut g) = self.gesture else { return };
        let last = g.last;
        g.last = (x, y);
        match g.mode {
            GMode::Pending => {
                let (sx, sy) = g.start;
                let (dx, dy) = (x - sx, y - sy);
                let draws = g.pen;
                use egui::CursorIcon as C;
                // The app shows a resize or grab cursor over handles: those are dragged, never scrolled.
                let handle = matches!(
                    self.cursor,
                    C::ResizeHorizontal | C::ResizeVertical | C::ResizeNeSw | C::ResizeNwSe | C::ResizeEast | C::ResizeWest
                        | C::ResizeNorth | C::ResizeSouth | C::ResizeNorthEast | C::ResizeNorthWest | C::ResizeSouthEast
                        | C::ResizeSouthWest | C::ResizeColumn | C::ResizeRow | C::Grab | C::Grabbing | C::Move | C::AllScroll
                );
                // A drag starts the moment the finger moves far enough to be a drag. A scroll or a
                // pan waits a little longer, so a light tap on a panel does not scroll it.
                let pointer = if g.host_ui || handle { true } else if g.on_canvas { draws } else { dy.abs() <= dx.abs() * 1.2 };
                if pointer {
                    if dx * dx + dy * dy <= 64.0 { return; }
                    g.mode = GMode::Pointer;
                    let sp = self.pt(sx, sy);
                    let cp = self.pt(x, y);
                    let m = self.modifiers;
                    self.push(egui::Event::PointerMoved(sp));
                    self.push(egui::Event::PointerButton { pos: sp, button: egui::PointerButton::Primary, pressed: true, modifiers: m });
                    self.push(egui::Event::PointerMoved(cp));
                } else if dx * dx + dy * dy <= 256.0 {
                    return;
                } else {
                    // A finger on the picture pans it; a finger dragging up or down on a panel or
                    // menu scrolls it.
                    g.mode = if g.on_canvas { GMode::Pan } else { GMode::Scroll };
                    let cp = self.pt(x, y);
                    self.push(egui::Event::PointerMoved(cp));
                    self.wheel(dx, dy);
                }
            }
            GMode::Pointer => {
                let cp = self.pt(x, y);
                self.push(egui::Event::PointerMoved(cp));
            }
            GMode::Scroll | GMode::Pan => {
                let cp = self.pt(x, y);
                self.push(egui::Event::PointerMoved(cp));
                self.wheel(x - last.0, y - last.1);
            }
        }
        self.gesture = Some(g);
    }

    fn gesture_end(&mut self, x: f32, y: f32, cancelled: bool) {
        let Some(g) = self.gesture.take() else { return };
        match g.mode {
            GMode::Pending => {
                let quick = g.t0.elapsed() < Duration::from_millis(500);
                // A quick tap clicks. A finger may tap anything that is not the picture (a tool
                // button, a panel, our own puck); on the picture a finger is a nudge, never a
                // select, so only the Pencil (and the mouse) can tap it.
                if !cancelled && !g.consumed && quick && (g.pen || !g.on_canvas) {
                    self.click(g.start.0, g.start.1, egui::PointerButton::Primary, false);
                }
            }
            GMode::Pointer => {
                let pos = self.pt(x, y);
                let m = self.modifiers;
                self.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: m });
                self.post(1, egui::Event::PointerGone);
            }
            GMode::Scroll | GMode::Pan => self.post(1, egui::Event::PointerGone),
        }
    }

    /// Releases a stroke that is still down: sends the button-up, clears the feed and lets go of
    /// the pen, so the canvas can never keep painting after the Pencil has left the screen. A
    /// finger landing while a stroke is somehow still down calls this first.
    fn end_pen(&mut self, x: f32, y: f32) {
        if !self.pen_down {
            return;
        }
        self.pen_down = false;
        let pos = self.pt(x, y);
        let m = self.modifiers;
        self.push(egui::Event::PointerMoved(pos));
        self.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: m });
        self.feed.set(None);
    }

    /// A shortcut pressed and released, like a key on a keyboard (exactly these modifiers).
    fn key_tap(&mut self, key: egui::Key, mods: egui::Modifiers) {
        self.set_modifiers(mods);
        self.push(egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: mods });
        self.post(1, egui::Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers: mods });
        self.set_modifiers(egui::Modifiers::NONE);
    }

    fn do_action(&mut self, a: TouchAction) {
        let cmd = to_mods(8);
        match a {
            TouchAction::Undo => self.key_tap(egui::Key::Z, cmd),
            TouchAction::Redo => self.key_tap(egui::Key::Z, to_mods(9)),
            TouchAction::Delete => self.key_tap(egui::Key::Backspace, egui::Modifiers::NONE),
            TouchAction::Softer => {
                self.pressure_divisor = (self.pressure_divisor - 0.3).max(1.0);
                save_touch_settings(self.pressure_divisor);
            }
            TouchAction::Firmer => {
                self.pressure_divisor = (self.pressure_divisor + 0.3).min(5.0);
                save_touch_settings(self.pressure_divisor);
            }
        }
    }

    fn handle_output(&self, out: &egui::PlatformOutput) {
        for cmd in &out.commands {
            match cmd {
                egui::OutputCommand::CopyText(text) => {
                    if let Some(f) = self.cb.copy_text {
                        let c = cstring(text);
                        unsafe { f(self.cb.user, c.as_ptr()) }
                    }
                }
                egui::OutputCommand::OpenUrl(open) => {
                    if let Some(f) = self.cb.open_url {
                        let c = cstring(&open.url);
                        unsafe { f(self.cb.user, c.as_ptr()) }
                    }
                }
                _ => {}
            }
        }
    }

    fn flags(&self, drew: bool) -> c_int {
        (drew as c_int) | ((self.wants_keyboard as c_int) << 1)
    }

    fn frame(&mut self) -> c_int {
        let now = Instant::now();
        let due = self.deadline.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| now >= t);
        if !due {
            return self.flags(false);
        }
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = None;

        let ppp = self.scale * self.ctx.zoom_factor().max(0.1);
        let rect = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(self.config.width as f32 / ppp, self.config.height as f32 / ppp),
        );
        let mut raw = egui::RawInput::default();
        raw.time = Some(self.start.elapsed().as_secs_f64());
        raw.screen_rect = Some(rect);
        raw.focused = self.focused;
        raw.system_theme = Some(if self.dark { egui::Theme::Dark } else { egui::Theme::Light });
        raw.max_texture_side = Some(self.rs.device.limits().max_texture_dimension_2d as usize);
        // A press that is held still opens the context menu / tool tray (a right click).
        if let Some(mut g) = self.gesture
            && g.mode == GMode::Pending
            && !g.consumed
            && !g.host_ui
            && g.t0.elapsed() >= Duration::from_millis(450)
        {
            g.consumed = true;
            self.gesture = Some(g);
            note("press and hold: right click");
            self.click(g.start.0, g.start.1, egui::PointerButton::Secondary, true);
        }
        let mut due = Vec::new();
        self.posts.retain_mut(|(n, e)| {
            if *n == 0 {
                due.push(e.clone());
                false
            } else {
                *n -= 1;
                true
            }
        });
        self.events.extend(due);
        if !self.posts.is_empty() {
            self.wake();
        }
        raw.events = std::mem::take(&mut self.events);
        let keys_in = raw.events.iter().filter(|e| matches!(e, egui::Event::Key { pressed: true, .. })).count();
        if let Some(v) = raw.viewports.get_mut(&egui::ViewportId::ROOT) {
            v.native_pixels_per_point = Some(self.scale);
            v.inner_rect = Some(rect);
        }

        let ctx = self.ctx.clone();
        eframe::App::raw_input_hook(&mut self.app, &ctx, &mut raw);
        // One pass, like eframe itself: the app's logic (shortcuts, typing) runs inside the UI pass.
        let mut actions: Vec<TouchAction> = Vec::new();
        let divisor = self.pressure_divisor;
        let touch = &mut self.touch;
        let mut full = ctx.run_ui(raw, |ui| {
            // Panel edges and splitters get a much wider grab area, so a fingertip can find them.
            let style_ctx = ui.ctx().clone();
            let theme = style_ctx.theme();
            if style_ctx.style_of(theme).interaction.resize_grab_radius_side < 14.0 {
                style_ctx.style_mut_of(theme, |s| {
                    s.interaction.resize_grab_radius_side = 14.0;
                    s.interaction.resize_grab_radius_corner = 18.0;
                });
            }
            eframe::App::logic(&mut self.app, ui.ctx(), &mut self.frame);
            eframe::App::ui(&mut self.app, ui, &mut self.frame);
            touch_ui(ui.ctx(), rect, self.app.last_canvas_rect, touch, divisor, &mut actions);
        });
        self.cursor = full.platform_output.cursor_icon;
        self.refresh_mods();
        for a in actions {
            self.do_action(a);
        }
        self.handle_output(&full.platform_output);
        // The on-screen keyboard is wanted only where text can be typed.
        self.wants_keyboard = full.platform_output.ime.is_some();
        if keys_in > 0 && KEY_LOG.load(Ordering::Relaxed) < 80 {
            let left = ctx.input(|i| i.events.iter().filter(|e| matches!(e, egui::Event::Key { pressed: true, .. })).count());
            note(&format!(
                "keys: {keys_in} pressed this frame, {left} left unused; typing field: {}; focus id: {:?}",
                self.wants_keyboard,
                ctx.memory(|m| m.focused())
            ));
            KEY_LOG.fetch_add(1, Ordering::Relaxed);
        }

        if let Some(vo) = full.viewport_output.get(&egui::ViewportId::ROOT)
            && let Some(at) = Instant::now().checked_add(vo.repaint_delay)
        {
            wake_at(&self.deadline, at);
        }

        let ppp_out = full.pixels_per_point;
        let shapes = std::mem::take(&mut full.shapes);
        let prims = ctx.tessellate(shapes, ppp_out);

        let rs = self.rs.clone();
        {
            let mut renderer = rs.renderer.write();
            for (id, deltas) in full.textures_delta.set.drain() {
                for d in deltas {
                    renderer.update_texture(&rs.device, &rs.queue, id, &d);
                }
            }
        }

        if self.needs_reconfigure {
            self.surface.configure(&rs.device, &self.config);
            self.needs_reconfigure = false;
        }
        let frame = if prims.is_empty() {
            // The app skips painting for one frame at times. Showing that empty frame would
            // flash a blank screen, so the picture on screen is simply kept.
            None
        } else {
            match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => Some(f),
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                    self.needs_reconfigure = true;
                    Some(f)
                }
                _ => {
                    self.surface.configure(&rs.device, &self.config);
                    wake_at(&self.deadline, Instant::now());
                    None
                }
            }
        };

        if let Some(frame) = frame {
            let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder =
                rs.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("photocraft") });
            let screen = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [self.config.width, self.config.height],
                pixels_per_point: ppp_out,
            };
            let extra = {
                let mut renderer = rs.renderer.write();
                renderer.update_buffers(&rs.device, &rs.queue, &mut encoder, &prims, &screen)
            };
            {
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("photocraft"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.09, g: 0.09, b: 0.10, a: 1.0 }),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                let renderer = rs.renderer.read();
                renderer.render(&mut pass.forget_lifetime(), &prims, &screen);
            }
            rs.queue.submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
            rs.queue.present(frame);
        }

        {
            let mut renderer = rs.renderer.write();
            for id in full.textures_delta.free.drain() {
                renderer.free_texture(&id);
            }
        }
        self.flags(true)
    }
}

// ---------------------------------------------------------------------------------------------
// The C interface Swift calls
// ---------------------------------------------------------------------------------------------

unsafe fn host<'a>(h: *mut c_void) -> Option<&'a mut Host> {
    unsafe { (h as *mut Host).as_mut() }
}

static HOOK: Once = Once::new();

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_create(layer: *mut c_void, width_px: u32, height_px: u32, scale: f32, cb: PCCallbacks) -> *mut c_void {
    HOOK.call_once(|| {
        // A fresh log each launch, once it has grown large.
        if let Some(dir) = documents_dir() {
            let log = dir.join("PhotoCraft-log.txt");
            if std::fs::metadata(&log).map(|m| m.len() > 3_000_000).unwrap_or(false) {
                let _ = std::fs::remove_file(&log);
            }
        }
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Info);
        std::panic::set_hook(Box::new(|info| {
            note(&format!("PANIC: {info}\n{}", std::backtrace::Backtrace::force_capture()));
        }));
    });
    guard(std::ptr::null_mut(), || match Host::new(layer, width_px, height_px, scale, cb) {
        Ok(h) => {
            note("started");
            Box::into_raw(Box::new(h)) as *mut c_void
        }
        Err(e) => {
            note(&format!("start failed: {e}"));
            std::ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_destroy(h: *mut c_void) {
    if !h.is_null() {
        guard((), || drop(unsafe { Box::from_raw(h as *mut Host) }));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_resize(h: *mut c_void, width_px: u32, height_px: u32, scale: f32) {
    guard((), || {
        if let Some(h) = unsafe { host(h) } {
            h.scale = scale;
            h.config.width = width_px.max(1);
            h.config.height = height_px.max(1);
            h.surface.configure(&h.rs.device, &h.config);
            h.wake();
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_set_dark(h: *mut c_void, dark: c_int) {
    if let Some(h) = unsafe { host(h) } {
        h.dark = dark != 0;
        h.wake();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_set_focused(h: *mut c_void, focused: c_int) {
    if let Some(h) = unsafe { host(h) } {
        h.focused = focused != 0;
        h.wake();
    }
}

/// The interface size: everything in the window is drawn this many times larger.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_set_ui_scale(h: *mut c_void, zoom: f32) {
    if let Some(h) = unsafe { host(h) }
        && zoom.is_finite()
    {
        h.ctx.set_zoom_factor(zoom.clamp(0.75, 2.5));
        h.wake();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_pointer_moved(h: *mut c_void, x: f32, y: f32) {
    if let Some(h) = unsafe { host(h) } {
        let p = h.pt(x, y);
        h.push(egui::Event::PointerMoved(p));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_pointer_gone(h: *mut c_void) {
    if let Some(h) = unsafe { host(h) } {
        h.push(egui::Event::PointerGone);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_pointer_button(h: *mut c_void, x: f32, y: f32, button: c_int, pressed: c_int, mods: u32) {
    if let Some(h) = unsafe { host(h) } {
        h.set_modifiers(to_mods(mods));
        let pos = h.pt(x, y);
        let button = match button {
            1 => egui::PointerButton::Secondary,
            2 => egui::PointerButton::Middle,
            _ => egui::PointerButton::Primary,
        };
        let m = h.modifiers;
        h.push(egui::Event::PointerButton { pos, button, pressed: pressed != 0, modifiers: m });
    }
}

/// Apple Pencil touching the screen. phase: 0 touches down, 1 moves, 2 lifts, 3 is cancelled.
/// Pressure is 0 to 1; tilt is in degrees (W3C style, like a browser's pen events); rotation is
/// the barrel roll in degrees. On the picture the Pencil draws at once; on a panel or menu it
/// taps, drags sliders and scrolls lists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_pen(
    h: *mut c_void,
    x: f32,
    y: f32,
    phase: c_int,
    pressure: f32,
    tilt_x: f32,
    tilt_y: f32,
    rotation: f32,
    mods: u32,
) {
    if let Some(h) = unsafe { host(h) } {
        h.set_modifiers(to_mods(mods));
        let pos = h.pt(x, y);
        let m = h.modifiers;
        // The shell sends the Pencil's force (1.0 is an average touch); full pressure is reached
        // at `pressure_divisor`.
        let sample = PenSample {
            pressure: h.pressure_curve(pressure, phase),
            tilt_x,
            tilt_y,
            rotation: rotation.rem_euclid(360.0),
            eraser: false,
        };
        if phase == 0 && PEN_LOG.fetch_add(1, Ordering::Relaxed) < 12 {
            note(&format!("pen down: force {pressure:.2}, tilt {tilt_x:.0}/{tilt_y:.0}, roll {rotation:.0}"));
        }
        match phase {
            0 => {
                if h.on_canvas(x, y) {
                    h.gesture = None;
                    h.pen_down = true;
                    h.feed.set(Some(sample));
                    h.push(egui::Event::PointerMoved(pos));
                    h.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: m });
                } else {
                    h.feed.set(None);
                    h.gesture_begin(x, y, true);
                }
            }
            1 => {
                if h.pen_down {
                    h.feed.set(Some(sample));
                    h.push(egui::Event::PointerMoved(pos));
                } else if h.gesture.is_some_and(|g| g.pen) {
                    h.gesture_move(x, y);
                }
            }
            _ => {
                if h.pen_down {
                    h.end_pen(x, y);
                } else if h.gesture.is_some_and(|g| g.pen) {
                    h.gesture_end(x, y, phase == 3);
                }
            }
        }
    }
}

/// Apple Pencil hovering above the screen: moves the pointer (the brush preview follows it) and
/// reports the Pencil's tilt and roll, with no pressure, like a pen hovering over a Mac tablet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_pen_hover(h: *mut c_void, x: f32, y: f32, tilt_x: f32, tilt_y: f32, rotation: f32) {
    if let Some(h) = unsafe { host(h) } {
        if h.pen_down {
            return;
        }
        let pos = h.pt(x, y);
        h.feed.set(Some(PenSample { pressure: 0.0, tilt_x, tilt_y, rotation: rotation.rem_euclid(360.0), eraser: false }));
        h.push(egui::Event::PointerMoved(pos));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_pen_hover_end(h: *mut c_void) {
    if let Some(h) = unsafe { host(h) } {
        if !h.pen_down {
            h.feed.set(None);
            h.push(egui::Event::PointerGone);
        }
    }
}

/// A finger. phase: 0 touches down, 1 moves, 2 lifts, 3 is cancelled. One finger taps our own
/// buttons, scrolls panels, pans the picture and opens menus; it never draws or selects on the
/// picture itself - only the Pencil and the mouse do that.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_finger(h: *mut c_void, phase: c_int, x: f32, y: f32, mods: u32) {
    if let Some(h) = unsafe { host(h) } {
        h.set_modifiers(to_mods(mods));
        match phase {
            0 => {
                // A finger landing always means no stroke is in progress. Release any stroke that
                // is somehow still down, so a lost Pencil lift can never make fingers paint.
                h.end_pen(x, y);
                h.gesture_begin(x, y, false);
            }
            1 => {
                if h.gesture.is_some_and(|g| !g.pen) {
                    h.gesture_move(x, y);
                }
            }
            _ => {
                if h.gesture.is_some_and(|g| !g.pen) {
                    h.gesture_end(x, y, phase == 3);
                }
            }
        }
    }
}

/// A long press by a finger: a right click there.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_long_press(h: *mut c_void, x: f32, y: f32) {
    if let Some(h) = unsafe { host(h) } {
        if let Some(g) = h.gesture.as_mut() {
            if g.mode != GMode::Pending {
                return;
            }
            g.consumed = true;
        }
        h.click(x, y, egui::PointerButton::Secondary, true);
    }
}

/// A click with the given button (0 left, 1 right) at a spot, for the Pencil squeeze.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_click(h: *mut c_void, x: f32, y: f32, button: c_int, mods: u32) {
    if let Some(h) = unsafe { host(h) } {
        h.set_modifiers(to_mods(mods));
        let b = if button == 1 { egui::PointerButton::Secondary } else { egui::PointerButton::Primary };
        h.click(x, y, b, button == 1);
    }
}

/// A fingertip rests on the screen while the Pencil draws: keeps Shift down (keeps proportions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_set_touch_shift(h: *mut c_void, on: c_int) {
    if let Some(h) = unsafe { host(h) } {
        h.touch_shift = on != 0;
        h.refresh_mods();
        h.wake();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_scroll(h: *mut c_void, dx: f32, dy: f32, mods: u32) {
    if let Some(h) = unsafe { host(h) } {
        h.set_modifiers(to_mods(mods));
        h.wheel(dx, dy);
    }
}

/// Pinch: zooms the picture around (x, y), like a Mac trackpad pinch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_zoom(h: *mut c_void, factor: f32, x: f32, y: f32) {
    if let Some(h) = unsafe { host(h) } {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let p = h.pt(x, y);
        h.push(egui::Event::PointerMoved(p));
        h.push(egui::Event::Zoom(factor));
    }
}

/// A two-finger twist: rotates the picture, like turning a trackpad with two fingers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_rotate(h: *mut c_void, radians: f32) {
    if let Some(h) = unsafe { host(h) } {
        h.push(egui::Event::Rotate(radians));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_key(h: *mut c_void, hid: u32, pressed: c_int, repeat: c_int, mods: u32) {
    if let Some(h) = unsafe { host(h) } {
        h.set_modifiers(to_mods(mods));
        let Some(key) = hid_to_key(hid) else { return };
        let m = h.modifiers;
        if pressed != 0 && KEY_LOG.load(Ordering::Relaxed) < 80 {
            note(&format!("key sent: {key:?} (command: {}, shift: {})", m.command, m.shift));
        }
        h.push(egui::Event::Key { key, physical_key: Some(key), pressed: pressed != 0, repeat: repeat != 0, modifiers: m });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_text(h: *mut c_void, utf8: *const c_char) {
    if let Some(h) = unsafe { host(h) }
        && let Some(s) = unsafe { from_c(utf8) }
        && !s.is_empty()
    {
        h.push(egui::Event::Text(s));
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_copy(h: *mut c_void) {
    if let Some(h) = unsafe { host(h) } {
        h.push(egui::Event::Copy);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_cut(h: *mut c_void) {
    if let Some(h) = unsafe { host(h) } {
        h.push(egui::Event::Cut);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_paste(h: *mut c_void, utf8: *const c_char) {
    if let Some(h) = unsafe { host(h) } {
        let s = unsafe { from_c(utf8) }.unwrap_or_default();
        h.push(egui::Event::Paste(s));
    }
}

/// A file opened from outside the app (Files, "Open in"): opened on the next frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_open_file(h: *mut c_void, name: *const c_char, data: *const u8, len: usize) {
    if let Some(h) = unsafe { host(h) }
        && let Some(name) = unsafe { from_c(name) }
    {
        note(&format!("a file arrived: {name}, {len} bytes"));
        let bytes = unsafe { bytes_from(data, len) };
        h.inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
        h.wake();
    }
}

/// One file chosen in the Open sheet. Call once per file, then pc_dialog_end.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_dialog_file(_h: *mut c_void, name: *const c_char, data: *const u8, len: usize) {
    if let Some(name) = unsafe { from_c(name) } {
        let bytes = unsafe { bytes_from(data, len) };
        note(&format!("the Open sheet returned {name}, {len} bytes"));
        DIALOG.with(|d| {
            if let Some(p) = d.borrow_mut().as_mut() {
                p.files.push((name, bytes));
            }
        });
    }
}

/// A file chosen in place (not copied): the app reads and saves it by its real path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_dialog_path(_h: *mut c_void, path: *const c_char) {
    if let Some(path) = unsafe { from_c(path) } {
        note(&format!("the Open sheet returned a path: {path}"));
        DIALOG.with(|d| {
            if let Some(p) = d.borrow_mut().as_mut() {
                p.paths.push(path);
            }
        });
    }
}

/// The Open sheet is finished (ok = 1) or was cancelled (ok = 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_dialog_end(h: *mut c_void, ok: c_int) {
    let pending = DIALOG.with(|d| d.borrow_mut().take());
    if let Some(p) = pending {
        let answer = if ok == 0 || (p.files.is_empty() && p.paths.is_empty()) {
            None
        } else if !p.paths.is_empty() {
            Some(FileDialogAnswer::Paths(p.paths))
        } else if p.multiple {
            Some(FileDialogAnswer::ContentsMany(p.files))
        } else {
            p.files.into_iter().next().map(|(name, bytes)| FileDialogAnswer::Contents(name, bytes))
        };
        p.reply.send(answer);
    }
    if let Some(h) = unsafe { host(h) } {
        h.wake();
    }
}

/// The Save to Files sheet is finished: the place chosen (a path), or null when cancelled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_save_dialog_end(h: *mut c_void, path: *const c_char) {
    let reply = SAVE_DIALOG.with(|d| d.borrow_mut().take());
    if let Some(reply) = reply {
        let chosen = unsafe { from_c(path) };
        note(&format!("the Save sheet returned: {chosen:?}"));
        reply.send(chosen.map(FileDialogAnswer::SaveTo));
    }
    if let Some(h) = unsafe { host(h) } {
        h.wake();
    }
}

/// Called every display refresh. Bit 0: a frame was drawn. Bit 1: the on-screen keyboard is wanted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_frame(h: *mut c_void) -> c_int {
    guard(0, || match unsafe { host(h) } {
        Some(h) => h.frame(),
        None => 0,
    })
}

/// The app goes to the background: lets the app save what it keeps between sessions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_save_state(h: *mut c_void) {
    guard((), || {
        if let Some(h) = unsafe { host(h) } {
            eframe::App::on_exit(&mut h.app);
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_is_dirty(h: *mut c_void) -> c_int {
    match unsafe { host(h) } {
        Some(h) => h.app.has_unsaved_work() as c_int,
        None => 0,
    }
}
