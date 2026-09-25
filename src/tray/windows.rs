//! Raw `Shell_NotifyIconW` tray icon on a hidden message window. The icon,
//! tooltip, and Open/Quit menu are registered directly with the shell, so
//! clicks arrive as window messages on the GUI thread and forward into the
//! [`kanal`] channel from [`spawn_proxy`]; there is no helper thread and no
//! polling. Notifications stay on WinRT toasts in `notifier::windows`, never
//! on the legacy `NIF_INFO` balloon this same struct could show.

use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use kanal::Sender;
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICONDATAW, NOTIFYICON_VERSION_4, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, GetCursorPos, HICON, HMENU,
    LookupIconIdFromDirectoryEx, LR_DEFAULTCOLOR, MF_STRING, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD,
    TrackPopupMenuEx, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, HWND_MESSAGE,
};

use super::TrayAction;

/// Bundled `.ico`; the shell picks its preferred image through the lookup
/// call in `load_icon`.
static ICON_ICO: &[u8] = include_bytes!("../../icons/icon.ico");

const TRAY_ID: u32 = 1;
/// Callback message the shell posts to the message window on tray input.
const WM_TRAY: u32 = WM_APP + 1;
const MENU_OPEN: i32 = 1001;
const MENU_QUIT: i32 = 1002;

#[cfg(debug_assertions)]
const TOOLTIP: &str = "Siphon Dev";
#[cfg(not(debug_assertions))]
const TOOLTIP: &str = "Siphon";

/// Clicks landing here come from the window procedure; set once by
/// [`spawn_proxy`] before the icon is built.
static TRAY_TX: OnceLock<Sender<TrayAction>> = OnceLock::new();

struct IconState {
    hwnd: HWND,
    icon: HICON,
}

// SAFETY: the handles are only ever touched on the GUI thread that owns the
// tray; the mutex never moves them across threads for use.
unsafe impl Send for IconState {}

/// Live icon for re-adding after Explorer restarts.
static ICON_STATE: OnceLock<Mutex<Option<IconState>>> = OnceLock::new();

fn icon_state() -> &'static Mutex<Option<IconState>> {
    ICON_STATE.get_or_init(|| Mutex::new(None))
}

/// `TaskbarCreated` broadcast id; the icon is re-added when it arrives.
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

fn taskbar_created() -> u32 {
    *TASKBAR_CREATED.get_or_init(|| {
        let name = wide("TaskbarCreated");
        // SAFETY: registering a well-formed message name; the pointer is
        // only read for the duration of the call.
        unsafe { RegisterWindowMessageW(name.as_ptr()) }
    })
}

/// Owns the tray icon and its message window. Held alive for the app
/// lifetime; dropping removes the icon from the tray.
pub struct Tray {
    hwnd: HWND,
    icon: HICON,
}

/// Creates the channel tray clicks forward into. The sender is stored for
/// the window procedure; the work task awaits the returned receiver
/// alongside UI intents, so no polling.
pub fn spawn_proxy() -> kanal::Receiver<TrayAction> {
    let (tx, rx) = kanal::unbounded::<TrayAction>();
    let _ = TRAY_TX.set(tx);
    rx
}

/// Builds the tray icon. Call on the GUI thread so the message window that
/// receives the shell callbacks pumps with the event loop.
pub fn build() -> Result<Tray, String> {
    // A throwaway keeps `build` usable where `spawn_proxy` never ran; clicks
    // then go nowhere instead of failing the build.
    TRAY_TX.get_or_init(|| kanal::unbounded::<TrayAction>().0);
    let hwnd = ensure_window()?;
    let icon = load_icon()?;
    add_icon(hwnd, icon)?;
    if let Ok(mut guard) = icon_state().lock() {
        *guard = Some(IconState { hwnd, icon });
    }
    Ok(Tray { hwnd, icon })
}

