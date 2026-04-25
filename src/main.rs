//! Overlay Stats — CPU & RAM overlay for Windows

//! Drag to reposition.
//! F8 toggles transparent background panel.
//! F9 toggles always-on-top.
//! F10 toggles click-through.
//! Tray menu: right-click tray icon for Settings / Quit.
//! Position is saved in config.json next to the executable.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs,
    os::windows::ffi::OsStrExt,
};

use serde::{Deserialize, Serialize};
use sysinfo::{CpuExt, System, SystemExt};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                HOT_KEY_MODIFIERS, RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey,
                VK_F10, VK_F8, VK_F9,
            },
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
};

// ─── Config ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    x: i32,
    y: i32,
    click_through: bool,
    always_on_top: bool,
    show_background: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            x: 100,
            y: 20,
            click_through: true,
            always_on_top: false,
            show_background: false,
        }
    }
}

fn load_config() -> Config {
    fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    if let Ok(j) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write("config.json", j);
    }
}

// ─── App state ───────────────────────────────────────────────────────────────
//
// Lives on the heap; raw pointer stored in GWLP_USERDATA.
// Everything runs on the main thread (WM_TIMER), so no Arc/Mutex needed.

struct AppState {
    cpu_text: String,
    ram_text: String,
    dragging: bool,
    drag_x: i32, // cursor-x inside window when drag began
    drag_y: i32,
    tray_menu: HMENU,
    config: Config,
    sys: System,
    font: HFONT,
}

// ─── Constants ───────────────────────────────────────────────────────────────

const HOTKEY_BG_ID: i32 = 1;
const HOTKEY_TOPMOST_ID: i32 = 2;
const HOTKEY_CLICKTHROUGH_ID: i32 = 3;
const TIMER_ID: usize = 1;
const REFRESH_MS: u32 = 1_000;
const WIN_W: i32 = 272;
const WIN_H: i32 = 34;
const WM_TRAYICON: u32 = WM_APP + 1;
const TRAY_ICON_ID: u32 = 1;
const TRAY_MENU_SETTINGS_ID: usize = 1001;
const TRAY_MENU_QUIT_ID: usize = 1002;

// COLORREF layout: 0x00_BB_GG_RR  (R = lowest byte, B = highest)
const KEY_COLOR: COLORREF = COLORREF(0x0000_0000); // pure black → transparent
const BG_COLOR: COLORREF  = COLORREF(0x001E_1E1E); // RGB(30,30,30) — panel when enabled
const CPU_COLOR: COLORREF = COLORREF(0x0000_FF00); // RGB(0,255,0)  — green
const RAM_COLOR: COLORREF = COLORREF(0x0000_A5FF); // RGB(255,165,0) — amber

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Extract a *signed* x coordinate from a packed LPARAM.
/// The cast chain handles negative values on multi-monitor setups correctly.
#[inline]
fn lp_x(l: LPARAM) -> i32 { (l.0 as u32 as u16) as i16 as i32 }

/// Extract a *signed* y coordinate from a packed LPARAM.
#[inline]
fn lp_y(l: LPARAM) -> i32 { ((l.0 as u32 >> 16) as u16) as i16 as i32 }

#[inline]
fn loword(value: usize) -> usize { value & 0xFFFF }

fn copy_wide_into<const N: usize>(dst: &mut [u16; N], text: &str) {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    if utf16.len() >= N {
        utf16.truncate(N - 1);
    }
    let len = utf16.len();
    dst[..len].copy_from_slice(&utf16);
    dst[len] = 0;
}

fn icon_path_candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("icon.ico"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("icon.ico"));
    }

    out
}

