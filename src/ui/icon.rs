//! The app icon. Colours and widget styling are left entirely to the
//! active GTK theme (including its accent colour); Reoling ships no
//! stylesheet of its own. For the AppImage, this is also where the
//! `.AppImage` file itself gets its icon in the file manager.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use gtk4::gdk::Display;
use md5::{Digest, Md5};

/// The icon name shipped in `data/icons/hicolor/*/apps/`; also the
/// application id, so desktop environments match the window to the icon.
pub const ICON_NAME: &str = "de.dersobi.reoling";

/// Makes the app icon available and sets it as every window's icon. Call
/// once the display exists (from the application's `startup` handler).
///
/// Installed builds find the icon through the system icon theme. When
/// running from a source checkout (`cargo run`) nothing is installed, so
/// this also looks for `data/icons` a few directories above the executable
/// (`target/{debug,release}/reoling` → the repository root), or in the
/// directory named by `REOLING_ICON_PATH`.
pub fn install() {
    let Some(display) = Display::default() else {
        return;
    };
    let theme = gtk4::IconTheme::for_display(&display);
    if let Some(dir) = std::env::var_os("REOLING_ICON_PATH") {
        theme.add_search_path(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(4) {
            let candidate = ancestor.join("data/icons");
            if candidate.join("hicolor").is_dir() {
                theme.add_search_path(candidate);
                break;
            }
        }
    }
    gtk4::Window::set_default_icon_name(ICON_NAME);
}

/// A source checkout (`cargo run`) has no installed desktop entry, and the
/// dock then shows the bare application id and no icon. Writes a per-user
/// entry (named "Reoling", pointing at this executable and the checkout's
/// icon) unless the system already has one; an installed build ships its own.
pub fn install_desktop_entry() {
    let file = "de.dersobi.reoling.desktop";
    let system_has_it = glib::system_data_dirs().iter().any(|d| d.join("applications").join(file).is_file());
    let Ok(exe) = std::env::current_exe() else { return };
    if system_has_it || !exe.ancestors().any(|a| a.join("data/icons/hicolor").is_dir()) {
        return;
    }
    let icon = exe
        .ancestors()
        .map(|a| a.join("data/icons/hicolor/256x256/apps/de.dersobi.reoling.png"))
        .find(|p| p.is_file())
        .map_or_else(|| ICON_NAME.to_string(), |p| p.display().to_string());
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Reoling\nGenericName=Reolink client\nComment=Unofficial client for Reolink cameras, NVRs and Home Hubs\nExec=\"{}\"\nIcon={icon}\nTerminal=false\nCategories=AudioVideo;Video;\nStartupWMClass={ICON_NAME}\n",
        exe.display()
    );
    let path = glib::user_data_dir().join("applications").join(file);
    if std::fs::read_to_string(&path).is_ok_and(|current| current == entry) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, entry) {
        eprintln!("could not write the desktop entry: {e}");
    }
}

/// Started from an AppImage, the file manager shows the `.AppImage` file
/// with the generic "executable" icon: nothing on a stock desktop reads the
/// icon an AppImage embeds, and the icon themes have none for its MIME
/// type. This writes the thumbnail-cache entries for this very file
/// (`~/.cache/thumbnails/{large,x-large}/<md5 of the file URI>.png`, see
/// [`thumbnail_sources`]) from the icons inside the mounted AppImage, and
/// drops a thumbnailing failure the desktop recorded for it. GNOME Files,
/// and every file manager that reads the shared cache, then shows the
/// Reoling icon for the file from the first run on, with no thumbnailer
/// involved. For this file only: Reoling installs no thumbnailer, which
/// the desktop would run on every AppImage the user so much as views in a
/// folder.
///
/// The entries are written only when the AppImage runtime set `APPIMAGE`
/// (the file's path) and `APPDIR` (its mount point), so the .deb and a
/// source build never do this, and again when the file was replaced (its
/// modification time changed) or moved (the entries are keyed by the
/// path); otherwise a second start rewrites nothing. Errors are printed
/// and otherwise ignored; the worst case is today's generic icon.
pub fn install_appimage_integration() {
    let (Some(appimage), Some(appdir)) = (std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR")) else {
        return;
    };
    let (appimage, appdir) = (PathBuf::from(appimage), PathBuf::from(appdir));
    let thumbnails = glib::user_cache_dir().join("thumbnails");
    // The runtime resolves every symbolic link for `APPIMAGE`, but the file
    // manager keys the thumbnail by the path it shows. A file started
    // through another path (a folder reached through a link) gets an entry
    // for that path too: `ARGV0` is the path as invoked, `OWD` the
    // directory it is relative to.
    let mut paths = vec![appimage.clone()];
    if let Some(invoked) = std::env::var_os("ARGV0").and_then(|argv0| invoked_path(&argv0, std::env::var_os("OWD"))) {
        if invoked != appimage && same_file(&invoked, &appimage) {
            paths.push(invoked);
        }
    }
    for path in &paths {
        if let Err(e) = seed_own_thumbnail(path, &appdir, &thumbnails) {
            eprintln!("could not write the AppImage's file icon to {}: {e}", thumbnails.display());
        }
    }
}

