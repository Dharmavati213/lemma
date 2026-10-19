//! Files and images attached to a prompt in the agent pane: the paperclip button (a file
//! chooser), files or images dropped on the pane, and images (or files copied in a file manager)
//! pasted into the prompt box. Each attachment becomes one [`PromptBlock`]:
//!
//! | What | Block |
//! |---|---|
//! | PNG, JPEG, GIF or WebP within [`MAX_IMAGE_BYTES`] and [`MAX_IMAGE_SIDE`] | `Image`, the file's bytes as they are |
//! | any other image GDK can read (BMP, TIFF, a pasted image, a larger one; files up to [`MAX_IMAGE_FILE_BYTES`]) | `Image`, re-encoded as PNG, scaled down to [`RESIZE_SIDE`] (and further) until it is within the limits |
//! | PDF up to [`MAX_EMBEDDED_DOCUMENT_BYTES`] | `Document` with its bytes |
//! | UTF-8 text without NUL, up to [`MAX_TEXT_BYTES`] | `Resource` with the text |
//! | a larger PDF or text file, any size | `Document` without bytes: a link, the agent opens the file itself |
//!
//! Links only work for ACP agents (Claude Code, Pi, Codex, …), which read files with their own
//! tools; for the API backend (`embed_only`) a file too large to include is refused. Anything
//! else is refused with a message for the user. Files are read with gio (never on the main thread
//! synchronously; a file sent as a link is only sniffed); images are decoded, scaled and encoded
//! on a worker (`gio::spawn_blocking`).

use super::PromptBlock;
use adw::prelude::*;
use std::path::Path;

/// The largest image sent: 5 MiB once base64-encoded, the Messages API's limit per image.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024 / 4 * 3;
/// The longest side the Messages API accepts.
pub const MAX_IMAGE_SIDE: u32 = 8000;
/// The longest side an image is scaled to when it has to be re-encoded smaller (larger images
/// are scaled down to this by the API anyway).
pub const RESIZE_SIDE: u32 = 1568;
/// Scaling stops here: an image that is still too large is refused.
const MIN_SIDE: u32 = 256;
/// The largest image file read (what is sent is scaled down to [`MAX_IMAGE_BYTES`]).
pub const MAX_IMAGE_FILE_BYTES: usize = 64 * 1024 * 1024;
/// The largest text file included in the prompt (about 130k tokens, most of a context window);
/// a larger one is sent as a link.
pub const MAX_TEXT_BYTES: usize = 512 * 1024;
/// The largest PDF included in the prompt: the API backend sends it base64-encoded in a request
/// of at most 32 MB. A larger one is sent as a link.
pub const MAX_EMBEDDED_DOCUMENT_BYTES: usize = 20 * 1024 * 1024;
/// How much of a file is read to tell its type.
const SNIFF_BYTES: usize = 4096;
/// The longest side of a preview.
pub const THUMBNAIL_SIDE: u32 = 160;
/// Image types sent as they are.
const SENT_AS_IS: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// One attachment waiting in the prompt box.
#[derive(Clone, Debug)]
pub struct Attachment {
    /// The file name (or "Pasted image").
    pub name: String,
    /// What it is, for the tooltip: "PNG image · 1920 × 1080 · 412 kB".
    pub detail: String,
    pub block: PromptBlock,
    /// A small preview of an image.
    pub thumbnail: Option<gdk::Texture>,
}

impl Attachment {
    /// The icon shown when there is no preview.
    pub fn icon_name(&self) -> &'static str {
        match &self.block {
            PromptBlock::Image { .. } => "image-x-generic-symbolic",
            PromptBlock::Document { mime, .. } if !mime.starts_with("text/") => {
                "x-office-document-symbolic"
            }
            _ => "text-x-generic-symbolic",
        }
    }

    /// True when the agent gets only a link to the file (it opens the file itself).
    pub fn is_link(&self) -> bool {
        matches!(&self.block, PromptBlock::Document { data, .. } if data.is_empty())
    }

    /// True when `self` and `other` carry the same file (dropping a file twice attaches it once).
    pub fn same_file(&self, other: &Attachment) -> bool {
        let uri = |b: &PromptBlock| match b {
            PromptBlock::Image { uri, .. }
            | PromptBlock::Document { uri, .. }
            | PromptBlock::Resource { uri, .. } => Some(uri.clone()),
            PromptBlock::Text(_) => None,
        };
        matches!((uri(&self.block), uri(&other.block)), (Some(a), Some(b)) if !a.is_empty() && a == b)
    }
}

