//! Win32/GDI front-end. GDI only, deliberately: the installer runs before the
//! Helios driver exists, i.e. on Microsoft Basic Display, where a GPU-backed
//! toolkit cannot be assumed to initialise.

use std::cell::{Cell, RefCell};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{prepare_payload, start_worker, Event, Op};

// ── Theme ───────────────────────────────────────────────────────────────────
const BG: u32 = 0x001B1614; // COLORREF is 0x00BBGGRR
const CARD: u32 = 0x0029201C;
const CARD_HOVER: u32 = 0x00382823;
const EDGE: u32 = 0x0040302A;     // #2A3040
const TEXT: u32 = 0x00F2EBE8;
const MUTED: u32 = 0x00B3A298;
const ACCENT_A: (u8, u8, u8) = (0x75, 0x50, 0xDC);
const ACCENT_B: (u8, u8, u8) = (0x3C, 0x88, 0xF5);
const SUCCESS: u32 = 0x008FC235;
const ERROR: u32 = 0x004A56F0;   // #F0564A
const ACCENT: u32 = 0x00DC5075;  // #7550DC
const LOG_BG: u32 = 0x0016110F;

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}
fn colorref(c: u32) -> COLORREF {
    c
}

#[derive(Clone, Copy, Default)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
impl Rect {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
    fn right(&self) -> f32 {
        self.x + self.w
    }
    fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Install,
    Repair,
    Uninstall,
    Copy,
    Reboot,
}

struct Button {
    id: Btn,
    label: String,
    rect: Rect,
    primary: bool,
    enabled: bool,
    visible: bool,
}

struct Logo {
    width: i32,
    height: i32,
    info: BITMAPINFO,
    bits: Vec<u8>,
}

struct State {
    hwnd: HWND,
    edit: HWND,
    edit_brush: HBRUSH,
    logo: Option<Logo>,
    font_title: HFONT,
    font_body: HFONT,
    font_small: HFONT,
    font_button: HFONT,
    font_status: HFONT,
    font_mono: HFONT,
    scale: f32,
    exe: PathBuf,
    payload_dir: PathBuf,
    payload_temporary: bool,
    workspace_ready: bool,
    prepared_once: bool,
    installed: bool,
    installed_version: String,
    bundle_version: String,
    has_install: bool,
    updating: bool,
    automatic: bool,
    worker: Option<Receiver<Event>>,
    running: bool,
    failed: bool,
    progress: u8,
    progress_active: bool,
    status: String,
    hover: Option<Btn>,
    buttons: Vec<Button>,
    card: Rect,
    progress_rect: Rect,
    log_rect: Rect,
    reboot_pending: bool,
    reboot_prompt: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    // WM_CTLCOLOR* is delivered to this window *re-entrantly*: append_log's
    // SendMessageW(EM_REPLACESEL) makes the EDIT paint, and the EDIT asks its
    // parent for the background brush while the caller's `with_state` borrow is
    // still live. Reaching into STATE from the WM_CTLCOLOR* handler therefore
    // panicked ("RefCell already borrowed"), and under panic=abort that aborted
    // the whole installer the moment any operation was started. The brush is a
    // plain handle, so it lives in its own slot and the handler never touches
    // STATE. See the WM_CTLCOLOR* arm below.
    static EDIT_BRUSH: Cell<HBRUSH> = const { Cell::new(std::ptr::null_mut()) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|cell| f(cell.borrow_mut().as_mut().expect("state")))
}

