//! What the composer can attach to a prompt: images (PNG, JPEG, GIF, WebP, plus automatic conversion of macOS HEIC,
//! TIFF, BMP, and ICO screenshots/photos), PDFs (native PDFKit text extraction), rich/Office documents (`.rtf`, `.doc`,
//! `.docx`, `.odt`, `.xlsx`, `.pptx`), archives (`.zip`, `.tar.gz`), data/notebooks (`.csv`, `.json`, `.ipynb`, `.sql`,
//! `.log`), source code, and binary file descriptors.
//!
//! Every attachment is checked before anything leaves the window: credential files (`.env`, `.pem`, `.key`, `id_rsa`,
//! `.netrc`, `credentials.json`, etc.) are strictly refused regardless of how they were dropped or selected.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use acp_core::Attachment;
use base64::Engine as _;

/// Maximum size of a single attached image (20 MB).
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Maximum UTF-8 text inlined verbatim before switching to a head + tail excerpt (1 MB).
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// Maximum file size accepted on disk for excerpts, PDFs, archives, or binary descriptors (50 MB).
pub const MAX_FILE_BYTES: usize = 50 * 1024 * 1024;
/// Head and tail byte budgets for large text/log/CSV files (> 1 MB).
const EXCERPT_HEAD_BYTES: usize = 256 * 1024;
const EXCERPT_TAIL_BYTES: usize = 64 * 1024;
/// Maximum attachments on a single prompt.
pub const MAX_ATTACHMENTS: usize = 16;

/// Visual category of an attachment for Composer cards and Transcript pills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Pdf,
    Document,
    Data,
    Archive,
    Code,
    Binary,
}

impl Kind {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Image => "▣",
            Self::Pdf => "📕",
            Self::Document => "📄",
            Self::Data => "⊞",
            Self::Archive => "◫",
            Self::Code => "⌨",
            Self::Binary => "◈",
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::Image => "IMG",
            Self::Pdf => "PDF",
            Self::Document => "DOC",
            Self::Data => "DATA",
            Self::Archive => "ZIP",
            Self::Code => "CODE",
            Self::Binary => "BIN",
        }
    }
}

/// Infer an attachment category from a filename (used for both staged cards and transcript pills).
pub fn kind_for_name(name: &str) -> Kind {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tar.bz2") || lower.ends_with(".tar.xz") {
        return Kind::Archive;
    }
    let ext = Path::new(&lower).extension().and_then(|e| e.to_str()).unwrap_or_default();
    if image_mime(ext).is_some() || needs_png_conversion(ext) {
        Kind::Image
    } else if ext == "pdf" {
        Kind::Pdf
    } else if matches!(ext, "rtf" | "doc" | "docx" | "odt" | "pptx" | "epub" | "md" | "txt" | "rst") {
        Kind::Document
    } else if matches!(ext, "csv" | "tsv" | "json" | "jsonl" | "yaml" | "yml" | "toml" | "xml" | "sql" | "ipynb" | "xlsx" | "parquet") {
        Kind::Data
    } else if matches!(ext, "zip" | "tar" | "gz" | "tgz" | "whl" | "jar") {
        Kind::Archive
    } else if matches!(
        ext,
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "go" | "swift" | "c" | "cc" | "cpp" | "h" | "hpp"
            | "java" | "kt" | "rb" | "php" | "sh" | "zsh" | "bash" | "css" | "scss" | "html" | "htm"
            | "svg" | "vue" | "svelte" | "diff" | "patch" | "dockerfile" | "makefile" | "lua" | "zig" | "log"
    ) {
        Kind::Code
    } else {
        Kind::Document
    }
}

/// One staged attachment: what the composer card shows (including live image thumbnail) and what goes out over ACP.
#[derive(Debug, Clone)]
pub struct Staged {
    pub name: String,
    pub size: usize,
    pub image: bool,
    pub kind: Kind,
    pub preview: Option<Arc<gpui::Image>>,
    pub attachment: Attachment,
}

