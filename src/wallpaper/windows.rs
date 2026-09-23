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
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_SZ,
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

    fn monitor_names(&self) -> Result<Vec<String>> {
        Ok(monitor_names_for(&self.real_monitor_ids()?))
    }

    fn set_wallpaper_for_monitor(&self, path: &Path, monitor_index: usize) -> Result<()> {
        enforce_fill_style().context("failed to enforce wallpaper Fill style")?;

        let monitors = self.real_monitor_ids()?;
        let Some(monitor) = monitors.get(monitor_index) else {
            bail!("monitor index {monitor_index} out of range ({} displays)", monitors.len());
        };

        let absolute = path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?;
        let wide: Vec<u16> = absolute
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();

        let desktop = self.desktop_wallpaper()?;
        let vtbl = Self::vtbl(desktop);
        let hr = unsafe { (vtbl.set_wallpaper)(desktop, monitor.as_ptr(), wide.as_ptr()) };
        unsafe { (vtbl.release)(desktop) };
        if hr < 0 {
            bail!("IDesktopWallpaper::SetWallpaper failed with 0x{hr:08X}");
        }
        Ok(())
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

/// Human-readable names for the displays identified by `monitors`.
///
/// `IDesktopWallpaper` only hands out opaque device interface paths, so each
/// name is recovered from the EDID block the display driver publishes in the
/// registry. Displays whose EDID cannot be read — or that carry no name
/// descriptor, as some virtual panels do — fall back to `显示器 <n>`.
fn monitor_names_for(monitors: &[Vec<u16>]) -> Vec<String> {
    monitors
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let path = wide_to_string(id);
            path.as_deref()
                .and_then(edid_monitor_name)
                .or_else(|| path.as_deref().and_then(display_pnp_id))
                .unwrap_or_else(|| format!("显示器 {}", index + 1))
        })
        .collect()
}

/// The EDID vendor/product segment of a display interface path, e.g. `LEN8BA1`.
///
/// Used when a panel publishes no name descriptor: the panel id is still a real
/// identifier that can be matched against Device Manager, which beats an
/// anonymous `显示器 N`.
fn display_pnp_id(device_path: &str) -> Option<String> {
    let mut segments = device_path.split('#');
    segments.next()?; // leading "\\?\DISPLAY"
    let pnp_id = segments.next()?;
    (!pnp_id.is_empty()).then(|| pnp_id.to_string())
}

fn wide_to_string(wide: &[u16]) -> Option<String> {
    let end = wide.iter().position(|unit| *unit == 0).unwrap_or(wide.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&wide[..end]))
}

/// Reads the monitor name out of a display's EDID block.
///
/// `device_path` is the interface path returned by
/// `IDesktopWallpaper::GetMonitorDevicePathAt`, e.g.
/// `\\?\DISPLAY#LEN8BA1#4&2c6c98d4&0&UID8388688#{e6f07b5f-...}`. Its middle
/// segments are exactly the registry key components, which makes the lookup
/// exact rather than dependent on enumeration order.
fn edid_monitor_name(device_path: &str) -> Option<String> {
    let mut segments = device_path.split('#');
    segments.next()?; // leading "\\?\DISPLAY"
    let pnp_id = segments.next()?;
    let instance = segments.next()?;
    if pnp_id.is_empty() || instance.is_empty() {
        return None;
    }

    let subkey = wide_null(&format!(
        "SYSTEM\\CurrentControlSet\\Enum\\DISPLAY\\{pnp_id}\\{instance}\\Device Parameters"
    ));

    let mut key: HKEY = ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    if status != ERROR_SUCCESS {
        return None;
    }

    let edid = read_reg_binary(key, "EDID");
    unsafe { RegCloseKey(key) };
    edid_descriptor_name(edid.as_deref()?)
}

fn read_reg_binary(key: HKEY, value_name: &str) -> Option<Vec<u8>> {
    let name = wide_null(value_name);

    let mut data_len: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut data_len,
        )
    };
    if status != ERROR_SUCCESS || data_len == 0 {
        return None;
    }

    let mut buffer = vec![0u8; data_len as usize];
    let mut written = data_len;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            buffer.as_mut_ptr(),
            &mut written,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    buffer.truncate(written as usize);
    Some(buffer)
}

