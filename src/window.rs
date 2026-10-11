//! Native Win32 UI. Hardware reads happen only on the separate worker thread.
use crate::model::WARNING_AMPS;
use crate::worker::{Controls, Shared, Worker};
use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
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
const ARM: usize = 104;
const DISARM: usize = 105;
const RESTORE: usize = 106;
const TEST_SOUNDS: usize = 107;
const MUTE_WARNING: usize = 108;
const FLUSH_HISTORY: usize = 109;
const MONITOR_ONLY: u32 = WM_APP + 2;
const CLASS: &str = "AstralPowerMonitorRustWindow";

struct App {
    icons: Icons,
    shared: Shared,
    controls: Controls,
    font: HFONT,
    small_font: HFONT,
    big_font: HFONT,
    menu: HMENU,
    taskbar_created: u32,
}
struct Icons {
    normal: HICON,
    warning: HICON,
    big: HICON,
}
impl Icons {
    unsafe fn load(instance: HINSTANCE) -> Result<Self, String> {
        let mut icons = Self {
            normal: null_mut(),
            warning: null_mut(),
            big: null_mut(),
        };
        let load = |resource: u16, width: i32, height: i32| {
            let handle = unsafe {
                LoadImageW(
                    instance,
                    resource as usize as *const u16,
                    IMAGE_ICON,
                    width,
                    height,
                    0,
                )
            };
            if handle.is_null() {
                Err("Cannot load embedded GPU icons".to_string())
            } else {
                Ok(handle)
            }
        };
        unsafe {
            icons.normal = load(
                1,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
            )?;
            icons.warning = load(
                2,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
            )?;
            icons.big = load(1, GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))?;
        }
        Ok(icons)
    }
}
impl Drop for Icons {
    fn drop(&mut self) {
        unsafe {
            for icon in [self.normal, self.warning, self.big] {
                if !icon.is_null() {
                    DestroyIcon(icon);
                }
            }
        }
    }
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
        info.hIcon = APP.with(|cell| {
            cell.try_borrow()
                .ok()
                .and_then(|app| {
                    app.as_ref().map(|app| {
                        if warning {
                            app.icons.warning
                        } else {
                            app.icons.normal
                        }
                    })
                })
                .unwrap_or(null_mut())
        });
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
        update_menu(app);
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
unsafe fn update_menu(app: &App) {
    let state = app.shared.lock().unwrap_or_else(|p| p.into_inner());
    let disabled = state.monitor_only
        || state.live_at(Instant::now()).is_none()
        || app.controls.gate.pending()
        || app
            .controls
            .gate
            .armed
            .load(std::sync::atomic::Ordering::SeqCst);
    unsafe {
        EnableMenuItem(
            app.menu,
            ARM as u32,
            MF_BYCOMMAND | if disabled { MF_GRAYED } else { MF_ENABLED },
        );
        CheckMenuItem(
            app.menu,
            MUTE_WARNING as u32,
            MF_BYCOMMAND
                | if app
                    .controls
                    .audio
                    .warning_muted
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                },
        );
    }
}

