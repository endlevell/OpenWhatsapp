//! WebKit webview integration.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gtk::prelude::*;
use tokio::sync::broadcast;
use uuid::Uuid;
use webkit6::prelude::*;

use crate::accounts::Account;
use crate::clipboard_paste;
use crate::config::{DownloadConfig, SpellcheckConfig};
use crate::customizations::{self, CustomizationStore};
use crate::downloads;
use crate::drag_drop;
use crate::ipc::{AppEvent, EventBus};
use crate::spellcheck;
use crate::utils::paths;

pub(crate) mod injection;
pub(crate) mod navigation;

const WHATSAPP_WEB_URI: &str = "https://web.whatsapp.com";
const GUEST_WEBVIEW_NAME: &str = "guest";
const WHATSAPP_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.7778.96 Safari/537.36";

/// Stack of isolated account webviews.
pub(crate) struct AccountWebViews {
    stack: gtk::Stack,
    webviews: Vec<webkit6::WebView>,
}

impl AccountWebViews {
    /// Creates one isolated WebKit webview per account.
    pub(crate) fn new(
        accounts: &[Account],
        spellcheck_config: &SpellcheckConfig,
        download_config: &DownloadConfig,
        customization_store: Arc<CustomizationStore>,
        events: EventBus,
    ) -> Result<Self> {
        let stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();

        let (account_names, webviews) = if accounts.is_empty() {
            let webview = add_guest_webview(
                &stack,
                spellcheck_config,
                Arc::clone(&customization_store),
                events.clone(),
            )?;
            (Vec::new(), vec![webview])
        } else {
            add_account_webviews(
                &stack,
                accounts,
                spellcheck_config,
                download_config,
                Arc::clone(&customization_store),
                events.clone(),
            )?
        };

        bind_account_switches(&stack, account_names, events);

        Ok(Self { stack, webviews })
    }

    /// Returns the GTK stack widget.
    pub(crate) fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    /// Returns the account WebKit widgets owned by this stack.
    pub(crate) fn webviews(&self) -> &[webkit6::WebView] {
        &self.webviews
    }
}

fn add_guest_webview(
    stack: &gtk::Stack,
    spellcheck_config: &SpellcheckConfig,
    customization_store: Arc<CustomizationStore>,
    events: EventBus,
) -> Result<webkit6::WebView> {
    let webview = build_default_webview(spellcheck_config, customization_store, events)?;
    stack.add_named(&webview, Some(GUEST_WEBVIEW_NAME));
    stack.set_visible_child_name(GUEST_WEBVIEW_NAME);

    Ok(webview)
}

fn add_account_webviews(
    stack: &gtk::Stack,
    accounts: &[Account],
    spellcheck_config: &SpellcheckConfig,
    download_config: &DownloadConfig,
    customization_store: Arc<CustomizationStore>,
    events: EventBus,
) -> Result<(Vec<String>, Vec<webkit6::WebView>)> {
    let mut account_names = Vec::with_capacity(accounts.len());
    let mut webviews = Vec::with_capacity(accounts.len());

    for account in accounts {
        let name = account.id.to_string();
        let webview = build_isolated_webview(
            account,
            spellcheck_config,
            download_config,
            Arc::clone(&customization_store),
            events.clone(),
        )?;

        stack.add_named(&webview, Some(&name));
        account_names.push(name);
        webviews.push(webview);
    }

    if let Some(first_account) = accounts.first() {
        stack.set_visible_child_name(&first_account.id.to_string());
    }

    Ok((account_names, webviews))
}

fn build_default_webview(
    spellcheck_config: &SpellcheckConfig,
    customization_store: Arc<CustomizationStore>,
    events: EventBus,
) -> Result<webkit6::WebView> {
    let content_manager = injection::content_manager();
    let webview = webkit6::WebView::builder()
        .user_content_manager(&content_manager)
        .settings(&whatsapp_settings())
        .build();
    configure_default_web_context_for_webrtc(&webview);
    configure_webview(&webview, Uuid::nil(), spellcheck_config, events)?;
    customizations::attach_to_webview(&webview, None, customization_store);
    Ok(webview)
}

