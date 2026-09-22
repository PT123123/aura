#![cfg_attr(windows, windows_subsystem = "windows")]

mod cache;
mod config;
mod crash_capture;
mod crash_ui;
mod debug_capture;
mod errors;
mod image_pipeline;
mod installer;
#[cfg(target_os = "linux")]
mod linux_desktop;
mod logging;
#[cfg(target_os = "macos")]
mod macos_app;
#[cfg(any(target_os = "macos", test))]
mod macos_support;
mod renderer;
mod rotation;
mod scheduler;
#[cfg(windows)]
mod settings;
mod sources;
mod state;
mod tray;
mod updater;
mod version;
mod wallpaper;
mod wallpaper_picker;

use crate::cache::CacheManager;
use crate::config::{load_from_path_with_warnings, ConfigWarning, RendererMode, ShaderConfig};
use crate::errors::Result;
use crate::installer::{SquirrelEvent, StartupRegistrationStatus};
use crate::renderer::{RendererEvent, ShaderRenderer};
use crate::rotation::RotationManager;
use crate::scheduler::{Scheduler, SchedulerEvent};
use crate::sources::{build_sources, ImageCandidate, ImageSource, Origin, SourceKind};
use crate::state::{PersistedState, StateStore};
use crate::tray::{format_config_duration, SessionStats, TrayEvent};
use crate::updater::{RestartContext, UpdateTrigger, UpdaterEvent, UpdaterStatus};
use anyhow::Context;
use std::collections::HashSet;
#[cfg(not(target_os = "macos"))]
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveMode {
    Image,
    Shader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReloadRendererAction {
    KeepImageMode,
    KeepCurrentShader,
    StartShader(ShaderConfig),
    ApplyShaderConfig(ShaderConfig),
    StopShader,
}

#[derive(Debug)]
struct CliOptions {
    config_path: PathBuf,
    tray_enabled: bool,
    debug_terminal: bool,
    print_version: bool,
    squirrel_event: Option<SquirrelEvent>,
}

#[cfg(not(target_os = "macos"))]
#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let debug_requested = debug_capture::is_debug_requested(&args);
    let _debug_capture = if debug_requested {
        match debug_capture::DebugCapture::init() {
            Ok(capture) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "debug logging enabled: {}",
                    capture.path().display()
                );
                Some(capture)
            }
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "failed to initialize debug logging: {error:#}"
                );
                None
            }
        }
    } else {
        None
    };
    crash_ui::install_panic_hook(debug_requested);
    if let Err(error) = crash_capture::install() {
        let _ = writeln!(
            std::io::stderr(),
            "failed to initialize native crash capture: {error:#}"
        );
    }

    if let Err(error) = run(args, debug_requested).await {
        let _ = writeln!(std::io::stderr(), "fatal error: {error:#}");
        crash_ui::show_fatal_error_dialog(&format!("{error:#}"));
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos_app::run();
}