fn disarm(app: &App) {
    app.controls.disarm();
    let gate = app.controls.gate.clone();
    let shared = app.shared.clone();
    shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .protection_status = "Protection OFF · cancelling any pending power-off…".into();
    // Cancellation must work even if the sampling thread is stuck in the driver.
    std::thread::spawn(move || {
        let result = gate.disarm_and_cancel();
        let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
        state.control_detail=match result {
            Ok(())=>"Disarmed; pending power-off cancelled if present. GPU limits retained until Restore.".into(),
            Err(e)=>format!("Disarmed, but cancellation failed: {e}"),
        };
    });
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
            &state.protection_status,
            if app
                .controls
                .gate
                .armed
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                amber
            } else {
                muted
            },
            app.small_font,
        );
        text(
            dc,
            28,
            495,
            650,
            &state.control_detail,
            muted,
            app.small_font,
        );
        let history = app.controls.history.status();
        let log_status = history
            .error
            .as_ref()
            .map(|e| format!("SQLite history unavailable: {e}"))
            .unwrap_or_else(|| {
                if history.dropped > 0 {
                    format!(
                        "History lost {} records during a storage failure",
                        history.dropped
                    )
                } else {
                    format!(
                        "SQLite history · 24 h retention · {} records buffered",
                        history.pending
                    )
                }
            });
        text(
            dc,
            28,
            528,
            650,
            &log_status,
            if history.error.is_some() || history.dropped > 0 {
                amber
            } else {
                muted
            },
            app.small_font,
        );
        text(
            dc,
            28,
            555,
            650,
            &app.controls
                .audio
                .error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_else(|| {
                    if app
                        .controls
                        .audio
                        .warning_muted
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        "Warning chime muted · critical / power-off alerts still sound".into()
                    } else {
                        "Audio alerts ON · warnings work while protection is OFF".into()
                    }
                }),
            muted,
            app.small_font,
        );
        text(
            dc,
            28,
            582,
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
                                    "Astral: {:.1} W · max {:.2} A · {}",
                                    r.watts(),
                                    r.max_amps(),
                                    if app.controls.gate.pending() {
                                        "POWER-OFF PENDING"
                                    } else if app
                                        .controls
                                        .gate
                                        .armed
                                        .load(std::sync::atomic::Ordering::SeqCst)
                                    {
                                        "ARMED"
                                    } else {
                                        "protection OFF"
                                    }
                                )
                            })
                            .unwrap_or_else(|| {
                                format!(
                                    "Astral: sensor unavailable · {}",
                                    if app
                                        .controls
                                        .gate
                                        .armed
                                        .load(std::sync::atomic::Ordering::SeqCst)
                                    {
                                        "ARMED"
                                    } else {
                                        "protection OFF"
                                    }
                                )
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
            WM_QUERYENDSESSION => {
                APP.with(|cell|{if let Some(app)=cell.borrow().as_ref(){
                    app.controls.history.event(serde_json::json!({"type":"system_shutdown","action":"query_end_session"}));
                    let _=app.controls.history.flush(Duration::from_millis(500));
                }});
                1
            }
            WM_ENDSESSION => {
                if wparam != 0 {
                    APP.with(|cell|{if let Some(app)=cell.borrow().as_ref(){
                    app.controls.gate.ending_session();
                    app.controls.history.event(serde_json::json!({"type":"system_shutdown","action":"end_session"}));
                    let _=app.controls.history.flush(Duration::from_millis(500));
                }});
                    PostQuitMessage(0);
                }
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
                                update_menu(app);
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
                        APP.with(|cell| {
                            if let Some(app) = cell.borrow().as_ref() {
                                app.controls.disarm();
                            }
                        });
                        DestroyWindow(hwnd);
                    }
                    ARM => {
                        APP.with(|cell| {
                            if let Some(app)=cell.borrow().as_ref() {
                                let monitor_only=app.shared.lock().unwrap_or_else(|p|p.into_inner()).monitor_only;
                                if monitor_only {
                                    MessageBoxW(hwnd,wide("This session was launched with --monitor-only. Restart normally to enable manual arming.").as_ptr(),wide("Monitor only").as_ptr(),MB_OK|MB_ICONINFORMATION);
                                } else if MessageBoxW(hwnd,wide("Enable real protection for this session?\n\n9.5 A for 15 s: limit GPU; 10 s grace.\n13 A for 3 s: limit GPU; 5 s grace.\n16 A on two samples: limit GPU and power off.\n\nIf limiting is unavailable or both changes fail, a trigger powers off directly. Power-off forces apps closed after 5 s; unsaved work can be lost.\n\nDisarm / Cancel aborts a pending countdown and keeps emergency GPU limits. The app starts disarmed next time.").as_ptr(),wide("Arm GPU protection").as_ptr(),MB_YESNO|MB_DEFBUTTON2|MB_ICONWARNING)==IDYES {
                                    app.controls.arm();
                                }
                            }
                        });
                    }
                    DISARM => {
                        APP.with(|cell| {
                            if let Some(app) = cell.borrow().as_ref() {
                                disarm(app);
                            }
                        });
                    }
                    RESTORE => {
                        APP.with(|cell| {
                            if let Some(app) = cell.borrow().as_ref() {
                                disarm(app);
                                app.controls.restore();
                            }
                        });
                    }
                    TEST_SOUNDS => {
                        APP.with(|cell| {
                            if let Some(app) = cell.borrow().as_ref() {
                                app.controls.audio.test();
                            }
                        });
                    }
                    MUTE_WARNING => {
                        APP.with(|cell| {
                            if let Some(app) = cell.borrow().as_ref() {
                                app.controls.audio.toggle_warning_mute();
                            }
                        });
                    }
                    FLUSH_HISTORY => {
                        APP.with(|cell| {
                            if let Some(app) = cell.borrow().as_ref() {
                                let _ = app.controls.history.flush(Duration::from_millis(500));
                            }
                        });
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
                if msg == MONITOR_ONLY {
                    APP.with(|cell| {
                        if let Some(app) = cell.borrow().as_ref() {
                            app.shared
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .monitor_only = true;
                            disarm(app);
                        }
                    });
                    return 0;
                }
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

pub fn request_exit() -> Result<(), String> {
    unsafe {
        let window = FindWindowW(wide(CLASS).as_ptr(), null());
        if window.is_null() {
            return Ok(());
        }
        if PostMessageW(window, WM_COMMAND, QUIT, 0) == 0 {
            return Err("Cannot quit the running monitor; matching administrator privileges may be required".into());
        }
    }
    Ok(())
}
pub fn run(start_hidden: bool, monitor_only: bool) -> Result<(), String> {
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
                if monitor_only && PostMessageW(existing, MONITOR_ONLY, 0, 0) == 0 {
                    CloseHandle(mutex);
                    return Err("Cannot disarm the existing instance; use its tray menu or launch --monitor-only with matching administrator privileges".into());
                }
                show(existing);
            }
            CloseHandle(mutex);
            return Ok(());
        }
        SetProcessDPIAware();
        let instance = GetModuleHandleW(null());
        let icons = match Icons::load(instance) {
            Ok(icons) => icons,
            Err(error) => {
                CloseHandle(mutex);
                return Err(error);
            }
        };
        let class_name = wide(CLASS);
        let mut class: WNDCLASSW = zeroed();
        class.lpfnWndProc = Some(window_proc);
        class.hInstance = instance;
        class.lpszClassName = class_name.as_ptr();
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hIcon = icons.big;
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
            715,
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
            (LOGS, "Open SQLite history folder"),
            (FLUSH_HISTORY, "Flush buffered history now"),
            (RESET, "Reset session peaks"),
            (ARM, "Arm protection for this session…"),
            (DISARM, "Disarm / Cancel pending power-off"),
            (RESTORE, "Restore saved GPU settings (disarms)"),
            (TEST_SOUNDS, "Test all four sounds (no protection actions)"),
            (MUTE_WARNING, "Mute warning chime (critical still sounds)"),
            (QUIT, "Quit"),
        ] {
            AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
        }
        let worker = Worker::start(monitor_only);
        APP.with(|cell| {
            *cell.borrow_mut() = Some(App {
                icons,
                shared: worker.shared.clone(),
                controls: worker.controls.clone(),
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