fn build_isolated_webview(
    account: &Account,
    spellcheck_config: &SpellcheckConfig,
    download_config: &DownloadConfig,
    customization_store: Arc<CustomizationStore>,
    events: EventBus,
) -> Result<webkit6::WebView> {
    let account_id = account.id;
    let data_dir = paths::account_web_data_dir(account_id)?;
    let cache_dir = paths::account_web_cache_dir(account_id)?;
    ensure_webkit_dirs(&data_dir, &cache_dir)?;

    let data_dir_text = path_to_string(&data_dir);
    let cache_dir_text = path_to_string(&cache_dir);
    let network_session = webkit6::NetworkSession::new(Some(&data_dir_text), Some(&cache_dir_text));
    downloads::attach_to_network_session(
        &network_session,
        account,
        download_config,
        events.clone(),
    );
    let content_manager = injection::content_manager();
    let webview = webkit6::WebView::builder()
        .network_session(&network_session)
        .user_content_manager(&content_manager)
        .settings(&whatsapp_settings())
        .build();
    configure_default_web_context_for_webrtc(&webview);
    configure_webview(&webview, account.id, spellcheck_config, events)?;
    customizations::attach_to_webview(&webview, Some(account.id), customization_store);

    Ok(webview)
}

fn configure_webview(
    webview: &webkit6::WebView,
    account_id: Uuid,
    spellcheck_config: &SpellcheckConfig,
    events: EventBus,
) -> Result<()> {
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    attach_compositor_redraw_safety_net(webview);
    spellcheck::apply_to_webview(webview, spellcheck_config)?;
    drag_drop::attach_to_webview(webview);
    clipboard_paste::attach_to_webview(webview);
    attach_load_diagnostics(webview);
    attach_notification_handlers(webview, account_id, events.clone());
    attach_unread_title_watcher(webview, account_id, events);
    webview.load_uri(WHATSAPP_WEB_URI);

    Ok(())
}

/// Installs a compositor-resume redraw nudge for Wayland compositors.
pub(crate) fn install_workspace_resume_workaround(
    window: &gtk::ApplicationWindow,
    stack: &gtk::Stack,
    webviews: &[webkit6::WebView],
) {
    let stack_for_focus = stack.clone();
    let webviews_for_focus = webviews.to_vec();
    window.connect_is_active_notify(move |window| {
        if window.is_active() {
            window.queue_draw();
            nudge_webviews(&stack_for_focus, &webviews_for_focus);
        }
    });

    let stack_for_map = stack.clone();
    let webviews_for_map = webviews.to_vec();
    window.connect_map(move |window| {
        window.queue_draw();
        nudge_webviews(&stack_for_map, &webviews_for_map);
    });

    let stack_for_visible = stack.clone();
    let webviews_for_visible = webviews.to_vec();
    window.connect_visible_notify(move |window| {
        if window.is_visible() {
            window.queue_draw();
            nudge_webviews(&stack_for_visible, &webviews_for_visible);
        }
    });

    let stack_for_realize = stack.clone();
    let webviews_for_realize = webviews.to_vec();
    window.connect_realize(move |window| {
        connect_toplevel_state_redraw(window, &stack_for_realize, &webviews_for_realize);
    });

    connect_toplevel_state_redraw(window, stack, webviews);
}

fn whatsapp_settings() -> webkit6::Settings {
    webkit6::Settings::builder()
        .auto_load_images(true)
        .enable_developer_extras(true)
        .enable_html5_local_storage(true)
        .enable_javascript(true)
        .enable_javascript_markup(true)
        .enable_media(true)
        .enable_media_capabilities(true)
        .enable_media_stream(true)
        .enable_encrypted_media(true)
        .enable_mediasource(true)
        .enable_page_cache(true)
        .enable_site_specific_quirks(true)
        .enable_webaudio(true)
        .enable_webgl(true)
        .enable_webrtc(true)
        .media_playback_requires_user_gesture(false)
        .hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Always)
        .javascript_can_access_clipboard(true)
        .javascript_can_open_windows_automatically(true)
        .user_agent(WHATSAPP_USER_AGENT)
        .build()
}