fn zeroed<T>() -> T {
    unsafe { std::mem::zeroed() }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ── msimg32 gradient ────────────────────────────────────────────────────────
#[repr(C)]
struct TriVertex {
    x: i32,
    y: i32,
    red: u16,
    green: u16,
    blue: u16,
    alpha: u16,
}
#[repr(C)]
struct GradientRect {
    upper_left: u32,
    lower_right: u32,
}
#[link(name = "msimg32")]
extern "system" {
    fn GradientFill(
        hdc: HDC,
        vertex: *const TriVertex,
        vertex_count: u32,
        mesh: *const GradientRect,
        mesh_count: u32,
        mode: u32,
    ) -> BOOL;
}
const GRADIENT_FILL_RECT_H: u32 = 0;

fn fill_gradient(hdc: HDC, rect: Rect, from: (u8, u8, u8), to: (u8, u8, u8)) {
    let v = [
        TriVertex {
            x: rect.x as i32,
            y: rect.y as i32,
            red: (from.0 as u16) * 257,
            green: (from.1 as u16) * 257,
            blue: (from.2 as u16) * 257,
            alpha: 0,
        },
        TriVertex {
            x: rect.right() as i32,
            y: rect.bottom() as i32,
            red: (to.0 as u16) * 257,
            green: (to.1 as u16) * 257,
            blue: (to.2 as u16) * 257,
            alpha: 0,
        },
    ];
    let mesh = GradientRect { upper_left: 0, lower_right: 1 };
    unsafe {
        GradientFill(hdc, v.as_ptr(), 2, &mesh, 1, GRADIENT_FILL_RECT_H);
    }
}

fn rounded(hdc: HDC, rect: Rect, radius: f32, fill: u32) {
    let brush = unsafe { CreateSolidBrush(colorref(fill)) };
    let pen = unsafe { CreatePen(PS_SOLID, 1, colorref(EDGE)) };
    let old_brush = unsafe { SelectObject(hdc, brush as HGDIOBJ) };
    let old_pen = unsafe { SelectObject(hdc, pen as HGDIOBJ) };
    unsafe {
        RoundRect(
            hdc,
            rect.x as i32,
            rect.y as i32,
            rect.right() as i32,
            rect.bottom() as i32,
            (radius * 2.0) as i32,
            (radius * 2.0) as i32,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        DeleteObject(pen as HGDIOBJ);
        DeleteObject(brush as HGDIOBJ);
    }
}

fn rounded_gradient(hdc: HDC, rect: Rect, radius: f32, from: (u8, u8, u8), to: (u8, u8, u8)) {
    let region = unsafe {
        CreateRoundRectRgn(
            rect.x as i32,
            rect.y as i32,
            rect.right() as i32 + 1,
            rect.bottom() as i32 + 1,
            (radius * 2.0) as i32,
            (radius * 2.0) as i32,
        )
    };
    unsafe {
        SelectClipRgn(hdc, region);
        fill_gradient(hdc, rect, from, to);
        SelectClipRgn(hdc, std::ptr::null_mut());
        DeleteObject(region as HGDIOBJ);
    }
}

fn text(hdc: HDC, font: HFONT, value: &str, rect: Rect, color: u32, align: u32) {
    let old = unsafe { SelectObject(hdc, font as HGDIOBJ) };
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, colorref(color));
        let mut r = RECT {
            left: rect.x as i32,
            top: rect.y as i32,
            right: rect.right() as i32,
            bottom: rect.bottom() as i32,
        };
        let mut w: Vec<u16> = value.encode_utf16().collect();
        DrawTextW(hdc, w.as_mut_ptr(), w.len() as i32, &mut r, align | DT_NOPREFIX);
        SelectObject(hdc, old);
    }
}

fn font(px: f32, weight: i32, family: &str) -> HFONT {
    let name = wide(family);
    unsafe {
        CreateFontW(
            -(px as i32),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_TT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            name.as_ptr(),
        )
    }
}

fn load_logo() -> Option<Logo> {
    let bytes = include_bytes!("../assets/logo_bg.bmp");
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return None;
    }
    let data_offset = u32::from_le_bytes(bytes[10..14].try_into().ok()?) as usize;
    let width = i32::from_le_bytes(bytes[18..22].try_into().ok()?);
    let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?);
    let bpp = u16::from_le_bytes(bytes[28..30].try_into().ok()?);
    if bpp != 24 {
        return None;
    }
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader.biSize = 40;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 24;
    info.bmiHeader.biCompression = BI_RGB;
    let stride = ((width * 3 + 3) / 4 * 4) as usize;
    let end = (data_offset + stride * height as usize).min(bytes.len());
    Some(Logo { width, height, info, bits: bytes[data_offset..end].to_vec() })
}

// ── Layout ──────────────────────────────────────────────────────────────────
const PAD: f32 = 24.0;
const HEADER: f32 = 84.0;
const CARD_H: f32 = 74.0;
const BTN_H: f32 = 44.0;
const BTN_GAP: f32 = 10.0;
const RADIUS: f32 = 10.0;
const LOGICAL_W: i32 = 780;
const LOGICAL_H: i32 = 620;

