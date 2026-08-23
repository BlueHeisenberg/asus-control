use crate::config::FAN_NAMES;
use crate::worker::Shared;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE},
            Gdi::*,
            GdiPlus::*,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::*,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE},
            WindowsAndMessaging::*,
        },
    },
};

// control ids
const ID_FAN_BASE: isize = 200; // 200..206 fan select buttons
const ID_TAKE_CONTROL: isize = 210;
const ID_PRESET_DEFAULT: isize = 211;
const ID_PRESET_QUIET: isize = 212;
const ID_PRESET_FULL: isize = 213;
const ID_RELEASE_ALL: isize = 214;
const ID_TICK: isize = 220;

const TEMP_MIN: f32 = 20.0;
const TEMP_MAX: f32 = 110.0;

// ---------------- palette (plain web RRGGBB — converted at use site) --------
mod col {
    pub const BG: u32 = 0x0F1115;
    pub const CARD: u32 = 0x171A21;
    pub const CARD_HI: u32 = 0x1C202A;
    pub const BORDER: u32 = 0x262B36;
    pub const GRID: u32 = 0x222733;
    pub const TEXT: u32 = 0xE8EAF0;
    pub const TEXT_2: u32 = 0x99A2B2;
    pub const TEXT_3: u32 = 0x646C7C;
    pub const ACCENT: u32 = 0x4C9EFF;
    pub const ACCENT_BG: u32 = 0x14304F;
    pub const OK: u32 = 0x3FB950;
    pub const WARN: u32 = 0xD29922;
    pub const HOT: u32 = 0xF85149;
    pub const DANGER: u32 = 0xE5534B;
    pub const DANGER_BG: u32 = 0x2C1A1B;
}

/// Win32 COLORREF is 0x00BBGGRR — take a normal 0xRRGGBB literal and swap.
fn rgb(hex: u32) -> COLORREF {
    COLORREF(((hex & 0xFF) << 16) | (hex & 0xFF00) | ((hex >> 16) & 0xFF))
}
/// GDI+ wants 0xAARRGGBB, which is what our literals already are.
fn argb(hex: u32, alpha: u8) -> u32 {
    ((alpha as u32) << 24) | (hex & 0x00FF_FFFF)
}

// ---------------- layout ----------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Layout {
    header: RECT,
    tabs: RECT,
    tab_w: i32,
    ctrl: RECT,
    card: RECT,
    plot: RECT,
    side: RECT,
    footer: RECT,
}

struct Ui {
    shared: Arc<Shared>,
    selected: usize,
    dragging: Option<usize>,
    hover_pt: Option<usize>,
    dpi: i32,
    lay: Layout,
    font: HFONT,
    font_bold: HFONT,
    font_small: HFONT,
    font_mono: HFONT,
    brush_bg: HBRUSH,
    last_sig: u64,
}

impl Ui {
    /// logical px -> device px
    fn s(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------- window ----------------------------------------------------

pub fn run(shared: Arc<Shared>) {
    unsafe {
        // GDI+ — used for the curve, cards and dots so they aren't stair-stepped
        let mut token: usize = 0;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        GdiplusStartup(&mut token, &input, std::ptr::null_mut());

        let hinstance = GetModuleHandleW(None).unwrap();
        let him = windows::Win32::Foundation::HINSTANCE(hinstance.0);
        let class_name = wide("asus_control_wnd");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: him,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("asus-control").as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1220,
            780,
            None,
            None,
            him,
            None,
        )
        .expect("CreateWindowExW failed");

        // dark title bar — without this the frame is a bright 1990s strip
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            4,
        );

        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let ui = Box::new(Ui {
            shared,
            selected: 0,
            dragging: None,
            hover_pt: None,
            dpi,
            lay: Layout::default(),
            font: HFONT::default(),
            font_bold: HFONT::default(),
            font_small: HFONT::default(),
            font_mono: HFONT::default(),
            brush_bg: CreateSolidBrush(rgb(col::BG)),
            last_sig: 0,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ui) as _);
        make_fonts(get_ui(hwnd));

        let ui = get_ui(hwnd);
        let tick = ui.shared.tick_ms.load(Ordering::Relaxed) as u32;

        // resize to a DPI-correct default now that we know the DPI
        let (w, h) = (ui.s(1220), ui.s(780));
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER);

        let mut id = ID_FAN_BASE;
        for name in FAN_NAMES.iter() {
            create_child(hwnd, hinstance, "BUTTON", name, BS_OWNERDRAW as u32, id);
            id += 1;
        }
        create_child(hwnd, hinstance, "BUTTON", "Take control of this fan", BS_OWNERDRAW as u32, ID_TAKE_CONTROL);
        create_child(hwnd, hinstance, "BUTTON", "Default", BS_OWNERDRAW as u32, ID_PRESET_DEFAULT);
        create_child(hwnd, hinstance, "BUTTON", "Quiet", BS_OWNERDRAW as u32, ID_PRESET_QUIET);
        create_child(hwnd, hinstance, "BUTTON", "Full Speed", BS_OWNERDRAW as u32, ID_PRESET_FULL);
        create_child(hwnd, hinstance, "BUTTON", "Release all fans", BS_OWNERDRAW as u32, ID_RELEASE_ALL);