/// Reads `file` and makes it an attachment (see the module docs for what is accepted).
/// `embed_only`: the agent cannot open files itself (the API backend), so a file too large to
/// include is refused instead of sent as a link.
pub async fn from_file(file: &gio::File, embed_only: bool) -> Result<Attachment, String> {
    let info = file
        .query_info_future(
            "standard::type,standard::size,standard::display-name",
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await
        .map_err(|e| format!("Could not attach {}: {e}", file.parse_name()))?;
    let name = info.display_name().to_string();
    if info.file_type() == gio::FileType::Directory {
        return Err(format!("{name} is a folder; only files can be attached"));
    }
    let size = usize::try_from(info.size()).unwrap_or(usize::MAX);
    let head = read_head(file)
        .await
        .map_err(|e| format!("Could not attach {name}: {e}"))?;
    let uri = file.uri().to_string();
    let mime = guess_mime(&name, &head);
    let format_size = |n: usize| glib::format_size(n as u64);
    if mime.starts_with("image/") && mime != "image/svg+xml" {
        if size > MAX_IMAGE_FILE_BYTES {
            return Err(format!(
                "{name} is too large to attach ({}; images up to {})",
                format_size(size),
                format_size(MAX_IMAGE_FILE_BYTES)
            ));
        }
        let data = load(file, &name).await?;
        let prepared = gio::spawn_blocking({
            let mime = mime.clone();
            move || prepare_image(data, &mime)
        })
        .await
        .map_err(|_| format!("Could not read the image {name}"))?
        .map_err(|e| format!("Could not attach the image {name}: {e}"))?;
        return Ok(prepared.into_attachment(name, uri));
    }
    if mime == "application/pdf" {
        if size <= MAX_EMBEDDED_DOCUMENT_BYTES {
            let data = load(file, &name).await?;
            let detail = format!("PDF document · {}", format_size(data.len()));
            return Ok(Attachment {
                name,
                detail,
                block: PromptBlock::Document { uri, mime, data },
                thumbnail: None,
            });
        }
        if embed_only {
            return Err(format!(
                "{name} is too large for the Anthropic API ({}; it takes PDFs up to {}). \
                 The ACP agents (Claude Code, Codex, …) take PDFs of any size.",
                format_size(size),
                format_size(MAX_EMBEDDED_DOCUMENT_BYTES)
            ));
        }
        return Ok(link(name, uri, mime, size, "PDF document"));
    }
    if size <= MAX_TEXT_BYTES {
        let data = load(file, &name).await?;
        return match text_of(data) {
            Some(text) => {
                let detail = format!("Text · {}", format_size(text.len()));
                Ok(Attachment {
                    block: PromptBlock::Resource {
                        uri,
                        mime: text_mime(&name, &mime).to_owned(),
                        text,
                    },
                    name,
                    detail,
                    thumbnail: None,
                })
            }
            None => Err(not_supported(&name)),
        };
    }
    if !looks_like_text(&head) {
        return Err(not_supported(&name));
    }
    if embed_only {
        return Err(format!(
            "{name} is too large for the Anthropic API ({}; it takes text files up to {}). \
             The ACP agents (Claude Code, Codex, …) take text files of any size.",
            format_size(size),
            format_size(MAX_TEXT_BYTES)
        ));
    }
    let mime = text_mime(&name, &mime).to_owned();
    Ok(link(name, uri, mime, size, "Text"))
}

fn not_supported(name: &str) -> String {
    format!("{name} is not a text file, an image or a PDF; it was not attached")
}

/// An attachment sent as a link to the file (no bytes): ACP agents open it themselves.
fn link(name: String, uri: String, mime: String, size: usize, kind: &str) -> Attachment {
    Attachment {
        detail: format!(
            "{kind} · {} · sent as a link: the agent opens the file itself",
            glib::format_size(size as u64)
        ),
        name,
        block: PromptBlock::Document {
            uri,
            mime,
            data: Vec::new(),
        },
        thumbnail: None,
    }
}

/// The whole file.
async fn load(file: &gio::File, name: &str) -> Result<Vec<u8>, String> {
    let (bytes, _) = file
        .load_contents_future()
        .await
        .map_err(|e| format!("Could not attach {name}: {e}"))?;
    Ok(bytes.to_vec())
}

/// The first [`SNIFF_BYTES`] of the file (fewer when it is shorter).
async fn read_head(file: &gio::File) -> Result<Vec<u8>, glib::Error> {
    let stream = file.read_future(glib::Priority::DEFAULT).await?;
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    while head.len() < SNIFF_BYTES {
        let bytes = stream
            .read_bytes_future(SNIFF_BYTES - head.len(), glib::Priority::DEFAULT)
            .await?;
        if bytes.is_empty() {
            break;
        }
        head.extend_from_slice(&bytes);
    }
    let _ = stream.close_future(glib::Priority::DEFAULT).await;
    Ok(head)
}

/// True when `head`, the start of a file, is UTF-8 without NUL (a character cut at the end is
/// allowed).
fn looks_like_text(head: &[u8]) -> bool {
    !head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            Err(e) => e.error_len().is_none(),
        }
}

