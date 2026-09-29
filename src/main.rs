mod accounts;
mod app;
mod clipboard_paste;
mod config;
mod customizations;
mod downloads;
mod drag_drop;
mod ipc;
mod notifications;
mod settings;
mod shortcuts;
mod spellcheck;
mod theme;
mod tray;
mod utils;
mod webview;
mod window;

use std::sync::Arc;

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    configure_wayland_webkit_environment();
    gtk::init().context("failed to initialize GTK4")?;
    utils::logging::init()?;
    warn_if_wayland_portal_unavailable();

    let config_path = utils::paths::config_file()?;
    let data_dir = utils::paths::data_dir()?;
    let database_path = utils::paths::database_file()?;
    let temp_download_dir = downloads::prepare_temp_folder()?;
    let config = config::load_or_create(&config_path)?;
    let account_store = accounts::AccountStore::open(&database_path)?;
    let accounts: Vec<accounts::Account> = account_store.list_accounts()?;
    customizations::ensure_base_dirs()?;
    let customization_store = Arc::new(customizations::CustomizationStore::open(&database_path)?);
    let events = ipc::EventBus::new(ipc::DEFAULT_EVENT_CAPACITY);
    let _receiver = events.subscribe();

    tracing::debug!(
        ?config_path,
        ?data_dir,
        ?database_path,
        ?temp_download_dir,
        account_count = accounts.len(),
        theme = ?config.appearance.theme,
        "openwhatsapp initialized"
    );
    app::run(config_path, config, accounts, customization_store, events);
    downloads::cleanup_temp_folder()?;

    Ok(())
}

fn configure_wayland_webkit_environment() {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        std::env::set_var("WEBKIT_USE_GSTREAMER_GL_TEXTURES", "1");
        std::env::set_var("GST_PLUGIN_FEATURE_RANK", "pipewire:512");
    }

    if !is_hyprland_session() {
        return;
    }

    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

fn is_hyprland_session() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP")
            .map(|desktop| desktop.to_ascii_lowercase().contains("hyprland"))
            .unwrap_or(false)
}

fn warn_if_wayland_portal_unavailable() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }

    let portal_available = std::process::Command::new("busctl")
        .args(["--user", "status", "org.freedesktop.portal.Desktop"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    if !portal_available {
        tracing::warn!(
            "xdg-desktop-portal not running; camera and screen sharing may not work on Wayland"
        );
    }
}
