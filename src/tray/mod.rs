use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum TrayEvent {
    NextWallpaper,
    ReloadSettings,
    OpenSettings,
    OpenWallpaperPicker,
    ApplyWallpaper(PathBuf),
    ApplyWallpaperToMonitor(PathBuf, usize),
    AddFavorite(PathBuf),
    CheckForUpdates,
    Exit,
}

#[derive(Debug)]
pub struct SessionStats {
    timer_display: RwLock<String>,
    remote_update_timer_display: RwLock<String>,
    app_update_status: RwLock<String>,
    shader_name: RwLock<String>,
    total_images: AtomicU64,
    images_shown: AtomicU64,
    manual_skips: AtomicU64,
    shader_active: AtomicBool,
    started_at: Instant,
}

impl SessionStats {
    pub fn new(
        timer_display: String,
        remote_update_timer_display: String,
        app_update_status: String,
        shader_name: String,
    ) -> Self {
        Self {
            timer_display: RwLock::new(timer_display),
            remote_update_timer_display: RwLock::new(remote_update_timer_display),
            app_update_status: RwLock::new(app_update_status),
            shader_name: RwLock::new(shader_name),
            total_images: AtomicU64::new(0),
            images_shown: AtomicU64::new(0),
            manual_skips: AtomicU64::new(0),
            shader_active: AtomicBool::new(false),
            started_at: Instant::now(),
        }
    }

    pub fn timer_display(&self) -> String {
        self.timer_display
            .read()
            .expect("timer display lock poisoned")
            .clone()
    }

    pub fn set_timer_display(&self, timer_display: String) {
        *self
            .timer_display
            .write()
            .expect("timer display lock poisoned") = timer_display;
    }

    pub fn remote_update_timer_display(&self) -> String {
        self.remote_update_timer_display
            .read()
            .expect("remote update timer display lock poisoned")
            .clone()
    }

    pub fn set_remote_update_timer_display(&self, remote_update_timer_display: String) {
        *self
            .remote_update_timer_display
            .write()
            .expect("remote update timer display lock poisoned") = remote_update_timer_display;
    }

    pub fn app_update_status(&self) -> String {
        self.app_update_status
            .read()
            .expect("app update status lock poisoned")
            .clone()
    }

    pub fn set_app_update_status(&self, app_update_status: String) {
        *self
            .app_update_status
            .write()
            .expect("app update status lock poisoned") = app_update_status;
    }

    pub fn shader_name(&self) -> String {
        self.shader_name
            .read()
            .expect("shader name lock poisoned")
            .clone()
    }

    pub fn set_shader_name(&self, shader_name: String) {
        *self.shader_name.write().expect("shader name lock poisoned") = shader_name;
    }

    pub fn set_total_images(&self, total_images: u64) {
        self.total_images.store(total_images, Ordering::Relaxed);
    }

    pub fn total_images(&self) -> u64 {
        self.total_images.load(Ordering::Relaxed)
    }

    pub fn inc_images_shown(&self) {
        self.images_shown.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_manual_skips(&self) {
        self.manual_skips.fetch_add(1, Ordering::Relaxed);
    }

    pub fn images_shown(&self) -> u64 {
        self.images_shown.load(Ordering::Relaxed)
    }

    pub fn manual_skips(&self) -> u64 {
        self.manual_skips.load(Ordering::Relaxed)
    }

    pub fn set_shader_active(&self, active: bool) {
        self.shader_active.store(active, Ordering::Relaxed);
    }

    pub fn is_shader_active(&self) -> bool {
        self.shader_active.load(Ordering::Relaxed)
    }

    pub fn running_duration(&self) -> Duration {
        self.started_at.elapsed()
    }
}

pub(crate) fn format_running_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(60) {
        return "<1m".to_string();
    }

    let total_minutes = duration.as_secs() / 60;
    let total_hours = total_minutes / 60;
    let minutes = total_minutes % 60;

    if total_hours > 72 {
        let days = total_hours / 24;
        let hours = total_hours % 24;
        return format!("{days}d {hours}h {minutes}m");
    }

    if total_hours == 0 {
        return format!("{minutes}m");
    }

    format!("{total_hours}h {minutes}m")
}

pub(crate) fn format_config_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    if total_seconds == 0 {
        return "0s".to_string();
    }

    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 {
        parts.push(format!("{seconds}s"));
    }

    parts.join(" ")
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{
    open_settings, try_acquire_single_instance, SingleInstanceGuard, TrayController,
};

#[cfg(windows)]
pub async fn spawn(
    config_path: std::path::PathBuf,
    event_tx: tokio::sync::mpsc::UnboundedSender<TrayEvent>,
    session_stats: std::sync::Arc<SessionStats>,
) -> crate::errors::Result<TrayController> {
    windows::spawn(config_path, event_tx, session_stats)
}