        let track = create_child(hwnd, hinstance, "msctls_trackbar32", "", TBS_HORZ | TBS_NOTICKS, ID_TICK);
        SendMessageW(track, TBM_SETRANGE, WPARAM(1), LPARAM((((2000u32) << 16) | 100u32) as i32 as isize));
        SendMessageW(track, TBM_SETPAGESIZE, WPARAM(0), LPARAM(100));
        SendMessageW(track, TBM_SETPOS, WPARAM(1), LPARAM(tick as isize));
        let _ = SetWindowTheme(track, PCWSTR(wide("DarkMode_Explorer").as_ptr()), PCWSTR(wide("").as_ptr()));

        relayout(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        SetTimer(hwnd, 1, tick, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        GdiplusShutdown(token);
    }
}

unsafe fn make_fonts(ui: &mut Ui) {
    for f in [ui.font, ui.font_bold, ui.font_small, ui.font_mono] {
        if !f.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(f.0));
        }
    }
    let mk = |px: i32, weight: i32, face: &str| -> HFONT {
        CreateFontW(
            -(px * ui.dpi / 96),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(wide(face).as_ptr()),
        )
    };
    ui.font = mk(15, 400, "Segoe UI");
    ui.font_bold = mk(15, 600, "Segoe UI");
    ui.font_small = mk(12, 600, "Segoe UI");
    ui.font_mono = mk(15, 600, "Consolas");
}

fn create_child(parent: HWND, hinstance: HMODULE, class: &str, text: &str, style: u32, id: isize) -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(wide(class).as_ptr()),
            PCWSTR(wide(text).as_ptr()),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
            0,
            0,
            10,
            10,
            parent,
            HMENU(id as *mut _),
            windows::Win32::Foundation::HINSTANCE(hinstance.0),
            None,
        )
        .expect("child creation failed")
    }
}

/// GdiPlus exports a constant named `Ok`, which shadows `Result::Ok` in this
/// module — so child lookups go through here instead of `if let Ok(..)`.
fn dlg(hwnd: HWND, id: isize) -> Option<HWND> {
    unsafe { GetDlgItem(hwnd, id as i32).ok().filter(|h| !h.is_invalid()) }
}

unsafe fn get_ui(hwnd: HWND) -> &'static mut Ui {
    &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui)
}