/// Extracts the display name from an EDID 1.x block.
///
/// The four 18-byte descriptor slots start at offset 54. `0xFC` holds the
/// monitor name proper, while `0xFE` is a free-form string that vendors
/// commonly use for brand plus model — Lenovo, for instance, emits two `0xFE`
/// descriptors (`LENOVO`, then `LEN160-3.2K`), so the last one is kept. A
/// `0xFC` descriptor always wins when present.
fn edid_descriptor_name(edid: &[u8]) -> Option<String> {
    let mut free_form = None;
    for offset in [54usize, 72, 90, 108] {
        let Some(descriptor) = edid.get(offset..offset + 18) else {
            continue;
        };
        if descriptor[0..3] != [0x00, 0x00, 0x00] {
            continue;
        }
        let kind = descriptor[3];
        if kind != 0xFC && kind != 0xFE {
            continue;
        }
        // 13 bytes of payload, terminated by 0x0A and padded with spaces.
        let payload = &descriptor[5..18];
        let payload = payload.split(|byte| *byte == 0x0A).next().unwrap_or(payload);
        let text = String::from_utf8_lossy(payload).trim().to_string();
        if text.is_empty() {
            continue;
        }
        if kind == 0xFC {
            return Some(text);
        }
        free_form = Some(text);
    }
    free_form
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

    /// EDID name descriptors must be read exactly as panels emit them.
    #[test]
    fn edid_descriptor_name_prefers_monitor_name_over_free_form() {
        // Lenovo-style: two free-form descriptors, brand first, model last.
        let mut edid = vec![0u8; 128];
        edid[54..72].copy_from_slice(&edid_descriptor(0xFE, "LENOVO"));
        edid[72..90].copy_from_slice(&edid_descriptor(0xFE, "LEN160-3.2K"));
        assert_eq!(edid_descriptor_name(&edid).as_deref(), Some("LEN160-3.2K"));

        // A real monitor-name descriptor outranks the free-form string.
        let mut edid = vec![0u8; 128];
        edid[54..72].copy_from_slice(&edid_descriptor(0xFE, "XIAOMI"));
        edid[90..108].copy_from_slice(&edid_descriptor(0xFC, "P27QBD-RG"));
        assert_eq!(edid_descriptor_name(&edid).as_deref(), Some("P27QBD-RG"));

        // Panels that publish nothing usable (or no EDID at all) yield None.
        assert_eq!(edid_descriptor_name(&[0u8; 128]), None);
        assert_eq!(edid_descriptor_name(&[]), None);
    }

    #[test]
    fn monitor_names_fall_back_when_the_device_path_cannot_be_resolved() {
        // A panel whose EDID cannot be read still exposes its panel id.
        let monitors = vec![
            wide_null(r"\\?\DISPLAY#AURA00#0&0&0&0&0000#{00000000-0000-0000-0000-000000000000}"),
            wide_null(r"\\?\DISPLAY#AURA01#0&0&0&0&0000#{00000000-0000-0000-0000-000000000000}"),
        ];
        assert_eq!(monitor_names_for(&monitors), vec!["AURA00", "AURA01"]);

        // Only a path with no usable segment drops to the index label.
        assert_eq!(monitor_names_for(&[Vec::new(), vec![0]]), vec!["显示器 1", "显示器 2"]);
    }

    #[test]
    fn reports_the_names_of_connected_monitors() {
        let backend = WindowsWallpaperBackend::new();
        let names = backend
            .monitor_names()
            .expect("monitor names should be available");
        assert_eq!(
            names.len(),
            backend.monitor_count().expect("monitor count should be available"),
            "every display must get a name"
        );
        eprintln!("aura: monitor names = {names:?}");
    }

    fn edid_descriptor(kind: u8, text: &str) -> [u8; 18] {
        let mut descriptor = [0x20u8; 18]; // space padded
        descriptor[0..4].copy_from_slice(&[0x00, 0x00, 0x00, kind]);
        descriptor[4] = 0x00;
        descriptor[5..5 + text.len()].copy_from_slice(text.as_bytes());
        descriptor[5 + text.len()] = 0x0A;
        descriptor
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