/// The absolute path a program was started by: `argv0`, relative to `owd`
/// (the working directory) unless absolute, with `.` and `..` folded
/// lexically and no link resolved, so it names the file the way the file
/// manager does. `None` when that gives no absolute path.
fn invoked_path(argv0: &OsStr, owd: Option<OsString>) -> Option<PathBuf> {
    let argv0 = Path::new(argv0);
    let joined = if argv0.is_absolute() { argv0.to_path_buf() } else { PathBuf::from(owd?).join(argv0) };
    let mut path = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            other => path.push(other),
        }
    }
    path.is_absolute().then_some(path)
}

/// Whether two paths name the same file (same device and inode).
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The icon inside the AppDir for each thumbnail size: the cache directory
/// name and the source, relative to `APPDIR`. Of the sizes the thumbnail
/// spec names (normal 128 px, large 256, x-large 512, xx-large 1024), the
/// two the icons cover: `.DirIcon` is the 256 px icon every AppImage has
/// at its root, the 512 px one the largest the icon theme files have. GIO
/// resolves `thumbnail::path`, the one attribute GNOME Files asks for, to
/// the largest entry present, and Files scales that to its icon size, so
/// a `normal` entry would never be looked at next to these.
fn thumbnail_sources() -> [(&'static str, PathBuf); 2] {
    [
        ("large", PathBuf::from(".DirIcon")),
        ("x-large", PathBuf::from(format!("usr/share/icons/hicolor/512x512/apps/{ICON_NAME}.png"))),
    ]
}

/// Writes the thumbnail-cache entries for the AppImage file at `appimage`
/// into `thumbnails` (`~/.cache/thumbnails`) from the icons in `appdir`,
/// unless they are already there for the file's current modification
/// time; drops a thumbnailing failure recorded for the file; and, when
/// something was written, re-sets the file's own modification time so a
/// file manager showing it re-reads its icon at once. An entry that cannot
/// be written is reported and leaves the rest to go on.
fn seed_own_thumbnail(appimage: &Path, appdir: &Path, thumbnails: &Path) -> io::Result<()> {
    let uri = glib::filename_to_uri(appimage, None).map_err(io::Error::other)?;
    let key = thumbnail_key(&uri);
    let modified = fs::metadata(appimage)?.modified()?;
    let mtime = modified.duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_secs();
    let mut written = false;
    for (size, icon) in thumbnail_sources() {
        let source = appdir.join(icon);
        if !source.is_file() {
            continue;
        }
        let entry = thumbnails.join(size).join(format!("{key}.png"));
        if fs::read(&entry).is_ok_and(|png| thumbnail_is_current(&png, &uri, mtime)) {
            continue;
        }
        let result = fs::read(&source)
            .and_then(|icon| thumbnail_png(&icon, &uri, mtime))
            .and_then(|png| write_private(&entry, &png));
        match result {
            Ok(()) => written = true,
            Err(e) => eprintln!("could not write {}: {e}", entry.display()),
        }
    }
    // A failure some thumbnailer recorded for this file and modification
    // time would keep the file manager from looking again; the entries
    // above are the answer.
    let _ = fs::remove_file(thumbnails.join("fail/gnome-thumbnail-factory").join(format!("{key}.png")));
    if written {
        // Setting the modification time to its current value changes
        // nothing about the file but raises a change notification, on
        // which the file manager re-queries the file and finds the entry;
        // without it the icon appears when the folder is next read. Opened
        // read-only: a running executable cannot be opened for writing.
        if let Ok(file) = File::open(appimage) {
            let _ = file.set_modified(modified);
        }
    }
    Ok(())
}

/// The name of a file's thumbnail-cache entry: the MD5 of its URI, as
/// `g_file_get_uri()` spells it.
fn thumbnail_key(uri: &str) -> String {
    format!("{:x}", Md5::digest(uri.as_bytes()))
}

/// Creates `path`'s directories as the thumbnail spec asks (0700) and the
/// file (0600) through a temporary name in the same directory, so a reader
/// never sees a half-written entry.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent directory"))?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// `icon` with the metadata the thumbnail spec asks for, as tEXt chunks
/// right after IHDR: `Thumb::URI` and `Thumb::MTime` (what GIO compares
/// with the file to call the entry valid) and `Software`. The image data
/// is left as it is.
fn thumbnail_png(icon: &[u8], uri: &str, mtime: u64) -> io::Result<Vec<u8>> {
    // IHDR is always the first chunk and 13 bytes long: 8 (signature) + 4
    // (length) + 4 (type) + 13 + 4 (CRC).
    const IHDR_END: usize = 33;
    if icon.len() < IHDR_END || icon[..8] != PNG_SIGNATURE || &icon[12..16] != b"IHDR" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a PNG image"));
    }
    let mut png = Vec::with_capacity(icon.len() + 160);
    png.extend_from_slice(&icon[..IHDR_END]);
    for (key, value) in [("Thumb::URI", uri), ("Thumb::MTime", &mtime.to_string()), ("Software", "Reoling")] {
        let mut data = Vec::with_capacity(key.len() + 1 + value.len());
        data.extend_from_slice(key.as_bytes());
        data.push(0);
        data.extend_from_slice(value.as_bytes());
        png_chunk(&mut png, b"tEXt", &data);
    }
    png.extend_from_slice(&icon[IHDR_END..]);
    Ok(png)
}

