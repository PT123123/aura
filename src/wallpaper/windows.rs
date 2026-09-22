use crate::errors::Result;
use crate::wallpaper::WallpaperBackend;
use anyhow::{bail, Context};
use std::ffi::c_void;
use std::iter;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use tracing::warn;
use windows_sys::core::{GUID, PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, RECT};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
};
use windows_sys::Win32::UI::Shell::DesktopWallpaper;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
};

/// `IID_IDesktopWallpaper` — {B92B56A9-8B55-4E14-9A89-0199BBB6F93B}.
///
/// windows-sys does not ship the `IDesktopWallpaper` interface, only its class
/// id (`Win32::UI::Shell::DesktopWallpaper`, {C2CF3110-460E-4FC1-B9D0-8A1C0C9CC4BD}),
/// so the vtable prefix used below is declared manually. Only the methods this
/// backend needs are defined; the trailing vtable entries are never touched.
const IID_IDESKTOP_WALLPAPER: GUID = GUID::from_u128(0xb92b56a9_8b55_4e14_9a89_0199bbb6f93b);

#[repr(C)]
struct DesktopWallpaperVtbl {
    // IUnknown (3 entries)
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    // IDesktopWallpaper (prefix)
    set_wallpaper: unsafe extern "system" fn(*mut c_void, PCWSTR, PCWSTR) -> i32,
    get_wallpaper: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut PWSTR) -> i32,
    get_monitor_device_path_at: unsafe extern "system" fn(*mut c_void, u32, *mut PWSTR) -> i32,
    get_monitor_device_path_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    get_monitor_rect: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut RECT) -> i32,
}

type HDesktopWallpaper = *mut c_void;

#[derive(Debug, Default)]
pub struct WindowsWallpaperBackend;

impl WindowsWallpaperBackend {
    pub fn new() -> Self {
        // COM must be initialised before CoCreateInstance on this thread.
        unsafe {
            let _ = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }
        Self
    }

    fn desktop_wallpaper(&self) -> Result<HDesktopWallpaper> {
        let mut instance: *mut c_void = ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &DesktopWallpaper,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IDESKTOP_WALLPAPER,
                &mut instance,
            )
        };
        if hr < 0 {
            bail!("CoCreateInstance(IDesktopWallpaper) failed with 0x{hr:08X}");
        }
        Ok(instance)
    }

    fn vtbl(instance: HDesktopWallpaper) -> &'static DesktopWallpaperVtbl {
        // The interface pointer points at an object whose first member is a
        // pointer to the vtable, so two indirections are required.
        unsafe { &**(instance as *const *const DesktopWallpaperVtbl) }
    }

    /// Enumerates the real, active displays as null-terminated wide strings.
    ///
    /// `IDesktopWallpaper` also reports pseudo-entries (empty ids,
    /// `legacy_monitor_001`, "Default Monitor" without EDID, ...). Those never
    /// expose a non-empty display rectangle, so each entry is validated with
    /// `GetMonitorRECT` before being accepted.
    fn real_monitor_ids(&self) -> Result<Vec<Vec<u16>>> {
        let desktop = self.desktop_wallpaper()?;
        let vtbl = Self::vtbl(desktop);

        let mut count: u32 = 0;
        let hr = unsafe { (vtbl.get_monitor_device_path_count)(desktop, &mut count) };
        if hr < 0 {
            unsafe { (vtbl.release)(desktop) };
            bail!("IDesktopWallpaper::GetMonitorDevicePathCount failed with 0x{hr:08X}");
        }

        let mut monitors: Vec<Vec<u16>> = Vec::new();
        for index in 0..count {
            let mut monitor_id: PWSTR = ptr::null_mut();
            let hr = unsafe { (vtbl.get_monitor_device_path_at)(desktop, index, &mut monitor_id) };
            if hr < 0 || monitor_id.is_null() {
                continue;
            }

            let mut wide: Vec<u16> = Vec::new();
            unsafe {
                let mut cursor = monitor_id;
                while cursor.read() != 0 {
                    wide.push(cursor.read());
                    cursor = cursor.add(1);
                }
                wide.push(0);
                CoTaskMemFree(monitor_id as *const c_void);
            }

            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let hr = unsafe { (vtbl.get_monitor_rect)(desktop, wide.as_ptr(), &mut rect) };
            if hr < 0 || rect.right <= rect.left || rect.bottom <= rect.top {
                warn!(index, "skipping display entry without a valid display rect");
                continue;
            }

            monitors.push(wide);
        }

        unsafe { (vtbl.release)(desktop) };
        Ok(monitors)
    }
}

