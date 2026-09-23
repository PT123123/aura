use crate::config::TrayDoubleClickAction;
use crate::errors::Result;
use crate::tray::{SessionStats, TrayEvent};
use anyhow::{anyhow, bail};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetWindowLongPtrW, InsertMenuItemW, LoadIconW, LoadImageW, PeekMessageW,
    PostMessageW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu,
    TranslateMessage, GWLP_USERDATA, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE,
    LR_SHARED, MENUITEMINFOW, MFT_SEPARATOR, MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STRING, MSG,
    PM_REMOVE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_NOANIMATION, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_APP, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    WS_EX_NOACTIVATE,
};

const TRAY_ICON_ID: u32 = 1;
const WM_TRAYICON: u32 = WM_APP + 1;
const SINGLE_INSTANCE_MUTEX_NAME: &str = "Local\\aura-tray-single-instance";
const TRAY_ICON_RESOURCE_ID: u16 = 101;
const TRAY_COMMAND_NEXT_BACKGROUND: u32 = 1000;
const TRAY_COMMAND_RELOAD_SETTINGS: u32 = 1001;
const TRAY_COMMAND_CHOOSE_WALLPAPER: u32 = 1003;
const TRAY_COMMAND_SETTINGS: u32 = 1004;
const TRAY_COMMAND_EXIT: u32 = 1005;

/// Opts the process into dark mode so native popup menus render dark.
/// Uses the undocumented `SetPreferredAppMode` export from uxtheme.dll
/// (ordinal 135); without it, TrackPopupMenu stays light-themed with
/// unreadable light text on a white background.
unsafe fn enable_dark_mode_for_menus() {
    let uxtheme = GetModuleHandleW(wide_null("uxtheme.dll").as_ptr());
    if uxtheme.is_null() {
        return;
    }
    let Some(proc) = GetProcAddress(uxtheme, 135usize as *const u8) else {
        return;
    };
    let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
        std::mem::transmute(proc);
    set_preferred_app_mode(1); // PreferredAppMode::AllowDark
}

pub struct SingleInstanceGuard {
    handle: HANDLE,
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

pub fn try_acquire_single_instance() -> Result<Option<SingleInstanceGuard>> {
    let name = wide_null(SINGLE_INSTANCE_MUTEX_NAME);
    let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        bail!("CreateMutexW failed");
    }

    let last_error = unsafe { GetLastError() };
    if last_error == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(handle);
        }
        return Ok(None);
    }

    Ok(Some(SingleInstanceGuard { handle }))
}

pub struct TrayController {
    shutdown_tx: Sender<()>,
    join_handle: Option<JoinHandle<()>>,
}

impl Drop for TrayController {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn spawn(
    config_path: PathBuf,
    event_tx: UnboundedSender<TrayEvent>,
    session_stats: Arc<SessionStats>,
) -> Result<TrayController> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

    let join_handle = thread::spawn(move || {
        if let Err(error) =
            run_tray_loop(config_path, event_tx, session_stats, shutdown_rx, ready_tx)
        {
            tracing::error!(error = %error, "tray loop failed");
        }
    });

    let ready = ready_rx
        .recv()
        .map_err(|_| anyhow!("tray thread terminated during startup"))?;
    ready?;

    Ok(TrayController {
        shutdown_tx,
        join_handle: Some(join_handle),
    })
}

struct WindowData {
    event_tx: UnboundedSender<TrayEvent>,
    session_stats: Arc<SessionStats>,
    hinstance: HINSTANCE,
    config_path: PathBuf,
}