/// Makes a pasted image an attachment (PNG, scaled down when it would exceed the limits).
pub async fn from_texture(texture: gdk::Texture) -> Result<Attachment, String> {
    let prepared = gio::spawn_blocking(move || {
        let thumbnail = scaled(&texture, THUMBNAIL_SIDE);
        let long = side(texture.width()).max(side(texture.height()));
        let (data, size) = png_within_limits(&texture, long.min(MAX_IMAGE_SIDE))?;
        Ok::<_, String>(Prepared {
            data,
            mime: "image/png".into(),
            size,
            original: (size.0.max(size.1) < long)
                .then(|| (side(texture.width()), side(texture.height()))),
            thumbnail: Some(thumbnail),
        })
    })
    .await
    .map_err(|_| "Could not read the pasted image".to_owned())??;
    Ok(prepared.into_attachment("Pasted image".into(), String::new()))
}

/// A preview of the encoded image `data` (the transcript's), made on a worker.
pub async fn thumbnail(data: Vec<u8>) -> Option<gdk::Texture> {
    gio::spawn_blocking(move || {
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)).ok()?;
        Some(scaled(&texture, THUMBNAIL_SIDE))
    })
    .await
    .ok()
    .flatten()
}

/// The MIME type of a file named `name` with contents `data`.
pub fn guess_mime(name: &str, data: &[u8]) -> String {
    let head = &data[..data.len().min(4096)];
    let (content_type, _) = gio::content_type_guess(Some(Path::new(name)), head);
    gio::content_type_get_mime_type(&content_type)
        .map(|m| m.to_string())
        .unwrap_or_else(|| content_type.to_string())
}

/// `data` as text: valid UTF-8 without NUL bytes.
fn text_of(data: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(data).ok()?;
    (!text.contains('\0')).then_some(text)
}

/// The MIME type a text attachment is sent with.
fn text_mime<'a>(name: &str, guessed: &'a str) -> &'a str {
    match super::panel::mime_for(Path::new(name)) {
        "text/plain"
            if guessed.starts_with("text/")
                || guessed.ends_with("json")
                || guessed.ends_with("xml") =>
        {
            guessed
        }
        mime => mime,
    }
}

/// An image ready to send.
struct Prepared {
    data: Vec<u8>,
    mime: String,
    /// Its size as sent.
    size: (u32, u32),
    /// The original size when it was scaled down.
    original: Option<(u32, u32)>,
    thumbnail: Option<gdk::Texture>,
}

impl Prepared {
    fn into_attachment(self, name: String, uri: String) -> Attachment {
        let kind = self
            .mime
            .strip_prefix("image/")
            .unwrap_or("image")
            .to_uppercase();
        let mut detail = format!(
            "{kind} image · {} × {} · {}",
            self.size.0,
            self.size.1,
            glib::format_size(self.data.len() as u64)
        );
        if let Some((w, h)) = self.original {
            detail.push_str(&format!(" (scaled down from {w} × {h})"));
        }
        Attachment {
            name,
            detail,
            block: PromptBlock::Image {
                uri,
                mime: self.mime,
                data: self.data,
            },
            thumbnail: self.thumbnail,
        }
    }
}