impl Drop for Tray {
    fn drop(&mut self) {
        let nid: NOTIFYICONDATAW = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        };
        // SAFETY: populated with the id from `add_icon`; the shell stops
        // tracking the icon.
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
        // SAFETY: icon created by `load_icon` and owned by this value.
        unsafe { DestroyIcon(self.icon) };
        // SAFETY: message window created by `ensure_window` for this value.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

fn send_action(action: TrayAction) {
    if let Some(tx) = TRAY_TX.get() {
        let _ = tx.send(action);
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn write_tip(tip: &mut [u16; 128], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().collect();
    let len = encoded.len().min(tip.len() - 1);
    tip[..len].copy_from_slice(&encoded[..len]);
}

/// Validates the `.ico` directory so offsets read below stay in bounds.
fn valid_ico(data: &[u8]) -> bool {
    const ICONDIR_SIZE: usize = 6;
    const ICONDIRENTRY_SIZE: usize = 16;
    if data.len() < ICONDIR_SIZE || data[0..2] != [0, 0] || data[2..4] != [1, 0] {
        return false;
    }
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    let directory = ICONDIR_SIZE + count.saturating_mul(ICONDIRENTRY_SIZE);
    if count == 0 || directory > data.len() {
        return false;
    }
    (0..count).all(|index| {
        let start = ICONDIR_SIZE + index * ICONDIRENTRY_SIZE;
        let entry = &data[start..start + ICONDIRENTRY_SIZE];
        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        size != 0
            && offset >= directory
            && offset.checked_add(size).is_some_and(|end| end <= data.len())
    })
}

/// Picks the shell-preferred image out of the bundled `.ico`.
fn load_icon() -> Result<HICON, String> {
    if !valid_ico(ICON_ICO) {
        return Err("bundled icon.ico failed validation".to_owned());
    }
    // SAFETY: `ICON_ICO` passed validation as a well-formed `.ico`
    // directory, so the lookup only reads its header.
    let offset = unsafe { LookupIconIdFromDirectoryEx(ICON_ICO.as_ptr(), 1, 0, 0, LR_DEFAULTCOLOR) };
    if offset <= 0 {
        return Err("bundled icon.ico lookup failed".to_owned());
    }
    let rest = ICON_ICO
        .get(offset as usize..)
        .ok_or_else(|| "bundled icon.ico entry out of bounds".to_owned())?;
    // SAFETY: `rest` starts at the validated image offset and spans to the
    // end of the bundle, which is exactly the resource slice the call reads.
    let icon = unsafe {
        CreateIconFromResourceEx(
            rest.as_ptr(),
            rest.len() as u32,
            1,
            0x0003_0000,
            0,
            0,
            LR_DEFAULTCOLOR,
        )
    };
    if icon.is_null() {
        return Err("CreateIconFromResourceEx failed".to_owned());
    }
    Ok(icon)
}

fn base_nid(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    write_tip(&mut nid.szTip, TOOLTIP);
    nid
}

/// Adds the icon and opts into version 4 event behavior.
fn add_icon(hwnd: HWND, icon: HICON) -> Result<(), String> {
    let nid = base_nid(hwnd, icon);
    // SAFETY: `nid` is fully populated with our live window, icon, tooltip,
    // and callback message; the shell only reads it for this call.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if added == 0 {
        return Err("Shell_NotifyIconW NIM_ADD failed".to_owned());
    }
    let mut versioned = nid;
    versioned.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    // SAFETY: same populated struct; the call only reads the id and the
    // version member.
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, &versioned) };
    Ok(())
}

fn re_add() {
    let guard = icon_state().lock();
    let Ok(guard) = guard else { return };
    if let Some(state) = guard.as_ref() {
        let _ = add_icon(state.hwnd, state.icon);
    }
}

fn show_menu(hwnd: HWND) {
    // SAFETY: creating an empty popup menu; the handle is only null-checked.
    let menu: HMENU = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    let open = wide("Open");
    let quit = wide("Quit");
    // SAFETY: appending a string item to a live menu with a nul-terminated
    // label that outlives the call.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_OPEN as usize, open.as_ptr()) };
    // SAFETY: same menu, second item.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, quit.as_ptr()) };
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: reading the cursor position into a struct we own.
    unsafe { GetCursorPos(&mut point) };
    // SAFETY: our own message window; foreground status lets the menu take
    // input and dismiss.
    unsafe { SetForegroundWindow(hwnd) };
    // SAFETY: modal menu tracking on our window with no clip rect; returns
    // the picked command id.
    let picked = unsafe {
        TrackPopupMenuEx(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
            point.x,
            point.y,
            hwnd,
            core::ptr::null(),
        )
    };
    // SAFETY: destroying the menu created above, exactly once.
    unsafe { DestroyMenu(menu) };
    if picked == MENU_OPEN {
        send_action(TrayAction::Show);
    } else if picked == MENU_QUIT {
        send_action(TrayAction::Quit);
    }
}

static CLASS_READY: AtomicBool = AtomicBool::new(false);

/// Registers the message-window class once and creates the hidden window
/// the shell posts tray input to.
fn ensure_window() -> Result<HWND, String> {
    // SAFETY: null name queries the current module handle.
    let instance = unsafe { GetModuleHandleW(core::ptr::null()) };
    if instance.is_null() {
        return Err("GetModuleHandleW failed".to_owned());
    }
    if !CLASS_READY.load(Ordering::Acquire) {
        let class = wide("SiphonTray");
        // SAFETY: zero is valid for every class field left unset; the
        // procedure and name below are filled in before registering.
        let mut wc: WNDCLASSW = unsafe { core::mem::zeroed() };
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        // SAFETY: registering the class described above; the name is copied
        // by the call, so the local stays valid.
        let atom = unsafe { RegisterClassW(&wc) };
        if atom == 0 {
            // SAFETY: reading the calling thread's last error after a failed
            // registration.
            let error = unsafe { GetLastError() };
            if error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(format!("RegisterClassW failed ({error})"));
            }
        }
        CLASS_READY.store(true, Ordering::Release);
    }
    let class = wide("SiphonTray");
    // SAFETY: class "SiphonTray" is registered above; a message-only window
    // needs no styles, geometry, or menu.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            core::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            core::ptr::null_mut(),
            instance,
            core::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("CreateWindowExW failed".to_owned());
    }
    Ok(hwnd)
}

/// Routes shell callbacks to tray actions; Explorer restarts re-add the
/// icon. Runs on the GUI thread that pumps the message window.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAY {
        match lparam as u32 {
            WM_LBUTTONUP => send_action(TrayAction::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        }
        return 0;
    }
    if msg == taskbar_created() {
        re_add();
        return 0;
    }
    // SAFETY: default handling for every message not processed above.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::{ICON_ICO, valid_ico};

    #[test]
    fn bundled_ico_validates() {
        assert!(valid_ico(ICON_ICO));
    }

    #[test]
    fn rejects_png_magic() {
        assert!(!valid_ico(b"\x89PNG\r\n\x1a\n00000000"));
    }

    #[test]
    fn rejects_truncated_directory() {
        assert!(!valid_ico(&ICON_ICO[..10]));
    }
}