impl PartialEq for Staged {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.size == other.size && self.image == other.image && self.kind == other.kind && self.attachment == other.attachment
    }
}

impl Eq for Staged {}

pub fn image_mime(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Raster formats common on macOS (Photos, Screenshot, Safari, Preview) that we convert to `image/png` automatically.
pub fn needs_png_conversion(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "heic" | "heif" | "tiff" | "tif" | "bmp" | "ico" | "avif")
}

fn gpui_format_for_mime(mime: &str) -> Option<gpui::ImageFormat> {
    match mime {
        "image/png" => Some(gpui::ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(gpui::ImageFormat::Jpeg),
        "image/gif" => Some(gpui::ImageFormat::Gif),
        "image/webp" => Some(gpui::ImageFormat::Webp),
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
        || lower.ends_with(".kdbx")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("id_ecdsa")
        || lower == ".netrc"
        || lower == ".npmrc"
        || lower == ".pypirc"
        || lower == "credentials"
        || lower == "credentials.json"
        || lower == "secrets.json"
}

pub fn image(name: &str, mime: &str, bytes: &[u8]) -> Result<Staged, String> {
    if bytes.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("{name} is over {} MB", MAX_IMAGE_BYTES / (1024 * 1024)));
    }
    let preview = gpui_format_for_mime(mime).map(|fmt| Arc::new(gpui::Image::from_bytes(fmt, bytes.to_vec())));
    Ok(Staged {
        name: name.to_owned(),
        size: bytes.len(),
        image: true,
        kind: Kind::Image,
        preview,
        attachment: Attachment::Image { mime: mime.to_owned(), data_base64: base64::engine::general_purpose::STANDARD.encode(bytes) },
    })
}

/// Stage raw clipboard image bytes in any GPUI format (converting TIFF/BMP/ICO/PNM to PNG automatically).
pub fn clipboard_image(name: &str, format: gpui::ImageFormat, bytes: &[u8]) -> Result<Staged, String> {
    match format {
        gpui::ImageFormat::Png => image(name, "image/png", bytes),
        gpui::ImageFormat::Jpeg => image(name, "image/jpeg", bytes),
        gpui::ImageFormat::Webp => image(name, "image/webp", bytes),
        gpui::ImageFormat::Gif => image(name, "image/gif", bytes),
        gpui::ImageFormat::Svg => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            Ok(Staged {
                name: name.to_owned(),
                size: bytes.len(),
                image: false,
                kind: Kind::Code,
                preview: None,
                attachment: Attachment::Text { uri: format!("clipboard://{name}"), text },
            })
        }
        gpui::ImageFormat::Tiff | gpui::ImageFormat::Bmp | gpui::ImageFormat::Ico | gpui::ImageFormat::Pnm => {
            let png = crate::platform::image_bytes_to_png(bytes).ok_or_else(|| format!("Could not decode {name} as PNG"))?;
            let png_name = Path::new(name).with_extension("png").file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| format!("{name}.png"));
            image(&png_name, "image/png", &png)
        }
    }
}