unsafe fn load_tray_hicon() -> HICON {
    for path in icon_path_candidates() {
        if !path.exists() {
            continue;
        }

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        if let Ok(handle) = LoadImageW(
            None,
            PCWSTR(wide.as_ptr()),
            IMAGE_ICON,
            16,
            16,
            LR_LOADFROMFILE | LR_DEFAULTSIZE | LR_SHARED,
        ) {
            if handle.0 != 0 {
                return HICON(handle.0);
            }
        }
    }

    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

unsafe fn create_tray_menu() -> HMENU {
    if let Ok(menu) = CreatePopupMenu() {
        let _ = AppendMenuW(menu, MF_STRING, TRAY_MENU_SETTINGS_ID, w!("Settings"));
        let _ = AppendMenuW(menu, MF_STRING, TRAY_MENU_QUIT_ID, w!("Quit"));
        return menu;
    }

    HMENU(0)
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ICON_ID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = load_tray_hicon();
    copy_wide_into(&mut nid.szTip, "Overlay Stats");

    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ICON_ID;
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

#[inline]
fn topmost_hwnd(always_on_top: bool) -> HWND {
    if always_on_top { HWND_TOPMOST } else { HWND_NOTOPMOST }
}

#[inline]
fn ex_style_from_config(cfg: &Config) -> WINDOW_EX_STYLE {
    // WS_EX_TOOLWINDOW keeps the window out of Alt-Tab/taskbar, i.e. desktop-widget behavior.
    let mut ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW;
    if cfg.click_through {
        ex |= WS_EX_TRANSPARENT;
    }
    if cfg.always_on_top {
        ex |= WS_EX_TOPMOST;
    }
    ex
}

unsafe fn apply_widget_mode(hwnd: HWND, cfg: &Config) {
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style_from_config(cfg).0 as isize);
    let _ = SetWindowPos(
        hwnd,
        topmost_hwnd(cfg.always_on_top),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
    );
}

// ─── Window procedure ────────────────────────────────────────────────────────

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    // The pointer is null only for very early messages (WM_NCCREATE etc.)
    // that we don't handle; they fall through to DefWindowProcW.
    let st = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState).as_mut();

    match msg {
        // ── Periodic update (main thread — zero extra threads) ─────────────
        WM_TIMER => {
            if let Some(s) = st {
                s.sys.refresh_cpu();
                s.sys.refresh_memory();

                let cpu = s.sys.global_cpu_info().cpu_usage();
                // sysinfo >= 0.27: used_memory() returns bytes
                let ram_gb = s.sys.used_memory() as f32 / (1024.0 * 1024.0 * 1024.0);

                s.cpu_text = format!("CPU {:>5.1}%", cpu);
                s.ram_text = format!("RAM {:>4.2}G", ram_gb);
            }
            // bErase=false: we fill the background ourselves in WM_PAINT
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }

        // ── Suppress default background erase to prevent flicker ──────────
        WM_ERASEBKGND => LRESULT(1),

        // ── Render ────────────────────────────────────────────────────────
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            // Fill with either panel colour or transparent key colour.
            let show_background = st.as_ref().map(|s| s.config.show_background).unwrap_or(false);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let bg = CreateSolidBrush(if show_background { BG_COLOR } else { KEY_COLOR });
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg);

            if let Some(s) = st {
                let prev = SelectObject(hdc, s.font);
                let _ = SetBkMode(hdc, TRANSPARENT);

                // CPU text — green
                let cpu_w: Vec<u16> = s.cpu_text.encode_utf16().collect();
                SetTextColor(hdc, CPU_COLOR);
                let _ = TextOutW(hdc, 10, 9, &cpu_w);

                // Measure CPU text width so RAM is positioned right after it
                let mut sz = SIZE::default();
                let _ = GetTextExtentPoint32W(hdc, &cpu_w, &mut sz);

                // RAM text — amber
                let ram_w: Vec<u16> = s.ram_text.encode_utf16().collect();
                SetTextColor(hdc, RAM_COLOR);
                let _ = TextOutW(hdc, 10 + sz.cx + 18, 9, &ram_w);

                let _ = SelectObject(hdc, prev);
            }

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        // ── Drag-to-reposition ────────────────────────────────────────────
        WM_LBUTTONDOWN => {
            if let Some(s) = st {
                s.dragging = true;
                s.drag_x   = lp_x(lp);
                s.drag_y   = lp_y(lp);
                SetCapture(hwnd);
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            if let Some(s) = st {
                s.dragging = false;
                let _ = ReleaseCapture();

                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                s.config.x = wr.left;
                s.config.y = wr.top;
                save_config(&s.config);
            }
            LRESULT(0)
        }

        WM_MOUSEMOVE => {
            if let Some(s) = st {
                if s.dragging {
                    // Use screen cursor position to avoid accumulated drift
                    let mut cur = POINT::default();
                    let _ = GetCursorPos(&mut cur);
                    let _ = SetWindowPos(
                        hwnd,
                        topmost_hwnd(s.config.always_on_top),
                        cur.x - s.drag_x,
                        cur.y - s.drag_y,
                        0, 0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            LRESULT(0)
        }

        // ── Global hotkeys (F8/F9/F10) ───────────────────────────────────
        WM_HOTKEY => {
            if let Some(s) = st {
                match wp.0 as i32 {
                    HOTKEY_BG_ID => {
                        s.config.show_background = !s.config.show_background;
                        let _ = InvalidateRect(hwnd, None, false);
                        save_config(&s.config);
                    }

                    HOTKEY_TOPMOST_ID => {
                        s.config.always_on_top = !s.config.always_on_top;
                        apply_widget_mode(hwnd, &s.config);
                        save_config(&s.config);
                    }

                    HOTKEY_CLICKTHROUGH_ID => {
                        s.config.click_through = !s.config.click_through;
                        apply_widget_mode(hwnd, &s.config);
                        save_config(&s.config);
                    }

                    _ => {}
                }
            }
            LRESULT(0)
        }

        WM_TRAYICON => {
            if let Some(s) = st {
                let ev = lp.0 as u32;
                if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU {
                    let mut cur = POINT::default();
                    let _ = GetCursorPos(&mut cur);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = TrackPopupMenu(s.tray_menu, TPM_RIGHTBUTTON, cur.x, cur.y, 0, hwnd, None);
                }
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            if let Some(s) = st {
                match loword(wp.0) {
                    TRAY_MENU_SETTINGS_ID => {
                        let _ = MessageBoxW(
                            hwnd,
                            w!("Edit config.json to change defaults.\nHotkeys: F8 background, F9 topmost, F10 click-through."),
                            w!("Overlay Stats - Settings"),
                            MB_OK | MB_ICONINFORMATION,
                        );

                        s.config.click_through = false;
                        apply_widget_mode(hwnd, &s.config);
                        let _ = InvalidateRect(hwnd, None, false);
                        save_config(&s.config);
                    }

                    TRAY_MENU_QUIT_ID => {
                        let _ = DestroyWindow(hwnd);
                    }

                    _ => {}
                }
            }
            LRESULT(0)
        }

        // ── Clean up ──────────────────────────────────────────────────────
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_ID);
            let _ = UnregisterHotKey(hwnd, HOTKEY_BG_ID);
            let _ = UnregisterHotKey(hwnd, HOTKEY_TOPMOST_ID);
            let _ = UnregisterHotKey(hwnd, HOTKEY_CLICKTHROUGH_ID);
            remove_tray_icon(hwnd);

            // Reclaim the Box so AppState (and System) are dropped properly
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState;
            if !raw.is_null() {
                let s = Box::from_raw(raw);
                let _ = DeleteObject(s.font);
                if s.tray_menu.0 != 0 {
                    let _ = DestroyMenu(s.tray_menu);
                }
                // `s` drops here, freeing `sys` and all strings
            }

            PostQuitMessage(0);
            LRESULT(0)
        }

        // Every unhandled message MUST return DefWindowProcW.
        // FIX: the original discarded its return value (semicolon), which is
        // undefined behaviour and caused WM_TIMER to return LRESULT(0) by
        // accident instead of the correct default handling.
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let config = load_config();

    unsafe {
        // Consolas at 15 px, anti-aliased (better edges over colour-key transparency)
        let font = CreateFontW(
            -15,
            0, 0, 0,
            400, // FW_NORMAL
            0, 0, 0,
            1,   // DEFAULT_CHARSET
            0,   // OUT_DEFAULT_PRECIS
            0,   // CLIP_DEFAULT_PRECIS
            4,   // ANTIALIASED_QUALITY
            49,  // FIXED_PITCH (1) | FF_MODERN (48)
            w!("Consolas"),
        );

        let tray_menu = create_tray_menu();

        // Prime sysinfo: the first sample is always 0%; a second call after
        // ~1 s gives an accurate delta.  The timer fires after REFRESH_MS.
        let mut sys = System::new_all();
        sys.refresh_cpu();

        let state: Box<AppState> = Box::new(AppState {
            cpu_text: "CPU   -.-% ".into(),
            ram_text: "RAM  -.--G".into(),
            dragging: false,
            drag_x: 0,
            drag_y: 0,
            tray_menu,
            config: config.clone(),
            sys,
            font,
        });
        let state_ptr = Box::into_raw(state);

        // ── Window class ──────────────────────────────────────────────────
        let hinstance = GetModuleHandleW(None)?;

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("overlay_cls"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        // ── Create window ─────────────────────────────────────────────────
        let ex_style = ex_style_from_config(&config);

        let hwnd = CreateWindowExW(
            ex_style,
            w!("overlay_cls"),
            w!("Overlay Stats"),
            WS_POPUP,
            config.x, config.y,
            WIN_W, WIN_H,
            None, None,
            hinstance,
            None,
        );
        if hwnd.0 == 0 {
            return Err(Error::from_win32());
        }

        // Pure-black pixels become fully transparent (colour-key layered window)
        SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY)?;

        // Rounded corners — OS takes ownership of the HRGN; don't DeleteObject it
        SetWindowRgn(hwnd, CreateRoundRectRgn(0, 0, WIN_W, WIN_H, 10, 10), false);

        // Attach state *before* starting the timer (timer fires on next message pump)
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        // Add tray icon with right-click menu.
        add_tray_icon(hwnd);

        // Register global hotkeys for widget control.
        let _ = RegisterHotKey(hwnd, HOTKEY_BG_ID, HOT_KEY_MODIFIERS(0), VK_F8.0 as u32);
        let _ = RegisterHotKey(hwnd, HOTKEY_TOPMOST_ID, HOT_KEY_MODIFIERS(0), VK_F9.0 as u32);
        let _ = RegisterHotKey(hwnd, HOTKEY_CLICKTHROUGH_ID, HOT_KEY_MODIFIERS(0), VK_F10.0 as u32);

        // 1-second periodic update, entirely on the main thread
        let _ = SetTimer(hwnd, TIMER_ID, REFRESH_MS, None);

        // Show without stealing focus
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);

        // ── Message loop ──────────────────────────────────────────────────
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }

    Ok(())
}