/// Single source of truth for geometry: children AND painted panels both come
/// from here, so they cannot drift apart and overlap.
fn relayout(hwnd: HWND) {
    unsafe {
        let ui = get_ui(hwnd);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        if rc.right < 10 {
            return;
        }
        let s = |v: i32| v * ui.dpi / 96;

        let pad = s(14);
        let header_h = s(40);
        let tab_h = s(34);
        let ctrl_h = s(34);
        let footer_h = s(58);
        let side_w = s(300).min((rc.right - pad * 2) / 3);
        let gap = s(8);

        let mut l = Layout::default();
        l.header = RECT { left: pad, top: s(6), right: rc.right - pad, bottom: s(6) + header_h };
        l.side = RECT { left: rc.right - pad - side_w, top: l.header.bottom, right: rc.right - pad, bottom: rc.bottom - footer_h };
        let left_r = l.side.left - gap;
        l.tabs = RECT { left: pad, top: l.header.bottom, right: left_r, bottom: l.header.bottom + tab_h };
        l.tab_w = ((l.tabs.right - l.tabs.left) - s(6) * 6) / FAN_NAMES.len() as i32;
        l.ctrl = RECT { left: pad, top: l.tabs.bottom + gap, right: left_r, bottom: l.tabs.bottom + gap + ctrl_h };
        l.card = RECT { left: pad, top: l.ctrl.bottom + gap, right: left_r, bottom: rc.bottom - footer_h };
        l.plot = RECT {
            left: l.card.left + s(46),
            top: l.card.top + s(46),
            right: l.card.right - s(18),
            bottom: l.card.bottom - s(48),
        };
        l.footer = RECT { left: pad, top: rc.bottom - footer_h, right: rc.right - pad, bottom: rc.bottom - s(10) };
        ui.lay = l;

        let mv = |id: isize, x: i32, y: i32, w: i32, h: i32| {
            if let Some(h_) = dlg(hwnd, id) {
                let _ = SetWindowPos(h_, None, x, y, w, h, SWP_NOZORDER);
            }
        };

        for i in 0..FAN_NAMES.len() as i32 {
            mv(ID_FAN_BASE + i as isize, l.tabs.left + i * (l.tab_w + s(6)), l.tabs.top, l.tab_w, tab_h - s(2));
        }

        // control bar: toggle then the three presets, right-aligned
        let cy = l.ctrl.top;
        let ch = ctrl_h - s(2);
        mv(ID_TAKE_CONTROL, l.ctrl.left, cy, s(228), ch);
        let pw = s(96);
        let px0 = l.ctrl.right - pw * 3 - s(12);
        mv(ID_PRESET_DEFAULT, px0, cy, pw, ch);
        mv(ID_PRESET_QUIET, px0 + pw + s(6), cy, pw, ch);
        mv(ID_PRESET_FULL, px0 + (pw + s(6)) * 2, cy, pw, ch);

        // footer
        let fh = s(32);
        let fy = l.footer.top + (l.footer.bottom - l.footer.top - fh) / 2;
        mv(ID_RELEASE_ALL, l.footer.left, fy, s(190), fh);
        mv(ID_TICK, l.footer.right - s(210), fy, s(210), fh);

        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if GetWindowLongPtrW(hwnd, GWLP_USERDATA) == 0 {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => paint(hwnd),
        WM_SIZE => relayout(hwnd),
        WM_GETMINMAXINFO => {
            let ui = get_ui(hwnd);
            let mmi = lp.0 as *mut MINMAXINFO;
            if !mmi.is_null() {
                (*mmi).ptMinTrackSize = POINT { x: ui.s(1000), y: ui.s(640) };
            }
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let ui = get_ui(hwnd);
            ui.dpi = ((wp.0 & 0xFFFF) as i32).max(96);
            make_fonts(ui);
            let r = lp.0 as *const RECT;
            if !r.is_null() {
                let _ = SetWindowPos(hwnd, None, (*r).left, (*r).top, (*r).right - (*r).left, (*r).bottom - (*r).top, SWP_NOZORDER);
            }
            relayout(hwnd);
            return LRESULT(0);
        }
        WM_CTLCOLORBTN | WM_CTLCOLORSTATIC => {
            let ui = get_ui(hwnd);
            SetBkColor(HDC(wp.0 as *mut _), rgb(col::BG));
            SetTextColor(HDC(wp.0 as *mut _), rgb(col::TEXT));
            return LRESULT(ui.brush_bg.0 as isize);
        }
        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                draw_owner_button(&*dis, get_ui(hwnd));
                return LRESULT(1);
            }
            return LRESULT(0);
        }
        WM_TIMER => {
            let ui = get_ui(hwnd);
            let sig = data_signature(ui);
            if sig != ui.last_sig {
                ui.last_sig = sig;
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
        WM_COMMAND => handle_command(hwnd, wp),
        WM_HSCROLL => {
            let ui = get_ui(hwnd);
            if let Some(track) = dlg(hwnd, ID_TICK) {
                let pos = SendMessageW(track, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as u32;
                ui.shared.tick_ms.store(pos.clamp(100, 2000), Ordering::Relaxed);
                ui.shared.persist();
                let _ = InvalidateRect(hwnd, Some(&ui.lay.footer), false);
            }
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN => mouse(hwnd, msg, lp),
        WM_MOUSEMOVE => {
            let ui = get_ui(hwnd);
            let pt = cursor_pos(lp);
            if let Some(idx) = ui.dragging {
                set_point_at_cursor(hwnd, pt, idx);
                let _ = InvalidateRect(hwnd, Some(&ui.lay.card), false);
            } else {
                let h = hit_point(ui, pt);
                if h != ui.hover_pt {
                    ui.hover_pt = h;
                    let _ = InvalidateRect(hwnd, Some(&ui.lay.card), false);
                }
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
        }
        WM_MOUSELEAVE => {
            let ui = get_ui(hwnd);
            if ui.hover_pt.take().is_some() {
                let _ = InvalidateRect(hwnd, Some(&ui.lay.card), false);
            }
        }
        WM_LBUTTONUP => {
            let ui = get_ui(hwnd);
            if ui.dragging.take().is_some() {
                let _ = ReleaseCapture();
                ui.shared.persist();
            }
        }
        WM_DESTROY => {
            let ui = get_ui(hwnd);
            ui.shared.persist();
            PostQuitMessage(0);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    LRESULT(0)
}

fn handle_command(hwnd: HWND, wp: WPARAM) {
    unsafe {
        let ui = get_ui(hwnd);
        let id = (wp.0 & 0xFFFF) as isize;
        let notify = ((wp.0 >> 16) & 0xFFFF) as u16;

        if (ID_FAN_BASE..ID_FAN_BASE + FAN_NAMES.len() as isize).contains(&id) && notify == 0 {
            ui.selected = (id - ID_FAN_BASE) as usize;
            for i in 0..FAN_NAMES.len() as isize {
                if let Some(b) = dlg(hwnd, ID_FAN_BASE + i) {
                    let _ = InvalidateRect(b, None, false);
                }
            }
            if let Some(t) = dlg(hwnd, ID_TAKE_CONTROL) {
                let _ = InvalidateRect(t, None, false);
            }
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }

        match id {
            ID_TAKE_CONTROL => {
                let sel = ui.selected;
                let was = ui.shared.fans.lock().unwrap()[sel].enabled;
                ui.shared.fans.lock().unwrap()[sel].enabled = !was;
                if was {
                    ui.shared.restore_mask.fetch_or(1 << sel, Ordering::Relaxed);
                }
                ui.shared.persist();
            }
            ID_PRESET_DEFAULT => set_preset(ui, vec![(30., 20.), (50., 35.), (65., 55.), (80., 80.), (95., 100.)]),
            ID_PRESET_QUIET => set_preset(ui, vec![(30., 15.), (60., 25.), (75., 45.), (90., 85.)]),
            ID_PRESET_FULL => set_preset(ui, vec![(20., 100.), (110., 100.)]),
            ID_RELEASE_ALL => ui.shared.release_all.store(true, Ordering::Relaxed),
            _ => {}
        }
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn set_preset(ui: &mut Ui, pts: Vec<(f32, f32)>) {
    ui.shared.fans.lock().unwrap()[ui.selected].points = pts;
    ui.shared.persist();
}

// ---------------- mouse -----------------------------------------------------

fn mouse(hwnd: HWND, msg: u32, lp: LPARAM) {
    unsafe {
        let ui = get_ui(hwnd);
        let pt = cursor_pos(lp);
        let r = ui.lay.plot;
        let slack = ui.s(10);
        if !(pt.x >= r.left - slack && pt.x <= r.right + slack && pt.y >= r.top - slack && pt.y <= r.bottom + slack) {
            return;
        }

        match msg {
            WM_LBUTTONDOWN => {
                if let Some(idx) = hit_point(ui, pt) {
                    ui.dragging = Some(idx);
                    let _ = SetCapture(hwnd);
                }
            }
            WM_LBUTTONDBLCLK => {
                if hit_point(ui, pt).is_none() {
                    add_point_at(ui, pt);
                    ui.shared.persist();
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            WM_RBUTTONDOWN => {
                if let Some(idx) = hit_point(ui, pt) {
                    let mut fans = ui.shared.fans.lock().unwrap();
                    if fans[ui.selected].points.len() > 2 {
                        fans[ui.selected].points.remove(idx);
                        drop(fans);
                        ui.hover_pt = None;
                        ui.shared.persist();
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            _ => {}
        }
    }
}

fn cursor_pos(lp: LPARAM) -> POINT {
    POINT {
        x: (lp.0 & 0xFFFF) as u16 as i16 as i32,
        y: ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    }
}

fn screen_to_temp_duty(r: &RECT, pt: POINT) -> (f32, f32) {
    let fx = (pt.x - r.left) as f32 / (r.right - r.left).max(1) as f32;
    let fy = (r.bottom - pt.y) as f32 / (r.bottom - r.top).max(1) as f32;
    (
        fx.clamp(0.0, 1.0) * (TEMP_MAX - TEMP_MIN) + TEMP_MIN,
        fy.clamp(0.0, 1.0) * 100.0,
    )
}

fn temp_duty_to_screen(r: &RECT, t: f32, d: f32) -> POINT {
    let x = r.left as f32 + ((t - TEMP_MIN) / (TEMP_MAX - TEMP_MIN)).clamp(0.0, 1.0) * (r.right - r.left) as f32;
    let y = r.bottom as f32 - (d / 100.0).clamp(0.0, 1.0) * (r.bottom - r.top) as f32;
    POINT { x: x as i32, y: y as i32 }
}

/// Points are stored unsorted but drawn sorted — hit-testing must use the
/// same sorted order the user sees, or a drag grabs the wrong point.
fn sorted_points(ui: &Ui) -> Vec<(f32, f32)> {
    let mut p = ui.shared.fans.lock().unwrap()[ui.selected].points.clone();
    p.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    p
}

fn hit_point(ui: &Ui, pt: POINT) -> Option<usize> {
    let r2 = (ui.s(11) * ui.s(11)) as f32;
    sorted_points(ui).iter().enumerate().find_map(|(i, (t, d))| {
        let sp = temp_duty_to_screen(&ui.lay.plot, *t, *d);
        let (dx, dy) = ((sp.x - pt.x) as f32, (sp.y - pt.y) as f32);
        (dx * dx + dy * dy < r2).then_some(i)
    })
}

fn set_point_at_cursor(hwnd: HWND, pt: POINT, idx: usize) {
    unsafe {
        let ui = get_ui(hwnd);
        let (t, d) = screen_to_temp_duty(&ui.lay.plot, pt);
        // index refers to the sorted view; map it back to the stored slot
        let sorted = sorted_points(ui);
        let Some(&target) = sorted.get(idx) else { return };
        let mut fans = ui.shared.fans.lock().unwrap();
        if let Some(slot) = fans[ui.selected].points.iter_mut().find(|p| **p == target) {
            *slot = (t, d);
        }
    }
}

fn add_point_at(ui: &mut Ui, pt: POINT) {
    let (t, d) = screen_to_temp_duty(&ui.lay.plot, pt);
    ui.shared.fans.lock().unwrap()[ui.selected].points.push((t, d));
}

// ---------------- GDI+ helpers ---------------------------------------------

struct Gp(*mut GpGraphics);

impl Gp {
    unsafe fn new(hdc: HDC, aa: bool) -> Gp {
        let mut g: *mut GpGraphics = std::ptr::null_mut();
        GdipCreateFromHDC(hdc, &mut g);
        GdipSetSmoothingMode(g, if aa { SmoothingModeAntiAlias } else { SmoothingModeNone });
        Gp(g)
    }
    unsafe fn aa(&self, on: bool) {
        GdipSetSmoothingMode(self.0, if on { SmoothingModeAntiAlias } else { SmoothingModeNone });
    }
    unsafe fn round_rect(&self, r: &RECT, rad: i32, fill: Option<u32>, border: Option<u32>) {
        let (x, y) = (r.left as f32 + 0.5, r.top as f32 + 0.5);
        let (w, h) = ((r.right - r.left) as f32 - 1.0, (r.bottom - r.top) as f32 - 1.0);
        let d = (rad * 2) as f32;
        let mut path: *mut GpPath = std::ptr::null_mut();
        GdipCreatePath(FillModeAlternate, &mut path);
        GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
        GdipAddPathArc(path, x + w - d, y, d, d, 270.0, 90.0);
        GdipAddPathArc(path, x + w - d, y + h - d, d, d, 0.0, 90.0);
        GdipAddPathArc(path, x, y + h - d, d, d, 90.0, 90.0);
        GdipClosePathFigure(path);
        if let Some(c) = fill {
            let mut b: *mut GpSolidFill = std::ptr::null_mut();
            GdipCreateSolidFill(c, &mut b);
            GdipFillPath(self.0, b as *mut GpBrush, path);
            GdipDeleteBrush(b as *mut GpBrush);
        }
        if let Some(c) = border {
            let mut p: *mut GpPen = std::ptr::null_mut();
            GdipCreatePen1(c, 1.0, UnitPixel, &mut p);
            GdipDrawPath(self.0, p, path);
            GdipDeletePen(p);
        }
        GdipDeletePath(path);
    }
    unsafe fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, color: u32, w: f32) {
        let mut p: *mut GpPen = std::ptr::null_mut();
        GdipCreatePen1(color, w, UnitPixel, &mut p);
        GdipDrawLine(self.0, p, x1, y1, x2, y2);
        GdipDeletePen(p);
    }
    unsafe fn polyline(&self, pts: &[PointF], color: u32, w: f32) {
        if pts.len() < 2 {
            return;
        }
        let mut p: *mut GpPen = std::ptr::null_mut();
        GdipCreatePen1(color, w, UnitPixel, &mut p);
        GdipSetPenLineJoin(p, LineJoinRound);
        GdipSetPenStartCap(p, LineCapRound);
        GdipSetPenEndCap(p, LineCapRound);
        GdipDrawLines(self.0, p, pts.as_ptr(), pts.len() as i32);
        GdipDeletePen(p);
    }
    unsafe fn polygon(&self, pts: &[PointF], color: u32) {
        if pts.len() < 3 {
            return;
        }
        let mut b: *mut GpSolidFill = std::ptr::null_mut();
        GdipCreateSolidFill(color, &mut b);
        GdipFillPolygon(self.0, b as *mut GpBrush, pts.as_ptr(), pts.len() as i32, FillModeAlternate);
        GdipDeleteBrush(b as *mut GpBrush);
    }
    unsafe fn disc(&self, cx: f32, cy: f32, r: f32, fill: u32, ring: Option<(u32, f32)>) {
        let mut b: *mut GpSolidFill = std::ptr::null_mut();
        GdipCreateSolidFill(fill, &mut b);
        GdipFillEllipse(self.0, b as *mut GpBrush, cx - r, cy - r, r * 2.0, r * 2.0);
        GdipDeleteBrush(b as *mut GpBrush);
        if let Some((c, w)) = ring {
            let mut p: *mut GpPen = std::ptr::null_mut();
            GdipCreatePen1(c, w, UnitPixel, &mut p);
            GdipDrawEllipse(self.0, p, cx - r, cy - r, r * 2.0, r * 2.0);
            GdipDeletePen(p);
        }
    }
}

impl Drop for Gp {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.0) };
    }
}

// ---------------- text helpers ----------------------------------------------

unsafe fn txt(hdc: HDC, s: &str, r: RECT, flags: DRAW_TEXT_FORMAT, font: HFONT, color: u32) {
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(color));
    let mut w: Vec<u16> = s.encode_utf16().collect();
    let mut rr = r;
    DrawTextW(hdc, &mut w, &mut rr, flags | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, old);
}

fn temp_color(t: f32) -> u32 {
    if t >= 85.0 {
        col::HOT
    } else if t >= 65.0 {
        col::WARN
    } else {
        col::OK
    }
}

// ---------------- painting --------------------------------------------------

fn paint(hwnd: HWND) {
    unsafe {
        let ui = get_ui(hwnd);
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
        let old_bmp = SelectObject(mem, HGDIOBJ(bmp.0));

        draw_shapes(ui, mem, &rc);
        draw_text(ui, mem, &rc);

        let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Pass 1 — every filled/stroked shape, via GDI+ so nothing is stair-stepped.
fn draw_shapes(ui: &Ui, hdc: HDC, rc: &RECT) {
    unsafe {
        let bg = CreateSolidBrush(rgb(col::BG));
        FillRect(hdc, rc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        let l = ui.lay;
        let g = Gp::new(hdc, true);
        let rad = ui.s(8);

        g.round_rect(&l.card, rad, Some(argb(col::CARD, 255)), Some(argb(col::BORDER, 255)));
        g.round_rect(&l.side, rad, Some(argb(col::CARD, 255)), Some(argb(col::BORDER, 255)));

        // ---- plot ----
        let p = l.plot;
        g.round_rect(&p, ui.s(4), Some(argb(0x11141B, 255)), None);

        // grid (axis-aligned — AA would only make it fuzzy)
        g.aa(false);
        let step = grid_step(&p, ui);
        let mut t = TEMP_MIN as i32;
        while t <= TEMP_MAX as i32 {
            let x = temp_duty_to_screen(&p, t as f32, 0.0).x as f32 + 0.5;
            g.line(x, p.top as f32, x, p.bottom as f32, argb(col::GRID, 255), 1.0);
            t += step;
        }
        for d in (0..=100i32).step_by(25) {
            let y = temp_duty_to_screen(&p, TEMP_MIN, d as f32).y as f32 + 0.5;
            g.line(p.left as f32, y, p.right as f32, y, argb(col::GRID, 255), 1.0);
        }
        g.aa(true);

        // curve: filled area + stroke
        let pts = sorted_points(ui);
        if pts.len() >= 2 {
            let mut poly: Vec<PointF> = pts
                .iter()
                .map(|(t, d)| {
                    let s = temp_duty_to_screen(&p, *t, *d);
                    PointF { X: s.x as f32, Y: s.y as f32 }
                })
                .collect();
            let mut area = poly.clone();
            area.push(PointF { X: poly[poly.len() - 1].X, Y: p.bottom as f32 });
            area.push(PointF { X: poly[0].X, Y: p.bottom as f32 });
            g.polygon(&area, argb(col::ACCENT, 38));
            g.polyline(&poly, argb(col::ACCENT, 255), ui.s(3).max(2) as f32 * 0.9);
            poly.clear();
        }

        // live temperature marker
        let ctemp = {
            let d = ui.shared.data.lock().unwrap();
            Shared::control_temp(&d)
        };
        if let Some(t) = ctemp {
            let x = temp_duty_to_screen(&p, t, 0.0).x as f32 + 0.5;
            g.aa(false);
            g.line(x, p.top as f32, x, p.bottom as f32, argb(col::WARN, 200), 1.0);
            g.aa(true);
            let dy = temp_duty_to_screen(&p, t, interp_at(&pts, t)).y as f32;
            g.disc(x, dy, ui.s(4) as f32, argb(col::WARN, 255), None);
        }

        // handles
        let r = ui.s(6) as f32;
        for (i, (t, d)) in pts.iter().enumerate() {
            let s = temp_duty_to_screen(&p, *t, *d);
            let hot = ui.hover_pt == Some(i) || ui.dragging == Some(i);
            g.disc(
                s.x as f32,
                s.y as f32,
                if hot { r + 1.5 } else { r },
                argb(if hot { 0xFFFFFF } else { col::CARD }, 255),
                Some((argb(col::ACCENT, 255), 2.5)),
            );
        }
    }
}

fn grid_step(p: &RECT, ui: &Ui) -> i32 {
    let per_deg = (p.right - p.left) as f32 / (TEMP_MAX - TEMP_MIN);
    if per_deg * 10.0 >= ui.s(46) as f32 {
        10
    } else {
        30
    }
}

fn interp_at(pts: &[(f32, f32)], t: f32) -> f32 {
    if pts.is_empty() {
        return 0.0;
    }
    if t <= pts[0].0 {
        return pts[0].1;
    }
    if t >= pts[pts.len() - 1].0 {
        return pts[pts.len() - 1].1;
    }
    for w in pts.windows(2) {
        if t >= w[0].0 && t <= w[1].0 {
            let k = (t - w[0].0) / (w[1].0 - w[0].0).max(0.001);
            return w[0].1 + k * (w[1].1 - w[0].1);
        }
    }
    pts[0].1
}

/// Pass 2 — all text, on top of the shapes.
fn draw_text(ui: &Ui, hdc: HDC, _rc: &RECT) {
    unsafe {
        let l = ui.lay;
        let s = |v: i32| ui.s(v);

        // ---- header: name + live status ----
        let h = l.header;
        txt(hdc, "asus-control", RECT { left: h.left, top: h.top, right: h.left + s(200), bottom: h.bottom }, DT_VCENTER, ui.font_bold, col::TEXT);
        let st = ui.shared.status.lock().unwrap().clone();
        let ok = ui.shared.hw_ok.load(Ordering::Relaxed);
        txt(
            hdc,
            st.trim_end_matches([' ', '·']),
            RECT { left: h.left + s(210), top: h.top, right: h.right, bottom: h.bottom },
            DT_VCENTER | DT_RIGHT | DT_END_ELLIPSIS,
            ui.font,
            if ok { col::OK } else { col::HOT },
        );

        // ---- card header: fan + live readouts ----
        let c = l.card;
        let hdr = RECT { left: c.left + s(16), top: c.top + s(10), right: c.right - s(16), bottom: c.top + s(38) };
        txt(hdc, FAN_NAMES[ui.selected], RECT { right: hdr.left + s(140), ..hdr }, DT_VCENTER, ui.font_bold, col::TEXT);

        let data = ui.shared.data.lock().unwrap();
        let rpm = data.rpm.get(ui.selected).copied().flatten();
        let duty = data.duty.get(ui.selected).copied().flatten();
        let ctemp = Shared::control_temp(&data);
        drop(data);

        // three right-aligned stat cells, fixed widths so they never collide
        let cellw = s(126);
        let mut x = hdr.right;
        for (label, val, color) in [
            ("CTRL TEMP", ctemp.map(|t| format!("{t:.1} °C")), ctemp.map(temp_color).unwrap_or(col::TEXT_3)),
            ("DUTY", duty.map(|d| format!("{d:.0} %")), col::ACCENT),
            ("RPM", rpm.map(|r| format!("{r:.0}")), col::TEXT),
        ] {
            let cell = RECT { left: x - cellw, top: hdr.top, right: x, bottom: hdr.bottom };
            txt(hdc, label, RECT { bottom: cell.top + s(12), ..cell }, DT_RIGHT, ui.font_small, col::TEXT_3);
            txt(
                hdc,
                &val.unwrap_or_else(|| "—".into()),
                RECT { top: cell.top + s(12), ..cell },
                DT_RIGHT,
                ui.font_mono,
                color,
            );
            x -= cellw;
        }

        // ---- axis labels ----
        let p = l.plot;
        let step = grid_step(&p, ui);
        let mut t = TEMP_MIN as i32;
        while t <= TEMP_MAX as i32 {
            let px = temp_duty_to_screen(&p, t as f32, 0.0).x;
            txt(
                hdc,
                &format!("{t}"),
                RECT { left: px - s(20), top: p.bottom + s(6), right: px + s(20), bottom: p.bottom + s(22) },
                DT_CENTER,
                ui.font_small,
                col::TEXT_3,
            );
            t += step;
        }
        for d in (0..=100i32).step_by(25) {
            let py = temp_duty_to_screen(&p, TEMP_MIN, d as f32).y;
            txt(
                hdc,
                &format!("{d}%"),
                RECT { left: c.left + s(8), top: py - s(9), right: p.left - s(8), bottom: py + s(9) },
                DT_RIGHT | DT_VCENTER,
                ui.font_small,
                col::TEXT_3,
            );
        }

        // dragged/hovered handle readout — only one, so labels never pile up
        if let Some(i) = ui.dragging.or(ui.hover_pt) {
            let pts = sorted_points(ui);
            if let Some((t, d)) = pts.get(i) {
                let sp = temp_duty_to_screen(&p, *t, *d);
                txt(
                    hdc,
                    &format!("{t:.0} °C → {d:.0} %"),
                    RECT { left: sp.x + s(12), top: sp.y - s(24), right: sp.x + s(140), bottom: sp.y - s(6) },
                    DT_LEFT,
                    ui.font_small,
                    col::TEXT,
                );
            }
        }

        txt(
            hdc,
            "drag to move  ·  double-click to add  ·  right-click to remove",
            RECT { left: c.left + s(16), top: c.bottom - s(26), right: c.right - s(16), bottom: c.bottom - s(8) },
            DT_LEFT | DT_VCENTER,
            ui.font_small,
            col::TEXT_3,
        );

        // ---- side panel ----
        let sr = l.side;
        let data = ui.shared.data.lock().unwrap();
        let mut y = sr.top + s(14);
        let line = s(21);
        let x0 = sr.left + s(14);
        let x1 = sr.right - s(14);

        let section = |hdc: HDC, y: &mut i32, title: &str| {
            txt(hdc, title, RECT { left: x0, top: *y, right: x1, bottom: *y + s(16) }, DT_LEFT, ui.font_small, col::TEXT_2);
            *y += s(22);
        };

        section(hdc, &mut y, "MOTHERBOARD");
        if data.temps.is_empty() {
            txt(hdc, "no SuperIO data", RECT { left: x0, top: y, right: x1, bottom: y + line }, DT_LEFT, ui.font, col::TEXT_3);
            y += line;
        }
        for (lbl, t) in &data.temps {
            if y + line > sr.bottom - s(8) {
                break;
            }
            txt(hdc, lbl, RECT { left: x0, top: y, right: x1 - s(74), bottom: y + line }, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS, ui.font, col::TEXT_2);
            txt(hdc, &format!("{t:.1} °C"), RECT { left: x1 - s(74), top: y, right: x1, bottom: y + line }, DT_RIGHT | DT_VCENTER, ui.font_mono, temp_color(*t));
            y += line;
        }

        if !data.bridge.is_empty() {
            y += s(12);
            section(hdc, &mut y, "SYSTEM");
        }
        for (lbl, v, unit) in &data.bridge {
            if y + line > sr.bottom - s(8) {
                break;
            }
            let color = if unit == "°C" { temp_color(*v) } else { col::TEXT };
            let val = if unit == "%" { format!("{v:.0} {unit}") } else { format!("{v:.1} {unit}") };
            txt(hdc, lbl, RECT { left: x0, top: y, right: x1 - s(74), bottom: y + line }, DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS, ui.font, col::TEXT_2);
            txt(hdc, &val, RECT { left: x1 - s(74), top: y, right: x1, bottom: y + line }, DT_RIGHT | DT_VCENTER, ui.font_mono, color);
            y += line;
        }
        drop(data);

        // ---- footer: poll rate label sits left of the slider ----
        let f = l.footer;
        let tick = ui.shared.tick_ms.load(Ordering::Relaxed);
        txt(
            hdc,
            &format!("Poll  {tick} ms"),
            RECT { left: f.right - s(340), top: f.top, right: f.right - s(220), bottom: f.bottom },
            DT_RIGHT | DT_VCENTER,
            ui.font,
            col::TEXT_2,
        );
    }
}

// ---------------- owner-drawn controls --------------------------------------

unsafe fn draw_owner_button(dis: &DRAWITEMSTRUCT, ui: &Ui) {
    // render off-screen, blit once — drawing straight into hDC flashes
    let rc = dis.rcItem;
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let mem = CreateCompatibleDC(dis.hDC);
    let bmp = CreateCompatibleBitmap(dis.hDC, w, h);
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    SetViewportOrgEx(mem, -rc.left, -rc.top, None);

    let mut d = *dis;
    d.hDC = mem;
    draw_owner_button_inner(&d, ui);

    SetViewportOrgEx(mem, 0, 0, None);
    let _ = BitBlt(dis.hDC, rc.left, rc.top, w, h, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
}

unsafe fn draw_owner_button_inner(dis: &DRAWITEMSTRUCT, ui: &Ui) {
    const ODS_SELECTED: u32 = 0x0001;
    const ODS_HOTLIGHT: u32 = 0x0040;

    let id = dis.CtlID as isize;
    let rc = dis.rcItem;
    let hdc = dis.hDC;
    let pressed = dis.itemState.0 & ODS_SELECTED != 0;
    let hot = dis.itemState.0 & ODS_HOTLIGHT != 0;

    let bg = CreateSolidBrush(rgb(col::BG));
    FillRect(hdc, &rc, bg);
    let _ = DeleteObject(HGDIOBJ(bg.0));

    let g = Gp::new(hdc, true);

    // ---- toggle switch ----
    if id == ID_TAKE_CONTROL {
        let on = ui.shared.fans.lock().unwrap()[ui.selected].enabled;
        let ph = ui.s(20);
        let pw = ui.s(40);
        let py = rc.top + (rc.bottom - rc.top - ph) / 2;
        let px = rc.left;
        g.round_rect(
            &RECT { left: px, top: py, right: px + pw, bottom: py + ph },
            ph / 2,
            Some(argb(if on { col::ACCENT } else { 0x2A2F3A }, 255)),
            Some(argb(if on { col::ACCENT } else { 0x3A404C }, 255)),
        );
        let kr = (ph as f32 - ui.s(6) as f32) / 2.0;
        let kx = if on { px + pw - ph / 2 } else { px + ph / 2 };
        g.disc(kx as f32, py as f32 + ph as f32 / 2.0, kr, argb(0xFFFFFF, 255), None);
        drop(g);
        txt(
            hdc,
            "Take control of this fan",
            RECT { left: px + pw + ui.s(10), top: rc.top, right: rc.right, bottom: rc.bottom },
            DT_LEFT | DT_VCENTER,
            ui.font,
            if on { col::TEXT } else { col::TEXT_2 },
        );
        return;
    }

    let is_tab = (ID_FAN_BASE..ID_FAN_BASE + FAN_NAMES.len() as isize).contains(&id);
    let selected_tab = is_tab && id == ID_FAN_BASE + ui.selected as isize;
    let controlled = is_tab && ui.shared.fans.lock().unwrap()[(id - ID_FAN_BASE) as usize].enabled;

    let (fill, border, fg) = if id == ID_RELEASE_ALL {
        if pressed {
            (col::DANGER, col::DANGER, 0xFFFFFF)
        } else if hot {
            (0x3A2224, col::DANGER, 0xFF9E97)
        } else {
            (col::DANGER_BG, 0x5A3436, 0xEE8A83)
        }
    } else if selected_tab {
        (col::ACCENT_BG, col::ACCENT, 0xCFE6FF)
    } else if pressed {
        (0x11141B, col::BORDER, col::TEXT_2)
    } else if hot {
        (0x232936, 0x39414F, col::TEXT)
    } else {
        (col::CARD_HI, col::BORDER, col::TEXT_2)
    };

    g.round_rect(&rc, ui.s(7), Some(argb(fill, 255)), Some(argb(border, 255)));

    // small dot on tabs whose fan is under software control
    if controlled {
        g.disc(rc.right as f32 - ui.s(10) as f32, rc.top as f32 + ui.s(10) as f32, ui.s(3) as f32, argb(col::OK, 255), None);
    }
    drop(g);

    let mut buf = [0u16; 64];
    let len = GetWindowTextW(dis.hwndItem, &mut buf);
    let s: String = String::from_utf16_lossy(&buf[..len as usize]);
    txt(hdc, &s, rc, DT_CENTER | DT_VCENTER, if selected_tab { ui.font_bold } else { ui.font }, fg);
}

/// Cheap hash of everything the UI displays.
fn data_signature(ui: &Ui) -> u64 {
    let mut h: u64 = 14695981039346656037;
    fn mix(h: &mut u64, b: u8) {
        *h ^= b as u64;
        *h = (*h).wrapping_mul(1099511628211);
    }
    let d = ui.shared.data.lock().unwrap();
    for (l, t) in d.temps.iter() {
        for b in l.bytes() {
            mix(&mut h, b);
        }
        for b in t.to_bits().to_le_bytes() {
            mix(&mut h, b);
        }
    }
    for (l, v, u) in d.bridge.iter() {
        for b in l.bytes() {
            mix(&mut h, b);
        }
        for b in v.to_bits().to_le_bytes() {
            mix(&mut h, b);
        }
        for b in u.bytes() {
            mix(&mut h, b);
        }
    }
    for r in d.rpm.iter().flatten() {
        for b in r.to_bits().to_le_bytes() {
            mix(&mut h, b);
        }
    }
    for v in d.duty.iter().flatten() {
        for b in v.to_bits().to_le_bytes() {
            mix(&mut h, b);
        }
    }
    drop(d);
    for b in ui.shared.status.lock().unwrap().bytes() {
        mix(&mut h, b);
    }
    let fans = ui.shared.fans.lock().unwrap();
    for (i, f) in fans.iter().enumerate() {
        mix(&mut h, f.enabled as u8);
        mix(&mut h, i as u8);
        if i == ui.selected {
            mix(&mut h, 0xFF);
        }
        for p in &f.points {
            for b in p.0.to_bits().to_le_bytes() {
                mix(&mut h, b);
            }
            for b in p.1.to_bits().to_le_bytes() {
                mix(&mut h, b);
            }
        }
    }
    h
}

// the only trackbar message windows-rs 0.58 omits (WM_USER + 0)
const TBM_GETPOS: u32 = 1024;
