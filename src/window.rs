//! Native Win32 UI. Hardware reads happen only on the separate worker thread.
use crate::model::WARNING_AMPS;
use crate::worker::{Shared, Worker};
use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const TRAY_MESSAGE: u32 = WM_APP + 1;
const SHOW: usize = 100;
const LOGS: usize = 101;
const RESET: usize = 102;
const QUIT: usize = 103;
const CLASS: &str = "AstralPowerMonitorRustWindow";

struct App {
    shared: Shared,
    font: HFONT,
    small_font: HFONT,
    big_font: HFONT,
    menu: HMENU,
    taskbar_created: u32,
}
thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

unsafe fn font(height: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide("Segoe UI").as_ptr(),
        )
    }
}
unsafe fn text(dc: HDC, x: i32, y: i32, width: i32, content: &str, color: u32, font: HFONT) {
    unsafe {
        let old = SelectObject(dc, font);
        SetTextColor(dc, color);
        let value = wide(content);
        let mut rect = RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + 35,
        };
        DrawTextW(
            dc,
            value.as_ptr(),
            value.len() as i32 - 1,
            &mut rect,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        SelectObject(dc, old);
    }
}
unsafe fn fill(dc: HDC, x: i32, y: i32, width: i32, height: i32, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        let rect = RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        };
        FillRect(dc, &rect, brush);
        DeleteObject(brush);
    }
}
unsafe fn notify(hwnd: HWND, operation: u32, tip: &str, warning: bool) -> bool {
    unsafe {
        let mut info: NOTIFYICONDATAW = zeroed();
        info.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        info.hWnd = hwnd;
        info.uID = 1;
        info.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        info.uCallbackMessage = TRAY_MESSAGE;
        info.hIcon = LoadIconW(
            null_mut(),
            if warning {
                IDI_WARNING
            } else {
                IDI_INFORMATION
            },
        );
        let value = wide(tip);
        for (target, source) in info.szTip.iter_mut().take(127).zip(value) {
            *target = source;
        }
        Shell_NotifyIconW(operation, &info) != 0
    }
}
unsafe fn show(hwnd: HWND) {
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }
}

unsafe fn popup(hwnd: HWND, app: &App) {
    unsafe {
        let mut point: POINT = zeroed();
        GetCursorPos(&mut point);
        SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            app.menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_COMMAND, command as usize, 0);
        PostMessageW(hwnd, WM_NULL, 0, 0);
    }
}

