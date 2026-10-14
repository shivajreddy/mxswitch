//! Windows glue: console output for CLI use, the autostart registry entry,
//! and the tray icon + global hotkey message loop.

use std::cell::RefCell;
use std::io::Write;
use std::ptr::{null, null_mut};

use windows_sys::core::{w, PCWSTR};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND,
    INVALID_HANDLE_VALUE, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetConsoleWindow, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
    VK_DELETE, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_NEXT, VK_PAUSE, VK_PRIOR,
    VK_SCROLL, VK_SPACE, VK_TAB,
};
use windows_sys::Win32::UI::Shell::{
    ExtractIconW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, MessageBoxW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage,
    HICON, IDI_APPLICATION, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_SETFOREGROUND,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use crate::hidpp::control_name;
use crate::watch::{self, Command, Event};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let max = dst.len() - 1;
    let mut i = 0;
    for u in s.encode_utf16() {
        if i >= max {
            break;
        }
        dst[i] = u;
        i += 1;
    }
    dst[i] = 0;
}

pub fn message_box(text: &str, title: &str, flags: u32) {
    let text = wide(text);
    let title = wide(title);
    unsafe {
        MessageBoxW(null_mut(), text.as_ptr(), title.as_ptr(), flags | MB_SETFOREGROUND);
    }
}

// ---------------------------------------------------------------------------
// Console output. The exe is a GUI-subsystem program so that it never opens a
// console window when started at login; when run from a terminal it attaches
// to that terminal's console instead, and falls back to a message box when
// there is none (e.g. double-clicked in Explorer).
// ---------------------------------------------------------------------------

fn have_stdout() -> bool {
    unsafe {
        // Attaching to the parent's console fills in the standard handles unless they
        // were already redirected to a pipe or file, which we can also write to.
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 || !GetConsoleWindow().is_null() {
            return true;
        }
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        !h.is_null() && h != INVALID_HANDLE_VALUE
    }
}

pub fn emit(text: &str, title: &str, is_error: bool) {
    if have_stdout() {
        let mut out = std::io::stdout();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    } else {
        message_box(text, title, if is_error { MB_ICONERROR } else { MB_ICONINFORMATION });
    }
}

// ---------------------------------------------------------------------------
// Autostart via HKCU\...\Run
// ---------------------------------------------------------------------------

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("MxSwitch");

pub fn autostart_command() -> Option<String> {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_READ, &mut key) != ERROR_SUCCESS {
            return None;
        }
        let mut data = vec![0u16; 2048];
        let mut size = (data.len() * 2) as u32;
        let rc = RegQueryValueExW(key, RUN_VALUE, null(), null_mut(), data.as_mut_ptr().cast(), &mut size);
        RegCloseKey(key);
        if rc != ERROR_SUCCESS {
            return None;
        }
        let n = (size as usize / 2).min(data.len());
        Some(String::from_utf16_lossy(&data[..n]).trim_end_matches('\0').to_string())
    }
}

fn quote_arg(a: &str) -> String {
    if a.contains(' ') || a.contains('"') {
        format!("\"{}\"", a.replace('"', "\\\""))
    } else {
        a.to_string()
    }
}

/// Enables or disables starting the tray app at login. Returns the registered command line.
pub fn set_autostart(enable: bool, extra_args: &[String]) -> Result<String, String> {
    let mut cmd = String::new();
    if enable {
        let exe = std::env::current_exe().map_err(|e| format!("cannot determine the exe path: {e}"))?;
        cmd = format!("\"{}\"", exe.display());
        for a in extra_args {
            cmd.push(' ');
            cmd.push_str(&quote_arg(a));
        }
    }
    unsafe {
        let mut key: HKEY = null_mut();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            null(),
            &mut key,
            null_mut(),
        );
        if rc != ERROR_SUCCESS {
            return Err(format!("cannot open the Run registry key (error {rc})"));
        }
        let rc = if enable {
            let data = wide(&cmd);
            RegSetValueExW(key, RUN_VALUE, 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
        } else {
            match RegDeleteValueW(key, RUN_VALUE) {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                other => other,
            }
        };
        RegCloseKey(key);
        if rc != ERROR_SUCCESS {
            return Err(format!("cannot update the Run registry value (error {rc})"));
        }
    }
    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Hotkey parsing
// ---------------------------------------------------------------------------

pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
    pub label: String,
}