#[cfg(target_os = "linux")]
pub use linux::{open_settings, spawn, TrayController};

#[cfg(target_os = "macos")]
pub use macos::{
    open_settings, spawn, try_acquire_single_instance, SingleInstanceGuard, TrayController,
};

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub struct TrayController;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
impl TrayController {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub struct SingleInstanceGuard;

#[cfg(not(any(windows, target_os = "macos")))]
use crate::errors::Result;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
use std::path::PathBuf;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
use tokio::sync::mpsc::UnboundedSender;

#[cfg(not(any(windows, target_os = "macos")))]
pub fn try_acquire_single_instance() -> Result<Option<SingleInstanceGuard>> {
    Ok(Some(SingleInstanceGuard))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub async fn spawn(
    _config_path: PathBuf,
    _event_tx: UnboundedSender<TrayEvent>,
    _session_stats: std::sync::Arc<SessionStats>,
) -> Result<TrayController> {
    Ok(TrayController::new())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn open_settings(_path: &std::path::Path) -> Result<()> {
    anyhow::bail!("opening settings is unsupported on this desktop")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_running_duration_handles_sub_minute() {
        assert_eq!(format_running_duration(Duration::from_secs(0)), "<1m");
        assert_eq!(format_running_duration(Duration::from_secs(59)), "<1m");
    }

    #[test]
    fn format_running_duration_handles_minutes_and_hours() {
        assert_eq!(format_running_duration(Duration::from_secs(60)), "1m");
        assert_eq!(
            format_running_duration(Duration::from_secs(59 * 60 + 59)),
            "59m"
        );
        assert_eq!(
            format_running_duration(Duration::from_secs(60 * 60)),
            "1h 0m"
        );
        assert_eq!(
            format_running_duration(Duration::from_secs((90 * 60) as u64)),
            "1h 30m"
        );
        assert_eq!(
            format_running_duration(Duration::from_secs((72 * 60 * 60) as u64)),
            "72h 0m"
        );
    }

    #[test]
    fn format_running_duration_handles_days_over_72_hours() {
        assert_eq!(
            format_running_duration(Duration::from_secs((72 * 60 * 60 + 60) as u64)),
            "72h 1m"
        );
        assert_eq!(
            format_running_duration(Duration::from_secs((73 * 60 * 60 + 60) as u64)),
            "3d 1h 1m"
        );
        assert_eq!(
            format_running_duration(Duration::from_secs((93 * 60 * 60 + 49 * 60) as u64)),
            "3d 21h 49m"
        );
    }

    #[test]
    fn session_stats_counters_increment() {
        let stats = SessionStats::new(
            "3h".to_string(),
            "2h".to_string(),
            "Idle".to_string(),
            "".to_string(),
        );
        assert_eq!(stats.images_shown(), 0);
        assert_eq!(stats.manual_skips(), 0);
        assert_eq!(stats.total_images(), 0);
        assert!(!stats.is_shader_active());
        assert_eq!(stats.timer_display(), "3h");
        assert_eq!(stats.remote_update_timer_display(), "2h");
        assert_eq!(stats.app_update_status(), "Idle");
        assert_eq!(stats.shader_name(), "");

        stats.set_total_images(42);
        stats.set_timer_display("15m".to_string());
        stats.set_remote_update_timer_display("45m".to_string());
        stats.set_app_update_status("Checking".to_string());
        stats.set_shader_name("gradient_glossy".to_string());
        stats.set_shader_active(true);
        stats.inc_images_shown();
        stats.inc_images_shown();
        stats.inc_manual_skips();

        assert_eq!(stats.total_images(), 42);
        assert_eq!(stats.timer_display(), "15m");
        assert_eq!(stats.remote_update_timer_display(), "45m");
        assert_eq!(stats.app_update_status(), "Checking");
        assert_eq!(stats.shader_name(), "gradient_glossy");
        assert!(stats.is_shader_active());
        assert_eq!(stats.images_shown(), 2);
        assert_eq!(stats.manual_skips(), 1);
    }

    #[test]
    fn format_config_duration_formats_expected_shapes() {
        assert_eq!(format_config_duration(Duration::from_secs(40)), "40s");
        assert_eq!(format_config_duration(Duration::from_secs(12 * 60)), "12m");
        assert_eq!(
            format_config_duration(Duration::from_secs(3 * 60 * 60)),
            "3h"
        );
        assert_eq!(
            format_config_duration(Duration::from_secs(90 * 60)),
            "1h 30m"
        );
        assert_eq!(
            format_config_duration(Duration::from_secs(24 * 60 * 60 + 61)),
            "1d 1m 1s"
        );
    }
}