unsafe fn paint(hwnd: HWND, app: &App) {
    unsafe {
        let mut ps: PAINTSTRUCT = zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut client: RECT = zeroed();
        GetClientRect(hwnd, &mut client);
        fill(dc, 0, 0, client.right, client.bottom, rgb(20, 25, 34));
        SetBkMode(dc, TRANSPARENT as i32);
        let white = rgb(232, 239, 246);
        let muted = rgb(154, 171, 187);
        let green = rgb(87, 213, 165);
        let amber = rgb(255, 186, 86);
        let state = app.shared.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let live = state.live_at(now);
        text(dc, 28, 22, 650, "Astral Power Monitor", white, app.big_font);
        text(dc, 28, 59, 650, &state.gpu_name, muted, app.font);
        let (status, status_color) = if let Some(reading) = live {
            if reading.max_amps() >= WARNING_AMPS {
                ("Current warning · a pin is at or above 9.2 A".into(), amber)
            } else {
                let age = state
                    .updated
                    .map(|time| now.saturating_duration_since(time).as_secs_f32())
                    .unwrap_or(0.0);
                (
                    format!("Live · updated {age:.1} s ago · {} samples", state.samples),
                    green,
                )
            }
        } else {
            (
                state
                    .error
                    .clone()
                    .unwrap_or_else(|| "Waiting for fresh sensor readings…".into()),
                amber,
            )
        };
        text(dc, 28, 91, 650, &status, status_color, app.small_font);
        let power = live
            .map(|r| format!("{:.1} W connector", r.watts()))
            .unwrap_or_else(|| "— W connector".into());
        let maximum = live
            .map(|r| format!("{:.2} A highest pin", r.max_amps()))
            .unwrap_or_else(|| "— A highest pin".into());
        text(dc, 28, 128, 310, &power, white, app.big_font);
        text(dc, 350, 128, 325, &maximum, white, app.big_font);
        for (x, width, label) in [
            (28, 80, "PIN"),
            (130, 180, "CURRENT"),
            (350, 105, "VOLTAGE"),
            (470, 110, "POWER"),
            (590, 100, "PEAK A"),
        ] {
            text(dc, x, 175, width, label, muted, app.small_font);
        }
        for index in 0..6 {
            let y = 208 + index as i32 * 42;
            text(
                dc,
                28,
                y,
                80,
                &format!("Pin {}", index + 1),
                white,
                app.font,
            );
            fill(dc, 130, y + 27, 180, 4, rgb(47, 59, 72));
            if let Some(reading) = live {
                let pin = reading.pins[index];
                let color = if pin.amps >= WARNING_AMPS {
                    amber
                } else {
                    green
                };
                text(
                    dc,
                    130,
                    y - 2,
                    180,
                    &format!("{:.2} A", pin.amps),
                    color,
                    app.font,
                );
                fill(
                    dc,
                    130,
                    y + 27,
                    (180.0 * (pin.amps / 12.0).clamp(0.0, 1.0)) as i32,
                    4,
                    color,
                );
                text(
                    dc,
                    350,
                    y,
                    110,
                    &format!("{:.3} V", pin.volts),
                    white,
                    app.font,
                );
                text(
                    dc,
                    470,
                    y,
                    110,
                    &format!("{:.2} W", pin.watts()),
                    white,
                    app.font,
                );
            } else {
                for x in [130, 350, 470] {
                    text(dc, x, y, 100, "—", muted, app.font);
                }
            }
            text(
                dc,
                590,
                y,
                100,
                &format!("{:.2}", state.peaks[index]),
                muted,
                app.font,
            );
        }
        text(
            dc,
            28,
            470,
            650,
            "Monitoring only · automatic protection is not enabled",
            muted,
            app.small_font,
        );
        let log_status = state
            .log_error
            .as_ref()
            .map(|e| format!("JSONL logging unavailable: {e}"))
            .unwrap_or_else(|| {
                "JSONL history is saved locally · open it from the tray menu".into()
            });
        text(
            dc,
            28,
            495,
            650,
            &log_status,
            if state.log_error.is_some() {
                amber
            } else {
                muted
            },
            app.small_font,
        );
        text(
            dc,
            28,
            528,
            650,
            "Close hides this window · right-click the tray icon to show it or quit",
            muted,
            app.small_font,
        );
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                APP.with(|cell| {
                    if let Ok(app) = cell.try_borrow()
                        && let Some(app) = app.as_ref()
                    {
                        paint(hwnd, app);
                    }
                });
                0
            }
            WM_ERASEBKGND => 1,
            WM_TIMER => {
                APP.with(|cell| {
                    if let Ok(app) = cell.try_borrow()
                        && let Some(app) = app.as_ref()
                    {
                        let state = app.shared.lock().unwrap_or_else(|p| p.into_inner());
                        let reading = state.live_at(Instant::now());
                        let tip = reading
                            .map(|r| {
                                format!(
                                    "Astral: {:.1} W · max {:.2} A · monitoring only",
                                    r.watts(),
                                    r.max_amps()
                                )
                            })
                            .unwrap_or_else(|| {
                                "Astral: sensor unavailable · monitoring only".into()
                            });
                        let warning = reading.is_none_or(|r| r.max_amps() >= WARNING_AMPS);
                        if !notify(hwnd, NIM_MODIFY, &tip, warning) {
                            notify(hwnd, NIM_ADD, &tip, warning);
                        }
                    }
                });
                InvalidateRect(hwnd, null(), 0);
                0
            }
            WM_CLOSE => {
                ShowWindow(hwnd, SW_HIDE);
                0
            }
            WM_CONTEXTMENU => {
                APP.with(|cell| {
                    if let Ok(app) = cell.try_borrow()
                        && let Some(app) = app.as_ref()
                    {
                        popup(hwnd, app);
                    }
                });
                0
            }
            WM_DESTROY => {
                notify(hwnd, NIM_DELETE, "", false);
                PostQuitMessage(0);
                0
            }
            TRAY_MESSAGE => {
                match (lparam as u32) & 0xffff {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => show(hwnd),
                    WM_RBUTTONUP => {
                        APP.with(|cell| {
                            if let Ok(app) = cell.try_borrow()
                                && let Some(app) = app.as_ref()
                            {
                                let mut point: POINT = zeroed();
                                GetCursorPos(&mut point);
                                SetForegroundWindow(hwnd);
                                let command = TrackPopupMenu(
                                    app.menu,
                                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                    point.x,
                                    point.y,
                                    0,
                                    hwnd,
                                    null(),
                                );
                                PostMessageW(hwnd, WM_COMMAND, command as usize, 0);
                                PostMessageW(hwnd, WM_NULL, 0, 0);
                            }
                        });
                    }
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                match wparam & 0xffff {
                    SHOW => show(hwnd),
                    QUIT => {
                        DestroyWindow(hwnd);
                    }
                    LOGS => {
                        APP.with(|cell| {
                            if let Ok(app) = cell.try_borrow()
                                && let Some(app) = app.as_ref()
                            {
                                let state = app.shared.lock().unwrap_or_else(|p| p.into_inner());
                                if let Some(folder) = state.log_path.parent() {
                                    ShellExecuteW(
                                        hwnd,
                                        wide("open").as_ptr(),
                                        wide(&folder.to_string_lossy()).as_ptr(),
                                        null(),
                                        null(),
                                        SW_SHOWNORMAL,
                                    );
                                }
                            }
                        });
                    }
                    RESET => {
                        APP.with(|cell| {
                            if let Ok(app) = cell.try_borrow()
                                && let Some(app) = app.as_ref()
                            {
                                app.shared.lock().unwrap_or_else(|p| p.into_inner()).peaks =
                                    [0.0; 6];
                            }
                        });
                    }
                    _ => {}
                }
                0
            }
            _ => {
                let recreated = APP.with(|cell| {
                    cell.try_borrow()
                        .ok()
                        .and_then(|app| app.as_ref().map(|a| a.taskbar_created == msg))
                        .unwrap_or(false)
                });
                if recreated {
                    notify(hwnd, NIM_ADD, "Astral: reconnecting tray icon", true);
                    0
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }
    }
}

pub fn run(start_hidden: bool) -> Result<(), String> {
    unsafe {
        let mutex = CreateMutexW(null(), 0, wide("Local\\AstralPowerMonitorRust").as_ptr());
        if mutex.is_null() {
            return Err(format!(
                "Cannot create application instance: {}",
                GetLastError()
            ));
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let existing = FindWindowW(wide(CLASS).as_ptr(), null());
            if !existing.is_null() {
                show(existing);
            }
            CloseHandle(mutex);
            return Ok(());
        }
        SetProcessDPIAware();
        let instance = GetModuleHandleW(null());
        let class_name = wide(CLASS);
        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = Some(window_proc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hIcon = LoadIconW(null_mut(), IDI_INFORMATION);
        if RegisterClassW(&class) == 0 {
            CloseHandle(mutex);
            return Err("Window registration failed".into());
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("Astral Power Monitor").as_ptr(),
            WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            730,
            640,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            CloseHandle(mutex);
            return Err("Window creation failed".into());
        }
        let menu = CreatePopupMenu();
        for (id, label) in [
            (SHOW, "Show readings"),
            (LOGS, "Open JSONL history"),
            (RESET, "Reset session peaks"),
            (QUIT, "Quit"),
        ] {
            AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
        }
        let worker = Worker::start();
        APP.with(|cell| {
            *cell.borrow_mut() = Some(App {
                shared: worker.shared.clone(),
                font: font(19),
                small_font: font(15),
                big_font: font(25),
                menu,
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            })
        });
        if !notify(hwnd, NIM_ADD, "Astral: connecting to sensor", true) {
            DestroyWindow(hwnd);
            CloseHandle(mutex);
            return Err("Could not create the tray icon".into());
        }
        SetTimer(hwnd, 1, 500, None);
        if !start_hidden {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
        }
        let mut message: MSG = zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        APP.with(|cell| {
            if let Some(app) = cell.borrow_mut().take() {
                DeleteObject(app.font);
                DeleteObject(app.small_font);
                DeleteObject(app.big_font);
                DestroyMenu(app.menu);
            }
        });
        drop(worker);
        CloseHandle(mutex);
        Ok(())
    }
}
