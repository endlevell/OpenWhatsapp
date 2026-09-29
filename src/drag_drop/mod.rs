//! Drag and drop integration.

use gtk::gio::prelude::*;
use gtk::prelude::*;

use crate::clipboard_paste::{self, PasteTarget, PastedFile};

const ACCEPTED_MIME_TYPES: &[&str] = &[
    "text/uri-list",
    "text/plain",
    "application/octet-stream",
    "image/png",
    "image/jpeg",
    "image/gif",
    "video/mp4",
    "audio/mpeg",
    "application/pdf",
];
const READ_MIME_TYPES: &[&str] = &["text/uri-list", "text/plain"];
const MAX_DROP_BYTES: usize = 1024 * 1024;
/// Files are copied into the page as base64, so keep them to a sane size.
const MAX_DROPPED_FILE_BYTES: i64 = 64 * 1024 * 1024;

/// Attaches file drag-and-drop handling to a WebKit webview.
pub(crate) fn attach_to_webview(webview: &webkit6::WebView) {
    let formats = gtk::gdk::ContentFormats::new(ACCEPTED_MIME_TYPES);
    let target = gtk::DropTargetAsync::new(Some(formats), gtk::gdk::DragAction::COPY);
    let webview_for_drop = webview.clone();

    target.connect_accept(|_, _| true);
    target.connect_drop(move |_, drop, _, _| {
        let drop = drop.clone();
        let webview = webview_for_drop.clone();

        gtk::glib::MainContext::default().spawn_local(async move {
            if let Err(error) = handle_drop(&webview, &drop).await {
                tracing::error!(?error, "failed to handle file drop");
                drop.finish(gtk::gdk::DragAction::empty());
                return;
            }

            drop.finish(gtk::gdk::DragAction::COPY);
        });

        true
    });

    webview.add_controller(target);
}

async fn handle_drop(webview: &webkit6::WebView, drop: &gtk::gdk::Drop) -> anyhow::Result<()> {
    let (stream, _mime_type) = drop
        .read_future(READ_MIME_TYPES, gtk::glib::Priority::DEFAULT)
        .await?;
    let bytes = stream
        .read_bytes_future(MAX_DROP_BYTES, gtk::glib::Priority::DEFAULT)
        .await?;
    let text = String::from_utf8_lossy(bytes.as_ref());
    let uris = parse_uri_list(&text);
    let mut files = Vec::with_capacity(uris.len());

    for uri in &uris {
        match load_dropped_file(uri).await {
            Ok(file) => files.push(file),
            Err(error) => tracing::warn!(?error, uri, "skipping dropped file"),
        }
    }

    if files.is_empty() {
        return Ok(());
    }

    clipboard_paste::inject_paste(webview, &files, PasteTarget::Composer);

    Ok(())
}

/// Reads a dropped `file://` URI into memory so it can be handed to the page;
/// web content cannot open local paths on its own.
async fn load_dropped_file(uri: &str) -> anyhow::Result<PastedFile> {
    let file = gtk::gio::File::for_uri(uri);
    let path = file
        .path()
        .ok_or_else(|| anyhow::anyhow!("not a local file"))?;
    let size = file
        .query_info_future(
            gtk::gio::FILE_ATTRIBUTE_STANDARD_SIZE,
            gtk::gio::FileQueryInfoFlags::NONE,
            gtk::glib::Priority::DEFAULT,
        )
        .await?
        .size();

    if size > MAX_DROPPED_FILE_BYTES {
        anyhow::bail!("file is {size} bytes, above the {MAX_DROPPED_FILE_BYTES} byte limit");
    }

    let (contents, _etag) = file.load_contents_future().await?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let (content_type, _uncertain) =
        gtk::gio::content_type_guess(Some(&path), Some(contents.as_ref()));
    let mime_type = gtk::gio::content_type_get_mime_type(&content_type)
        .map(|mime| mime.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_owned());

    Ok(PastedFile::new(&contents, &name, &mime_type))
}

fn parse_uri_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_parser_ignores_comments_and_blanks() {
        assert_eq!(
            parse_uri_list("# comment\n\nfile:///tmp/a.png\nfile:///tmp/b.pdf\n"),
            vec![
                "file:///tmp/a.png".to_string(),
                "file:///tmp/b.pdf".to_string()
            ]
        );
    }
}