/// Appends one PNG chunk: length, type, data, CRC-32 of type and data.
fn png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    png.extend_from_slice(&crc.finalize().to_be_bytes());
}

/// The tEXt entries (keyword, text) of a PNG, in order; empty for
/// anything that is not a PNG.
fn png_text_chunks(png: &[u8]) -> Vec<(String, String)> {
    let mut texts = Vec::new();
    if png.len() < 8 || png[..8] != PNG_SIGNATURE {
        return texts;
    }
    let mut at = 8;
    while let Some(head) = png.get(at..at + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = &head[4..8];
        let Some(data) = png.get(at + 8..at + 8 + len) else { break };
        if kind == b"tEXt" {
            if let Some(nul) = data.iter().position(|&b| b == 0) {
                let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
                texts.push((text(&data[..nul]), text(&data[nul + 1..])));
            }
        }
        if kind == b"IEND" {
            break;
        }
        at += 12 + len;
    }
    texts
}

/// Whether a cache entry is the one GIO calls valid for the file: its
/// `Thumb::URI` and `Thumb::MTime` are the file's.
fn thumbnail_is_current(png: &[u8], uri: &str, mtime: u64) -> bool {
    let texts = png_text_chunks(png);
    let has = |key: &str, value: &str| texts.iter().any(|(k, v)| k == key && v == value);
    has("Thumb::URI", uri) && has("Thumb::MTime", &mtime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// A 1x1 transparent RGBA PNG: signature, IHDR, IDAT, IEND.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
        0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0xe9, 0xfa,
        0xdc, 0xd8, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    const URI: &str = "file:///home/user/Downloads/Reoling-1.0.0-x86_64.AppImage";

    /// A fresh, empty directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reoling-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The chunks (type, data) of a PNG, each one's CRC checked.
    fn chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(png[..8], PNG_SIGNATURE);
        let mut out = Vec::new();
        let mut at = 8;
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let data = &png[at + 8..at + 8 + len];
            let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(kind);
            hasher.update(data);
            assert_eq!(crc, hasher.finalize(), "CRC of chunk {}", String::from_utf8_lossy(kind));
            out.push((String::from_utf8_lossy(kind).into_owned(), data.to_vec()));
            at += 12 + len;
        }
        assert_eq!(at, png.len());
        out
    }

    #[test]
    fn thumbnail_png_adds_the_metadata_after_ihdr() {
        let png = thumbnail_png(TINY_PNG, URI, 1_761_300_000).unwrap();
        let chunks = chunks(&png);
        let kinds: Vec<&str> = chunks.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, ["IHDR", "tEXt", "tEXt", "tEXt", "IDAT", "IEND"]);
        assert_eq!(chunks[0].1, TINY_PNG[16..29]);
        assert_eq!(chunks[1].1, b"Thumb::URI\0file:///home/user/Downloads/Reoling-1.0.0-x86_64.AppImage");
        assert_eq!(chunks[2].1, b"Thumb::MTime\x001761300000");
        assert_eq!(chunks[3].1, b"Software\0Reoling");
        // The image itself is byte for byte the icon's.
        assert_eq!(png[png.len() - (TINY_PNG.len() - 33)..], TINY_PNG[33..]);
        let text = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            png_text_chunks(&png),
            [text("Thumb::URI", URI), text("Thumb::MTime", "1761300000"), text("Software", "Reoling")]
        );
        assert!(thumbnail_is_current(&png, URI, 1_761_300_000));
        assert!(!thumbnail_is_current(&png, URI, 1_761_300_001));
        assert!(!thumbnail_is_current(&png, "file:///home/user/Reoling-1.0.0-x86_64.AppImage", 1_761_300_000));
        assert!(!thumbnail_is_current(TINY_PNG, URI, 1_761_300_000));
        assert!(png_text_chunks(b"\x7fELF").is_empty());
    }

    #[test]
    fn thumbnail_png_wants_a_png() {
        assert!(thumbnail_png(b"\x7fELF, an executable rather than a picture", URI, 1).is_err());
        assert!(thumbnail_png(&TINY_PNG[..20], URI, 1).is_err());
    }

    #[test]
    fn thumbnail_key_is_the_md5_of_the_uri() {
        // printf 'file:///home/user/Downloads/Reoling-1.0.0-x86_64.AppImage' | md5sum
        assert_eq!(thumbnail_key(URI), "c01434f1a7469376485416299c013ecf");
    }

    #[test]
    fn seed_writes_the_entries_once_and_drops_a_recorded_failure() {
        let dir = scratch("thumbnail-seed");
        let appdir = dir.join("appdir");
        for icon in thumbnail_sources().map(|(_, icon)| icon) {
            fs::create_dir_all(appdir.join(&icon).parent().unwrap()).unwrap();
            fs::write(appdir.join(&icon), TINY_PNG).unwrap();
        }
        let appimage = dir.join("Reoling-1.0.0-x86_64.AppImage");
        fs::write(&appimage, b"\x7fELF").unwrap();
        let thumbnails = dir.join("thumbnails");
        let uri = glib::filename_to_uri(&appimage, None).unwrap();
        let key = thumbnail_key(&uri);
        let failed = thumbnails.join("fail/gnome-thumbnail-factory").join(format!("{key}.png"));
        fs::create_dir_all(failed.parent().unwrap()).unwrap();
        fs::write(&failed, TINY_PNG).unwrap();

        let mtime = |path: &Path| fs::metadata(path).unwrap().modified().unwrap();
        let seconds = |path: &Path| mtime(path).duration_since(UNIX_EPOCH).unwrap().as_secs();
        let entries = || thumbnail_sources().map(|(size, _)| thumbnails.join(size).join(format!("{key}.png")));
        let inodes = || entries().map(|entry| fs::metadata(entry).unwrap().ino());
        let before = mtime(&appimage);

        seed_own_thumbnail(&appimage, &appdir, &thumbnails).unwrap();
        for entry in entries() {
            let meta = fs::metadata(&entry).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600, "{}", entry.display());
            assert_eq!(fs::metadata(entry.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
            assert!(thumbnail_is_current(&fs::read(&entry).unwrap(), &uri, seconds(&appimage)));
            // Only the entry: no temporary file left behind.
            assert_eq!(fs::read_dir(entry.parent().unwrap()).unwrap().count(), 1);
        }
        assert!(!failed.exists());
        // The nudge keeps the file's modification time as it was.
        assert_eq!(mtime(&appimage), before);

        // A second start rewrites nothing.
        let first = inodes();
        seed_own_thumbnail(&appimage, &appdir, &thumbnails).unwrap();
        assert_eq!(inodes(), first);

        // The file changed (a new download under the same name): new entries.
        let later = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::open(&appimage).unwrap().set_modified(later).unwrap();
        seed_own_thumbnail(&appimage, &appdir, &thumbnails).unwrap();
        assert_ne!(inodes(), first);
        for entry in entries() {
            assert!(thumbnail_is_current(&fs::read(&entry).unwrap(), &uri, 1_700_000_000));
        }
        assert_eq!(mtime(&appimage), later);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_goes_on_when_one_entry_fails() {
        let dir = scratch("thumbnail-seed-partial");
        let appdir = dir.join("appdir");
        let [(large, dir_icon), (x_large, theme_icon)] = thumbnail_sources();
        // The first source, .DirIcon, is no PNG; the 512 px icon is one.
        fs::create_dir_all(appdir.join(&theme_icon).parent().unwrap()).unwrap();
        fs::write(appdir.join(&dir_icon), b"not a PNG").unwrap();
        fs::write(appdir.join(&theme_icon), TINY_PNG).unwrap();
        let appimage = dir.join("Reoling-1.0.0-x86_64.AppImage");
        fs::write(&appimage, b"\x7fELF").unwrap();
        let thumbnails = dir.join("thumbnails");
        let key = thumbnail_key(&glib::filename_to_uri(&appimage, None).unwrap());
        let failed = thumbnails.join("fail/gnome-thumbnail-factory").join(format!("{key}.png"));
        fs::create_dir_all(failed.parent().unwrap()).unwrap();
        fs::write(&failed, TINY_PNG).unwrap();

        seed_own_thumbnail(&appimage, &appdir, &thumbnails).unwrap();
        assert!(!thumbnails.join(large).exists());
        assert!(thumbnails.join(x_large).join(format!("{key}.png")).is_file());
        assert!(!failed.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invoked_path_is_the_path_as_spelled() {
        let owd = || Some(OsString::from("/home/user/Apps"));
        let path = |s: &str| Some(PathBuf::from(s));
        assert_eq!(invoked_path(OsStr::new("/home/user/Apps/R.AppImage"), None), path("/home/user/Apps/R.AppImage"));
        assert_eq!(invoked_path(OsStr::new("./R.AppImage"), owd()), path("/home/user/Apps/R.AppImage"));
        assert_eq!(invoked_path(OsStr::new("../Pictures/./R.AppImage"), owd()), path("/home/user/Pictures/R.AppImage"));
        assert_eq!(invoked_path(OsStr::new("/home/user/../../opt/R.AppImage"), None), path("/opt/R.AppImage"));
        assert_eq!(invoked_path(OsStr::new("R.AppImage"), None), None);
        // A file reached through a linked folder keeps that spelling, and
        // is still known to be the same file.
        let dir = scratch("invoked-path");
        let real = dir.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("R.AppImage"), b"\x7fELF").unwrap();
        std::os::unix::fs::symlink(&real, dir.join("link")).unwrap();
        let through_link = invoked_path(dir.join("link/R.AppImage").as_os_str(), None).unwrap();
        assert_eq!(through_link, dir.join("link/R.AppImage"));
        assert!(same_file(&through_link, &real.join("R.AppImage")));
        assert!(!same_file(&through_link, &dir.join("missing")));
        let _ = fs::remove_dir_all(&dir);
    }
}
