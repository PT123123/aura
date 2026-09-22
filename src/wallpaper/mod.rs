use crate::errors::Result;
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub trait WallpaperBackend: Send + Sync {
    fn set_wallpaper(&self, path: &Path) -> Result<()>;

    /// Number of displays the backend currently manages.
    ///
    /// Backends that cannot query the display topology default to `1`.
    fn monitor_count(&self) -> Result<usize> {
        Ok(1)
    }

    /// Apply a distinct wallpaper to every display.
    ///
    /// `wallpapers[i]` is applied to display `i` (wrapping around when there
    /// are fewer wallpapers than displays). The default implementation only
    /// supports a single shared wallpaper and applies `wallpapers[0]` to the
    /// whole desktop; backends that can address displays individually (e.g.
    /// Windows via `IDesktopWallpaper`) override this.
    fn set_wallpapers(&self, wallpapers: &[&Path]) -> Result<()> {
        let Some(first) = wallpapers.first() else {
            anyhow::bail!("no wallpapers provided")
        };
        self.set_wallpaper(first)
    }

    /// Human-readable names for the managed displays (e.g. "显示器 1").
    fn monitor_names(&self) -> Result<Vec<String>> {
        let count = self.monitor_count()?;
        Ok((0..count).map(|index| format!("显示器 {}", index + 1)).collect())
    }

    /// Apply a wallpaper to a single display by index. The default backend
    /// cannot address individual displays and falls back to the shared path.
    fn set_wallpaper_for_monitor(&self, path: &Path, monitor_index: usize) -> Result<()> {
        let count = self.monitor_count()?;
        if monitor_index >= count {
            anyhow::bail!("monitor index {monitor_index} out of range ({count} displays)");
        }
        self.set_wallpaper(path)
    }
}

#[cfg(windows)]
pub fn default_backend() -> Box<dyn WallpaperBackend> {
    Box::new(windows::WindowsWallpaperBackend::new())
}

#[cfg(target_os = "linux")]
pub fn default_backend() -> Box<dyn WallpaperBackend> {
    Box::new(linux::LinuxWallpaperBackend)
}

#[cfg(target_os = "macos")]
pub fn default_backend() -> Box<dyn WallpaperBackend> {
    Box::new(macos::MacWallpaperBackend)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
struct UnsupportedWallpaperBackend;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
impl WallpaperBackend for UnsupportedWallpaperBackend {
    fn set_wallpaper(&self, _path: &Path) -> Result<()> {
        anyhow::bail!("wallpaper updates require Windows, GNOME, or KDE Plasma")
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn default_backend() -> Box<dyn WallpaperBackend> {
    Box::new(UnsupportedWallpaperBackend)
}