impl WallpaperBackend for WindowsWallpaperBackend {
    fn set_wallpaper(&self, path: &Path) -> Result<()> {
        enforce_fill_style().context("failed to enforce wallpaper Fill style")?;

        let absolute = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?;

        let wide: Vec<u16> = absolute
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();

        let ok = unsafe {
            SystemParametersInfoW(
                SPI_SETDESKWALLPAPER,
                0,
                wide.as_ptr() as *mut _,
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        };
        if ok == 0 {
            bail!("SystemParametersInfoW(SPI_SETDESKWALLPAPER) failed");
        }
        Ok(())
    }

    fn monitor_count(&self) -> Result<usize> {
        Ok(self.real_monitor_ids()?.len())
    }

    fn set_wallpapers(&self, wallpapers: &[&Path]) -> Result<()> {
        enforce_fill_style().context("failed to enforce wallpaper Fill style")?;

        let Some(first) = wallpapers.first() else {
            bail!("no wallpapers provided");
        };

        let monitors = self.real_monitor_ids()?;

        // No real displays were reported (e.g. a service session): fall back
        // to the legacy whole-desktop path.
        if monitors.is_empty() {
            return self.set_wallpaper(first);
        }

        let desktop = self.desktop_wallpaper()?;
        let vtbl = Self::vtbl(desktop);

        let mut failed = 0usize;
        for (index, monitor) in monitors.iter().enumerate() {
            let path = wallpapers[index % wallpapers.len()];
            let absolute = match path.canonicalize() {
                Ok(absolute) => absolute,
                Err(error) => {
                    failed += 1;
                    warn!(path = %path.display(), error = %error, "failed to canonicalize wallpaper for monitor");
                    continue;
                }
            };

            let wide: Vec<u16> = absolute
                .as_os_str()
                .encode_wide()
                .chain(iter::once(0))
                .collect();

            let hr = unsafe { (vtbl.set_wallpaper)(desktop, monitor.as_ptr(), wide.as_ptr()) };
            if hr < 0 {
                failed += 1;
                warn!(index, hr, "IDesktopWallpaper::SetWallpaper failed");
            }
        }

        unsafe { (vtbl.release)(desktop) };

        if failed == monitors.len() {
            bail!("all per-monitor wallpaper updates failed");
        }
        Ok(())
    }
}

fn enforce_fill_style() -> Result<()> {
    let mut key: HKEY = ptr::null_mut();
    let subkey = wide_null("Control Panel\\Desktop");
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if status != ERROR_SUCCESS {
        bail!("RegOpenKeyExW failed with status {status}");
    }

    let close_result = (|| -> Result<()> {
        write_reg_sz(key, "WallpaperStyle", "10")?;
        write_reg_sz(key, "TileWallpaper", "0")?;
        Ok(())
    })();

    let close_status = unsafe { RegCloseKey(key) };
    if close_status != ERROR_SUCCESS {
        bail!("RegCloseKey failed with status {close_status}");
    }

    close_result
}

fn write_reg_sz(key: HKEY, value_name: &str, value: &str) -> Result<()> {
    let value_name_w = wide_null(value_name);
    let value_w = wide_null(value);
    let data_len = (value_w.len() * size_of::<u16>()) as u32;

    let status = unsafe {
        RegSetValueExW(
            key,
            value_name_w.as_ptr(),
            0,
            REG_SZ,
            value_w.as_ptr() as *const u8,
            data_len,
        )
    };

    if status != ERROR_SUCCESS {
        bail!("RegSetValueExW({value_name}) failed with status {status}");
    }
    Ok(())
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn enumerates_connected_monitors() {
        let backend = WindowsWallpaperBackend::new();
        let count = backend.monitor_count().expect("monitor enumeration should succeed");
        assert!(count >= 1, "expected at least one connected monitor");
        eprintln!("aura: connected monitors = {count}");
    }

    #[test]
    #[ignore = "applies wallpapers to the live desktop; run manually to verify per-monitor behaviour"]
    fn set_distinct_wallpapers_per_monitor() {
        let backend = WindowsWallpaperBackend::new();
        let dir = PathBuf::from(
            r"C:\Users\ted\Doubao\chats\2026-09-22\new-chat\aura-multiscreen-test",
        );
        let paths: Vec<PathBuf> = ["wallpaper-red.png", "wallpaper-green.png", "wallpaper-blue.png"]
            .iter()
            .map(|name| dir.join(name))
            .collect();
        let wallpapers: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        backend
            .set_wallpapers(&wallpapers)
            .expect("per-monitor wallpapers should be applied");
    }
}