async fn run(args: Vec<String>, debug_requested: bool) -> Result<()> {
    write_startup_stage(debug_requested, "parse_cli_options");
    let options = parse_cli_options(&args)?;
    write_startup_stage(debug_requested, "handle_squirrel_event");
    let relaunch_args: Vec<String> = args
        .iter()
        .filter(|arg| SquirrelEvent::from_flag(arg).is_none())
        .cloned()
        .collect();
    let launched_from_squirrel_firstrun = options.squirrel_event == Some(SquirrelEvent::Firstrun);

    if installer::handle_squirrel_event(options.squirrel_event)? {
        return Ok(());
    }

    if !options.debug_terminal {
        ensure_debug_console(&options)?;
    }
    if options.print_version {
        print_version_banner();
        return Ok(());
    }

    let mut _single_instance_guard: Option<tray::SingleInstanceGuard> = None;
    #[cfg(target_os = "macos")]
    {
        _single_instance_guard = match tray::try_acquire_single_instance()? {
            Some(guard) => Some(guard),
            None => {
                let _ = writeln!(
                    std::io::stderr(),
                    "another Aura instance is already running; activating it"
                );
                macos_app::activate_existing_instance();
                return Ok(());
            }
        };
    }

    let config_path = options.config_path.clone();
    write_startup_stage(debug_requested, "ensure_config_exists");
    let created = ensure_config_exists(&config_path)?;

    write_startup_stage(debug_requested, "load_config");
    let loaded_config = load_from_path_with_warnings(&config_path)?;
    let mut config = loaded_config.config;
    write_startup_stage(debug_requested, "init_tracing");
    logging::init(&config.log_level);
    log_config_warnings(&loaded_config.warnings);
    write_startup_stage(debug_requested, "ensure_startup_registered");
    match installer::ensure_startup_registered() {
        Ok(StartupRegistrationStatus::SkippedNotInstalled) => {
            info!("startup registration check skipped: app is not running from a managed package install");
        }
        Ok(StartupRegistrationStatus::AlreadyRegistered) => {
            info!("startup registration already present");
        }
        Ok(StartupRegistrationStatus::RegisteredNow) => {
            info!("startup registration was missing and has been restored");
        }
        Ok(StartupRegistrationStatus::ApprovalRequired) => {
            warn!("login startup requires approval in System Settings");
        }
        Err(error) => {
            warn!(error = %error, "failed to enforce startup registration");
        }
    }
    write_startup_stage(debug_requested, "runtime_started");
    if created {
        info!(path = %config_path.display(), "created default config");
    }
    info!(path = %config_path.display(), "loaded config");

    let mut cache = Arc::new(CacheManager::new(&config)?);
    if let Err(error) = cache.cleanup() {
        warn!(error = %error, "cache cleanup failed");
    }

    let mut sources = build_sources(&config, cache.clone())?;
    let mut state_store = StateStore::new(config.state_file.clone());

    let persisted_state = match state_store.load() {
        Ok(state) => state,
        Err(error) => {
            warn!(error = %error, "failed to load state, starting fresh");
            PersistedState::default()
        }
    };

    let initial_candidates = refresh_all_sources(&mut sources).await?;
    let (mut local_images_count, mut remote_images_count) =
        count_images_by_origin(&initial_candidates);
    let mut rotation = RotationManager::new();
    rotation.rebuild_pool(initial_candidates);
    rotation.restore_state(&persisted_state);
    let mut updater_runtime = updater::initialize(&config.updater, relaunch_args.clone());
    let mut updater_event_rx = updater_runtime.take_event_receiver();
    let mut updater_restart_context = updater_runtime.restart_context();
    let mut updater_operation_in_progress = false;
    let mut restart_pending_on_next_switch = false;
    let mut startup_update_check_deadline = None;
    let mut updater_interval = updater_runtime.check_interval().map(|check_interval| {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + check_interval, check_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    });
    let initial_shader_name = if config.renderer == RendererMode::Shader {
        config
            .shader
            .as_ref()
            .map(|shader| shader.name.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let session_stats = Arc::new(SessionStats::new(
        format_config_duration(config.image.timer),
        format_config_duration(config.image.remote_update_timer),
        updater_runtime.status().label().to_string(),
        initial_shader_name,
    ));
    session_stats.set_total_images(local_images_count + remote_images_count);

    let (tray_event_tx, mut tray_event_rx) = tokio::sync::mpsc::unbounded_channel::<TrayEvent>();
    let mut _tray_controller: Option<tray::TrayController> = None;
    #[cfg(windows)]
    if options.tray_enabled {
        _single_instance_guard = match tray::try_acquire_single_instance()? {
            Some(guard) => Some(guard),
            None => {
                info!("another tray-enabled aura instance is already running, exiting");
                return Ok(());
            }
        };

        _tray_controller = Some(
            tray::spawn(
                config_path.clone(),
                tray_event_tx.clone(),
                session_stats.clone(),
            )
            .await?,
        );
        info!("tray mode enabled");
    }

    #[cfg(target_os = "macos")]
    {
        if options.tray_enabled {
            _tray_controller = Some(
                tray::spawn(
                    config_path.clone(),
                    tray_event_tx.clone(),
                    session_stats.clone(),
                )
                .await?,
            );
            info!("macOS menu bar mode enabled");
        }
    }

    #[cfg(target_os = "linux")]
    {
        let session = linux_desktop::initialize(
            config_path.clone(),
            options.tray_enabled,
            tray_event_tx.clone(),
            session_stats.clone(),
        )
        .await?;
        if options.tray_enabled && session.desktop == linux_desktop::DesktopKind::Plasma {
            _tray_controller = Some(
                tray::spawn(
                    config_path.clone(),
                    tray_event_tx.clone(),
                    session_stats.clone(),
                )
                .await?,
            );
            info!("Plasma StatusNotifierItem tray enabled");
        }
    }

    let backend = wallpaper::default_backend();
    let mut last_image_id = persisted_state.last_image_id.clone();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let mut initial_image_applied = false;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let initial_image_applied = false;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if config.renderer == RendererMode::Shader {
        match try_switch_once(&mut rotation, cache.as_ref(), &*backend, &config).await? {
            Some(next_id) => {
                session_stats.inc_images_shown();
                last_image_id = Some(next_id);
                initial_image_applied = true;
                persist_state(&state_store, &rotation, last_image_id.clone())?;
                info!("prepared a persistent image fallback beneath the live shader");
            }
            None => {
                #[cfg(target_os = "macos")]
                return Err(anyhow::anyhow!(
                    "macOS shader mode requires at least one image for its crash-safe fallback"
                ));
                #[cfg(target_os = "linux")]
                warn!("no persistent image fallback was available beneath the Linux shader");
            }
        }
    }

    let mut renderer: Option<ShaderRenderer> = None;
    let mut renderer_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<RendererEvent>> = None;
    let mut active_mode = ActiveMode::Image;
    if config.renderer == RendererMode::Shader {
        if let Some(shader_config) = config.shader.clone() {
            let shader_name = shader_config.name.clone();
            match ShaderRenderer::start(shader_config) {
                Ok(mut shader_renderer) => {
                    renderer_event_rx = shader_renderer.take_event_receiver();
                    renderer = Some(shader_renderer);
                    active_mode = ActiveMode::Shader;
                    session_stats.set_shader_name(shader_name);
                    info!("shader renderer started");
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        "shader renderer startup failed, falling back to image mode"
                    );
                }
            }
        } else {
            warn!("renderer is set to shader but shader config is missing, using image mode");
        }
    }
    session_stats.set_shader_active(active_mode == ActiveMode::Shader);
    if active_mode != ActiveMode::Shader {
        session_stats.set_shader_name(String::new());
    }

    if active_mode == ActiveMode::Image && !initial_image_applied {
        if let Some(next_id) =
            try_switch_once(&mut rotation, cache.as_ref(), &*backend, &config).await?
        {
            session_stats.inc_images_shown();
            last_image_id = Some(next_id);
            persist_state(&state_store, &rotation, last_image_id.clone())?;
        }
    }

    let mut scheduler = Scheduler::new(config.image.timer, config.image.remote_update_timer);
    if updater_runtime.status() == UpdaterStatus::Idle {
        if launched_from_squirrel_firstrun {
            startup_update_check_deadline =
                Some(tokio::time::Instant::now() + Duration::from_secs(15));
            info!("delaying startup app update check because this is squirrel first run");
        } else if request_update_check(
            &updater_runtime,
            &mut updater_operation_in_progress,
            UpdateTrigger::Startup,
        ) {
            info!("startup app update check requested");
        }
    }
    info!("aura is running");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, stopping aura");
                stop_renderer(&mut renderer, "ctrl-c shutdown").await;
                persist_state(&state_store, &rotation, last_image_id.clone())?;
                break;
            }
            _ = async {
                if let Some(deadline) = startup_update_check_deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if startup_update_check_deadline.is_some() => {
                startup_update_check_deadline = None;
                if request_update_check(
                    &updater_runtime,
                    &mut updater_operation_in_progress,
                    UpdateTrigger::Startup,
                ) {
                    info!("delayed startup app update check requested");
                }
            }
            _ = async {
                if let Some(interval) = updater_interval.as_mut() {
                    interval.tick().await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if updater_interval.is_some() => {
                let _ = request_update_check(
                    &updater_runtime,
                    &mut updater_operation_in_progress,
                    UpdateTrigger::Periodic,
                );
            }
            updater_event = async {
                if let Some(receiver) = updater_event_rx.as_mut() {
                    receiver.recv().await
                } else {
                    std::future::pending::<Option<UpdaterEvent>>().await
                }
            }, if updater_event_rx.is_some() => {
                match updater_event {
                    Some(UpdaterEvent::Status(status)) => {
                        session_stats.set_app_update_status(status.label().to_string());
                        updater_operation_in_progress = updater_status_in_progress(status);
                    }
                    Some(UpdaterEvent::InstallReady) => {
                        restart_pending_on_next_switch = true;
                        if active_mode == ActiveMode::Shader
                            && restart_after_update(
                                updater_restart_context.as_ref(),
                                &state_store,
                                &rotation,
                                last_image_id.clone(),
                                &mut _single_instance_guard,
                            )
                        {
                            stop_renderer(
                                &mut renderer,
                                "update restart while shader mode is active",
                            )
                            .await;
                            break;
                        }
                    }
                    None => {
                        updater_event_rx = None;
                        updater_operation_in_progress = false;
                        session_stats.set_app_update_status(UpdaterStatus::Unsupported.label().to_string());
                    }
                }
            }
            renderer_event = async {
                if let Some(receiver) = renderer_event_rx.as_mut() {
                    receiver.recv().await
                } else {
                    std::future::pending::<Option<RendererEvent>>().await
                }
            } => {
                if let Some(renderer_event) = renderer_event {
                    match renderer_event {
                        RendererEvent::Ready => info!("shader renderer ready"),
                        RendererEvent::Running => info!("shader renderer running"),
                        RendererEvent::Stopped => info!("shader renderer stopped"),
                        RendererEvent::Fatal { message } => {
                            warn!(error = %message, "shader renderer failed, switching to image mode");
                            stop_renderer(&mut renderer, "shader fatal fallback").await;
                            renderer = None;
                            renderer_event_rx = None;
                            active_mode = ActiveMode::Image;
                            session_stats.set_shader_active(false);
                            session_stats.set_shader_name(String::new());
                            match try_switch_once(&mut rotation, cache.as_ref(), &*backend, &config).await {
                                Ok(Some(next_id)) => {
                                    session_stats.inc_images_shown();
                                    last_image_id = Some(next_id);
                                    if let Err(error) = persist_state(&state_store, &rotation, last_image_id.clone()) {
                                        warn!(error = %error, "failed to persist state after shader fallback");
                                    }
                                }
                                Ok(None) => {
                                    #[cfg(target_os = "linux")]
                                    return Err(anyhow::anyhow!(
                                        "Linux shell renderer failed and no image was available for desktop fallback"
                                    ));
                                    #[cfg(not(target_os = "linux"))]
                                    warn!("shader fallback requested image mode but no image was available");
                                }
                                Err(error) => {
                                    return Err(error).context(
                                        "shader renderer failed and the Linux/Windows image fallback could not be applied",
                                    );
                                }
                            }
                        }
                    }
                }
            }
            tray_event = tray_event_rx.recv() => {
                match tray_event {
                    Some(TrayEvent::NextWallpaper) => {
                        if active_mode != ActiveMode::Image {
                            warn!("Next Background ignored while shader mode is active");
                            continue;
                        }
                        match refresh_local_sources(&mut sources).await {
                            Ok(updated) => {
                                let (next_local_count, _) = count_images_by_origin(&updated);
                                local_images_count = next_local_count;
                                session_stats
                                    .set_total_images(local_images_count + remote_images_count);
                                let merged =
                                    merge_with_existing_remote_candidates(&rotation, updated);
                                rotation.rebuild_pool(merged);
                                info!(pool_size = rotation.pool_size(), "local refresh complete before tray switch");
                            }
                            Err(error) => warn!(error = %error, "local refresh failed before tray switch"),
                        }
                        match try_switch_once(&mut rotation, cache.as_ref(), &*backend, &config).await {
                            Ok(Some(next_id)) => {
                                session_stats.inc_images_shown();
                                session_stats.inc_manual_skips();
                                last_image_id = Some(next_id);
                                if let Err(error) = persist_state(&state_store, &rotation, last_image_id.clone()) {
                                    warn!(error = %error, "failed to persist state after tray wallpaper switch");
                                }
                            }
                            Ok(None) => warn!("tray requested switch but no image available"),
                            Err(error) => warn!(error = %error, "tray-requested wallpaper switch failed"),
                        }
                        if restart_pending_on_next_switch
                            && restart_after_update(
                                updater_restart_context.as_ref(),
                                &state_store,
                                &rotation,
                                last_image_id.clone(),
                                &mut _single_instance_guard,
                            )
                        {
                            stop_renderer(&mut renderer, "restart after tray wallpaper switch").await;
                            break;
                        }
                    }
                    Some(TrayEvent::ReloadSettings) => {
                        info!("tray requested settings reload");

                        let loaded_config = match load_from_path_with_warnings(&config_path) {
                            Ok(loaded_config) => loaded_config,
                            Err(error) => {
                                warn!(error = %error, "failed to reload config; keeping current runtime settings");
                                continue;
                            }
                        };
                        log_config_warnings(&loaded_config.warnings);
                        let new_config = loaded_config.config;

                        let new_cache = match CacheManager::new(&new_config) {
                            Ok(new_cache) => Arc::new(new_cache),
                            Err(error) => {
                                warn!(error = %error, "failed to initialize cache from reloaded config; keeping current runtime settings");
                                continue;
                            }
                        };
                        if let Err(error) = new_cache.cleanup() {
                            warn!(error = %error, "cache cleanup failed after settings reload");
                        }

                        let mut new_sources = match build_sources(&new_config, new_cache.clone()) {
                            Ok(new_sources) => new_sources,
                            Err(error) => {
                                warn!(error = %error, "failed to build sources from reloaded config; keeping current runtime settings");
                                continue;
                            }
                        };

                        let refreshed_candidates = match refresh_all_sources(&mut new_sources).await {
                            Ok(candidates) => candidates,
                            Err(error) => {
                                warn!(error = %error, "failed to refresh sources after settings reload; keeping current runtime settings");
                                continue;
                            }
                        };

                        let (next_local_count, next_remote_count) =
                            count_images_by_origin(&refreshed_candidates);
                        let mut preserved_state = rotation.export_state();
                        preserved_state.last_image_id = last_image_id.clone();
                        rotation.rebuild_pool(refreshed_candidates);
                        rotation.restore_state(&preserved_state);

                        let current_shader_config = config.shader.clone();
                        let renderer_action = determine_reload_renderer_action(
                            active_mode,
                            renderer.is_some(),
                            current_shader_config.as_ref(),
                            &new_config,
                        );

                        config = new_config;
                        cache = new_cache;
                        sources = new_sources;
                        state_store = StateStore::new(config.state_file.clone());
                        scheduler =
                            Scheduler::new(config.image.timer, config.image.remote_update_timer);
                        local_images_count = next_local_count;
                        remote_images_count = next_remote_count;
                        session_stats.set_total_images(local_images_count + remote_images_count);
                        session_stats.set_timer_display(format_config_duration(config.image.timer));
                        session_stats.set_remote_update_timer_display(format_config_duration(
                            config.image.remote_update_timer,
                        ));
                        logging::set_level(&config.log_level);

                        updater_runtime = updater::initialize(&config.updater, relaunch_args.clone());
                        updater_event_rx = updater_runtime.take_event_receiver();
                        updater_restart_context = updater_runtime.restart_context();
                        updater_operation_in_progress = false;
                        startup_update_check_deadline = None;
                        updater_interval = updater_runtime.check_interval().map(|check_interval| {
                            let mut interval = tokio::time::interval_at(
                                tokio::time::Instant::now() + check_interval,
                                check_interval,
                            );
                            interval
                                .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            interval
                        });
                        session_stats
                            .set_app_update_status(updater_runtime.status().label().to_string());
                        if updater_runtime.status() == UpdaterStatus::Idle {
                            let _ = request_update_check(
                                &updater_runtime,
                                &mut updater_operation_in_progress,
                                UpdateTrigger::Startup,
                            );
                        }

                        match renderer_action {
                            ReloadRendererAction::KeepImageMode => {
                                session_stats.set_shader_active(false);
                                session_stats.set_shader_name(String::new());
                            }
                            ReloadRendererAction::KeepCurrentShader => {
                                if config.shader.is_none() {
                                    config.shader = current_shader_config.clone();
                                    warn!("reloaded config omitted shader settings while shader mode is active; keeping current shader runtime");
                                }
                                session_stats.set_shader_active(true);
                                session_stats.set_shader_name(
                                    config
                                        .shader
                                        .as_ref()
                                        .map(|shader| shader.name.clone())
                                        .unwrap_or_default(),
                                );
                                info!("settings reload left live shader configuration unchanged");
                            }
                            ReloadRendererAction::StopShader => {
                                stop_renderer(&mut renderer, "settings reload switched runtime to image mode").await;
                                renderer = None;
                                renderer_event_rx = None;
                                active_mode = ActiveMode::Image;
                                session_stats.set_shader_active(false);
                                session_stats.set_shader_name(String::new());
                                info!("settings reload switched runtime to image mode");
                            }
                            ReloadRendererAction::StartShader(shader_config) => {
                                stop_renderer(&mut renderer, "settings reload restarting shader mode").await;
                                renderer = None;
                                renderer_event_rx = None;
                                active_mode = ActiveMode::Image;
                                session_stats.set_shader_active(false);
                                session_stats.set_shader_name(String::new());

                                let shader_name = shader_config.name.clone();
                                match ShaderRenderer::start(shader_config) {
                                    Ok(mut new_renderer) => {
                                        renderer_event_rx = new_renderer.take_event_receiver();
                                        renderer = Some(new_renderer);
                                        active_mode = ActiveMode::Shader;
                                        session_stats.set_shader_active(true);
                                        session_stats.set_shader_name(shader_name);
                                        info!("settings reload switched runtime to shader mode");
                                    }
                                    Err(error) => {
                                        warn!(error = %error, "failed to start shader mode from reloaded settings");
                                    }
                                }
                            }
                            ReloadRendererAction::ApplyShaderConfig(shader_config) => {
                                let shader_name = shader_config.name.clone();
                                match renderer.as_ref() {
                                    Some(renderer) => match renderer.apply_config(shader_config).await {
                                        Ok(()) => {
                                            active_mode = ActiveMode::Shader;
                                            session_stats.set_shader_active(true);
                                            session_stats.set_shader_name(shader_name);
                                            info!("settings reload updated live shader configuration");
                                        }
                                        Err(error) => {
                                            config.shader = current_shader_config.clone();
                                            session_stats.set_shader_active(true);
                                            session_stats.set_shader_name(
                                                current_shader_config
                                                    .as_ref()
                                                    .map(|shader| shader.name.clone())
                                                    .unwrap_or_default(),
                                            );
                                            warn!(error = %error, "failed to apply reloaded shader settings; keeping current shader runtime");
                                        }
                                    },
                                    None => {
                                        warn!("shader reload requested a live config update but no renderer was active; attempting a fresh shader start");
                                        if let Some(shader_config) = config.shader.clone() {
                                            let shader_name = shader_config.name.clone();
                                            match ShaderRenderer::start(shader_config) {
                                                Ok(mut new_renderer) => {
                                                    renderer_event_rx = new_renderer.take_event_receiver();
                                                    renderer = Some(new_renderer);
                                                    active_mode = ActiveMode::Shader;
                                                    session_stats.set_shader_active(true);
                                                    session_stats.set_shader_name(shader_name);
                                                    info!("settings reload recovered shader mode with a fresh renderer start");
                                                }
                                                Err(error) => {
                                                    active_mode = ActiveMode::Image;
                                                    session_stats.set_shader_active(false);
                                                    session_stats.set_shader_name(String::new());
                                                    warn!(error = %error, "failed to recover shader mode after missing renderer during settings reload");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if active_mode == ActiveMode::Image {
                            match try_switch_once(&mut rotation, cache.as_ref(), &*backend, &config).await {
                                Ok(Some(next_id)) => {
                                    session_stats.inc_images_shown();
                                    last_image_id = Some(next_id);
                                }
                                Ok(None) => warn!("settings reload kept image mode but no image was available"),
                                Err(error) => warn!(error = %error, "failed to apply wallpaper after settings reload"),
                            }
                        }

                        if restart_pending_on_next_switch
                            && active_mode == ActiveMode::Shader
                            && restart_after_update(
                                updater_restart_context.as_ref(),
                                &state_store,
                                &rotation,
                                last_image_id.clone(),
                                &mut _single_instance_guard,
                            )
                        {
                            stop_renderer(&mut renderer, "restart after settings reload").await;
                            break;
                        }

                        if let Err(error) =
                            persist_state(&state_store, &rotation, last_image_id.clone())
                        {
                            warn!(error = %error, "failed to persist state after settings reload");
                        }
                        info!("settings reload complete");
                    }
                    Some(TrayEvent::CheckForUpdates) => {
                        let _ = request_update_check(
                            &updater_runtime,
                            &mut updater_operation_in_progress,
                            UpdateTrigger::Manual,
                        );
                    }
                    Some(TrayEvent::OpenSettings) => {
                        #[cfg(windows)]
                        crate::settings::open_settings_window(config_path.clone(), tray_event_tx.clone());
                        #[cfg(not(windows))]
                        if let Err(error) = tray::open_settings(&config_path) {
                            warn!(error = %error, path = %config_path.display(), "failed to open settings");
                        }
                    }
                    Some(TrayEvent::OpenWallpaperPicker) => {
                        #[cfg(windows)]
                        {
                            let remote_images = match cache.list_remote_images() {
                                Ok(images) => images,
                                Err(error) => {
                                    warn!(error = %error, "failed to list remote cache images");
                                    Vec::new()
                                }
                            };
                            let local_images =
                                wallpaper_picker::collect_local_images(&config.image.sources);
                            crate::wallpaper_picker::open_wallpaper_picker(
                                remote_images,
                                local_images,
                                tray_event_tx.clone(),
                            );
                        }
                        #[cfg(not(windows))]
                        warn!("wallpaper picker is only supported on Windows");
                    }
                    Some(TrayEvent::ApplyWallpaper(path)) => {
                        if active_mode != ActiveMode::Image {
                            warn!("picked wallpaper ignored while shader mode is active");
                            continue;
                        }
                        if !path.is_file() {
                            warn!(path = %path.display(), "picked wallpaper no longer exists");
                            continue;
                        }
                        match backend.set_wallpapers(&[path.as_path()]) {
                            Ok(()) => {
                                info!(path = %path.display(), "picked wallpaper applied to all displays");
                                session_stats.inc_images_shown();
                                last_image_id = Some(path.to_string_lossy().into_owned());
                                if let Err(error) =
                                    persist_state(&state_store, &rotation, last_image_id.clone())
                                {
                                    warn!(error = %error, "failed to persist state after picked wallpaper");
                                }
                            }
                            Err(error) => warn!(
                                error = %error,
                                path = %path.display(),
                                "failed to apply picked wallpaper"
                            ),
                        }
                    }
                    Some(TrayEvent::Exit) => {
                        info!("tray requested exit, stopping aura");
                        stop_renderer(&mut renderer, "tray exit").await;
                        persist_state(&state_store, &rotation, last_image_id.clone())?;
                        break;
                    }
                    None => {
                        info!("tray event channel closed, stopping aura");
                        stop_renderer(&mut renderer, "tray event channel closed").await;
                        persist_state(&state_store, &rotation, last_image_id.clone())?;
                        break;
                    }
                }
            }
            event = scheduler.next_event() => {
                match event {
                    SchedulerEvent::SwitchImage => {
                        if active_mode != ActiveMode::Image {
                            continue;
                        }
                        match refresh_local_sources(&mut sources).await {
                            Ok(updated) => {
                                let (next_local_count, _) = count_images_by_origin(&updated);
                                local_images_count = next_local_count;
                                session_stats
                                    .set_total_images(local_images_count + remote_images_count);
                                let merged =
                                    merge_with_existing_remote_candidates(&rotation, updated);
                                rotation.rebuild_pool(merged);
                                info!(pool_size = rotation.pool_size(), "local refresh complete before timer switch");
                            }
                            Err(error) => warn!(error = %error, "local refresh failed before timer switch"),
                        }
                        match try_switch_once(&mut rotation, cache.as_ref(), &*backend, &config).await {
                            Ok(Some(next_id)) => {
                                session_stats.inc_images_shown();
                                last_image_id = Some(next_id);
                                if let Err(error) = persist_state(&state_store, &rotation, last_image_id.clone()) {
                                    warn!(error = %error, "failed to persist state after wallpaper switch");
                                }
                            }
                            Ok(None) => {
                                warn!("no image available for switch");
                            }
                            Err(error) => {
                                warn!(error = %error, "failed to switch wallpaper");
                            }
                        }
                        if restart_pending_on_next_switch
                            && restart_after_update(
                                updater_restart_context.as_ref(),
                                &state_store,
                                &rotation,
                                last_image_id.clone(),
                                &mut _single_instance_guard,
                            )
                        {
                            stop_renderer(&mut renderer, "restart after scheduled wallpaper switch").await;
                            break;
                        }
                    }
                    SchedulerEvent::RefreshRemote => {
                        match refresh_all_sources(&mut sources).await {
                            Ok(updated) => {
                                let (next_local_count, next_remote_count) =
                                    count_images_by_origin(&updated);
                                local_images_count = next_local_count;
                                remote_images_count = next_remote_count;
                                session_stats
                                    .set_total_images(local_images_count + remote_images_count);
                                rotation.rebuild_pool(updated);
                                info!(pool_size = rotation.pool_size(), "full refresh complete");
                            }
                            Err(error) => warn!(error = %error, "source refresh failed"),
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn stop_renderer(renderer: &mut Option<ShaderRenderer>, action: &str) {
    if let Some(renderer) = renderer.as_mut() {
        if let Err(error) = renderer.stop_async().await {
            warn!(error = %error, action, "failed to stop shader renderer");
        }
    }
}

fn determine_reload_renderer_action(
    active_mode: ActiveMode,
    has_renderer: bool,
    current_shader_config: Option<&ShaderConfig>,
    new_config: &config::AuraConfig,
) -> ReloadRendererAction {
    match new_config.renderer {
        RendererMode::Image => {
            if active_mode == ActiveMode::Shader && has_renderer {
                ReloadRendererAction::StopShader
            } else {
                ReloadRendererAction::KeepImageMode
            }
        }
        RendererMode::Shader => {
            let Some(shader_config) = new_config.shader.clone() else {
                return if active_mode == ActiveMode::Shader && has_renderer {
                    ReloadRendererAction::KeepCurrentShader
                } else {
                    ReloadRendererAction::KeepImageMode
                };
            };

            if active_mode == ActiveMode::Shader && has_renderer {
                if current_shader_config == Some(&shader_config) {
                    ReloadRendererAction::KeepCurrentShader
                } else {
                    ReloadRendererAction::ApplyShaderConfig(shader_config)
                }
            } else {
                ReloadRendererAction::StartShader(shader_config)
            }
        }
    }
}

fn log_config_warnings(warnings: &[ConfigWarning]) {
    for warning in warnings {
        if let Some(raw_value) = warning.raw_value.as_deref() {
            warn!(
                config_key = %warning.key_path,
                issue = %warning.issue,
                fallback = %warning.fallback,
                raw_value = %raw_value,
                "config warning"
            );
        } else {
            warn!(
                config_key = %warning.key_path,
                issue = %warning.issue,
                fallback = %warning.fallback,
                "config warning"
            );
        }
    }
}

fn write_startup_stage(debug_requested: bool, stage: &str) {
    if !debug_requested {
        return;
    }
    let _ = writeln!(std::io::stderr(), "stage: {stage}");
    let _ = std::io::stderr().flush();
}

fn request_update_check(
    updater_runtime: &updater::UpdaterRuntime,
    updater_operation_in_progress: &mut bool,
    trigger: UpdateTrigger,
) -> bool {
    if *updater_operation_in_progress {
        return false;
    }
    if updater_runtime.request_check(trigger) {
        *updater_operation_in_progress = true;
        return true;
    }
    false
}

fn updater_status_in_progress(status: UpdaterStatus) -> bool {
    matches!(
        status,
        UpdaterStatus::Checking
            | UpdaterStatus::UpdateAvailable
            | UpdaterStatus::Installing
            | UpdaterStatus::InstalledPendingRestart
    )
}

fn restart_after_update(
    restart_context: Option<&RestartContext>,
    state_store: &StateStore,
    rotation: &RotationManager,
    last_image_id: Option<String>,
    single_instance_guard: &mut Option<tray::SingleInstanceGuard>,
) -> bool {
    let Some(restart_context) = restart_context else {
        warn!("unable to restart after update: restart context is unavailable");
        return false;
    };
    if let Err(error) = persist_state(state_store, rotation, last_image_id) {
        warn!(error = %error, "failed to persist state before update restart");
        return false;
    }

    let released_tray_guard = single_instance_guard.take();
    if released_tray_guard.is_some() {
        info!("released tray single-instance guard before relaunch");
    }

    if let Err(error) = updater::restart_installed_app(restart_context) {
        warn!(error = %error, "failed to relaunch app after update install");
        if released_tray_guard.is_some() {
            match tray::try_acquire_single_instance() {
                Ok(Some(guard)) => {
                    *single_instance_guard = Some(guard);
                    warn!("reacquired tray single-instance guard after relaunch failure");
                }
                Ok(None) => {
                    warn!("failed to reacquire tray single-instance guard after relaunch failure: guard is already held");
                }
                Err(error) => {
                    warn!(error = %error, "failed to reacquire tray single-instance guard after relaunch failure");
                }
            }
        }
        return false;
    }

    info!("relaunch command succeeded; exiting current process for update handoff");
    true
}

fn print_version_banner() {
    let info = version::get_version();
    println!("{}", info.full_version_number(true));
}

#[cfg(windows)]
fn ensure_debug_console(options: &CliOptions) -> Result<()> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Console::AllocConsole;

    if !options.debug_terminal && !options.print_version {
        return Ok(());
    }

    if unsafe { AllocConsole() } != 0 {
        return Ok(());
    }

    let alloc_error = unsafe { GetLastError() };
    if alloc_error == ERROR_ACCESS_DENIED {
        return Ok(());
    }

    anyhow::bail!("failed to initialize debug console (alloc_error={alloc_error})");
}

#[cfg(not(windows))]
fn ensure_debug_console(_: &CliOptions) -> Result<()> {
    Ok(())
}

async fn refresh_all_sources(sources: &mut [Box<dyn ImageSource>]) -> Result<Vec<ImageCandidate>> {
    refresh_sources_filtered(sources, |_| true, "full").await
}

async fn refresh_local_sources(
    sources: &mut [Box<dyn ImageSource>],
) -> Result<Vec<ImageCandidate>> {
    refresh_sources_filtered(
        sources,
        |kind| matches!(kind, SourceKind::File | SourceKind::Directory),
        "local",
    )
    .await
}

async fn refresh_sources_filtered<F>(
    sources: &mut [Box<dyn ImageSource>],
    mut should_refresh: F,
    scope: &str,
) -> Result<Vec<ImageCandidate>>
where
    F: FnMut(SourceKind) -> bool,
{
    let mut candidates = Vec::new();
    let mut unique = HashSet::new();

    for source in sources.iter_mut() {
        if !should_refresh(source.kind()) {
            continue;
        }

        match source.refresh().await {
            Ok(items) => {
                info!(
                    scope = scope,
                    source = source.name(),
                    count = items.len(),
                    "source refresh"
                );
                for item in items {
                    if unique.insert(item.id.clone()) {
                        candidates.push(item);
                    }
                }
            }
            Err(error) => {
                warn!(source = source.name(), error = %error, "source refresh failed");
            }
        }
    }

    candidates.sort_by(|a, b| {
        let a_key = a
            .mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let b_key = b
            .mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        b_key
            .cmp(&a_key)
            .then_with(|| a.sort_key().cmp(b.sort_key()))
    });

    info!(scope = scope, count = candidates.len(), "merged candidates");
    Ok(candidates)
}

async fn try_switch_once(
    rotation: &mut RotationManager,
    cache: &CacheManager,
    backend: &dyn wallpaper::WallpaperBackend,
    config: &config::AuraConfig,
) -> Result<Option<String>> {
    if rotation.pool_size() == 0 {
        return Ok(None);
    }

    // Query the display topology so each display can receive its own image.
    let display_count = match backend.monitor_count() {
        Ok(count) if count > 0 => count,
        Ok(_) => 1,
        Err(error) => {
            warn!(error = %error, "failed to query monitor count, assuming a single display");
            1
        }
    };

    let mut applied: Vec<PathBuf> = Vec::with_capacity(display_count);
    let mut first_id: Option<String> = None;
    let mut attempts = 0;
    let max_attempts = rotation.pool_size();

    while applied.len() < display_count && attempts < max_attempts {
        attempts += 1;
        let candidate = match rotation.next() {
            Some(candidate) => candidate,
            None => break,
        };
        let source_input = candidate.display_source();

        let resolved = match candidate.resolve_local_path().await {
            Ok(Some(path)) => path,
            Ok(None) if matches!(candidate.origin, Origin::Rss) => {
                warn!(id = %candidate.id, input = %source_input, "skipping RSS image with no downloadable content");
                continue;
            }
            Ok(None) => {
                warn!(id = %candidate.id, input = %source_input, "skipping image candidate with no local path");
                continue;
            }
            Err(error) if matches!(candidate.origin, Origin::Rss) => {
                warn!(
                    id = %candidate.id,
                    input = %source_input,
                    error = %error,
                    "failed to resolve RSS image, skipping candidate"
                );
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to resolve {}", source_input));
            }
        };

        let processed = image_pipeline::prepare_for_output(
            &resolved,
            cache,
            config.image.format,
            config.image.jpeg_quality,
        )
        .with_context(|| format!("failed to process {}", resolved.display()))?;

        if first_id.is_none() {
            first_id = Some(candidate.id.clone());
        }
        applied.push(processed);
    }

    if applied.is_empty() {
        return Ok(None);
    }

    let refs: Vec<&Path> = applied.iter().map(PathBuf::as_path).collect();
    backend
        .set_wallpapers(&refs)
        .with_context(|| format!("failed to set wallpapers across {} display(s)", refs.len()))?;

    prefetch_next_candidate(rotation);

    info!(
        id = %first_id.as_deref().unwrap_or(""),
        displays = refs.len(),
        "wallpaper updated across displays"
    );

    Ok(first_id)
}

fn prefetch_next_candidate(rotation: &mut RotationManager) {
    let Some(next_candidate) = rotation.peek_next() else {
        return;
    };
    if !next_candidate.is_prefetchable() {
        return;
    }

    tokio::spawn(async move {
        let source_input = next_candidate.display_source();
        if let Err(error) = next_candidate.prefetch().await {
            warn!(
                input = %source_input,
                error = %error,
                "failed to prefetch RSS image"
            );
        }
    });
}

fn count_images_by_origin(candidates: &[ImageCandidate]) -> (u64, u64) {
    let mut local_images_count = 0_u64;
    let mut remote_images_count = 0_u64;

    for candidate in candidates {
        match candidate.origin {
            Origin::File | Origin::Directory => local_images_count += 1,
            Origin::Rss => remote_images_count += 1,
        }
    }

    (local_images_count, remote_images_count)
}

fn merge_with_existing_remote_candidates(
    rotation: &RotationManager,
    mut local_candidates: Vec<ImageCandidate>,
) -> Vec<ImageCandidate> {
    let mut seen: HashSet<String> = local_candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect();
    for candidate in rotation.candidates() {
        if matches!(candidate.origin, Origin::Rss) && seen.insert(candidate.id.clone()) {
            local_candidates.push(candidate);
        }
    }
    local_candidates
}

fn persist_state(
    state_store: &StateStore,
    rotation: &RotationManager,
    last_image_id: Option<String>,
) -> Result<()> {
    let mut persisted = rotation.export_state();
    persisted.last_image_id = last_image_id;
    state_store.save(&persisted)?;
    Ok(())
}

fn parse_cli_options(args: &[String]) -> Result<CliOptions> {
    let mut tray_enabled = true;
    let mut debug_terminal = false;
    let mut print_version = false;
    let mut squirrel_event = None;
    let mut config_arg: Option<String> = None;

    for arg in args {
        if arg == "--no-tray" {
            tray_enabled = false;
            continue;
        }
        if arg == "--debug" {
            debug_terminal = true;
            continue;
        }
        if arg == "--version" {
            print_version = true;
            continue;
        }
        if let Some(event) = SquirrelEvent::from_flag(arg) {
            if squirrel_event.is_some() {
                anyhow::bail!("only one squirrel lifecycle flag is supported");
            }
            squirrel_event = Some(event);
            continue;
        }

        if arg.starts_with('-') {
            anyhow::bail!("unknown flag: {arg}");
        }

        if config_arg.is_some() {
            anyhow::bail!("only one config path positional argument is supported");
        }
        config_arg = Some(arg.clone());
    }

    let config_path = if print_version {
        match config_arg {
            Some(config_arg) => expand_tilde(&config_arg)?,
            None => PathBuf::new(),
        }
    } else if let Some(config_arg) = config_arg {
        expand_tilde(&config_arg)?
    } else {
        default_user_config_path()?
    };

    Ok(CliOptions {
        config_path,
        tray_enabled,
        debug_terminal,
        print_version,
        squirrel_event,
    })
}

fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        let home = dirs::home_dir().context("failed to resolve home directory")?;
        if path == "~" {
            return Ok(home);
        }
        let suffix = &path[2..];
        return Ok(home.join(suffix));
    }
    Ok(PathBuf::from(path))
}

fn default_user_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(home.join(".config").join("aura.hcl"))
}

fn default_pictures_dir() -> Result<PathBuf> {
    if let Some(path) = dirs::picture_dir() {
        return Ok(path);
    }
    let home = dirs::home_dir().context("failed to resolve home directory for Pictures path")?;
    Ok(home.join("Pictures"))
}

fn ensure_config_exists(config_path: &Path) -> Result<bool> {
    let pictures = default_pictures_dir()?;
    ensure_config_exists_with_pictures(config_path, &pictures)
}

fn ensure_config_exists_with_pictures(config_path: &Path, pictures_dir: &Path) -> Result<bool> {
    if config_path.exists() {
        return Ok(false);
    }

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    fs::create_dir_all(pictures_dir).with_context(|| {
        format!(
            "failed to create pictures directory {}",
            pictures_dir.display()
        )
    })?;

    let payload = config::default_hcl(pictures_dir);
    let tmp_path = config_path.with_extension("tmp");
    fs::write(&tmp_path, payload)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, config_path)
        .with_context(|| format!("failed to create config {}", config_path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheManager;
    use crate::config::{
        AuraConfig, ImageConfig, OutputFormat, RendererMode, ShaderColorSpace, ShaderConfig,
        ShaderDesktopScope, UpdaterConfig,
    };
    use crate::sources::rss::test_support::{ResponseSpec, TestServer};
    use crate::wallpaper::WallpaperBackend;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn reload_keeps_current_shader_when_shader_config_is_unchanged() {
        let tmp = tempdir().unwrap();
        let shader_config = test_shader_config("gradient_glossy");
        let mut config = test_config(tmp.path());
        config.renderer = RendererMode::Shader;
        config.shader = Some(shader_config.clone());

        let action = determine_reload_renderer_action(
            ActiveMode::Shader,
            true,
            Some(&shader_config),
            &config,
        );

        assert_eq!(action, ReloadRendererAction::KeepCurrentShader);
    }

    #[test]
    fn reload_applies_shader_config_when_shader_settings_change() {
        let tmp = tempdir().unwrap();
        let current_shader = test_shader_config("gradient_glossy");
        let mut config = test_config(tmp.path());
        config.renderer = RendererMode::Shader;
        config.shader = Some(ShaderConfig {
            name: "silk".to_string(),
            ..current_shader.clone()
        });

        let action = determine_reload_renderer_action(
            ActiveMode::Shader,
            true,
            Some(&current_shader),
            &config,
        );

        assert_eq!(
            action,
            ReloadRendererAction::ApplyShaderConfig(config.shader.clone().unwrap())
        );
    }

    #[test]
    fn reload_starts_shader_when_switching_from_image_mode() {
        let tmp = tempdir().unwrap();
        let shader_config = test_shader_config("gradient_glossy");
        let mut config = test_config(tmp.path());
        config.renderer = RendererMode::Shader;
        config.shader = Some(shader_config.clone());

        let action = determine_reload_renderer_action(ActiveMode::Image, false, None, &config);

        assert_eq!(action, ReloadRendererAction::StartShader(shader_config));
    }

    #[test]
    fn reload_stops_shader_when_switching_to_image_mode() {
        let tmp = tempdir().unwrap();
        let config = test_config(tmp.path());

        let action = determine_reload_renderer_action(ActiveMode::Shader, true, None, &config);

        assert_eq!(action, ReloadRendererAction::StopShader);
    }

    #[test]
    fn creates_missing_config_with_rss_source() {
        let tmp = tempdir().unwrap();
        let config_path = tmp.path().join(".config").join("aura.hcl");
        let pictures = tmp.path().join("Pictures");

        let created = ensure_config_exists_with_pictures(&config_path, &pictures).unwrap();
        assert!(created);
        assert!(config_path.exists());
        assert!(pictures.exists());

        let text = fs::read_to_string(&config_path).unwrap();
        let parsed = config::parse_from_str(&text, &config_path).unwrap();
        assert_eq!(parsed.image.sources.len(), 1);
        match &parsed.image.sources[0] {
            config::SourceConfig::Rss {
                url,
                max_items,
                download_dir,
            } => {
                assert_eq!(url, "https://mrrtt.me/atv");
                assert_eq!(*max_items, 1000);
                assert!(download_dir.is_none());
            }
            _ => panic!("expected RSS source in generated config"),
        }
    }

    #[test]
    fn does_not_overwrite_existing_config() {
        let tmp = tempdir().unwrap();
        let config_path = tmp.path().join(".config").join("aura.hcl");
        let pictures = tmp.path().join("Pictures");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, "image = { timer = 300, sources = [] }\n").unwrap();

        let created = ensure_config_exists_with_pictures(&config_path, &pictures).unwrap();
        assert!(!created);
    }

    #[test]
    fn cli_defaults_to_tray_with_default_path() {
        let options = parse_cli_options(&[]).unwrap();
        assert!(options.tray_enabled);
        assert!(!options.debug_terminal);
        assert!(!options.print_version);
        assert_eq!(options.squirrel_event, None);
        assert_eq!(options.config_path.file_name().unwrap(), "aura.hcl");
    }

    #[test]
    fn cli_supports_no_tray_flag() {
        let options = parse_cli_options(&["--no-tray".to_string()]).unwrap();
        assert!(!options.tray_enabled);
        assert!(!options.debug_terminal);
        assert!(!options.print_version);
        assert_eq!(options.squirrel_event, None);
    }

    #[test]
    fn cli_supports_debug_flag() {
        let options = parse_cli_options(&["--debug".to_string()]).unwrap();
        assert!(options.tray_enabled);
        assert!(options.debug_terminal);
        assert!(!options.print_version);
        assert_eq!(options.squirrel_event, None);
    }

    #[test]
    fn cli_supports_version_flag() {
        let options = parse_cli_options(&["--version".to_string()]).unwrap();
        assert!(!options.debug_terminal);
        assert!(options.print_version);
        assert_eq!(options.squirrel_event, None);
        assert!(options.config_path.as_os_str().is_empty());
    }

    #[test]
    fn cli_supports_version_with_no_tray() {
        let options =
            parse_cli_options(&["--version".to_string(), "--no-tray".to_string()]).unwrap();
        assert!(options.print_version);
        assert!(!options.tray_enabled);
        assert!(!options.debug_terminal);
        assert_eq!(options.squirrel_event, None);
    }

    #[test]
    fn cli_supports_debug_with_version_and_no_tray() {
        let options = parse_cli_options(&[
            "--debug".to_string(),
            "--version".to_string(),
            "--no-tray".to_string(),
        ])
        .unwrap();
        assert!(options.print_version);
        assert!(!options.tray_enabled);
        assert!(options.debug_terminal);
        assert_eq!(options.squirrel_event, None);
    }

    #[test]
    fn cli_supports_squirrel_install_flag() {
        let options = parse_cli_options(&["--squirrel-install".to_string()]).unwrap();
        assert_eq!(options.squirrel_event, Some(SquirrelEvent::Install));
    }

    #[test]
    fn cli_supports_squirrel_firstrun_flag() {
        let options = parse_cli_options(&["--squirrel-firstrun".to_string()]).unwrap();
        assert_eq!(options.squirrel_event, Some(SquirrelEvent::Firstrun));
    }

    #[test]
    fn cli_rejects_multiple_squirrel_flags() {
        let result = parse_cli_options(&[
            "--squirrel-install".to_string(),
            "--squirrel-updated".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn try_switch_once_downloads_current_rss_and_prefetches_next() {
        let server = TestServer::start();
        server.set_response(
            "/current.png",
            ResponseSpec::ok("image/png", tiny_png_bytes()),
        );
        server.set_response("/next.png", ResponseSpec::ok("image/png", tiny_png_bytes()));

        let tmp = tempdir().unwrap();
        let config = test_config(tmp.path());
        let cache = CacheManager::new(&config).unwrap();
        let download_dir = tmp.path().join("rss");
        let current = ImageCandidate::rss(
            "current".to_string(),
            server.url("/current.png"),
            download_dir.clone(),
            None,
        );
        let next = ImageCandidate::rss(
            "next".to_string(),
            server.url("/next.png"),
            download_dir,
            None,
        );

        let mut rotation = RotationManager::new();
        rotation.rebuild_pool(vec![current, next]);
        rotation.restore_state(&PersistedState {
            remaining_queue: vec!["current".to_string(), "next".to_string()],
            shown_ids: Vec::new(),
            last_image_id: None,
        });

        let backend = RecordingBackend::default();
        let selected = try_switch_once(&mut rotation, &cache, &backend, &config)
            .await
            .unwrap();

        assert_eq!(selected, Some("current".to_string()));
        assert_eq!(backend.calls(), 0, "the per-monitor path is always used");
        assert_eq!(backend.batches().len(), 1);
        assert_eq!(server.hits("/current.png"), 1);

        for _ in 0..20 {
            if server.hits("/next.png") > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(server.hits("/next.png"), 1);
    }

    #[tokio::test]
    async fn try_switch_once_skips_failed_rss_candidate_and_uses_next() {
        let server = TestServer::start();
        server.set_response("/good.png", ResponseSpec::ok("image/png", tiny_png_bytes()));

        let tmp = tempdir().unwrap();
        let config = test_config(tmp.path());
        let cache = CacheManager::new(&config).unwrap();
        let download_dir = tmp.path().join("rss");
        let fallback_local = tmp.path().join("fallback.png");
        fs::write(&fallback_local, tiny_png_bytes()).unwrap();

        let failed = ImageCandidate::rss(
            "failed".to_string(),
            server.url("/missing.png"),
            download_dir.clone(),
            None,
        );
        let good = ImageCandidate::rss(
            "good".to_string(),
            server.url("/good.png"),
            download_dir,
            None,
        );
        let local =
            ImageCandidate::local("local".to_string(), Origin::Directory, fallback_local, None);

        let mut rotation = RotationManager::new();
        rotation.rebuild_pool(vec![failed, good, local]);
        rotation.restore_state(&PersistedState {
            remaining_queue: vec![
                "failed".to_string(),
                "good".to_string(),
                "local".to_string(),
            ],
            shown_ids: Vec::new(),
            last_image_id: None,
        });

        let backend = RecordingBackend::default();
        let selected = try_switch_once(&mut rotation, &cache, &backend, &config)
            .await
            .unwrap();

        assert_eq!(selected, Some("good".to_string()));
        assert_eq!(backend.calls(), 0, "the per-monitor path is always used");
        assert_eq!(backend.batches().len(), 1);
        assert_eq!(server.hits("/missing.png"), 1);
        assert_eq!(server.hits("/good.png"), 1);
    }

    #[tokio::test]
    async fn try_switch_once_applies_distinct_wallpapers_per_monitor() {
        let tmp = tempdir().unwrap();
        let config = test_config(tmp.path());
        let cache = CacheManager::new(&config).unwrap();

        let img_a = tmp.path().join("a.png");
        let img_b = tmp.path().join("b.png");
        let img_c = tmp.path().join("c.png");
        for path in [&img_a, &img_b, &img_c] {
            fs::write(path, tiny_png_bytes()).unwrap();
        }

        let mut rotation = RotationManager::new();
        rotation.rebuild_pool(vec![
            ImageCandidate::local("a".to_string(), Origin::Directory, img_a, None),
            ImageCandidate::local("b".to_string(), Origin::Directory, img_b, None),
            ImageCandidate::local("c".to_string(), Origin::Directory, img_c, None),
        ]);
        rotation.restore_state(&PersistedState {
            remaining_queue: vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
            ],
            shown_ids: Vec::new(),
            last_image_id: None,
        });

        let backend = RecordingBackend::with_monitors(3);
        let selected = try_switch_once(&mut rotation, &cache, &backend, &config)
            .await
            .unwrap();

        assert_eq!(selected, Some("a".to_string()));
        assert_eq!(
            backend.calls(),
            0,
            "per-monitor mode must not use the legacy single-wallpaper path"
        );
        let batches = backend.batches();
        assert_eq!(batches.len(), 1, "one set_wallpapers call expected");
        assert_eq!(batches[0].len(), 3, "one wallpaper per monitor expected");
        assert!(batches[0].iter().all(|p| p.exists()));
    }

    #[derive(Default)]
    struct RecordingBackend {
        calls: Mutex<Vec<PathBuf>>,
        batches: Mutex<Vec<Vec<PathBuf>>>,
        monitors: Mutex<Option<usize>>,
    }

    impl RecordingBackend {
        fn calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn with_monitors(monitors: usize) -> Self {
            let backend = Self::default();
            *backend.monitors.lock().unwrap() = Some(monitors);
            backend
        }

        fn batches(&self) -> Vec<Vec<PathBuf>> {
            self.batches.lock().unwrap().clone()
        }
    }

    impl WallpaperBackend for RecordingBackend {
        fn set_wallpaper(&self, path: &Path) -> Result<()> {
            assert!(path.exists());
            self.calls.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }

        fn monitor_count(&self) -> Result<usize> {
            Ok(self.monitors.lock().unwrap().unwrap_or(1))
        }

        fn set_wallpapers(&self, wallpapers: &[&Path]) -> Result<()> {
            for path in wallpapers {
                assert!(path.exists());
            }
            self.batches
                .lock()
                .unwrap()
                .push(wallpapers.iter().map(|p| p.to_path_buf()).collect());
            Ok(())
        }
    }

    fn test_config(base: &Path) -> AuraConfig {
        AuraConfig {
            image: ImageConfig {
                timer: Duration::from_secs(300),
                remote_update_timer: Duration::from_secs(3600),
                sources: Vec::new(),
                format: OutputFormat::Png,
                jpeg_quality: 90,
            },
            updater: UpdaterConfig {
                enabled: false,
                check_interval: Duration::from_secs(6 * 3600),
                feed_url: "https://github.com/hmerritt/aura/releases/latest/download".to_string(),
            },
            cache_dir: base.join("cache"),
            state_file: base.join("state.json"),
            log_level: "info".to_string(),
            max_cache_bytes: 1024 * 1024,
            max_cache_age: Duration::from_secs(24 * 60 * 60),
            renderer: RendererMode::Image,
            shader: None,
        }
    }

    fn test_shader_config(name: &str) -> ShaderConfig {
        ShaderConfig {
            name: name.to_string(),
            target_fps: 60,
            resolution: 100,
            mouse_enabled: false,
            desktop_scope: ShaderDesktopScope::Virtual,
            color_space: ShaderColorSpace::Unorm,
        }
    }

    fn tiny_png_bytes() -> Vec<u8> {
        let image: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
}