fn key_code(t: &str) -> Option<(u32, String)> {
    if let Some(n) = t.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then(|| (VK_F1 as u32 + n - 1, format!("F{n}")));
    }
    if t.len() == 1 {
        let c = t.chars().next()?.to_ascii_uppercase();
        return c.is_ascii_alphanumeric().then(|| (c as u32, c.to_string()));
    }
    let (code, name) = match t {
        "pause" | "break" => (VK_PAUSE, "Pause"),
        "scrolllock" | "scroll" => (VK_SCROLL, "ScrollLock"),
        "insert" | "ins" => (VK_INSERT, "Insert"),
        "delete" | "del" => (VK_DELETE, "Delete"),
        "home" => (VK_HOME, "Home"),
        "end" => (VK_END, "End"),
        "pageup" | "pgup" => (VK_PRIOR, "PageUp"),
        "pagedown" | "pgdn" => (VK_NEXT, "PageDown"),
        "space" => (VK_SPACE, "Space"),
        "tab" => (VK_TAB, "Tab"),
        "esc" | "escape" => (VK_ESCAPE, "Esc"),
        _ => return None,
    };
    Some((code as u32, name.to_string()))
}

/// Parses specs like `F21`, `ctrl+alt+m`, `win+shift+F12`.
pub fn parse_hotkey(spec: &str) -> Result<Hotkey, String> {
    let mut modifiers = 0u32;
    let mut labels: Vec<String> = Vec::new();
    let mut key: Option<(u32, String)> = None;
    for token in spec.split(['+', '-', ' ']).filter(|t| !t.is_empty()) {
        let t = token.to_ascii_lowercase();
        let modifier = match t.as_str() {
            "ctrl" | "control" => Some((MOD_CONTROL, "Ctrl")),
            "alt" => Some((MOD_ALT, "Alt")),
            "shift" => Some((MOD_SHIFT, "Shift")),
            "win" | "super" | "meta" => Some((MOD_WIN, "Win")),
            _ => None,
        };
        match modifier {
            Some((flag, label)) => {
                modifiers |= flag;
                labels.push(label.to_string());
            }
            None => {
                if key.is_some() {
                    return Err(format!("hotkey '{spec}' has more than one key"));
                }
                key = Some(key_code(&t).ok_or_else(|| format!("unknown key '{token}' in hotkey '{spec}'"))?);
            }
        }
    }
    let (vk, name) = key.ok_or_else(|| format!("hotkey '{spec}' has no key"))?;
    labels.push(name);
    Ok(Hotkey { modifiers, vk, label: labels.join("+") })
}

// ---------------------------------------------------------------------------
// Tray icon + hotkey
// ---------------------------------------------------------------------------

const MUTEX_NAME: PCWSTR = w!("Local\\MxSwitchTray");
const WM_TRAY: u32 = WM_APP + 1;
const WM_DEVICE: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;

const ID_TOGGLE: usize = 1;
const ID_STATUS: usize = 2;
const ID_AUTOSTART: usize = 3;
const ID_EXIT: usize = 4;
const ID_HOST_BASE: usize = 10;

pub struct TrayConfig {
    pub hotkey: Option<Hotkey>,
    pub button: Option<u16>,
    pub hosts: (u8, u8),
    pub device: Option<String>,
    pub extra_args: Vec<String>,
}

impl TrayConfig {
    fn trigger_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(b) = self.button {
            let name = control_name(b);
            parts.push(if name.is_empty() { format!("mouse button 0x{b:04X}") } else { format!("{name} on the mouse") });
        }
        if let Some(k) = &self.hotkey {
            parts.push(k.label.clone());
        }
        if parts.is_empty() {
            "tray menu only".to_string()
        } else {
            parts.join(" or ")
        }
    }
}