fn run_tray_loop(
    config_path: PathBuf,
    event_tx: UnboundedSender<TrayEvent>,
    session_stats: Arc<SessionStats>,
    shutdown_rx: Receiver<()>,
    ready_tx: Sender<Result<()>>,
) -> Result<()> {
    unsafe {
        enable_dark_mode_for_menus();
    }
    let class_name = wide_null("aura_tray_window");
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(ptr::null()) };

    let wnd_class = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        lpszClassName: class_name.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    let atom = unsafe { RegisterClassW(&wnd_class) };
    if atom == 0 {
        let _ = ready_tx.send(Err(anyhow!("RegisterClassW failed")));
        return Ok(());
    }

    let user_data = Box::new(WindowData {
        event_tx,
        session_stats: session_stats.clone(),
        hinstance,
        config_path: config_path.clone(),
    });
    let user_data_ptr = Box::into_raw(user_data);

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            hinstance,
            user_data_ptr as *const _,
        )
    };
    if hwnd.is_null() {
        unsafe {
            drop(Box::from_raw(user_data_ptr));
        }
        let _ = ready_tx.send(Err(anyhow!("CreateWindowExW failed")));
        return Ok(());
    }

    let nid = create_notify_icon_data(hwnd, hinstance);
    let add_ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if add_ok == 0 {
        unsafe {
            DestroyWindow(hwnd);
        }
        let _ = ready_tx.send(Err(anyhow!("Shell_NotifyIconW(NIM_ADD) failed")));
        return Ok(());
    }

    let _ = ready_tx.send(Ok(()));
    tracing::info!("tray icon initialized");

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        thread::sleep(Duration::from_millis(50));
    }

    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &nid);
        DestroyWindow(hwnd);
    }
    tracing::info!("tray icon shutdown complete");
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create_struct =
                lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            if !create_struct.is_null() {
                let data_ptr = (*create_struct).lpCreateParams as isize;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, data_ptr);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_TRAYICON => {
            let event_code = lparam as u32;
            if let Some(data) = get_window_data(hwnd) {
                match event_code {
                    WM_LBUTTONDBLCLK => {
                        if !data.session_stats.is_shader_active() {
                            match crate::config::read_tray_double_click(&data.config_path) {
                                TrayDoubleClickAction::Next => {
                                    let _ = data.event_tx.send(TrayEvent::NextWallpaper);
                                }
                                TrayDoubleClickAction::Picker => {
                                    let _ = data.event_tx.send(TrayEvent::OpenWallpaperPicker);
                                }
                            }
                        }
                    }
                    WM_RBUTTONUP => {
                        show_context_menu(hwnd, data);
                    }
                    _ => {}
                }
            }
            return 0;
        }
        WM_NCDESTROY => {
            let ptr_value = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr_value != 0 {
                let ptr = ptr_value as *mut WindowData;
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        _ => {}
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn get_window_data(hwnd: HWND) -> Option<&'static mut WindowData> {
    let ptr_value = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if ptr_value == 0 {
        None
    } else {
        Some(&mut *(ptr_value as *mut WindowData))
    }
}

fn create_notify_icon_data(hwnd: HWND, hinstance: HINSTANCE) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ICON_ID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = load_tray_icon(hinstance);
    fill_tip(&mut nid.szTip, "aura");
    nid
}

unsafe fn show_context_menu(hwnd: HWND, data: &mut WindowData) {
    let mut anchor_point: POINT = std::mem::zeroed();
    if GetCursorPos(&mut anchor_point) == 0 {
        tracing::warn!("GetCursorPos failed for tray menu");
        return;
    }

    let menu = CreatePopupMenu();
    if menu.is_null() {
        tracing::warn!("CreatePopupMenu failed");
        return;
    }

    let next_background_label = wide_null("设置下一个背景");
    let choose_wallpaper_label = wide_null("选择壁纸");
    let reload_settings_label = wide_null("重新加载设置");
    let settings_label = wide_null("设置");
    let exit_label = wide_null("退出");

    let mut position: u32 = 0;
    if !data.session_stats.is_shader_active() {
        if !insert_command_menu_item(
            menu,
            position,
            TRAY_COMMAND_NEXT_BACKGROUND,
            next_background_label.as_ptr(),
        ) {
            tracing::warn!("failed to add Next Background tray menu item");
        }
        position += 1;
    }
    if !insert_command_menu_item(
        menu,
        position,
        TRAY_COMMAND_CHOOSE_WALLPAPER,
        choose_wallpaper_label.as_ptr(),
    ) {
        tracing::warn!("failed to add Choose Wallpaper tray menu item");
    }
    position += 1;
    if !insert_command_menu_item(
        menu,
        position,
        TRAY_COMMAND_RELOAD_SETTINGS,
        reload_settings_label.as_ptr(),
    ) {
        tracing::warn!("failed to add Reload Settings tray menu item");
    }
    position += 1;
    if !insert_command_menu_item(
        menu,
        position,
        TRAY_COMMAND_SETTINGS,
        settings_label.as_ptr(),
    ) {
        tracing::warn!("failed to add Settings tray menu item");
    }
    position += 1;
    if !insert_separator_menu_item(menu, position) {
        tracing::warn!("failed to add separator tray menu item");
    }
    position += 1;
    if !insert_command_menu_item(
        menu,
        position,
        TRAY_COMMAND_EXIT,
        exit_label.as_ptr(),
    ) {
        tracing::warn!("failed to add Exit tray menu item");
    }

    SetForegroundWindow(hwnd);
    let selected_command = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NOANIMATION,
        anchor_point.x,
        anchor_point.y,
        0,
        hwnd,
        ptr::null(),
    );
    if selected_command != 0 {
        handle_tray_command(hwnd, data, selected_command as u32);
    }
    PostMessageW(hwnd, WM_NULL, 0, 0);

    DestroyMenu(menu);
}

