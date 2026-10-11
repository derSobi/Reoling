//! The picture shown for a device in its settings: Reolink's own product
//! photo for that model, fetched from Reolink's CDN (as the official app
//! does) and cached on disk, with a drawing of our own per device category
//! while it loads, or when there is no photo for that model.

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::glib;
use std::path::PathBuf;
use std::time::Duration;

/// What kind of device a picture stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Camera,
    Nvr,
    Hub,
}

impl Category {
    /// From the device's own model or type string (`nvr`, `homehub`, `RLN36`,
    /// `Home Hub Pro`…); a camera otherwise.
    pub fn from_model(model: &str) -> Category {
        let m = model.to_lowercase();
        if m.contains("hub") {
            Category::Hub
        } else if m.contains("nvr") || m.starts_with("rln") {
            Category::Nvr
        } else {
            Category::Camera
        }
    }

    /// Our own drawing for this category (never Reolink's artwork).
    pub fn placeholder(self) -> gdk::Texture {
        let svg = match self {
            Category::Camera => CAMERA_SVG,
            Category::Nvr => NVR_SVG,
            Category::Hub => HUB_SVG,
        };
        gdk::Texture::from_bytes(&glib::Bytes::from_static(svg.as_bytes()))
            .expect("bundled placeholder SVGs are valid")
    }
}

/// Where Reolink keeps its product photos, keyed by the model string.
fn photo_url(model: &str) -> String {
    let encoded: String = model
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://home-cdn.reolink.us/wp-content/assets/app/model-images/{encoded}/product.png")
}

fn cache_dir() -> PathBuf {
    glib::user_cache_dir().join("reoling").join("model-images")
}

fn cache_name(model: &str) -> String {
    model.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// How long a "no photo for this model" answer is trusted before asking
/// Reolink's CDN again.
const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The model's photo: from the cache, else downloaded (blocking — call it
/// off the GTK thread). `None` when Reolink has no photo for it.
fn fetch_blocking(model: &str) -> Option<Vec<u8>> {
    let dir = cache_dir();
    let png = dir.join(format!("{}.png", cache_name(model)));
    let miss = dir.join(format!("{}.miss", cache_name(model)));
    if let Ok(bytes) = std::fs::read(&png) {
        return Some(bytes);
    }
    if let Ok(meta) = std::fs::metadata(&miss) {
        if meta.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < MISS_TTL) {
            return None;
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build();
    let fetched = agent.get(&photo_url(model)).call().ok().and_then(|response| {
        let is_png = response.content_type() == "image/png";
        let mut bytes = Vec::new();
        use std::io::Read;
        response.into_reader().take(4 * 1024 * 1024).read_to_end(&mut bytes).ok()?;
        (is_png && !bytes.is_empty()).then_some(bytes)
    });
    match &fetched {
        Some(bytes) => {
            let _ = std::fs::write(&png, bytes);
        }
        None => {
            let _ = std::fs::write(&miss, b"");
        }
    }
    fetched
}

/// Shows `category`'s drawing in `picture` right away, then the model's
/// photo once it is available — scaled to the image's pixel size, whatever
/// size the photo itself is.
pub fn show(picture: &gtk4::Image, model: &str, category: Category) {
    picture.set_paintable(Some(&category.placeholder()));
    let model = model.trim().to_string();
    if model.is_empty() {
        return;
    }
    let (tx, rx) = async_channel::bounded::<Option<Vec<u8>>>(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(fetch_blocking(&model));
    });
    let picture = picture.downgrade();
    glib::spawn_future_local(async move {
        let Ok(Some(bytes)) = rx.recv().await else { return };
        let Some(picture) = picture.upgrade() else { return };
        if let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
            picture.set_paintable(Some(&texture));
        }
    });
}

const CAMERA_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
<rect x="4" y="4" width="120" height="120" rx="24" fill="#8c95a1" fill-opacity="0.16"/>
<path d="M30 52 L82 40 Q92 38 94 48 L97 64 Q99 74 89 76 L37 88 Q29 90 27 82 L24 64 Q22 54 30 52 Z" fill="#6b7480"/>
<circle cx="86" cy="58" r="9" fill="#2e333a"/>
<circle cx="86" cy="58" r="4" fill="#9aa4b1"/>
<path d="M50 86 L54 98 L44 104 L70 104 L66 94 L62 84 Z" fill="#6b7480"/>
</svg>"##;

const NVR_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
<rect x="4" y="4" width="120" height="120" rx="24" fill="#8c95a1" fill-opacity="0.16"/>
<rect x="18" y="46" width="92" height="38" rx="6" fill="#6b7480"/>
<rect x="18" y="80" width="92" height="4" fill="#2e333a" fill-opacity="0.35"/>
<rect x="28" y="57" width="34" height="5" rx="2" fill="#2e333a"/>
<rect x="28" y="67" width="34" height="5" rx="2" fill="#2e333a"/>
<circle cx="92" cy="64" r="4" fill="#9fd08b"/>
<circle cx="80" cy="64" r="3" fill="#2e333a"/>
</svg>"##;

const HUB_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
<rect x="4" y="4" width="120" height="120" rx="24" fill="#8c95a1" fill-opacity="0.16"/>
<rect x="34" y="30" width="60" height="68" rx="14" fill="#6b7480"/>
<circle cx="64" cy="56" r="11" fill="none" stroke="#2e333a" stroke-width="4"/>
<path d="M50 80 Q64 72 78 80" fill="none" stroke="#2e333a" stroke-width="4" stroke-linecap="round"/>
<circle cx="64" cy="88" r="3" fill="#9fd08b"/>
</svg>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_categories_follow_the_model_string() {
        assert_eq!(
            photo_url("E1 Outdoor PoE"),
            "https://home-cdn.reolink.us/wp-content/assets/app/model-images/E1%20Outdoor%20PoE/product.png"
        );
        assert_eq!(Category::from_model("RLN36"), Category::Nvr);
        assert_eq!(Category::from_model("nvr"), Category::Nvr);
        assert_eq!(Category::from_model("Home Hub Pro"), Category::Hub);
        assert_eq!(Category::from_model("RLC-1224A"), Category::Camera);
        assert_eq!(cache_name("E1 Outdoor PoE"), "E1_Outdoor_PoE");
    }
}