/// Convert a macOS raster image file (HEIC, TIFF, BMP, ICO, AVIF) to PNG via AppKit or `/usr/bin/sips`.
fn convert_image_file_to_png(path: &Path, name: &str) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    if raw.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if let Some(png) = crate::platform::image_bytes_to_png(&raw) {
        return Ok(png);
    }
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let tmp = std::env::temp_dir().join(format!("cordata-img-{}-{}.png", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let status = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "png"])
        .arg(path)
        .arg("--out")
        .arg(&tmp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let result = match status {
        Ok(s) if s.success() => std::fs::read(&tmp).map_err(|e| format!("{name}: {e}")),
        _ => Err(format!("Could not convert {name} to PNG")),
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

/// A dropped or chosen file, staged as an image, extracted document/PDF/archive, or UTF-8/excerpted resource.
pub fn from_path(path: &Path) -> Result<Staged, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    if looks_secret(&name) {
        return Err(format!("{name} looks like a credentials file — not attached"));
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    let file_len = meta.len() as usize;
    let lower_name = name.to_ascii_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();

    // 1. Direct web/vision images (PNG, JPEG, GIF, WebP)
    if let Some(mime) = image_mime(&ext) {
        if file_len > MAX_IMAGE_BYTES {
            return Err(format!("{name} is over {} MB", MAX_IMAGE_BYTES / (1024 * 1024)));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        return image(&name, mime, &bytes);
    }

    // 2. macOS / desktop raster images (HEIC, HEIF, TIFF, BMP, ICO, AVIF) -> auto-converted to PNG
    if needs_png_conversion(&ext) {
        if file_len > MAX_IMAGE_BYTES * 2 {
            return Err(format!("{name} is over {} MB", (MAX_IMAGE_BYTES * 2) / (1024 * 1024)));
        }
        let png = convert_image_file_to_png(path, &name)?;
        return image(&name, "image/png", &png);
    }

    if file_len > MAX_FILE_BYTES {
        return Err(format!("{name} is over {} MB", MAX_FILE_BYTES / (1024 * 1024)));
    }

    let uri = file_uri(path);

    // 3. PDF documents (native PDFKit extraction + file reference)
    if ext == "pdf" {
        let body = match crate::platform::extract_pdf_text(path) {
            Some((pages, extracted)) if !extracted.trim().is_empty() => {
                let clipped = truncate_utf8(&extracted, MAX_TEXT_BYTES);
                let note = if clipped.len() < extracted.len() { " — excerpted to 1 MB; full PDF on disk" } else { "" };
                format!("[PDF document: {} ({} page{}, {}){}]\n{}", path.display(), pages, if pages == 1 { "" } else { "s" }, size_label(file_len), note, clipped.trim())
            }
            Some((pages, _)) => format!(
                "[PDF document: {} ({} page{}, {}) — scanned or no embedded text layer; inspect via file_read or python_repl]",
                path.display(),
                pages,
                if pages == 1 { "" } else { "s" },
                size_label(file_len)
            ),
            None => format!("[PDF document: {} ({}) — inspect via file_read or python_repl]", path.display(), size_label(file_len)),
        };
        return Ok(Staged { name, size: file_len, image: false, kind: Kind::Pdf, preview: None, attachment: Attachment::Text { uri, text: body } });
    }

    // 4. Rich text & Word/OpenDocument files (.rtf, .doc, .docx, .odt) via macOS `/usr/bin/textutil`
    if matches!(ext.as_str(), "rtf" | "doc" | "docx" | "odt") {
        let extracted = extract_via_command("/usr/bin/textutil", &["-convert", "txt", "-stdout"], path);
        let body = match extracted {
            Some(txt) if !txt.trim().is_empty() => {
                let clipped = truncate_utf8(&txt, MAX_TEXT_BYTES);
                format!("[Document: {} ({})]\n{}", path.display(), size_label(file_len), clipped.trim())
            }
            _ => format!("[Document attached: {} ({}) — inspect via file_read or python_repl]", path.display(), size_label(file_len)),
        };
        return Ok(Staged { name, size: file_len, image: false, kind: Kind::Document, preview: None, attachment: Attachment::Text { uri, text: body } });
    }

    // 5. ZIP-based archives & Office workbooks/decks (.zip, .xlsx, .pptx, .epub, .jar, .whl)
    if matches!(ext.as_str(), "zip" | "xlsx" | "pptx" | "epub" | "jar" | "whl") {
        let kind = match ext.as_str() {
            "xlsx" => Kind::Data,
            "pptx" | "epub" => Kind::Document,
            _ => Kind::Archive,
        };
        let listing = extract_via_command("/usr/bin/unzip", &["-l"], path)
            .map(|out| out.lines().take(160).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let body = if listing.trim().is_empty() {
            format!("[Attached {ext} file: {} ({}) — inspect on disk via python_repl or shell]", path.display(), size_label(file_len))
        } else {
            format!("[Attached {ext} file: {} ({}) — contents listing below; inspect full contents on disk via python_repl or file_read]\n{}", path.display(), size_label(file_len), listing)
        };
        return Ok(Staged { name, size: file_len, image: false, kind, preview: None, attachment: Attachment::Text { uri, text: body } });
    }

    // 6. Tarballs (.tar, .tgz, .tar.gz, .tar.bz2, .tar.xz)
    if matches!(ext.as_str(), "tar" | "tgz") || lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tar.bz2") || lower_name.ends_with(".tar.xz") {
        let listing = extract_via_command("/usr/bin/tar", &["-tf"], path)
            .map(|out| out.lines().take(160).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let body = if listing.trim().is_empty() {
            format!("[Attached archive: {} ({}) — inspect on disk]", path.display(), size_label(file_len))
        } else {
            format!("[Attached archive: {} ({}) — entries (first 160)]\n{}", path.display(), size_label(file_len), listing)
        };
        return Ok(Staged { name, size: file_len, image: false, kind: Kind::Archive, preview: None, attachment: Attachment::Text { uri, text: body } });
    }

    // 7. Standard UTF-8 files <= 1 MB (inlined verbatim)
    let kind = kind_for_name(&name);
    if file_len <= MAX_TEXT_BYTES {
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        if !bytes.contains(&0)
            && let Ok(text) = String::from_utf8(bytes)
        {
            return Ok(Staged { name, size: text.len(), image: false, kind, preview: None, attachment: Attachment::Text { uri, text } });
        }
        // Binary or non-UTF8 file <= 1 MB: attach a structured descriptor so the agent can inspect it with tools.
        let label_ext = if ext.is_empty() { "binary" } else { ext.as_str() };
        let body = format!(
            "[Binary file attached: {} ({}, {}) — inspect on disk via file_read, python_repl, or shell]",
            path.display(),
            label_ext,
            size_label(file_len)
        );
        return Ok(Staged { name, size: file_len, image: false, kind: Kind::Binary, preview: None, attachment: Attachment::Text { uri, text: body } });
    }

    // 8. Large files (1 MB .. 50 MB): head + tail excerpt for text/logs/CSVs, or binary descriptor
    let mut file = std::fs::File::open(path).map_err(|e| format!("{name}: {e}"))?;
    let mut head_buf = vec![0u8; EXCERPT_HEAD_BYTES.min(file_len)];
    let head_n = file.read(&mut head_buf).map_err(|e| format!("{name}: {e}"))?;
    head_buf.truncate(head_n);
    if head_buf[..head_n.min(8192)].contains(&0) {
        let label_ext = if ext.is_empty() { "binary" } else { ext.as_str() };
        let body = format!(
            "[Binary file attached: {} ({}, {}) — inspect on disk via file_read, python_repl, or shell]",
            path.display(),
            label_ext,
            size_label(file_len)
        );
        return Ok(Staged { name, size: file_len, image: false, kind: Kind::Binary, preview: None, attachment: Attachment::Text { uri, text: body } });
    }
    let mut tail_buf = vec![0u8; EXCERPT_TAIL_BYTES.min(file_len)];
    let tail_start = file_len.saturating_sub(tail_buf.len()) as u64;
    let _ = file.seek(SeekFrom::Start(tail_start));
    let tail_n = file.read(&mut tail_buf).unwrap_or(0);
    tail_buf.truncate(tail_n);
    let head_str = String::from_utf8_lossy(&head_buf);
    let tail_str = String::from_utf8_lossy(&tail_buf);
    let omitted = file_len.saturating_sub(head_n + tail_n);
    let body = format!(
        "[Large file attached: {} ({}) — showing first {} and last {} ({} omitted in middle); use file_read to inspect specific ranges]\n{}\n\n… [{} omitted] …\n\n{}",
        path.display(),
        size_label(file_len),
        size_label(head_n),
        size_label(tail_n),
        size_label(omitted),
        head_str,
        size_label(omitted),
        tail_str
    );
    Ok(Staged { name, size: file_len, image: false, kind, preview: None, attachment: Attachment::Text { uri, text: body } })
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn extract_via_command(program: &str, args: &[&str], path: &Path) -> Option<String> {
    if !Path::new(program).exists() {
        return None;
    }
    let output = std::process::Command::new(program)
        .args(args)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
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
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("cordata-attach-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn files_and_images_are_checked_before_they_go() {
        let d = dir();
        std::fs::write(d.join("notes.md"), "hello").unwrap();
        let text = from_path(&d.join("notes.md")).unwrap();
        assert!(!text.image && text.kind == Kind::Document && matches!(&text.attachment, Attachment::Text { uri, text } if uri.starts_with("file://") && text == "hello"));

        std::fs::write(d.join("pic.PNG"), [0x89, b'P', b'N', b'G']).unwrap();
        let img = from_path(&d.join("pic.PNG")).unwrap();
        assert!(img.image && img.kind == Kind::Image && img.preview.is_some() && matches!(&img.attachment, Attachment::Image { mime, .. } if mime == "image/png"));

        // Credentials remain strictly refused.
        std::fs::write(d.join(".env"), "KEY=1").unwrap();
        assert!(from_path(&d.join(".env")).unwrap_err().contains("credentials"));
        std::fs::write(d.join("credentials.json"), "{}").unwrap();
        assert!(from_path(&d.join("credentials.json")).unwrap_err().contains("credentials"));

        // Binary files stage cleanly with an on-disk descriptor instead of failing.
        std::fs::write(d.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let bin = from_path(&d.join("blob.bin")).unwrap();
        assert!(!bin.image && bin.kind == Kind::Binary && matches!(&bin.attachment, Attachment::Text { text, .. } if text.contains("Binary file attached")));

        // Large UTF-8 logs/data (> MAX_TEXT_BYTES) stage with a head + tail excerpt instead of failing.
        std::fs::write(d.join("big.log"), "A".repeat(MAX_TEXT_BYTES + 4096)).unwrap();
        let big = from_path(&d.join("big.log")).unwrap();
        assert!(big.kind == Kind::Code && matches!(&big.attachment, Attachment::Text { text, .. } if text.contains("Large file attached") && text.contains("omitted")));

        // RTF document extracts readable text on macOS.
        std::fs::write(d.join("readme.rtf"), r"{\rtf1\ansi Hello from RTF document}").unwrap();
        let rtf = from_path(&d.join("readme.rtf")).unwrap();
        assert_eq!(rtf.kind, Kind::Document);
        if cfg!(target_os = "macos") {
            assert!(matches!(&rtf.attachment, Attachment::Text { text, .. } if text.contains("Hello from RTF document")));
        }

        // Minimal valid PDF stages as Kind::Pdf.
        std::fs::write(d.join("spec.pdf"), b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n").unwrap();
        let pdf = from_path(&d.join("spec.pdf")).unwrap();
        assert_eq!(pdf.kind, Kind::Pdf);

        assert!(from_path(&d).is_err(), "a folder itself is not a regular file");
        assert!(image("e.png", "image/png", &[]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn secret_names_and_sizes() {
        for name in ["id_rsa", "id_ed25519.pub", "server.pem", ".env.local", ".netrc", "secrets.json", "vault.kdbx"] {
            assert!(looks_secret(name), "{name}");
        }
        assert!(!looks_secret("main.rs"));
        assert_eq!(size_label(2048), "2 KB");
        assert_eq!(file_uri(Path::new("/a b/c.rs")), "file:///a%20b/c.rs");
        assert_eq!(kind_for_name("diagram.heic"), Kind::Image);
        assert_eq!(kind_for_name("paper.pdf"), Kind::Pdf);
        assert_eq!(kind_for_name("metrics.csv"), Kind::Data);
        assert_eq!(kind_for_name("bundle.tar.gz"), Kind::Archive);
    }
}