fn layout(state: &mut State, client_w: i32, client_h: i32) {
    let s = state.scale;
    let pad = PAD * s;
    let cw = client_w as f32;
    let ch = client_h as f32;
    let mut y = pad + HEADER * s;
    state.card = Rect { x: pad, y, w: cw - 2.0 * pad, h: CARD_H * s };
    y += CARD_H * s + 18.0 * s;

    let gap = BTN_GAP * s;
    let bw = (cw - 2.0 * pad - 2.0 * gap) / 3.0;
    let bh = BTN_H * s;
    for (index, button) in state.buttons.iter_mut().enumerate() {
        if index < 3 {
            button.rect = Rect { x: pad + index as f32 * (bw + gap), y, w: bw, h: bh };
        }
    }
    y += bh + 20.0 * s + 20.0 * s;
    state.progress_rect = Rect { x: pad, y, w: cw - 2.0 * pad, h: 10.0 * s };
    y += 10.0 * s + 18.0 * s;
    let footer_h = 40.0 * s;
    let log_h = (ch - pad - footer_h - 8.0 * s - y).max(120.0 * s);
    state.log_rect = Rect { x: pad, y, w: cw - 2.0 * pad, h: log_h };

    let footer_y = ch - pad - footer_h + 4.0 * s;
    set_button(state, Btn::Copy, Rect { x: pad, y: footer_y, w: 140.0 * s, h: 32.0 * s });
    set_button(state, Btn::Reboot, Rect { x: cw - pad - 150.0 * s, y: footer_y, w: 150.0 * s, h: 32.0 * s });

    if state.edit != std::ptr::null_mut() {
        unsafe {
            MoveWindow(
                state.edit,
                state.log_rect.x as i32,
                state.log_rect.y as i32,
                state.log_rect.w as i32,
                state.log_rect.h as i32,
                1,
            );
        }
    }
}

fn set_button(state: &mut State, id: Btn, rect: Rect) {
    if let Some(button) = state.buttons.iter_mut().find(|b| b.id == id) {
        button.rect = rect;
    }
}

// ── State detection ─────────────────────────────────────────────────────────
fn json_string(text: &str, key: &str) -> String {
    let needle = format!("\"{key}\"");
    let Some(k) = text.find(&needle) else { return String::new() };
    let Some(colon) = text[k + needle.len()..].find(':') else { return String::new() };
    let rest = &text[k + needle.len() + colon + 1..];
    let Some(open) = rest.find('"') else { return String::new() };
    let Some(close) = rest[open + 1..].find('"') else { return String::new() };
    rest[open + 1..open + 1 + close].to_string()
}

fn read_manifest_version(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("manifest.json"))
        .map(|t| json_string(&t, "version"))
        .unwrap_or_default()
}

fn refresh(state: &mut State) {
    let state_path = crate::helios_data_dir().join("install-state.json");
    state.installed = state_path.is_file();
    state.installed_version = std::fs::read_to_string(&state_path)
        .map(|t| json_string(&t, "version"))
        .map(|v| if v.is_empty() { v } else { format!("v{v}") })
        .unwrap_or_default();
    let install = state.payload_dir.join("Install-Helios.ps1").is_file();
    let uninstall = state.payload_dir.join("Uninstall-Helios.ps1").is_file();
    state.has_install = install;
    let bundle = read_manifest_version(&state.payload_dir);
    state.bundle_version = if bundle.is_empty() { String::new() } else { format!("v{bundle}") };
    state.updating = state.installed
        && !state.installed_version.is_empty()
        && state.bundle_version != state.installed_version;

    let busy = state.running;
    let updating = state.updating;
    for button in state.buttons.iter_mut() {
        button.label = match button.id {
            Btn::Repair if updating => "Update".to_string(),
            Btn::Install => "Install".to_string(),
            Btn::Repair => "Repair".to_string(),
            Btn::Uninstall => "Uninstall".to_string(),
            Btn::Copy => "Copy logs".to_string(),
            Btn::Reboot => "Reboot now".to_string(),
        };
        button.enabled = match button.id {
            Btn::Install => !state.installed && install && !busy,
            Btn::Repair => state.installed && install && !busy,
            Btn::Uninstall => state.installed && uninstall && !busy,
            Btn::Copy => true,
            Btn::Reboot => state.reboot_pending,
        };
        if button.id == Btn::Reboot {
            button.visible = state.reboot_pending;
        }
    }
}

