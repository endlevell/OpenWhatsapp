//! Clipboard image paste integration.
//!
//! WebKitGTK does not expose clipboard images to web content: on Ctrl+V the
//! page receives a `paste` event whose `clipboardData` has no types and no
//! files, so WhatsApp Web ignores it. We intercept the shortcut, read the image
//! through GDK and re-dispatch a `paste` event carrying it as a `File`.

use gtk::prelude::*;
use webkit6::prelude::*;

const PASTED_FILE_NAME: &str = "image.png";
const PASTED_MIME_TYPE: &str = "image/png";

/// Attaches clipboard image paste handling to a WebKit webview.
pub(crate) fn attach_to_webview(webview: &webkit6::WebView) {
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let webview_weak = webview.downgrade();

    controller.connect_key_pressed(move |_, key, _, state| {
        if !is_paste_shortcut(key, state) {
            return gtk::glib::Propagation::Proceed;
        }

        let Some(webview) = webview_weak.upgrade() else {
            return gtk::glib::Propagation::Proceed;
        };
        let clipboard = webview.clipboard();
        let mime_types = clipboard.formats().mime_types();
        let mime_types: Vec<&str> = mime_types.iter().map(|mime| mime.as_str()).collect();

        if !should_intercept_paste(&mime_types) {
            return gtk::glib::Propagation::Proceed;
        }

        gtk::glib::MainContext::default().spawn_local(async move {
            if let Err(error) = paste_clipboard_image(&webview, &clipboard).await {
                tracing::error!(?error, "failed to paste clipboard image");
            }
        });

        gtk::glib::Propagation::Stop
    });

    webview.add_controller(controller);
}

async fn paste_clipboard_image(
    webview: &webkit6::WebView,
    clipboard: &gtk::gdk::Clipboard,
) -> anyhow::Result<()> {
    let texture = clipboard
        .read_texture_future()
        .await?
        .ok_or_else(|| anyhow::anyhow!("clipboard has no image"))?;
    let png = texture.save_to_png_bytes();
    let file = PastedFile::new(png.as_ref(), PASTED_FILE_NAME, PASTED_MIME_TYPE);

    inject_paste(webview, &[file], PasteTarget::Focused);

    Ok(())
}

/// A file handed to the page inside a synthetic `paste` event.
pub(crate) struct PastedFile {
    base64_data: String,
    name: String,
    mime_type: String,
}

impl PastedFile {
    pub(crate) fn new(bytes: &[u8], name: &str, mime_type: &str) -> Self {
        Self {
            base64_data: gtk::glib::base64_encode(bytes).to_string(),
            name: name.to_owned(),
            mime_type: mime_type.to_owned(),
        }
    }
}

/// Where the synthetic `paste` event is dispatched.
#[derive(Clone, Copy)]
pub(crate) enum PasteTarget {
    /// The element that currently has focus (keyboard paste).
    Focused,
    /// The message composer of the open chat (files dropped on the window).
    Composer,
}

/// Dispatches a `paste` event carrying `files`, the path WhatsApp Web uses to
/// open its attachment preview.
pub(crate) fn inject_paste(webview: &webkit6::WebView, files: &[PastedFile], target: PasteTarget) {
    let script = build_paste_script(files, target);

    webview.evaluate_javascript(
        &script,
        None,
        None,
        None::<&webkit6::gio::Cancellable>,
        |result| {
            if let Err(error) = result {
                tracing::error!(?error, "failed to inject paste event");
            }
        },
    );
}

fn is_paste_shortcut(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    state.contains(gtk::gdk::ModifierType::CONTROL_MASK)
        && !state.contains(gtk::gdk::ModifierType::ALT_MASK)
        && key.to_lower() == gtk::gdk::Key::v
}

/// Only take over when the clipboard holds an image and no plain text, so text
/// pastes keep going through WebKit's native path.
fn should_intercept_paste(mime_types: &[&str]) -> bool {
    let has_image = mime_types.iter().any(|mime| mime.starts_with("image/"));
    let has_text = mime_types
        .iter()
        .any(|mime| mime.starts_with("text/plain") || *mime == "text/uri-list");

    has_image && !has_text
}

fn build_paste_script(files: &[PastedFile], target: PasteTarget) -> String {
    let files_js = files
        .iter()
        .map(|file| {
            format!(
                r#"{{ data: "{}", name: "{}", type: "{}" }}"#,
                file.base64_data,
                escape_js_string(&file.name),
                escape_js_string(&file.mime_type)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let target_js = match target {
        PasteTarget::Focused => "document.activeElement || document.body",
        PasteTarget::Composer => r#"document.querySelector('footer [contenteditable="true"]')"#,
    };

    format!(
        r#"(() => {{
  const target = {target_js};
  if (!target) return "no-target";
  target.focus();
  const data = new DataTransfer();
  for (const entry of [{files_js}]) {{
    const binary = atob(entry.data);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    data.items.add(new File([bytes], entry.name, {{ type: entry.type }}));
  }}
  target.dispatchEvent(new ClipboardEvent("paste", {{ clipboardData: data, bubbles: true, cancelable: true }}));
  return "ok";
}})();"#
    )
}

pub(crate) fn escape_js_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercepts_image_only_clipboard() {
        assert!(should_intercept_paste(&["image/png"]));
        assert!(should_intercept_paste(&["image/png", "text/html"]));
    }

    #[test]
    fn leaves_text_pastes_to_webkit() {
        assert!(!should_intercept_paste(&["text/plain", "UTF8_STRING"]));
        assert!(!should_intercept_paste(&[
            "image/png",
            "text/plain;charset=utf-8"
        ]));
        assert!(!should_intercept_paste(&["text/uri-list", "image/png"]));
        assert!(!should_intercept_paste(&[]));
    }

    #[test]
    fn paste_shortcut_requires_ctrl_v() {
        use gtk::gdk::{Key, ModifierType};

        assert!(is_paste_shortcut(Key::v, ModifierType::CONTROL_MASK));
        assert!(is_paste_shortcut(
            Key::V,
            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
        ));
        assert!(!is_paste_shortcut(Key::v, ModifierType::empty()));
        assert!(!is_paste_shortcut(
            Key::v,
            ModifierType::CONTROL_MASK | ModifierType::ALT_MASK
        ));
    }

    #[test]
    fn paste_script_dispatches_files_in_clipboard_event() {
        let files = [
            PastedFile::new(b"png", "image.png", "image/png"),
            PastedFile::new(b"pdf", "relat\"o.pdf", "application/pdf"),
        ];
        let script = build_paste_script(&files, PasteTarget::Focused);

        assert!(script.contains(r#"{ data: "cG5n", name: "image.png", type: "image/png" }"#));
        assert!(script.contains(r#"name: "relat\"o.pdf""#));
        assert!(script.contains("document.activeElement"));
        assert!(script.contains(r#"new ClipboardEvent("paste""#));
    }

    #[test]
    fn composer_target_skips_when_no_chat_is_open() {
        let script = build_paste_script(&[], PasteTarget::Composer);

        assert!(script.contains(r#"document.querySelector('footer [contenteditable="true"]')"#));
        assert!(script.contains(r#"if (!target) return "no-target";"#));
    }

    #[test]
    fn js_string_escape_handles_quotes_and_newlines() {
        assert_eq!(escape_js_string("a\"b\nc"), "a\\\"b\\nc");
    }
}
