//! What the composer can attach to a prompt: images (pasted, dropped, or chosen) and text files. Each is checked here,
//! before anything leaves the window: images by type and size, files by size, encoding and name — a key file, an
//! `.env` or a private key is never attached, whatever the person dropped.

use std::path::Path;

use acp_core::Attachment;
use base64::Engine as _;

/// The most an image may weigh (the backend's vision path re-encodes; this keeps a 40 MB screenshot out of the pipe).
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// The most a text file may weigh.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Attachments on one prompt.
pub const MAX_ATTACHMENTS: usize = 8;

/// One staged attachment: what the chip shows and what goes out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub name: String,
    pub size: usize,
    pub image: bool,
    pub attachment: Attachment,
}

pub fn image_mime(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Files that hold credentials by convention. Never attached.
pub fn looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || lower == ".netrc"
        || lower == ".npmrc"
        || lower == ".pypirc"
        || lower == "credentials"
}

pub fn image(name: &str, mime: &str, bytes: &[u8]) -> Result<Staged, String> {
    if bytes.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("{name} is over {} MB", MAX_IMAGE_BYTES / (1024 * 1024)));
    }
    Ok(Staged {
        name: name.to_owned(),
        size: bytes.len(),
        image: true,
        attachment: Attachment::Image { mime: mime.to_owned(), data_base64: base64::engine::general_purpose::STANDARD.encode(bytes) },
    })
}

/// A dropped or chosen file, as an image or a text file — or why it cannot be attached.
pub fn from_path(path: &Path) -> Result<Staged, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    if looks_secret(&name) {
        return Err(format!("{name} looks like a credentials file — not attached"));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    if let Some(mime) = image_mime(ext) {
        if meta.len() as usize > MAX_IMAGE_BYTES {
            return Err(format!("{name} is over {} MB", MAX_IMAGE_BYTES / (1024 * 1024)));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        return image(&name, mime, &bytes);
    }
    if meta.len() as usize > MAX_TEXT_BYTES {
        return Err(format!("{name} is over {} KB", MAX_TEXT_BYTES / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    if bytes.contains(&0) {
        return Err(format!("{name} is a binary file"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8 text"))?;
    Ok(Staged { name, size: text.len(), image: false, attachment: Attachment::Text { uri: file_uri(path), text } })
}

fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.display().to_string().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `12 KB`, `1.4 MB`.
pub fn size_label(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cordata-attach-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn files_and_images_are_checked_before_they_go() {
        let d = dir();
        std::fs::write(d.join("notes.md"), "hello").unwrap();
        let text = from_path(&d.join("notes.md")).unwrap();
        assert!(!text.image && matches!(&text.attachment, Attachment::Text { uri, text } if uri.starts_with("file://") && text == "hello"));
        std::fs::write(d.join("pic.PNG"), [0x89, b'P', b'N', b'G']).unwrap();
        let img = from_path(&d.join("pic.PNG")).unwrap();
        assert!(img.image && matches!(&img.attachment, Attachment::Image { mime, .. } if mime == "image/png"));
        std::fs::write(d.join(".env"), "KEY=1").unwrap();
        assert!(from_path(&d.join(".env")).unwrap_err().contains("credentials"));
        std::fs::write(d.join("blob.bin"), [0u8, 1, 2]).unwrap();
        assert!(from_path(&d.join("blob.bin")).unwrap_err().contains("binary"));
        std::fs::write(d.join("big.txt"), "x".repeat(MAX_TEXT_BYTES + 1)).unwrap();
        assert!(from_path(&d.join("big.txt")).unwrap_err().contains("KB"));
        assert!(from_path(&d).is_err(), "a folder is not attached");
        assert!(image("e.png", "image/png", &[]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn secret_names_and_sizes() {
        for name in ["id_rsa", "id_ed25519.pub", "server.pem", ".env.local", ".netrc"] {
            assert!(looks_secret(name), "{name}");
        }
        assert!(!looks_secret("main.rs"));
        assert_eq!(size_label(2048), "2 KB");
        assert_eq!(file_uri(Path::new("/a b/c.rs")), "file:///a%20b/c.rs");
    }
}