/// Decodes the image file `data` of type `mime` and decides what to send. **Blocking.**
fn prepare_image(data: Vec<u8>, mime: &str) -> Result<Prepared, String> {
    let bytes = glib::Bytes::from_owned(data);
    let texture = gdk::Texture::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let (w, h) = (side(texture.width()), side(texture.height()));
    let thumbnail = Some(scaled(&texture, THUMBNAIL_SIDE));
    let long = w.max(h);
    if SENT_AS_IS.contains(&mime) && bytes.len() <= MAX_IMAGE_BYTES && long <= MAX_IMAGE_SIDE {
        return Ok(Prepared {
            data: bytes.to_vec(),
            mime: mime.to_owned(),
            size: (w, h),
            original: None,
            thumbnail,
        });
    }
    // A file that is too large as it is would be larger still as a PNG of the same size.
    let start = if bytes.len() > MAX_IMAGE_BYTES {
        long.min(RESIZE_SIDE)
    } else {
        long.min(MAX_IMAGE_SIDE)
    };
    let (data, size) = png_within_limits(&texture, start)?;
    Ok(Prepared {
        data,
        mime: "image/png".into(),
        size,
        original: (size.0.max(size.1) < long).then_some((w, h)),
        thumbnail,
    })
}

/// `texture` as a PNG of at most [`MAX_IMAGE_BYTES`], its longest side `start` or less: tries
/// `start`, then [`RESIZE_SIDE`], then ¾ of the side each time down to [`MIN_SIDE`]. **Blocking.**
fn png_within_limits(texture: &gdk::Texture, start: u32) -> Result<(Vec<u8>, (u32, u32)), String> {
    let mut long = start.max(1);
    loop {
        let image = scaled(texture, long);
        let png = image.save_to_png_bytes();
        if png.len() <= MAX_IMAGE_BYTES {
            return Ok((png.to_vec(), (side(image.width()), side(image.height()))));
        }
        if long <= MIN_SIDE {
            return Err("it is too large, even scaled down".into());
        }
        long = if long > RESIZE_SIDE {
            RESIZE_SIDE
        } else {
            (long * 3 / 4).max(MIN_SIDE)
        };
    }
}