struct App {
    cfg: TrayConfig,
    worker: Option<watch::Handle>,
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Hands listener-thread events to the UI thread as window messages.
struct PostSink(usize);

impl watch::Sink for PostSink {
    fn post(&self, event: Event) {
        let ptr = Box::into_raw(Box::new(event));
        unsafe {
            if PostMessageW(self.0 as HWND, WM_DEVICE, ptr as usize, 0) == 0 {
                drop(Box::from_raw(ptr));
            }
        }
    }
}

fn send(cmd: Command) {
    APP.with(|a| {
        if let Some(app) = a.borrow().as_ref() {
            if let Some(w) = &app.worker {
                let _ = w.tx.send(cmd);
            }
        }
    });
}

pub fn is_tray_running() -> bool {
    unsafe {
        let h = CreateMutexW(null(), 0, MUTEX_NAME);
        let exists = GetLastError() == ERROR_ALREADY_EXISTS;
        if !h.is_null() {
            CloseHandle(h);
        }
        exists
    }
}

unsafe fn load_icon() -> HICON {
    // The mouse icon that Windows itself uses for Mouse Properties.
    let icon = ExtractIconW(null_mut(), w!("main.cpl"), 0);
    if icon.is_null() || icon as usize == 1 {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        icon
    }
}

pub fn run_tray(cfg: TrayConfig) -> i32 {
    unsafe {
        // Keep the mutex for the life of the process: it is the single-instance guard.
        let mutex = CreateMutexW(null(), 0, MUTEX_NAME);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message_box("MxSwitch is already running – look for its icon in the tray.", "MxSwitch", MB_ICONINFORMATION);
            return 1;
        }
        let _ = mutex;

        let hinstance = GetModuleHandleW(null());
        let class_name = w!("MxSwitchTrayWindow");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name,
        };
        if RegisterClassW(&wc) == 0 {
            message_box("Cannot register the tray window class.", "MxSwitch", MB_ICONERROR);
            return 1;
        }
        let hwnd = CreateWindowExW(
            0,
            class_name,
            w!("MxSwitch"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        );
        if hwnd.is_null() {
            message_box("Cannot create the tray window.", "MxSwitch", MB_ICONERROR);
            return 1;
        }

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = load_icon();
        copy_wide(&mut nid.szTip, "MxSwitch – looking for the mouse...");
        Shell_NotifyIconW(NIM_ADD, &nid);

        let mut hotkey_problem = None;
        if let Some(k) = &cfg.hotkey {
            if RegisterHotKey(hwnd, HOTKEY_ID, k.modifiers | MOD_NOREPEAT, k.vk) == 0 {
                hotkey_problem = Some(k.label.clone());
            }
        }

        let worker = watch::spawn(
            watch::Config { hosts: cfg.hosts, device: cfg.device.clone(), button: cfg.button },
            PostSink(hwnd as usize),
        );
        APP.with(|a| *a.borrow_mut() = Some(App { cfg, worker: Some(worker), hwnd, nid }));

        if let Some(label) = hotkey_problem {
            message_box(
                &format!(
                    "Could not register the {label} hotkey – another program already uses it.\n\
                     The tray icon still works; pick a different key with --key."
                ),
                "MxSwitch",
                MB_ICONWARNING,
            );
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        msg.wParam as i32
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            do_toggle();
            0
        }
        WM_TRAY => {
            match (lparam & 0xFFFF) as u32 {
                WM_RBUTTONUP | WM_LBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONDBLCLK => do_toggle(),
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            handle_command(wparam & 0xFFFF);
            0
        }
        WM_DEVICE => {
            let event = Box::from_raw(wparam as *mut Event);
            handle_event(*event);
            0
        }
        WM_DESTROY => {
            cleanup();
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn handle_event(event: Event) {
    match event {
        Event::Connected { name, channel } => {
            let trigger = APP.with(|a| a.borrow().as_ref().map(|app| app.cfg.trigger_label()).unwrap_or_default());
            set_tooltip(&format!("{name} – on channel {}. Switch: {trigger}", channel + 1));
        }
        Event::Disconnected => set_tooltip("MxSwitch – the mouse is not on this PC right now"),
        Event::Notice { title, text } => notify(&title, &text),
    }
}

fn do_toggle() {
    send(Command::Toggle);
}

fn switch_to(host: u8) {
    send(Command::SetHost(host));
}

fn set_tooltip(text: &str) {
    APP.with(|a| {
        let Ok(mut guard) = a.try_borrow_mut() else { return };
        let Some(app) = guard.as_mut() else { return };
        copy_wide(&mut app.nid.szTip, text);
        let mut nid = app.nid;
        nid.uFlags = NIF_TIP;
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    });
}

fn notify(title: &str, text: &str) {
    APP.with(|a| {
        let Ok(guard) = a.try_borrow() else { return };
        let Some(app) = guard.as_ref() else { return };
        let mut nid = app.nid;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    });
}

unsafe fn show_menu(hwnd: HWND) {
    // Ask the listener thread for the current channel; nothing here holds the
    // app state across TrackPopupMenu, which runs its own message loop.
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    send(Command::Current(reply_tx));
    let (status, current) = match reply_rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(Ok((t, info))) => (format!("{} – on channel {}", t.name, info.current + 1), Some(info.current)),
        _ => ("Mouse not found on this PC".to_string(), None),
    };
    let trigger = APP.with(|a| a.borrow().as_ref().map(|app| app.cfg.trigger_label()).unwrap_or_default());
    let autostart = autostart_command().is_some();

    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&status).as_ptr());
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&format!("Trigger: {trigger}")).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_TOGGLE, w!("Switch to the other PC"));
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    for h in 0..3u8 {
        let checked = if current == Some(h) { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | checked, ID_HOST_BASE + h as usize, wide(&format!("Channel {}", h + 1)).as_ptr());
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_STATUS, w!("Status..."));
    AppendMenuW(menu, MF_STRING | if autostart { MF_CHECKED } else { 0 }, ID_AUTOSTART, w!("Start with Windows"));
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe fn handle_command(id: usize) {
    match id {
        ID_TOGGLE => do_toggle(),
        ID_STATUS => {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            send(Command::Status(reply_tx));
            let text = reply_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|_| "The listener did not answer in time.".to_string());
            message_box(&text, "MxSwitch status", MB_ICONINFORMATION);
        }
        ID_AUTOSTART => {
            let extra = APP.with(|a| a.borrow().as_ref().map(|app| app.cfg.extra_args.clone()).unwrap_or_default());
            let enable = autostart_command().is_none();
            if let Err(e) = set_autostart(enable, &extra) {
                message_box(&e, "MxSwitch", MB_ICONERROR);
            }
        }
        ID_EXIT => {
            // Stop the listener first so it can hand the button back to the mouse.
            let (worker, hwnd) = APP.with(|a| {
                let mut guard = a.borrow_mut();
                let app = guard.as_mut().expect("tray state");
                (app.worker.take(), app.hwnd)
            });
            if let Some(w) = worker {
                let _ = w.tx.send(Command::Quit);
                let _ = w.thread.join();
            }
            DestroyWindow(hwnd);
        }
        id if (ID_HOST_BASE..ID_HOST_BASE + 3).contains(&id) => switch_to((id - ID_HOST_BASE) as u8),
        _ => {}
    }
}

fn cleanup() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().take() {
            if let Some(w) = app.worker {
                let _ = w.tx.send(Command::Quit);
                let _ = w.thread.join();
            }
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &app.nid);
                if app.cfg.hotkey.is_some() {
                    UnregisterHotKey(app.hwnd, HOTKEY_ID);
                }
            }
        }
    });
}