fn configure_default_web_context_for_webrtc(webview: &webkit6::WebView) {
    let context = webview.context().or_else(webkit6::WebContext::default);
    if context.is_none() {
        tracing::warn!("WebKit WebContext unavailable; WebRTC device access may fail");
    }
}

fn attach_load_diagnostics(webview: &webkit6::WebView) {
    webview.connect_load_changed(|webview, event| {
        tracing::debug!(
            ?event,
            uri = webview.uri().as_deref().unwrap_or_default(),
            "webview load changed"
        );
    });

    webview.connect_load_failed(|_webview, event, uri, error| {
        tracing::warn!(?event, ?uri, ?error, "webview load failed");
        false
    });
}

fn attach_compositor_redraw_safety_net(webview: &webkit6::WebView) {
    webview.connect_map(|webview| {
        webview.queue_draw();
        webview.queue_resize();
    });

    let weak_webview = webview.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(500), move || {
        let Some(webview) = weak_webview.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };

        webview.queue_draw();
        webview.queue_resize();
        gtk::glib::ControlFlow::Continue
    });
}

fn attach_notification_handlers(webview: &webkit6::WebView, account_id: Uuid, events: EventBus) {
    webview.connect_permission_request(|_, request| {
        if request.is::<webkit6::NotificationPermissionRequest>() {
            request.allow();
            return true;
        }

        if request.is::<webkit6::UserMediaPermissionRequest>() {
            if let Some(media_request) =
                request.downcast_ref::<webkit6::UserMediaPermissionRequest>()
            {
                tracing::debug!(
                    audio = webkit6::functions::user_media_permission_is_for_audio_device(
                        media_request
                    ),
                    video = webkit6::functions::user_media_permission_is_for_video_device(
                        media_request
                    ),
                    display = webkit6::functions::user_media_permission_is_for_display_device(
                        media_request
                    ),
                    "granting WebRTC media permission"
                );
            }
            request.allow();
            return true;
        }

        if request.is::<webkit6::DeviceInfoPermissionRequest>() {
            tracing::debug!("granting WebRTC device-info permission");
            request.allow();
            return true;
        }

        false
    });

    webview.connect_query_permission_state(|_, query| {
        if query
            .name()
            .map(|name| is_auto_granted_permission_query(name.as_str()))
            .unwrap_or(false)
        {
            query.finish(webkit6::PermissionState::Granted);
            return true;
        }

        false
    });

    webview.connect_show_notification(move |_, notification| {
        let title = notification
            .title()
            .map(|title| normalize_notification_title(title.as_str()))
            .unwrap_or_else(|| "WhatsApp".to_string());
        let body = notification
            .body()
            .map(|body| body.to_string())
            .unwrap_or_default();
        let notification_for_click = notification.clone();
        let events_for_click = events.clone();
        notification.connect_clicked(move |_| {
            let _subscriber_count = events_for_click.emit(AppEvent::WindowShow);
            let _subscriber_count = events_for_click.emit(AppEvent::AccountSwitched(account_id));
            notification_for_click.clicked();
        });
        let _subscriber_count = events.emit(AppEvent::NotificationReceived {
            account_id,
            title,
            body,
        });
        true
    });
}

fn is_auto_granted_permission_query(name: &str) -> bool {
    matches!(
        name,
        "camera" | "microphone" | "speaker-selection" | "display-capture" | "notifications"
    )
}

fn attach_unread_title_watcher(webview: &webkit6::WebView, account_id: Uuid, events: EventBus) {
    webview.connect_title_notify(move |webview| {
        let Some(title) = webview.title() else {
            return;
        };

        if let Some(count) = unread_count_from_title(title.as_str()) {
            let _subscriber_count = events.emit(AppEvent::UnreadCountChanged { account_id, count });
        }
    });
}

fn connect_toplevel_state_redraw(
    window: &gtk::ApplicationWindow,
    stack: &gtk::Stack,
    webviews: &[webkit6::WebView],
) {
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(toplevel) = surface.dynamic_cast::<gtk::gdk::Toplevel>() else {
        return;
    };

    let stack = stack.clone();
    let webviews = webviews.to_vec();
    toplevel.connect_state_notify(move |_| {
        nudge_webviews(&stack, &webviews);
    });
}