// ── Painting ────────────────────────────────────────────────────────────────
fn paint(state: &State, hdc: HDC, client_w: i32, client_h: i32) {
    let s = state.scale;
    let pad = PAD * s;

    unsafe {
        let bg = CreateSolidBrush(colorref(BG));
        let mut full = RECT { left: 0, top: 0, right: client_w, bottom: client_h };
        FillRect(hdc, &mut full, bg);
        DeleteObject(bg as HGDIOBJ);
    }

    // Header.
    if let Some(logo) = &state.logo {
        let dest = Rect { x: pad, y: pad, w: 56.0 * s, h: 56.0 * s };
        unsafe {
            StretchDIBits(
                hdc,
                dest.x as i32,
                dest.y as i32,
                dest.w as i32,
                dest.h as i32,
                0,
                0,
                logo.width,
                logo.height,
                logo.bits.as_ptr() as *const _,
                &logo.info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    text(
        hdc,
        state.font_title,
        "Helios vGPU",
        Rect { x: pad + 72.0 * s, y: pad - 4.0 * s, w: client_w as f32 - 2.0 * pad - 72.0 * s, h: 36.0 * s },
        TEXT,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let subtitle = if state.bundle_version.is_empty() {
        "Windows graphics stack setup".to_string()
    } else {
        format!("Windows graphics stack setup  \u{2022}  {}", state.bundle_version)
    };
    text(
        hdc,
        state.font_small,
        &subtitle,
        Rect { x: pad + 73.0 * s, y: pad + 32.0 * s, w: client_w as f32 - 2.0 * pad - 72.0 * s, h: 22.0 * s },
        MUTED,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Card.
    rounded(hdc, state.card, RADIUS * s, CARD);
    unsafe {
        let tint = if state.failed {
            ERROR
        } else if state.updating {
            ACCENT
        } else if state.installed {
            SUCCESS
        } else {
            MUTED
        };
        let dot = CreateSolidBrush(colorref(tint));
        let old = SelectObject(hdc, dot as HGDIOBJ);
        Ellipse(
            hdc,
            (state.card.x + 18.0 * s) as i32,
            (state.card.y + state.card.h / 2.0 - 5.0 * s) as i32,
            (state.card.x + 28.0 * s) as i32,
            (state.card.y + state.card.h / 2.0 + 5.0 * s) as i32,
        );
        SelectObject(hdc, old);
        DeleteObject(dot as HGDIOBJ);
    }
    let card_text_x = state.card.x + 40.0 * s;
    let card_w = state.card.w - 56.0 * s;
    let installed_label = if state.installed_version.is_empty() {
        "Helios is installed".to_string()
    } else {
        format!("Helios {} is installed", state.installed_version)
    };
    let card_state = if state.installed { installed_label } else { "Helios is not installed".to_string() };
    text(
        hdc,
        state.font_body,
        &card_state,
        Rect { x: card_text_x, y: state.card.y + 12.0 * s, w: card_w, h: 24.0 * s },
        TEXT,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let hint = if state.installed && !state.has_install {
        "This is the stored installer. Uninstall is available; run a full bundle to update.".to_string()
    } else if state.updating {
        format!("Update {} to {} installs the newer driver and registrations.", state.installed_version, state.bundle_version)
    } else if state.installed {
        "Repair re-applies the driver and registrations; Uninstall removes them.".to_string()
    } else if !state.has_install {
        "The package payload is missing; this installer cannot perform a fresh install.".to_string()
    } else {
        "Install the WDDM driver, Direct3D 11/12, Vulkan, OpenGL and OpenCL.".to_string()
    };
    text(
        hdc,
        state.font_small,
        &hint,
        Rect { x: card_text_x, y: state.card.y + 36.0 * s, w: card_w, h: 22.0 * s },
        MUTED,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Buttons.
    for button in &state.buttons {
        if !button.visible {
            continue;
        }
        let hot = state.hover == Some(button.id) && button.enabled;
        if button.primary && button.enabled {
            rounded_gradient(hdc, button.rect, RADIUS * s, ACCENT_A, ACCENT_B);
            if hot {
                unsafe {
                    let pen = CreatePen(PS_SOLID, 1, colorref(0x00FFFFFF));
                    let old = SelectObject(hdc, pen as HGDIOBJ);
                    let null = CreateSolidBrush(colorref(CARD));
                    let oldb = SelectObject(hdc, null as HGDIOBJ);
                    RoundRect(hdc, button.rect.x as i32, button.rect.y as i32,
                              button.rect.right() as i32, button.rect.bottom() as i32,
                              (RADIUS * s * 2.0) as i32, (RADIUS * s * 2.0) as i32);
                    SelectObject(hdc, oldb);
                    SelectObject(hdc, old);
                    DeleteObject(null as HGDIOBJ);
                    DeleteObject(pen as HGDIOBJ);
                }
            }
            text(hdc, state.font_button, &button.label, button.rect, 0x00FFFFFF, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        } else {
            rounded(hdc, button.rect, RADIUS * s, if !button.enabled { CARD } else if hot { CARD_HOVER } else { CARD });
            let color = if button.enabled { TEXT } else { 0x00A3938A };
            text(hdc, state.font_button, &button.label, button.rect, color, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
    }

    // Status + percent.
    text(
        hdc,
        state.font_status,
        &state.status,
        Rect { x: pad, y: state.progress_rect.y - 22.0 * s, w: client_w as f32 - 2.0 * pad, h: 20.0 * s },
        MUTED,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    if state.progress_active {
        text(
            hdc,
            state.font_status,
            &format!("{}%", state.progress),
            Rect { x: pad, y: state.progress_rect.y - 22.0 * s, w: client_w as f32 - 2.0 * pad, h: 20.0 * s },
            MUTED,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    // Progress bar.
    rounded(hdc, state.progress_rect, state.progress_rect.h / 2.0, CARD);
    if state.progress_active && state.progress > 0 {
        let mut fill = state.progress_rect;
        fill.w = state.progress_rect.w * (state.progress as f32 / 100.0);
        if fill.w < fill.h {
            fill.w = fill.h;
        }
        rounded_gradient(hdc, fill, fill.h / 2.0, ACCENT_A, ACCENT_B);
    }
}

fn draw(state: &State, hwnd: HWND) {
    let mut ps = zeroed();
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = zeroed();
        GetClientRect(hwnd, &mut client);
        let w = client.right;
        let h = client.bottom;
        let mem = CreateCompatibleDC(hdc);
        let bitmap = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bitmap as HGDIOBJ);
        paint(state, mem, w, h);
        BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bitmap as HGDIOBJ);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

// ── Operations ──────────────────────────────────────────────────────────────
fn ensure_workspace(state: &mut State) -> Result<(), String> {
    if state.workspace_ready {
        return Ok(());
    }
    if !state.prepared_once {
        match prepare_payload(&state.exe) {
            Ok(payload) => {
                state.payload_dir = payload.dir.clone();
                state.has_install = payload.has_install;
                state.payload_temporary = payload.temporary;
            }
            Err(error) => return Err(error.to_string()),
        }
        state.prepared_once = true;
    } else {
        return Err("the embedded payload could not be prepared".to_string());
    }
    state.workspace_ready = true;
    Ok(())
}

fn start_operation(state: &mut State, op: Op) {
    if state.running {
        return;
    }
    if let Err(error) = ensure_workspace(state) {
        state.status = format!("Could not prepare the payload: {error}");
        return;
    }
    if matches!(op, Op::Install | Op::Repair) && !state.has_install {
        state.status = "This stored installer can only uninstall.".to_string();
        return;
    }
    state.running = true;
    state.failed = false;
    state.progress = 0;
    state.progress_active = true;
    state.reboot_pending = false;
    state.status = match op {
        Op::Install => "Installing Helios\u{2026}".to_string(),
        Op::Repair if state.updating => "Updating Helios\u{2026}".to_string(),
        Op::Repair => "Repairing the Helios installation\u{2026}".to_string(),
        Op::Uninstall => "Removing Helios\u{2026}".to_string(),
    };
    append_log(state, &format!("[setup] {}", state.status));
    let payload = crate::Payload {
        dir: state.payload_dir.clone(),
        temporary: state.payload_temporary,
        has_install: state.has_install,
    };
    let receiver = start_worker(payload, op, state.automatic);
    state.worker = Some(receiver);
    unsafe {
        SetTimer(state.hwnd, 1, 50, None);
    }
}

fn drain(state: &mut State) {
    let mut events = Vec::new();
    if let Some(receiver) = &state.worker {
        loop {
            match receiver.try_recv() {
                Ok(event) => events.push(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The sender drops right after sending Done, so a normal
                    // completion is followed by Disconnected. Only synthesise a
                    // failure if no Done was received at all.
                    if !events.iter().any(|event| matches!(event, Event::Done(_))) {
                        events.push(Event::Done(1));
                    }
                    break;
                }
            }
        }
    }
    let mut done = None;
    for event in events {
        match event {
            Event::Log(line) => append_log(state, &line),
            Event::Progress(percent, message) => {
                state.progress = percent;
                state.progress_active = true;
                state.status = message;
            }
            Event::Done(code) => done = Some(code),
        }
    }
    if let Some(code) = done {
        state.worker = None;
        state.running = false;
        state.progress_active = false;
        if code == 0 {
            state.progress = 100;
        }
        refresh(state);
        if code == 0 {
            state.status = "Done.".to_string();
            append_log(state, "[setup] completed successfully.");
        } else if code == 3010 {
            state.status = "A reboot is required to finish.".to_string();
            state.reboot_pending = true;
            // drain runs under with_state, so defer the prompt until the borrow
            // is released (WM_TIMER shows it). See EDIT_BRUSH for the reentrancy.
            state.reboot_prompt = true;
            append_log(state, "[setup] a reboot is required to finish the installation.");
        } else if code == 2 {
            state.status = "This stored installer can only uninstall.".to_string();
        } else {
            state.failed = true;
            state.status = format!("Setup failed (exit code {code}). See the log above.");
            append_log(state, &format!("[setup] failed with exit code {code}."));
        }
        unsafe {
            KillTimer(state.hwnd, 1);
            InvalidateRect(state.hwnd, std::ptr::null(), 0);
        }
    }
    unsafe {
        InvalidateRect(state.hwnd, std::ptr::null(), 0);
    }
}

fn append_log(state: &mut State, line: &str) {
    if state.edit.is_null() {
        return;
    }
    let length = unsafe { GetWindowTextLengthW(state.edit) };
    let mut text: Vec<u16> = line.encode_utf16().chain("\r\n".encode_utf16()).collect();
    text.push(0);
    unsafe {
        SendMessageW(state.edit, EM_SETSEL, length as WPARAM, length as LPARAM);
        SendMessageW(state.edit, EM_REPLACESEL, 0, text.as_mut_ptr() as LPARAM);
        SendMessageW(state.edit, EM_SCROLLCARET, 0, 0);
    }
}

/// Show the reboot prompt. Must be called with the STATE borrow released:
/// MessageBoxW pumps messages, and a re-entrant `with_state` would panic.
fn prompt_reboot(hwnd: HWND) {
    let answer = unsafe {
        MessageBoxW(
            hwnd,
            wide("Helios is installed, but Windows must restart to load the new display driver.\r\n\r\nRestart now?").as_ptr(),
            wide("Helios vGPU Setup").as_ptr(),
            MB_ICONINFORMATION | MB_YESNO,
        )
    };
    if answer == IDYES {
        do_reboot();
    }
}

fn do_reboot() {
    // `shutdown.exe` avoids a hand-rolled token/privilege dance and is present
    // on every supported Windows. The process is already elevated.
    let _ = std::process::Command::new("shutdown.exe")
        .args(["/r", "/t", "0", "/f"])
        .creation_flags(0x0000_0008 /* DETACHED_PROCESS */)
        .spawn();
}

fn copy_logs(state: &State) {
    if state.edit.is_null() {
        return;
    }
    let length = unsafe { GetWindowTextLengthW(state.edit) };
    unsafe {
        SendMessageW(state.edit, EM_SETSEL, 0, length as LPARAM);
        SendMessageW(state.edit, WM_COPY, 0, 0);
    }
}

// ── Window procedure ────────────────────────────────────────────────────────
unsafe extern "system" fn wndproc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_CREATE => 0,
        WM_SIZE => {
            let w = (lparam as u32 & 0xFFFF) as i32;
            let h = ((lparam as u32 >> 16) & 0xFFFF) as i32;
            // WM_SIZE fires while the window is being created, before STATE is
            // installed; ignore it then, the post-create pass lays out.
            STATE.with(|cell| {
                if let Some(state) = cell.borrow_mut().as_mut() {
                    layout(state, w, h);
                }
            });
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            with_state(|state| draw(state, hwnd));
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let hdc = wparam as HDC;
            SetTextColor(hdc, rgb(0xC8, 0xD0, 0xDE));
            SetBkColor(hdc, colorref(LOG_BG));
            // Deliberately NOT with_state: this arm can run re-entrantly while a
            // with_state borrow is live (see EDIT_BRUSH).
            EDIT_BRUSH.with(|brush| brush.get() as LRESULT)
        }
        WM_MOUSEMOVE => {
            let x = (lparam as i32 & 0xFFFF) as f32;
            let y = ((lparam as i32 >> 16) & 0xFFFF) as f32;
            with_state(|state| {
                let hit = state
                    .buttons
                    .iter()
                    .find(|b| b.visible && b.enabled && b.rect.contains(x, y))
                    .map(|b| b.id);
                if hit != state.hover {
                    state.hover = hit;
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            });
            0
        }
        WM_MOUSELEAVE => {
            with_state(|state| state.hover = None);
            InvalidateRect(hwnd, std::ptr::null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let x = (lparam as i32 & 0xFFFF) as f32;
            let y = ((lparam as i32 >> 16) & 0xFFFF) as f32;
            let hit = with_state(|state| {
                state
                    .buttons
                    .iter()
                    .find(|b| b.visible && b.enabled && b.rect.contains(x, y))
                    .map(|b| b.id)
            });
            match hit {
                Some(Btn::Install) => with_state(|s| start_operation(s, Op::Install)),
                Some(Btn::Repair) => with_state(|s| start_operation(s, Op::Repair)),
                Some(Btn::Uninstall) => with_state(|s| start_operation(s, Op::Uninstall)),
                Some(Btn::Copy) => with_state(|s| copy_logs(s)),
                Some(Btn::Reboot) => {
                    let answer = MessageBoxW(
                        hwnd,
                        wide("Reboot Windows now to finish installing Helios?").as_ptr(),
                        wide("Helios vGPU Setup").as_ptr(),
                        MB_ICONQUESTION | MB_YESNO,
                    );
                    if answer == IDYES {
                        do_reboot();
                    }
                }
                None => {}
            }
            0
        }
        WM_SETCURSOR => {
            if (lparam as u32 & 0xFFFF) == HTCLIENT as u32 {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                ScreenToClient(hwnd, &mut p);
                let over = with_state(|state| {
                    state
                        .buttons
                        .iter()
                        .any(|b| b.visible && b.enabled && b.rect.contains(p.x as f32, p.y as f32))
                });
                if over {
                    SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_HAND));
                    return 1;
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_TIMER => {
            if wparam == 1 {
                let prompt = with_state(|state| {
                    drain(state);
                    std::mem::take(&mut state.reboot_prompt)
                });
                if prompt {
                    prompt_reboot(hwnd);
                }
            }
            0
        }
        WM_CLOSE => {
            let running = with_state(|state| state.running);
            if running {
                MessageBoxW(
                    hwnd,
                    wide("Helios setup is still running. Wait for it to finish.").as_ptr(),
                    wide("Helios vGPU Setup").as_ptr(),
                    MB_ICONINFORMATION | MB_OK,
                );
                return 0;
            }
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            with_state(|state| {
                for f in [
                    state.font_title,
                    state.font_body,
                    state.font_small,
                    state.font_button,
                    state.font_status,
                    state.font_mono,
                ] {
                    if !f.is_null() {
                        DeleteObject(f as HGDIOBJ);
                    }
                }
                if !state.edit_brush.is_null() {
                    DeleteObject(state.edit_brush as HGDIOBJ);
                }
            });
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

pub fn run(exe: &Path, automatic: bool) -> i32 {
    unsafe {
        SetProcessDPIAware();
    }
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = wide("HeliosSetupWindow");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: (CS_HREDRAW as u32) | (CS_VREDRAW as u32),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: unsafe { LoadIconW(instance, 101 as *const u16) },
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        hbrBackground: unsafe { CreateSolidBrush(colorref(BG)) },
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
        hIconSm: unsafe { LoadIconW(instance, 101 as *const u16) },
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return 1;
    }

    let mut desired = RECT { left: 0, top: 0, right: LOGICAL_W, bottom: LOGICAL_H };
    let style = (WS_OVERLAPPED as u32) | (WS_CAPTION as u32) | (WS_SYSMENU as u32) | (WS_MINIMIZEBOX as u32);
    unsafe {
        AdjustWindowRect(&mut desired, style, 0);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Helios vGPU Setup").as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            desired.right - desired.left,
            desired.bottom - desired.top,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return 1;
        }
        let dark: BOOL = 1;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, &dark as *const _ as *const _, std::mem::size_of::<BOOL>() as u32);

        STATE.with(|cell| {
            let mut state = State {
                hwnd,
                edit: std::ptr::null_mut(),
                edit_brush: std::ptr::null_mut(),
                logo: load_logo(),
                font_title: font(23.0, 700, "Segoe UI"),
                font_body: font(12.5, 400, "Segoe UI"),
                font_small: font(10.5, 400, "Segoe UI"),
                font_button: font(12.0, 600, "Segoe UI"),
                font_status: font(11.0, 400, "Segoe UI"),
                font_mono: font(11.0, 400, "Consolas"),
                scale: 1.0,
                exe: exe.to_path_buf(),
                payload_dir: exe.parent().unwrap_or(Path::new(".")).to_path_buf(),
                payload_temporary: false,
                workspace_ready: false,
                prepared_once: false,
                installed: false,
                installed_version: String::new(),
                bundle_version: String::new(),
                has_install: false,
                updating: false,
                automatic,
                worker: None,
                running: false,
                failed: false,
                progress: 0,
                progress_active: false,
                status: String::new(),
                hover: None,
                buttons: vec![
                    Button { id: Btn::Install, label: "Install".into(), rect: Rect::default(), primary: true, enabled: true, visible: true },
                    Button { id: Btn::Repair, label: "Repair".into(), rect: Rect::default(), primary: true, enabled: false, visible: true },
                    Button { id: Btn::Uninstall, label: "Uninstall".into(), rect: Rect::default(), primary: false, enabled: false, visible: true },
                    Button { id: Btn::Copy, label: "Copy logs".into(), rect: Rect::default(), primary: false, enabled: true, visible: true },
                    Button { id: Btn::Reboot, label: "Reboot now".into(), rect: Rect::default(), primary: false, enabled: false, visible: false },
                ],
                card: Rect::default(),
                progress_rect: Rect::default(),
                log_rect: Rect::default(),
                reboot_pending: false,
                reboot_prompt: false,
            };
            let mut client = zeroed();
            GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd);
            state.scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
            // Recreate fonts at the measured DPI.
            for f in [state.font_title, state.font_body, state.font_small, state.font_button, state.font_status, state.font_mono] {
                DeleteObject(f as HGDIOBJ);
            }
            state.font_title = font(23.0 * state.scale, 700, "Segoe UI");
            state.font_body = font(12.5 * state.scale, 400, "Segoe UI");
            state.font_small = font(10.5 * state.scale, 400, "Segoe UI");
            state.font_button = font(12.0 * state.scale, 600, "Segoe UI");
            state.font_status = font(11.0 * state.scale, 400, "Segoe UI");
            state.font_mono = font(11.0 * state.scale, 400, "Consolas");
            *cell.borrow_mut() = Some(state);
        });

        // The window was created at 96-DPI physical size. Scale the client up
        // for the real monitor DPI before laying out, or the scaled layout
        // overflows and the log control paints over the footer buttons.
        {
            let scale = with_state(|state| state.scale);
            let mut frame = RECT {
                left: 0,
                top: 0,
                right: (LOGICAL_W as f32 * scale) as i32,
                bottom: (LOGICAL_H as f32 * scale) as i32,
            };
            AdjustWindowRect(&mut frame, style, 0);
            with_state(|state| {
                SetWindowPos(
                    state.hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    frame.right - frame.left,
                    frame.bottom - frame.top,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            });
        }

        with_state(|state| {
            let edit = CreateWindowExW(
                0,
                wide("EDIT").as_ptr(),
                wide("").as_ptr(),
                (WS_CHILD as u32) | (WS_VISIBLE as u32) | (WS_VSCROLL as u32) | (ES_MULTILINE as u32) | (ES_READONLY as u32) | (ES_AUTOVSCROLL as u32),
                0,
                0,
                10,
                10,
                state.hwnd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            state.edit = edit;
            SendMessageW(edit, WM_SETFONT, state.font_mono as WPARAM, 1);
            state.edit_brush = CreateSolidBrush(colorref(LOG_BG));
            EDIT_BRUSH.with(|brush| brush.set(state.edit_brush));
            let mut client = zeroed();
            GetClientRect(state.hwnd, &mut client);
            layout(state, client.right, client.bottom);
            append_log(state, "Helios vGPU Setup");
            append_log(state, &format!("Installer: {}", state.exe.display()));
            if let Err(error) = ensure_workspace(state) {
                append_log(state, &format!("[setup] {error}"));
            }
            // ensure_workspace extracts the embedded payload and can turn a
            // "no Install-Helios.ps1" state into an installable one, so BOTH the
            // buttons and the ready-state line must be computed after it. The
            // shipped single-file exe otherwise reports the stored-installer
            // status and greys out Install even though the payload is right
            // there in the extracted directory.
            refresh(state);
            state.status = if state.installed && !state.has_install {
                "Only uninstall is available from this stored installer.".to_string()
            } else if state.updating {
                "An update is available.".to_string()
            } else if state.installed {
                "Ready to repair or uninstall.".to_string()
            } else {
                "Ready to install.".to_string()
            };
        });

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        let mut message = zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        with_state(|state| {
            if state.payload_temporary {
                let _ = std::fs::remove_dir_all(&state.payload_dir);
            }
        });
        0
    }
}

/// Attach to the invoking console so a CLI/silent run reports the external step.
pub fn console_line(value: &str) {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                let mut line: Vec<u16> = value.encode_utf16().collect();
                line.push(b'\r' as u16);
                line.push(b'\n' as u16);
                 let mut written = 0u32;
                 WriteConsoleW(handle, line.as_ptr() as *const _, line.len() as u32, &mut written, std::ptr::null());
             }
             FreeConsole();
         }
     }
}