fn side(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// `texture` scaled down (box filter) so that its longest side is at most `max_side`; the
/// texture itself when it is that small already. **Blocking** for large textures.
pub fn scaled(texture: &gdk::Texture, max_side: u32) -> gdk::Texture {
    let (w, h) = (side(texture.width()), side(texture.height()));
    let (nw, nh) = fit(w, h, max_side);
    if (nw, nh) == (w, h) || w == 0 || h == 0 {
        return texture.clone();
    }
    let mut downloader = gdk::TextureDownloader::new(texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8Premultiplied);
    let (pixels, stride) = downloader.download_bytes();
    let out = downscale_rgba(&pixels, stride, w, h, nw, nh);
    gdk::MemoryTexture::new(
        i32::try_from(nw).unwrap_or(i32::MAX),
        i32::try_from(nh).unwrap_or(i32::MAX),
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &glib::Bytes::from_owned(out),
        nw as usize * 4,
    )
    .upcast()
}

/// The size of a `w` × `h` image scaled down (never up) to fit `max_side`, keeping the aspect
/// ratio; each side at least 1.
pub fn fit(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_side || long == 0 {
        return (w, h);
    }
    let scale = |n: u32| {
        u32::try_from((u64::from(n) * u64::from(max_side) + u64::from(long) / 2) / u64::from(long))
            .unwrap_or(max_side)
            .max(1)
    };
    (scale(w), scale(h))
}

/// Scales 4-byte pixels (`w` × `h`, rows `stride` bytes apart) down to `nw` × `nh` by averaging
/// the source pixels each target pixel covers. The result has rows of `nw * 4` bytes.
pub fn downscale_rgba(src: &[u8], stride: usize, w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    let (w, h, nw, nh) = (
        w as usize,
        h as usize,
        nw.max(1) as usize,
        nh.max(1) as usize,
    );
    // The source span [start, end) of each target column / row.
    let spans = |n: usize, total: usize| -> Vec<(usize, usize)> {
        (0..n)
            .map(|i| {
                let start = i * total / n;
                let end = ((i + 1) * total / n).max(start + 1).min(total);
                (start, end)
            })
            .collect()
    };
    let columns = spans(nw, w);
    let rows = spans(nh, h);
    let mut out = vec![0u8; nw * nh * 4];
    let mut sums = vec![0u64; nw * 4];
    for (ty, &(y0, y1)) in rows.iter().enumerate() {
        sums.fill(0);
        for y in y0..y1 {
            let row = &src[y * stride..y * stride + w * 4];
            for (tx, &(x0, x1)) in columns.iter().enumerate() {
                let sum = &mut sums[tx * 4..tx * 4 + 4];
                for pixel in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    for (s, &p) in sum.iter_mut().zip(pixel) {
                        *s += u64::from(p);
                    }
                }
            }
        }
        for (tx, &(x0, x1)) in columns.iter().enumerate() {
            let area = ((x1 - x0) * (y1 - y0)) as u64;
            let target = &mut out[(ty * nw + tx) * 4..(ty * nw + tx) * 4 + 4];
            for (t, &s) in target.iter_mut().zip(&sums[tx * 4..tx * 4 + 4]) {
                *t = u8::try_from((s + area / 2) / area).unwrap_or(u8::MAX);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_the_aspect_ratio_and_never_enlarges() {
        assert_eq!(fit(4000, 3000, 1568), (1568, 1176));
        assert_eq!(fit(3000, 4000, 1568), (1176, 1568));
        assert_eq!(fit(800, 600, 1568), (800, 600));
        assert_eq!(fit(10_000, 1, 100), (100, 1));
        assert_eq!(fit(0, 0, 100), (0, 0));
    }

    #[test]
    fn downscale_averages_the_covered_pixels() {
        // 4 × 2 → 2 × 1: each target pixel averages a 2 × 2 block.
        #[rustfmt::skip]
        let src: Vec<u8> = vec![
            0, 0, 0, 255,      100, 100, 100, 255,   10, 20, 30, 40,   10, 20, 30, 40,
            200, 200, 200, 255, 100, 100, 100, 255,  10, 20, 30, 40,   10, 20, 30, 40,
        ];
        let out = downscale_rgba(&src, 16, 4, 2, 2, 1);
        assert_eq!(out, [100, 100, 100, 255, 10, 20, 30, 40]);
        // A stride wider than the row is honoured.
        let mut padded = Vec::new();
        for row in src.chunks(16) {
            padded.extend_from_slice(row);
            padded.extend_from_slice(&[9; 8]);
        }
        assert_eq!(downscale_rgba(&padded, 24, 4, 2, 2, 1), out);
        // Uneven ratios cover every source pixel once.
        let wide = vec![60u8; 5 * 3 * 4];
        assert_eq!(downscale_rgba(&wide, 20, 5, 3, 2, 2), vec![60u8; 16]);
    }

    #[test]
    fn the_start_of_a_text_file_looks_like_text() {
        assert!(looks_like_text(b"\\documentclass{article}"));
        // A multibyte character cut by the end of the sniffed bytes.
        assert!(looks_like_text(&"é".as_bytes()[..1]));
        assert!(!looks_like_text(&[b'a', 0xff, b'b']));
        assert!(!looks_like_text(b"PK\x03\x04\0\0"));
    }

    #[test]
    fn text_needs_utf8_without_nul() {
        assert_eq!(
            text_of(b"\\section{A}".to_vec()).as_deref(),
            Some("\\section{A}")
        );
        assert_eq!(text_of(vec![0xff, 0xfe]), None);
        assert_eq!(text_of(b"a\0b".to_vec()), None);
    }

    #[test]
    fn text_attachments_keep_a_useful_mime_type() {
        assert_eq!(text_mime("main.tex", "text/x-tex"), "text/x-tex");
        assert_eq!(text_mime("refs.bib", "text/x-bibtex"), "text/x-bibtex");
        assert_eq!(text_mime("data.csv", "text/csv"), "text/csv");
        assert_eq!(text_mime("notes", "application/octet-stream"), "text/plain");
        assert_eq!(text_mime("a.json", "application/json"), "application/json");
    }
}