fn nudge_webviews(stack: &gtk::Stack, webviews: &[webkit6::WebView]) {
    stack.queue_draw();
    for webview in webviews {
        webview.queue_draw();
        webview.queue_resize();
    }

    if let Some(webview) = active_webview(stack) {
        webview.grab_focus();
        webview.evaluate_javascript("void 0", None, None, None::<&gtk::gio::Cancellable>, |_| {});
    }
}

fn normalize_notification_title(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        "WhatsApp".to_string()
    } else {
        title.to_string()
    }
}

fn unread_count_from_title(title: &str) -> Option<u32> {
    let title = title.trim_start();
    let after_open = title.strip_prefix('(')?;
    let closing = after_open.find(')')?;
    after_open[..closing].parse::<u32>().ok()
}

fn ensure_webkit_dirs(data_dir: &Path, cache_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create WebKit data dir {}", data_dir.display()))?;
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create WebKit cache dir {}", cache_dir.display()))
}

fn bind_account_switches(stack: &gtk::Stack, account_names: Vec<String>, events: EventBus) {
    let mut receiver = events.subscribe();
    let stack = stack.clone();

    gtk::glib::MainContext::default().spawn_local(async move {
        loop {
            match receiver.recv().await {
                Ok(AppEvent::AccountSwitched(account_id)) => {
                    let name = account_id.to_string();
                    if account_names
                        .iter()
                        .any(|account_name| account_name == &name)
                    {
                        stack.set_visible_child_name(&name);
                    }
                }
                Ok(AppEvent::NextAccount) => switch_relative(&stack, &account_names, 1),
                Ok(AppEvent::PreviousAccount) => switch_relative(&stack, &account_names, -1),
                Ok(AppEvent::ReloadActiveWebView) => reload_active_webview(&stack),
                Ok(AppEvent::OpenInspector) => open_active_inspector(&stack),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn switch_relative(stack: &gtk::Stack, account_names: &[String], offset: isize) {
    if account_names.is_empty() {
        return;
    }

    let current_name = stack.visible_child_name().map(|name| name.to_string());
    let current_index = current_name
        .as_ref()
        .and_then(|name| {
            account_names
                .iter()
                .position(|account_name| account_name == name)
        })
        .unwrap_or(0);
    let next_index = wrap_index(current_index, offset, account_names.len());

    stack.set_visible_child_name(&account_names[next_index]);
}

fn reload_active_webview(stack: &gtk::Stack) {
    if let Some(webview) = active_webview(stack) {
        webview.reload();
    }
}

fn open_active_inspector(stack: &gtk::Stack) {
    if let Some(webview) = active_webview(stack) {
        if let Some(inspector) = webview.inspector() {
            inspector.show();
        }
    }
}

fn active_webview(stack: &gtk::Stack) -> Option<webkit6::WebView> {
    stack
        .visible_child()
        .and_then(|widget| widget.downcast::<webkit6::WebView>().ok())
}

fn wrap_index(index: usize, offset: isize, len: usize) -> usize {
    let len = isize::try_from(len).unwrap_or(1);
    let index = isize::try_from(index).unwrap_or_default();

    (index + offset).rem_euclid(len) as usize
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn path_to_string_preserves_path_text() {
        assert_eq!(
            path_to_string(&PathBuf::from("/tmp/openwhatsapp")),
            "/tmp/openwhatsapp"
        );
    }

    #[test]
    fn whatsapp_user_agent_spoofs_chrome() {
        assert!(WHATSAPP_USER_AGENT.contains("Chrome/"));
        assert!(WHATSAPP_USER_AGENT.contains("Safari/"));
    }

    #[test]
    fn unread_count_parses_whatsapp_title_prefix() {
        assert_eq!(unread_count_from_title("(3) WhatsApp"), Some(3));
        assert_eq!(unread_count_from_title("WhatsApp"), None);
    }

    #[test]
    fn guest_webview_name_is_stable() {
        assert_eq!(GUEST_WEBVIEW_NAME, "guest");
    }

    #[test]
    fn relative_index_wraps_around() {
        assert_eq!(wrap_index(0, -1, 3), 2);
        assert_eq!(wrap_index(2, 1, 3), 0);
    }
}