unsafe fn handle_tray_command(_hwnd: HWND, data: &mut WindowData, command_id: u32) {
    match command_id {
        TRAY_COMMAND_NEXT_BACKGROUND => {
            let _ = data.event_tx.send(TrayEvent::NextWallpaper);
        }
        TRAY_COMMAND_CHOOSE_WALLPAPER => {
            let _ = data.event_tx.send(TrayEvent::OpenWallpaperPicker);
        }
        TRAY_COMMAND_RELOAD_SETTINGS => {
            let _ = data.event_tx.send(TrayEvent::ReloadSettings);
        }
        TRAY_COMMAND_SETTINGS => {
            let _ = data.event_tx.send(TrayEvent::OpenSettings);
        }
        TRAY_COMMAND_EXIT => {
            let _ = data.event_tx.send(TrayEvent::Exit);
        }
        _ => {}
    }
}

pub fn open_settings(path: &Path) -> Result<()> {
    let operation = wide_null("open");
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = wide_null(&path.to_string_lossy());
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            path.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        bail!("ShellExecuteW failed to open the Aura settings file");
    }
    Ok(())
}

unsafe fn insert_command_menu_item(
    menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU,
    position: u32,
    command_id: u32,
    label: *const u16,
) -> bool {
    let mut menu_item: MENUITEMINFOW = std::mem::zeroed();
    menu_item.cbSize = size_of::<MENUITEMINFOW>() as u32;
    menu_item.fMask = MIIM_ID | MIIM_STRING | MIIM_FTYPE;
    menu_item.fType = MFT_STRING;
    menu_item.wID = command_id;
    menu_item.dwTypeData = label as *mut u16;
    InsertMenuItemW(menu, position, 1, &menu_item) != 0
}

unsafe fn insert_separator_menu_item(
    menu: windows_sys::Win32::UI::WindowsAndMessaging::HMENU,
    position: u32,
) -> bool {
    let mut menu_item: MENUITEMINFOW = std::mem::zeroed();
    menu_item.cbSize = size_of::<MENUITEMINFOW>() as u32;
    menu_item.fMask = MIIM_FTYPE;
    menu_item.fType = MFT_SEPARATOR;
    InsertMenuItemW(menu, position, 1, &menu_item) != 0
}

fn load_tray_icon(hinstance: HINSTANCE) -> HICON {
    let custom = unsafe {
        LoadImageW(
            hinstance,
            make_int_resource(TRAY_ICON_RESOURCE_ID),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        ) as HICON
    };
    if !custom.is_null() {
        tracing::info!(
            resource_id = TRAY_ICON_RESOURCE_ID,
            "loaded custom tray icon"
        );
        return custom;
    }

    tracing::warn!(
        resource_id = TRAY_ICON_RESOURCE_ID,
        "custom tray icon not found, falling back to default"
    );
    unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) }
}

fn make_int_resource(id: u16) -> *const u16 {
    id as usize as *const u16
}

fn fill_tip(buf: &mut [u16], text: &str) {
    if buf.is_empty() {
        return;
    }
    let mut encoded = text.encode_utf16().collect::<Vec<_>>();
    encoded.truncate(buf.len().saturating_sub(1));
    let len = encoded.len();
    buf[..len].copy_from_slice(&encoded);
    buf[len] = 0;
    for item in &mut buf[(len + 1)..] {
        *item = 0;
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
