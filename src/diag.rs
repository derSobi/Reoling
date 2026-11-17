//! The diagnostic log: the negotiation debug lines. `REOLING_DEBUG_NEGOTIATION` prints them on stderr, the lines
//! that were there before the log as they are (`line`, `always`; a test harness reads them), the others masked
//! (`masked`); the Diagnostic Log switch in the settings writes them all, masked, to a file the user can attach to
//! an issue. What the mask removes and what it cannot recognise: `mask_with`. Text from a device reaches a line
//! only through an allowlist checked where it enters the line: element and attribute names (`name_word`), numbers
//! and fixed words of fixed elements and versions of version fields (`mask_body`, `version_word`), and the model
//! word (`model_word`). Whatever does not fit becomes a fixed placeholder or its length; the mask then checks the
//! whole line as a second layer. A version is kept only as the whole text of a `firmwareVersion` or
//! `hardwareVersion` element, surrounding whitespace aside: an optional `v`, then at most 40 bytes of digits, dots and
//! `_` starting with a digit. Exactly four dotted numbers (`192.0.2.9`, `v192.0.2.9`) are an address and masked; a
//! suffix or another number of groups (`v3.0.0.88_2306`, `3.0.0.88.1`, `3.0.0.88.`) keeps the value a version.
//!
//! Masking (`mask_with`, `mask_body`) and the line format (`format_line`) are pure functions; the rest is the one
//! open file and the names the window has seen, both process-wide.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// The longest line the file takes; the rest is cut and counted.
pub const LINE_LIMIT: usize = 4096;
/// The longest reply body text a line quotes.
pub const BODY_LIMIT: usize = 2048;
/// How much of a reply body `mask_body` reads: the rest is counted, never parsed.
pub const BODY_INPUT_LIMIT: usize = 2 * BODY_LIMIT;
/// How many names `conceal` keeps; past that every line is withheld rather than risk an unmasked name.
const NAME_CAP: usize = 512;
/// What stands in the file, and on stderr for a masked line, for a line that cannot be masked safely.
pub const WITHHELD: &str = "NEGOTIATION line withheld: it could not be masked safely";
/// The file is moved to `PREVIOUS_NAME` and started again when a line would take it past this.
const FILE_LIMIT: u64 = 5 * 1024 * 1024;
const FILE_NAME: &str = "reoling-diagnostic.log";
const PREVIOUS_NAME: &str = "reoling-diagnostic.1.log";

tokio::task_local! {
    /// The device's 1-based place in the device list, for the `[dev D]` of the lines its thread writes.
    pub static DEVICE: u32;
}

static CLOCK: OnceLock<Instant> = OnceLock::new();
static FILE_ON: AtomicBool = AtomicBool::new(false);
static FILE: Mutex<Option<LogFile>> = Mutex::new(None);
/// Why the file the switch asks for is not written (see `recording`); taken after `FILE` when both are held.
static FAILURE: Mutex<Option<Failure>> = Mutex::new(None);
/// What `conceal` was given.
static NAMES: Mutex<Names> = Mutex::new(Names { list: Vec::new(), overflow: false });
/// Whether a connection was made in this process: a file opened later says that it misses those.
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// Held by every test that opens or closes the file: there is one per process.
#[cfg(test)]
pub(crate) static TEST_FILE: Mutex<()> = Mutex::new(());

/// Starts the clock the lines count from; the first line starts it otherwise.
pub fn start_clock() {
    CLOCK.get_or_init(Instant::now);
}

/// Whether `REOLING_DEBUG_NEGOTIATION` is set, read once.
fn env_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("REOLING_DEBUG_NEGOTIATION").is_ok())
}

/// Whether the debug lines go anywhere: callers ask before building what a line prints.
pub fn on() -> bool {
    env_on() || FILE_ON.load(Relaxed)
}

/// A debug line that was there before the diagnostic log: on stderr as it is with `REOLING_DEBUG_NEGOTIATION` (a
/// test harness reads those), and masked into the file while it is open.
pub fn line(text: fmt::Arguments) {
    if env_on() {
        eprintln!("{text}");
    }
    if FILE_ON.load(Relaxed) {
        write_masked(&text.to_string(), true);
    }
}

/// A debug line added with the diagnostic log: masked on stderr with `REOLING_DEBUG_NEGOTIATION` and in the file.
pub fn masked(text: fmt::Arguments) {
    emit(text, env_on(), true);
}

/// `masked` without the learned names: for the device's model, which a device name often repeats. The token masks
/// (UIDs, MAC and IP addresses, e-mail addresses) still apply.
pub fn masked_keeping_names(text: fmt::Arguments) {
    emit(text, env_on(), false);
}

/// A line that was there before the diagnostic log and that stderr always gets, with or without the debug switch:
/// on stderr as it is, never withheld, and masked into the file while it is open.
pub fn always(text: fmt::Arguments) {
    always_under("", text);
}

/// `always` for a line of a numbered connection: stderr as it is, and the file under `tag` (`[conn N] `), as the
/// other lines of that connection.
pub fn always_under(tag: &str, text: fmt::Arguments) {
    eprintln!("{text}");
    if FILE_ON.load(Relaxed) {
        write_masked(&format!("{tag}{text}"), true);
    }
}

/// A line that was there before the diagnostic log, too long for one file line: `stderr` as it is with
/// `REOLING_DEBUG_NEGOTIATION`, and each of `file` masked into the file.
pub fn line_split(stderr: fmt::Arguments, file: &[String]) {
    if env_on() {
        eprintln!("{stderr}");
    }
    if FILE_ON.load(Relaxed) {
        for text in file {
            write_masked(text, true);
        }
    }
}

fn emit(text: fmt::Arguments, stderr: bool, learned: bool) {
    let file = FILE_ON.load(Relaxed);
    if !stderr && !file {
        return;
    }
    let text = text.to_string();
    let shown = safe_text(&text, &names(learned));
    if stderr {
        eprintln!("{shown}");
    }
    if file {
        append(&mut file_slot(), &format_line(elapsed(), device(), &shown));
    }
}

/// Masks every later line's mention of `name` (a device's or a camera's name, a device's UID or address, a user
/// name) as `<name>`. Kept in memory only.
pub fn conceal(name: &str) {
    NAMES.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).learn(name);
}

/// The names a line is masked with: those `conceal` was given when `learned`, else none. Past `NAME_CAP` both
/// overflow, so that every line is withheld.
fn names(learned: bool) -> Names {
    let names = NAMES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Names { list: if learned { names.list.clone() } else { Vec::new() }, overflow: names.overflow }
}

/// The learned names: each once whatever its case, longest first. Past `NAME_CAP` none is added and `overflow`
/// withholds every line, so that a name that could not be kept never reaches the file.
#[derive(Default)]
struct Names {
    list: Vec<String>,
    overflow: bool,
}

impl Names {
    fn learn(&mut self, name: &str) {
        let name = name.trim();
        let lower = name.to_lowercase();
        if name.is_empty() || self.list.iter().any(|n| n.to_lowercase() == lower) {
            return;
        }
        if self.list.len() >= NAME_CAP {
            self.overflow = true;
            return;
        }
        self.list.push(name.to_string());
        self.list.sort_by_key(|n| std::cmp::Reverse(n.len()));
    }
}

/// `text` as a line shows it: masked with `names`, or `WITHHELD`.
fn safe_text(text: &str, names: &Names) -> String {
    if names.overflow {
        return WITHHELD.to_string();
    }
    mask_with(text, &names.list).unwrap_or_else(|| WITHHELD.to_string())
}

fn device() -> Option<u32> {
    DEVICE.try_with(|d| *d).ok()
}

/// `future` under the caller's `DEVICE`, for a task the caller spawns: tokio does not carry a task-local into it.
pub fn with_device<F: std::future::Future>(future: F) -> impl std::future::Future<Output = F::Output> {
    let device = device();
    async move {
        match device {
            Some(device) => DEVICE.scope(device, future).await,
            None => future.await,
        }
    }
}

/// Why the file the switch asks for is not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: PathBuf,
    /// Opening the file failed; else a write to it.
    pub opening: bool,
    pub kind: std::io::ErrorKind,
}

/// What happens to the lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recording {
    Off,
    /// Written to the file at this path.
    On(PathBuf),
    /// The switch asks for a file that could not be opened or written; the lines go nowhere.
    Failed(Failure),
}

/// Whether the lines reach a file now.
pub fn recording() -> Recording {
    let slot = file_slot();
    if let Some(file) = slot.as_ref() {
        return Recording::On(file.path.clone());
    }
    match failure_slot().clone() {
        Some(failure) => Recording::Failed(failure),
        None => Recording::Off,
    }
}

fn failure_slot() -> MutexGuard<'static, Option<Failure>> {
    FAILURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Notes that a connection was made (see `CONNECTED`).
pub(crate) fn connection_started() {
    CONNECTED.store(true, Relaxed);
}

/// Opens the file at `path`, or closes it with `None`. Opening moves a log already there to `PREVIOUS_NAME`;
/// asking again for the file already open, or for one that could not be opened or written (see `recording`), does
/// nothing: every settings edit applies the settings again, and only turning the switch off and on tries again.
/// Closing keeps the file.
pub fn set_file(path: Option<PathBuf>) {
    // One hold of the lock: no other thread's line comes after the closing line or before the opening one.
    let mut slot = file_slot();
    if path.is_some() && slot.as_ref().map(|f| &f.path) == path.as_ref() {
        return;
    }
    if path.is_some() && failure_slot().as_ref().map(|f| &f.path) == path.as_ref() {
        return;
    }
    failure_slot().take();
    if slot.is_some() {
        let line = masked_line("NEGOTIATION diagnostics off");
        append(&mut slot, &line);
        slot.take();
        FILE_ON.store(false, Relaxed);
    }
    let Some(path) = path else { return };
    match LogFile::open(&path) {
        Ok(file) => {
            *slot = Some(file);
            FILE_ON.store(true, Relaxed);
            if CONNECTED.load(Relaxed) {
                let line = masked_line("NEGOTIATION diagnostics on (connections made earlier are not in this log)");
                append(&mut slot, &line);
            }
        }
        Err(e) => {
            eprintln!("diagnostic log: could not open ({:?})", e.kind());
            *failure_slot() = Some(Failure { path, opening: true, kind: e.kind() });
        }
    }
}

/// Writes out what the open file still buffers.
pub fn flush() {
    if let Some(file) = file_slot().as_mut() {
        let _ = file.out.flush();
    }
}

/// The open file's path.
pub fn current_path() -> Option<PathBuf> {
    file_slot().as_ref().map(|f| f.path.clone())
}

/// Why `export` wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportError {
    /// The destination is the log itself, under its own or another name: copying would empty it.
    TheLogItself,
    Io(std::io::ErrorKind),
}

/// Copies the log at `from` to `to`, refusing `from` itself or another name of it. While the log is open, its
/// buffer is written out first and no line is added during the copy, so the copy holds whole lines of one session.
pub fn export(from: &Path, to: &Path) -> Result<u64, ExportError> {
    let mut slot = file_slot();
    if let Some(file) = slot.as_mut() {
        file.out.flush().map_err(|e| ExportError::Io(e.kind()))?;
    }
    if same_file(from, to) {
        return Err(ExportError::TheLogItself);
    }
    std::fs::copy(from, to).map_err(|e| ExportError::Io(e.kind()))
}

/// Whether `to` names the file `from` names (a hard or symbolic link counts); a path not there yet never does.
fn same_file(from: &Path, to: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(from), std::fs::metadata(to)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The log file under the XDG state directory: `$XDG_STATE_HOME/reoling` when that is set and absolute, else
/// `~/.local/state/reoling`.
pub fn log_path(xdg_state_home: Option<&OsStr>, home: &Path) -> PathBuf {
    let base = match xdg_state_home {
        Some(dir) if Path::new(dir).is_absolute() => PathBuf::from(dir),
        _ => home.join(".local").join("state"),
    };
    base.join("reoling").join(FILE_NAME)
}

struct LogFile {
    path: PathBuf,
    out: BufWriter<File>,
    written: u64,
}

impl LogFile {
    /// A new file at `path`, readable by the user only, with the header line; a file already there becomes the
    /// previous one.
    fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            if path.exists() {
                std::fs::rename(path, dir.join(PREVIOUS_NAME))?;
            }
        }
        let file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        let mut log = Self { path: path.to_path_buf(), out: BufWriter::new(file), written: 0 };
        let header = format!(
            "Reoling {} diagnostic log. Masked wherever they stand: UIDs, MAC, IPv6 and e-mail addresses, IPv4 \
             addresses except at the start of a firmware or hardware version that has more than four dotted numbers \
             or anything after them (a build number, say), and the device, camera and user names Reoling knows. \
             Text from a device appears only in fixed forms: element and attribute names of ASCII letters, digits \
             and underscores, numbers and fixed words from fixed fields, versions from version fields (a version of \
             exactly four dotted numbers is masked), and the model as a short word of letters, digits, spaces, - \
             and +; anything else as a placeholder or its length. Identifiers and names in any \
             other shape are not recognised; text that only looks like an identifier may be masked too, and a line \
             that cannot be masked safely is withheld. Never written: passwords, nonces, keys.",
            env!("CARGO_PKG_VERSION")
        );
        // Through the mask like every line; a fixed text, so it comes through whole unless the names overflowed.
        log.write(&format_line(elapsed(), None, &safe_text(&header, &names(false))))?;
        Ok(log)
    }

    fn write(&mut self, line: &str) -> std::io::Result<()> {
        writeln!(self.out, "{line}")?;
        self.out.flush()?;
        self.written += line.len() as u64 + 1;
        Ok(())
    }
}

fn file_slot() -> MutexGuard<'static, Option<LogFile>> {
    FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn elapsed() -> Duration {
    CLOCK.get_or_init(Instant::now).elapsed()
}

/// Masks `text` (with the learned names when `learned`) and appends it to the open file; `WITHHELD` in its place
/// when it cannot be masked safely.
fn write_masked(text: &str, learned: bool) {
    let line = format_line(elapsed(), device(), &safe_text(text, &names(learned)));
    append(&mut file_slot(), &line);
}

/// `text` as the file takes it (`safe_text`, `format_line`).
fn masked_line(text: &str) -> String {
    format_line(elapsed(), device(), &safe_text(text, &names(true)))
}

/// Appends `line` to the file in `slot`, moving a full file aside first; a write error closes the file for the
/// session.
fn append(slot: &mut MutexGuard<'static, Option<LogFile>>, line: &str) {
    let Some(file) = slot.as_mut() else { return };
    let written = if file.written + line.len() as u64 + 1 > FILE_LIMIT {
        LogFile::open(&file.path).and_then(|fresh| {
            *file = fresh;
            file.write(line)
        })
    } else {
        file.write(line)
    };
    if let Err(e) = written {
        if let Some(file) = slot.take() {
            *failure_slot() = Some(Failure { path: file.path, opening: false, kind: e.kind() });
        }
        FILE_ON.store(false, Relaxed);
        eprintln!("diagnostic log: could not write ({:?})", e.kind());
    }
}

/// One line of the file: the time since the process started, the device's place in the list when known, `[app]`
/// for a line tied to no device or connection, then `text`, cut to `LINE_LIMIT` bytes. A `text` that names its
/// device itself (`[dev D] …`, from the window) keeps that.
pub fn format_line(elapsed: Duration, device: Option<u32>, text: &str) -> String {
    let mut line = format!("+{}.{:03} ", elapsed.as_secs(), elapsed.subsec_millis());
    let owned = ["[race ", "[conn ", "[dev "].iter().any(|tag| text.starts_with(tag));
    match device {
        Some(device) => line.push_str(&format!("[dev {device}] ")),
        None if !owned => line.push_str("[app] "),
        None => {}
    }
    line.push_str(text);
    cut(&mut line, LINE_LIMIT);
    line
}

/// Cuts `text` to `limit` bytes on a character boundary and says how much was left out.
fn cut(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut at = limit;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let more = text.len() - at;
    text.truncate(at);
    text.push_str(&format!("… ({more} more bytes)"));
}

/// `text` masked for the file. What it guarantees:
/// - it reads at most `LINE_LIMIT` bytes, cut back past the longest name and then to a space, so that the cut splits
///   no identifier or name; the rest is counted;
/// - control characters become spaces, so a line stays one line;
/// - these shapes become their kind wherever they stand, also joined to letters, digits or punctuation (`eth0-`,
///   `face`, `dev`, `x1`): an e-mail address (an `@` with a letter or digit after it and a local part of letters,
///   digits and `._%+-` before it), an IPv6 address (with its zone; one without a decimal digit needs four hex
///   digits), a MAC address (six hex pairs between `:`, `-` or `.`, mixed too, or three groups of four hex digits
///   between `.`), an IPv4 address (four dotted numbers up to 255; the first and last may run on into other digits,
///   which are masked with it; not a version held by a version element, see `version_at`; a version element's
///   value of exactly four dotted numbers is masked), a UID (a run of 12 or more letters and digits with both in
///   it, or of 16 or more digits, in any case) and a run of twelve or more hex digits;
/// - every whole-word mention of a name in `names` becomes `<name>`, in any case as far as comparing each
///   character in lower case goes ("GRĂDINĂ" matches "Grădină"; foldings such as "ß" and "SS" do not);
/// - `None` when the masked text still holds one of these shapes (`has_shapes`) or, by a coarser check that does not
///   use the finders (`residue`), something like one: the caller writes `WITHHELD`.
///
/// It masks too much rather than too little: a twelve-digit counter shows as `<mac>`, a long word with a digit as
/// `<uid>`, a Rust path such as `Feed::` as an IPv6 address, a dotted number of four parts as an IPv4 address, and a
/// line with a long dashed hex run is withheld. What it cannot do: recognise a secret, a name or an identifier that
/// has none of these shapes and was never learned, a name written another way ("Garden_Cam" for "Garden Cam"), or a
/// mention of part of a name. It is the callers that keep such text out: the lines added with this log give device
/// text only through its allowlist (`name_word`, `version_word`, `model_word`), and a test checks that no line
/// formats a password, a nonce or a key. The older lines were written for public reports (numbers and fixed words),
/// and the mask is their second guard, not their first.
pub fn mask_with(text: &str, names: &[String]) -> Option<String> {
    mask_through(text, names, mask_shapes)
}

/// `mask_with` with `shapes` as the first layer; a test hands it a broken one to reach the second layer.
fn mask_through(text: &str, names: &[String], shapes: fn(&str) -> String) -> Option<String> {
    let mut read = text.len();
    if read > LINE_LIMIT {
        read = LINE_LIMIT.saturating_sub(names.iter().map(String::len).max().unwrap_or(0));
        while !text.is_char_boundary(read) {
            read -= 1;
        }
        read = text[..read].rfind(char::is_whitespace).unwrap_or(0);
    }
    let flat: String = text[..read].chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut masked = conceal_names(&shapes(&flat), names);
    if withholds(&masked) {
        return None;
    }
    if read < text.len() {
        masked.push_str(&format!("… ({} more bytes)", text.len() - read));
    }
    Some(masked)
}

/// The second layer, on the whole masked line: whether it still holds a shape the mask knows (`has_shapes`) or
/// something like one (`residue`), so that `mask_with` withholds it. Masking leaves no shape it knows in one pass, so
/// `has_shapes` fires only if a finder or their order breaks.
fn withholds(masked: &str) -> bool {
    has_shapes(masked) || residue(masked)
}

/// Whether `text` holds one of the shapes `mask_with` masks: the mask's own check that withholds a line.
fn has_shapes(text: &str) -> bool {
    mask_shapes(text) != text
}

/// The second check that withholds a line, by character classes alone: it shares no finder with the mask, so an
/// identifier the mask misses, or one in a shape it does not know, is withheld too. True when `text` has an `@`
/// between two characters that are neither space nor bracket; a run of hex digits, `:`, `-` and `.` with twelve hex
/// digits or more; a run of hex digits, `:` and `.` with `::` or five `:`, and a decimal digit or four hex digits in
/// it; or four dotted numbers that are not a version held by a version element (`version_at`).
fn residue(text: &str) -> bool {
    let bytes = text.as_bytes();
    let joined = |b: Option<&u8>| b.is_some_and(|b| !b.is_ascii_whitespace() && !b"<>\"'()[],;".contains(b));
    let email = (1..bytes.len()).any(|at| bytes[at] == b'@' && joined(bytes.get(at - 1)) && joined(bytes.get(at + 1)));
    let hex = |run: &str| run.bytes().filter(u8::is_ascii_hexdigit).count();
    let long_hex = runs(text, 0, |c| c.is_ascii_hexdigit() || ":-.".contains(c)).any(|(s, e)| hex(&text[s..e]) >= 12);
    let ipv6 = runs(text, 0, |c| c.is_ascii_hexdigit() || ":.".contains(c)).any(|(s, e)| {
        let run = &text[s..e];
        let colons = run.contains("::") || run.matches(':').count() >= 5;
        colons && (run.bytes().any(|b| b.is_ascii_digit()) || hex(run) >= 4)
    });
    let ipv4 = runs(text, 0, |c| c.is_ascii_digit() || c == '.').any(|(s, e)| {
        text[s..e].split('.').filter(|g| !g.is_empty()).count() >= 4 && !version_at(text, s)
    });
    email || long_hex || ipv6 || ipv4
}

/// `mask_with` with the names `conceal` was given (`None` too once more names were given than it keeps).
pub fn mask(text: &str) -> Option<String> {
    let names = names(true);
    (!names.overflow).then(|| mask_with(text, &names.list)).flatten()
}

/// Every shape of `mask_with` in `text` replaced by its kind, one kind after the other. IPv6 before MAC: a MAC's six
/// pairs can end an IPv6 address, whose start would show otherwise.
fn mask_shapes(text: &str) -> String {
    let shapes: [fn(&str, usize) -> Option<(usize, usize, String)>; 6] =
        [find_email, find_ipv6, find_mac, find_ipv4, find_uid, find_hex_run];
    let mut out = text.to_string();
    for find in shapes {
        let mut from = 0;
        while let Some((start, end, kind)) = find(&out, from) {
            out.replace_range(start..end, &kind);
            from = start + kind.len();
        }
    }
    out
}

/// The maximal runs of characters `is_part` takes, from `from` on: (start, end) in bytes.
fn runs<'a>(
    text: &'a str,
    from: usize,
    is_part: impl Fn(char) -> bool + 'a,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut at = from;
    std::iter::from_fn(move || {
        let start = at + text[at..].find(|c| is_part(c))?;
        let end = text[start..].find(|c| !is_part(c)).map_or(text.len(), |len| start + len);
        at = end;
        Some((start, end))
    })
}

/// An `@` with a letter or digit after it and a local part with a letter or digit in it before it, and the address
/// characters around it.
fn find_email(text: &str, from: usize) -> Option<(usize, usize, String)> {
    let local = |c: char| c.is_alphanumeric() || "._%+-".contains(c);
    let domain = |c: char| c.is_alphanumeric() || ".-".contains(c);
    for (at, _) in text[from..].match_indices('@').map(|(at, s)| (from + at, s)) {
        let after_char = |i: usize| i + text[i..].chars().next().map_or(1, char::len_utf8);
        let start = text[..at].rfind(|c| !local(c)).map_or(0, after_char);
        let after = text[at + 1..].chars().next();
        if !(text[start..at].chars().any(char::is_alphanumeric) && after.is_some_and(char::is_alphanumeric)) {
            continue;
        }
        let end = text[at + 1..].find(|c| !domain(c)).map_or(text.len(), |len| at + 1 + len);
        let end = start.max(at + 1 + text[at + 1..end].trim_end_matches(['.', '-']).len());
        return Some((start, end, "<email>".to_string()));
    }
    None
}

/// Six or more hex pairs between `:`, `-` or `.` (mixed too), or three groups of four hex digits between `.`. A
/// pair may be the end of a longer first group or the start of a longer last one (`mac00:…`, `…:55a`): the rest of
/// those groups is left. A chain of more than six pairs is masked whole.
fn find_mac(text: &str, from: usize) -> Option<(usize, usize, String)> {
    for (start, end) in runs(text, from, |c| c.is_ascii_hexdigit() || ":-.".contains(c)) {
        // Each group's place in the run and its length.
        let mut groups = Vec::new();
        let mut at = 0;
        for group in text[start..end].split([':', '-', '.']) {
            groups.push((at, group.len()));
            at += group.len() + 1;
        }
        let mut i = 0;
        while i < groups.len() {
            let (first, first_len) = groups[i];
            let (mut pairs, mut last_end, mut j) = (1, first + first_len, i + 1);
            while let Some(&(group, len)) = groups.get(j).filter(|_| first_len >= 2) {
                if len < 2 {
                    break;
                }
                (pairs, last_end, j) = (pairs + 1, group + 2, j + 1);
                if len > 2 {
                    break;
                }
            }
            if pairs >= 6 {
                return Some((start + first + first_len - 2, start + last_end, "<mac>".to_string()));
            }
            i = j.max(i + 1);
        }
    }
    let bytes = text.as_bytes();
    (from..bytes.len()).find_map(|at| {
        let shape = bytes.get(at..at + 14).is_some_and(|run| {
            run.iter().enumerate().all(|(i, b)| if i % 5 == 4 { *b == b'.' } else { b.is_ascii_hexdigit() })
        });
        shape.then(|| (at, at + 14, "<mac>".to_string()))
    })
}

/// Whether the run of digits and dots starting at `start` of `text` is a version where a version is held: it opens
/// the text of a version element (`<firmwareVersion>3.0.0.88_2306`, also after a `v` or whitespace) and that text,
/// up to the next `<` and trimmed as `masked_text` trims it, is a version by `version_word` (so
/// `<firmwareVersion>192.0.2.9` is not). Nowhere else, not in a model word, an element name or free text.
fn version_at(text: &str, start: usize) -> bool {
    let before = &text[..start];
    let (before, v) = before.strip_suffix(['v', 'V']).map_or((before, 0), |b| (b, 1));
    let before = before.trim_end();
    let value = &text[start - v..];
    let value = value[..value.find('<').unwrap_or(value.len())].trim();
    VERSION_ELEMENTS.iter().any(|e| before.ends_with(&format!("<{e}>"))) && version_word(Field::Element, value)
}

/// Four dotted numbers up to 255 in a run of digits and dots, not a version held by a version element (`version_at`).
/// The first and last group may run on into other digits (`x1192.168.1.50`): their last or first one to three digits
/// count, and the whole group is masked.
fn find_ipv4(text: &str, from: usize) -> Option<(usize, usize, String)> {
    fn octet(g: &str) -> bool {
        (1..=3).contains(&g.len()) && g.parse::<u16>().is_ok_and(|n| n <= 255)
    }
    fn last(g: &str) -> Option<&str> {
        (1..=g.len().min(3)).rev().map(|n| &g[g.len() - n..]).find(|d| octet(d))
    }
    fn first(g: &str) -> Option<&str> {
        (1..=g.len().min(3)).rev().map(|n| &g[..n]).find(|d| octet(d))
    }
    for (start, end) in runs(text, from, |c| c.is_ascii_digit() || c == '.') {
        if version_at(text, start) {
            continue;
        }
        let run = &text[start..end];
        let mut group_starts = vec![0];
        group_starts.extend(run.match_indices('.').map(|(i, _)| i + 1));
        let groups: Vec<&str> = run.split('.').collect();
        for i in 0..groups.len().saturating_sub(3) {
            let (Some(a), Some(d)) = (last(groups[i]), first(groups[i + 3])) else { continue };
            if !(octet(groups[i + 1]) && octet(groups[i + 2])) {
                continue;
            }
            let (s, e) = (start + group_starts[i], start + group_starts[i + 3] + groups[i + 3].len());
            let address = format!("{a}.{}.{}.{d}", groups[i + 1], groups[i + 2]);
            return Some((s, e, address.parse::<Ipv4Addr>().map_or("<ipv4>", ipv4_kind).to_string()));
        }
    }
    None
}

/// An IPv6 address with a decimal digit or four hex digits in it, and its zone, inside a run of hex digits, `:` and
/// `.`: the longest one at the first place one starts (`peer:::1` is `peer:` and `::1`). `d::` of `std::io` is left.
fn find_ipv6(text: &str, from: usize) -> Option<(usize, usize, String)> {
    for (start, end) in runs(text, from, |c| c.is_ascii_hexdigit() || c == ':' || c == '.') {
        if text[start..end].matches(':').count() < 2 {
            continue;
        }
        for a in start..end {
            // The longest address written out is 45 characters.
            for b in (a + 2..=end.min(a + 45)).rev() {
                let candidate = &text[a..b];
                let Ok(ip) = candidate.parse::<Ipv6Addr>() else { continue };
                let decimal = candidate.bytes().any(|c| c.is_ascii_digit());
                if !decimal && candidate.bytes().filter(u8::is_ascii_hexdigit).count() < 4 {
                    continue;
                }
                let zone = text[b..].strip_prefix('%').map_or(0, |rest| {
                    1 + rest.find(|c: char| !(c.is_ascii_alphanumeric() || "._-".contains(c))).unwrap_or(rest.len())
                });
                return Some((a, b + if zone > 1 { zone } else { 0 }, ipv6_kind(ip).to_string()));
            }
        }
    }
    None
}

fn ipv6_kind(ip: Ipv6Addr) -> &'static str {
    let first = ip.segments()[0];
    if ip.is_loopback() {
        "<ipv6 loopback>"
    } else if first & 0xffc0 == 0xfe80 {
        "<ipv6 link-local>"
    } else if first & 0xfe00 == 0xfc00 {
        "<ipv6 unique-local>"
    } else {
        "<ipv6 public>"
    }
}

/// A run of 12 or more ASCII letters and digits with both in it, or of 16 or more digits; twelve hex digits alone
/// are left to `find_hex_run`, as a MAC.
fn find_uid(text: &str, from: usize) -> Option<(usize, usize, String)> {
    runs(text, from, |c| c.is_ascii_alphanumeric()).find_map(|(start, end)| {
        let run = &text[start..end];
        let digits = run.bytes().filter(u8::is_ascii_digit).count();
        let mac = run.len() == 12 && run.bytes().all(|b| b.is_ascii_hexdigit());
        let mixed = run.len() >= 12 && digits > 0 && digits < run.len() && !mac;
        let uid = mixed || (run.len() >= 16 && digits == run.len());
        uid.then(|| (start, end, "<uid>".to_string()))
    })
}

/// A run of twelve or more hex digits between other characters: exactly twelve is a MAC written together, more an
/// identifier of another kind.
fn find_hex_run(text: &str, from: usize) -> Option<(usize, usize, String)> {
    let kind = |len: usize| if len == 12 { "<mac>" } else { "<uid>" };
    let mut hex = runs(text, from, |c| c.is_ascii_hexdigit());
    hex.find(|(start, end)| end - start >= 12).map(|(start, end)| (start, end, kind(end - start).to_string()))
}

fn ipv4_kind(ip: Ipv4Addr) -> &'static str {
    if ip.is_private() {
        "<ipv4 private>"
    } else if ip.is_loopback() {
        "<ipv4 loopback>"
    } else if ip.is_link_local() {
        "<ipv4 link-local>"
    } else if ip.is_unspecified() {
        "<ipv4 any>"
    } else if ip.is_broadcast() {
        "<ipv4 broadcast>"
    } else {
        "<ipv4 public>"
    }
}

/// Every whole-word mention of one of `names` (longest first, in any case) as `<name>`: a camera named "Main" leaves
/// `mainStream` alone, and one named "Al" leaves "Alarm" alone.
fn conceal_names(text: &str, names: &[String]) -> String {
    let mut names: Vec<&str> = names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mut out = text.to_string();
    for name in names {
        let mut from = 0;
        while let Some((at, end)) = find_ignoring_case(&out[from..], name).map(|(at, end)| (from + at, from + end)) {
            let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
            if !word(out[..at].chars().next_back()) && !word(out[end..].chars().next()) {
                out.replace_range(at..end, "<name>");
                from = at + "<name>".len();
            } else {
                from = at + out[at..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    out
}

/// Where `needle` first appears in `haystack`, compared char by char in lower case (so "Ă" matches "ă"): the start
/// and end of the match in `haystack`, both on character boundaries.
fn find_ignoring_case(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    let wanted: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if wanted.is_empty() {
        return None;
    }
    'start: for (start, _) in haystack.char_indices() {
        let mut rest = wanted.iter().peekable();
        for (at, c) in haystack[start..].char_indices() {
            for lower in c.to_lowercase() {
                if rest.next() != Some(&lower) {
                    continue 'start;
                }
            }
            if rest.peek().is_none() {
                return Some((start, start + at + c.len_utf8()));
            }
        }
    }
    None
}

// Text a device supplies enters a line only through the three functions below (`name_word`, `version_word`,
// `model_word`), each with its own allowlist of characters, length and form; what does not fit becomes a fixed
// placeholder or its length. The shape mask (`mask_with`) then checks the whole line as a second layer.

/// What stands in a line for an element or attribute name that is not in its allowed form (`name_word`).
pub const NAME_PLACEHOLDER: &str = "name-withheld";

/// An element or attribute name as a line shows it: ASCII letters, digits and `_`, starting with a letter, at most
/// 32 bytes; anything else is `NAME_PLACEHOLDER`.
fn name_word(name: &str) -> &str {
    let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if plain && name.len() <= 32 && name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        name
    } else {
        NAME_PLACEHOLDER
    }
}

/// Whether `value` is kept as the version `field` holds: in a version attribute one to three dotted groups of
/// digits (`1.1`), in a version element an optional `v`, then digits, dots and `_` starting with a digit, at most 40
/// bytes (`v3.0.0.88_2306`). A version element's value of exactly four dotted groups of digits and nothing else
/// (`192.0.2.9`, `v192.0.2.9`) is an address, not a version. No other field keeps a version.
fn version_word(field: Field, value: &str) -> bool {
    match field {
        Field::Attribute => {
            let group = |g: &str| !g.is_empty() && g.len() <= 5 && g.bytes().all(|b| b.is_ascii_digit());
            value.split('.').count() <= 3 && value.split('.').all(group)
        }
        Field::Element => {
            let digits = value.strip_prefix(['v', 'V']).unwrap_or(value);
            let group = |g: &str| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit());
            let dotted_quad = digits.split('.').count() == 4 && digits.split('.').all(group);
            value.len() <= 40
                && !dotted_quad
                && digits.starts_with(|c: char| c.is_ascii_digit())
                && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'_')
        }
    }
}

/// Where a version stands (see `version_word`).
#[derive(Clone, Copy)]
enum Field {
    Attribute,
    Element,
}

/// A model name for the debug lines: ASCII letters, digits, spaces, `-` and `+`, starting with a letter, at most 32
/// bytes once trimmed (`RLC-810A`, `E1 Zoom+`); anything else gives its length only. No dot, colon or `_`, so that
/// no address shape can be read as a model.
pub fn model_word(model: &str) -> std::borrow::Cow<'_, str> {
    let model = model.trim();
    let plain = model.bytes().all(|b| b.is_ascii_alphanumeric() || b" +-".contains(&b));
    if plain && model.len() <= 32 && model.starts_with(|c: char| c.is_ascii_alphabetic()) {
        model.into()
    } else {
        format!("<{} bytes>", model.len()).into()
    }
}

/// Elements whose numbers and fixed words a masked body keeps: none of them holds a device's or a user's text.
const KEPT_ELEMENTS: &[&str] = &[
    "channelId", "handle", "streamType", "policy", "version", "type", "code", "result", "rsp", "lver", "width",
    "height", "frameRate", "bitRate", "encodeType", "resolutionName",
];
const FIXED_WORDS: &[&str] =
    &["mainStream", "subStream", "externStream", "none", "md5", "aes", "true", "false", "newestIFrame"];
/// Elements whose version-shaped text a masked body keeps: it names a release, not a device.
const VERSION_ELEMENTS: &[&str] = &["firmwareVersion", "hardwareVersion"];

/// Attributes whose value a masked body keeps when it is a short run of digits and dots (`version="1.1"`).
const KEPT_ATTRIBUTES: &[&str] = &["version"];

/// A reply body's text with the structure kept and the text of every element replaced by its length, except
/// numbers and fixed words in `KEPT_ELEMENTS` and versions in `VERSION_ELEMENTS`; every attribute value the same,
/// except a version-shaped one of `KEPT_ATTRIBUTES`. Works on malformed markup: what does not read as a tag is text.
/// Element and attribute names are kept when they fit `name_word`, else replaced by `NAME_PLACEHOLDER`; a plain word
/// a device puts in a name still reaches the file. Reads `BODY_INPUT_LIMIT` bytes at most and cuts the result to
/// `BODY_LIMIT` bytes, saying how much was left out.
pub fn mask_body(text: &str) -> String {
    let mut read = text.len().min(BODY_INPUT_LIMIT);
    while !text.is_char_boundary(read) {
        read -= 1;
    }
    let unread = text.len() - read;
    let mut out = String::new();
    let mut open: Vec<&str> = Vec::new();
    let mut rest = &text[..read];
    while !rest.is_empty() {
        let text_end = match rest.find('<') {
            Some(0) => {
                let tag = rest.find('>').and_then(|end| Some((end, masked_tag(&rest[1..end])?)));
                if let Some((end, (masked, name, kind))) = tag {
                    out.push_str(&masked);
                    match kind {
                        TagKind::Open => open.push(name),
                        TagKind::Close => {
                            if let Some(at) = open.iter().rposition(|n| *n == name) {
                                open.truncate(at);
                            }
                        }
                        TagKind::Empty => {}
                    }
                    rest = &rest[end + 1..];
                    continue;
                }
                rest[1..].find('<').map_or(rest.len(), |at| at + 1)
            }
            Some(at) => at,
            None => rest.len(),
        };
        out.push_str(&masked_text(&rest[..text_end], open.last().copied()));
        rest = &rest[text_end..];
    }
    // Room for the note on what was not read, within the same bound.
    cut(&mut out, if unread > 0 { BODY_LIMIT - 32 } else { BODY_LIMIT });
    if unread > 0 {
        out.push_str(&format!(" ({unread} bytes not read)"));
    }
    out
}

enum TagKind {
    Open,
    Close,
    /// A declaration, or an element closed in its own tag.
    Empty,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_:.-".contains(c)
}

/// The inside of `<…>` masked, with the element's name and what it does; `None` when it is no tag.
fn masked_tag(inner: &str) -> Option<(String, &str, TagKind)> {
    if inner.contains('<') {
        return None;
    }
    let (start, body, end, kind) = if let Some(decl) = inner.strip_prefix('?').and_then(|i| i.strip_suffix('?')) {
        ("<?", decl, "?>", TagKind::Empty)
    } else if let Some(name) = inner.strip_prefix('/') {
        ("</", name, ">", TagKind::Close)
    } else if let Some(element) = inner.strip_suffix('/') {
        ("<", element, "/>", TagKind::Empty)
    } else {
        ("<", inner, ">", TagKind::Open)
    };
    let name_end = body.find(|c| !is_name_char(c)).unwrap_or(body.len());
    if name_end == 0 {
        return None;
    }
    let name = name_word(&body[..name_end]);
    let mut out = format!("{start}{name}");
    let mut rest = &body[name_end..];
    while !rest.is_empty() {
        let space = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..space]);
        rest = &rest[space..];
        if rest.is_empty() {
            break;
        }
        let attribute = (|| {
            let key_end = rest.find(|c| !is_name_char(c)).filter(|&e| e > 0)?;
            let after = rest[key_end..].strip_prefix('=')?;
            let quote = after.chars().next().filter(|q| *q == '"' || *q == '\'')?;
            let value_end = after[1..].find(quote)?;
            Some((&rest[..key_end], &after[1..1 + value_end], quote, key_end + 1 + value_end + 2))
        })();
        let Some((key, value, quote, used)) = attribute else {
            out.push_str(&format!("<{} bytes>", rest.len()));
            break;
        };
        let key = name_word(key);
        let kept = KEPT_ATTRIBUTES.contains(&key) && version_word(Field::Attribute, value);
        let value = if kept { value.to_string() } else { format!("<{} bytes>", value.len()) };
        out.push_str(&format!("{key}={quote}{value}{quote}"));
        rest = &rest[used..];
    }
    out.push_str(end);
    Some((out, name, kind))
}

/// A text run inside `element`: kept when it is whitespace, a number or fixed word of a kept element, or a version
/// of a version element; else its length.
fn masked_text(run: &str, element: Option<&str>) -> String {
    let word = run.trim();
    if word.is_empty() {
        return run.to_string();
    }
    let number = word.strip_prefix("0x").map_or_else(
        || word.strip_prefix('-').unwrap_or(word).bytes().all(|b| b.is_ascii_digit()),
        |hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()),
    );
    let kept = match element {
        Some(e) if KEPT_ELEMENTS.contains(&e) => number || FIXED_WORDS.contains(&word),
        Some(e) if VERSION_ELEMENTS.contains(&e) => version_word(Field::Element, word),
        _ => false,
    };
    if kept {
        run.to_string()
    } else {
        format!("<{} bytes>", run.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    /// The design's test table: (input, learned names, body?, expected parts, forbidden parts).
    #[test]
    fn masking_follows_the_table() {
        let none: &[&str] = &[];
        let cases: &[(&str, &[&str], bool, &[&str], &[&str])] = &[
            ("uid TESTCAMUID000001 on", none, false, &["uid <uid> on"], &["TESTCAMUID"]),
            ("9527000000000001", none, false, &["<uid>"], &["9527"]),
            ("mac 00:11:22:AA:bb:CC", none, false, &["mac <mac>"], &["AA:bb"]),
            ("mac 00-11-22-AA-BB-CC", none, false, &["mac <mac>"], &["AA-BB"]),
            ("192.168.1.50:9000", none, false, &["<ipv4 private>:9000"], &["192.168"]),
            ("203.0.113.7", none, false, &["<ipv4 public>"], &["203.0"]),
            ("255.255.255.255:2018", none, false, &["<ipv4 broadcast>:2018"], &["255.255"]),
            ("fe80::1%wlan0", none, false, &["<ipv6 link-local>"], &["fe80"]),
            ("2001:db8::5", none, false, &["<ipv6 public>"], &["2001:db8"]),
            ("user.name+tag@example.com", none, false, &["<email>"], &["example.com"]),
            (
                "<body><VersionInfo version=\"1.1\"><name>Front door</name><type>TESTMODEL</type>\
                 <serialNumber>X1</serialNumber></VersionInfo></body>",
                none,
                true,
                &["<name><10 bytes></name>", "version=\"1.1\""],
                &["Front", "TESTMODEL", "X1"],
            ),
            (
                "<Preview version=\"1.1\"><channelId>0</channelId><handle>256</handle>\
                 <streamType>subStream</streamType></Preview>",
                none,
                true,
                &["<Preview version=\"1.1\"><channelId>0</channelId><handle>256</handle>\
                   <streamType>subStream</streamType></Preview>"],
                &[],
            ),
            (
                "<Encryption><type>md5</type><nonce>ABCDEF0123</nonce></Encryption>",
                none,
                true,
                &["<nonce><10 bytes></nonce>"],
                &["ABCDEF0123"],
            ),
            ("osd Garden cam ok", &["Garden Cam"], false, &["osd <name> ok"], &["Garden"]),
            (
                "channels 0x1: mainStream 2560x1440 15 fps",
                &["Main"],
                false,
                &["channels 0x1: mainStream 2560x1440 15 fps"],
                &["<name>"],
            ),
            ("device Hub online", &["Hub"], false, &["device <name> online"], &["Hub"]),
            (
                "<VersionInfo version=\"1.1\"><firmwareVersion>v3.0.0.2356_23062000</firmwareVersion></VersionInfo>",
                none,
                true,
                &["v3.0.0.2356_23062000"],
                &[],
            ),
            (
                "<VersionInfo version=\"1.1\"><firmwareVersion>3.0.0.88_2306</firmwareVersion></VersionInfo>",
                none,
                true,
                &["<firmwareVersion>3.0.0.88_2306</firmwareVersion>"],
                &[],
            ),
            (
                "[race 4 +180 ms] NEGOTIATION race: lookup answered by p2p7.reolink.com, 14 server address(es) asked",
                none,
                false,
                &["[race 4 +180 ms] NEGOTIATION race: lookup answered by p2p7.reolink.com, 14 server address(es) \
                   asked"],
                &[],
            ),
            ("code 0xdd12 class 0x6614", none, false, &["code 0xdd12 class 0x6614"], &[]),
            ("reoling 1.0.6 diag-1", none, false, &["reoling 1.0.6 diag-1"], &[]),
        ];
        for (input, learned, body, wanted, forbidden) in cases {
            let text = if *body { mask_body(input) } else { input.to_string() };
            let out = mask_with(&text, &names(learned)).unwrap_or_else(|| panic!("{input}: dropped"));
            for part in *wanted {
                assert!(out.contains(part), "{input}: {out} lacks {part}");
            }
            for part in *forbidden {
                assert!(!out.contains(part), "{input}: {out} holds {part}");
            }
        }
    }

    /// The gaps the first review found, one row each: (input, learned names, body?, expected parts, forbidden parts).
    #[test]
    fn masking_closes_the_reviewed_gaps() {
        let none: &[&str] = &[];
        let cases: &[(&str, &[&str], bool, &[&str], &[&str])] = &[
            ("uid testcamuid000001 on", none, false, &["uid <uid> on"], &["testcamuid"]),
            ("mac 001122aabbcc x", none, false, &["mac <mac> x"], &["aabbcc"]),
            ("mac 001122AABBCC x", none, false, &["mac <mac> x"], &["AABBCC"]),
            ("mac 001122334455 x", none, false, &["mac <mac> x"], &["334455"]),
            ("mac 00.11.22.aa.bb.cc x", none, false, &["mac <mac> x"], &["aa.bb"]),
            ("mac 0011.22aa.bbcc x", none, false, &["mac <mac> x"], &["22aa"]),
            ("mail owner@localhost x", none, false, &["mail <email> x"], &["owner", "localhost"]),
            ("<broken version=\"SYNTHETIC\"/>", none, true, &["version=\"<9 bytes>\""], &["SYNTHETIC"]),
            ("<x key=\"1234\" version=\"1.1\">", none, true, &["key=\"<4 bytes>\"", "version=\"1.1\""], &["1234"]),
            ("camera Al online, Alarm on", &["Al"], false, &["camera <name> online, Alarm on"], &[]),
            ("camera GRĂDINĂ ok", &["Grădină"], false, &["camera <name> ok"], &["GRĂDINĂ"]),
            ("camera grădină ok", &["GRĂDINĂ"], false, &["camera <name> ok"], &["grădină"]),
        ];
        for (input, learned, body, wanted, forbidden) in cases {
            let text = if *body { mask_body(input) } else { input.to_string() };
            let out = mask_with(&text, &names(learned)).unwrap_or_else(|| panic!("{input}: dropped"));
            for part in *wanted {
                assert!(out.contains(part), "{input}: {out} lacks {part}");
            }
            for part in *forbidden {
                assert!(!out.contains(part), "{input}: {out} holds {part}");
            }
        }
    }

    /// The second review's gaps: identifier shapes joined to other text, one row each (input, expected parts,
    /// forbidden parts). The fail-closed check (`has_shapes`) finds each in the raw input too. All values synthetic.
    #[test]
    fn masking_finds_the_shapes_inside_other_text() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("model M_001122334455", &["model M_<mac>"], &["334455"]),
            ("id_001122334455 x", &["id_<mac> x"], &["334455"]),
            ("id-001122334455 x", &["id-<mac> x"], &["334455"]),
            ("id.001122334455 x", &["id.<mac> x"], &["334455"]),
            ("id+001122334455 x", &["id+<mac> x"], &["334455"]),
            ("id:001122334455 x", &["id:<mac> x"], &["334455"]),
            // Joined to letters, the twelve hex digits read as part of a UID.
            ("Mx001122aabbcc", &["<uid>"], &["aabbcc"]),
            // Over-masking, accepted: `ac` reads as a seventh pair.
            ("mac:00:11:22:33:44:55", &["m<mac>"], &["44:55"]),
            ("eth0-00-11-22-33-44-55", &["eth0-<mac>"], &["44-55"]),
            ("hw.00.11.22.aa.bb.cc", &["hw.<mac>"], &["aa.bb"]),
            ("00:11:22-33-44-55", &["<mac>"], &["33-44"]),
            ("cisco x0011.22aa.bbcc", &["cisco x<mac>"], &["22aa"]),
            ("x_TESTCAMUID000001_y", &["x_<uid>_y"], &["TESTCAMUID"]),
            ("addr:2001:db8::5", &["addr:<ipv6 public>"], &["db8"]),
            ("peer:::1 up", &["peer:<ipv6 loopback> up"], &["::1"]),
            ("peer:fd00::5", &["peer:<ipv6 unique-local>"], &["fd00"]),
            ("from10.0.0.7 on", &["from<ipv4 private> on"], &["10.0"]),
            ("ip=192.168.1.50:9000", &["ip=<ipv4 private>:9000"], &["192.168"]),
            ("ion@ăbc.example wrote", &["<email> wrote"], &["ăbc"]),
            // Over-masking, accepted: a 12-digit counter reads as a MAC.
            ("counter 123456789012 ok", &["counter <mac> ok"], &["789012"]),
        ];
        for (input, wanted, forbidden) in cases {
            assert!(has_shapes(input), "{input}: the check misses it");
            let out = mask_with(input, &[]).unwrap_or_else(|| panic!("{input}: dropped"));
            for part in *wanted {
                assert!(out.contains(part), "{input}: {out} lacks {part}");
            }
            for part in *forbidden {
                assert!(!out.contains(part), "{input}: {out} holds {part}");
            }
        }
        // Left alone: no identifier shape.
        for text in ["std::io::Error at 12:34:56", "bytes12-13=256 class 0x6414"] {
            assert!(!has_shapes(text), "{text}");
            assert_eq!(mask_with(text, &[]).as_deref(), Some(text));
        }
    }

    /// The third review's gaps: shapes joined to a hex letter, a digit or a `v`, e-mail local parts that end in
    /// punctuation, IPv6 addresses without a decimal digit, long hex runs. One row each (input, expected parts,
    /// forbidden parts). The mask removes each, and both checks that withhold a line find each in the raw input: the
    /// mask's own (`has_shapes`) and the coarser one that does not use its finders (`residue`). All values synthetic.
    #[test]
    fn masking_finds_the_shapes_joined_to_letters_digits_and_punctuation() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            ("mac00:11:22:33:44:55", &["mac<mac>"], &["44:55"]),
            ("face00:11:22:33:44:55", &["face<mac>"], &["44:55"]),
            ("a00-11-22-33-44-55 x", &["a<mac> x"], &["44-55"]),
            ("00:11:22:33:44:55a", &["<mac>a"], &["44:55"]),
            ("00:11:22:33:44:55face", &["<mac>face"], &["44:55"]),
            ("hwaddr 00:11:22:33:44:55.1", &["hwaddr <mac>.1"], &["44:55"]),
            ("dev10.0.0.7 up", &["dev<ipv4 private> up"], &["10.0"]),
            ("srv192.168.1.50", &["srv<ipv4 private>"], &["192.168"]),
            ("dev192.0.2.7", &["dev<ipv4 public>"], &["192.0"]),
            ("x1192.168.1.50", &["x<ipv4 private>"], &["168.1"]),
            ("john_@example.com", &["<email>"], &["john", "example"]),
            ("unit_@example.invalid", &["<email>"], &["unit", "invalid"]),
            ("unit+@example.invalid", &["<email>"], &["unit", "invalid"]),
            ("peer fd::ab:cd", &["peer <ipv6 public>"], &["ab:cd"]),
            ("peer:::abcd", &["peer:<ipv6 public>"], &["abcd"]),
            ("peer ::abcd up", &["peer <ipv6 public> up"], &["abcd"]),
            ("id 1001122334455 x", &["id <uid> x"], &["334455"]),
            ("aabbccddeeffab", &["<uid>"], &["ccdd"]),
        ];
        for (input, wanted, forbidden) in cases {
            assert!(has_shapes(input), "{input}: the mask's check misses it");
            assert!(residue(input), "{input}: the coarse check misses it");
            let out = mask_with(input, &[]).unwrap_or_else(|| panic!("{input}: dropped"));
            for part in *wanted {
                assert!(out.contains(part), "{input}: {out} lacks {part}");
            }
            for part in *forbidden {
                assert!(!out.contains(part), "{input}: {out} holds {part}");
            }
        }
        // Over-masking, accepted: a Rust path whose last segment reads as hex ("Abc::" parses as an address).
        assert_eq!(mask_with("Feed::x", &[]).as_deref(), Some("<ipv6 public>x"));
        // A firmware version keeps its shape only as the text of a version element; elsewhere it reads as an address.
        for version in ["3.0.0.88_2306", "v3.0.0.88_2306"] {
            let text = format!("<firmwareVersion>{version}</firmwareVersion>");
            let text = text.as_str();
            assert!(!residue(text), "{text}");
            assert_eq!(mask_with(text, &[]).as_deref(), Some(text));
        }
        assert_eq!(mask_with("fw v3.0.0.88_2306 ok", &[]).as_deref(), Some("fw v<ipv4 public>_2306 ok"));
    }

    /// The third review's file cases: the model line and a body's element names reach the file with the joined
    /// shapes masked.
    #[test]
    fn joined_shapes_never_reach_the_file() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let path = scratch("joined").join(FILE_NAME);
        set_file(Some(path.clone()));
        masked_keeping_names(format_args!("[conn 999007] DEBUG identity model: dev192.0.2.7"));
        let body = mask_body("<face00:11:22:33:44:55/><peer:::abcd/>");
        masked(format_args!("[conn 999007] DEBUG version body text (as read): {body}"));
        set_file(None);
        let ours: Vec<String> = lines_of(&path).into_iter().filter(|l| l.contains("[conn 999007]")).collect();
        assert_eq!(ours.len(), 2, "{ours:?}");
        for leak in ["192.0.2.7", "44:55", "abcd", "WITHHELD"] {
            assert!(!ours.iter().any(|l| l.contains(leak)), "{leak} in {ours:?}");
        }
    }

    /// Text a device supplies reaches a line only in its allowed form, else as a fixed placeholder: element and
    /// attribute names, a version where a version is held, the model word. Rows: (body or model, kept, refused).
    /// All values synthetic.
    #[test]
    fn device_text_reaches_a_line_only_in_its_allowed_form() {
        let bodies: &[(&str, &[&str], &[&str])] = &[
            (
                "<VersionInfo version=\"1.1\"><channelId>3</channelId></VersionInfo>",
                &["<VersionInfo version=\"1.1\">", "<channelId>3</channelId>"],
                &[],
            ),
            ("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>", &["<?xml version=\"1.0\" encoding=\"<5 bytes>\" ?>"], &[]),
            ("<peer:::be/>", &[NAME_PLACEHOLDER], &["::be", "peer"]),
            ("<v192.0.2.9_42/>", &[NAME_PLACEHOLDER], &["192.0", "v192"]),
            ("<a.b>1</a.b>", &[NAME_PLACEHOLDER], &["a.b"]),
            ("<_x/><9x/><x-y/>", &[NAME_PLACEHOLDER], &["_x", "9x", "x-y"]),
            ("<ThisElementNameIsLongerThanThirtyTwoBytes/>", &[NAME_PLACEHOLDER], &["ThisElement"]),
            ("<x fe80:1=\"1\"/>", &[NAME_PLACEHOLDER], &["fe80"]),
            ("<x v10.0.0.7_1=\"1\"/>", &[NAME_PLACEHOLDER], &["10.0"]),
            ("<x version=\"192.0.2.9\"/>", &["version=\"<9 bytes>\""], &["192.0"]),
            ("<firmwareVersion>v3.0.0.88_2306</firmwareVersion>", &["v3.0.0.88_2306"], &[]),
            ("<hardwareVersion>3.0.0.88_2306</hardwareVersion>", &["3.0.0.88_2306"], &[]),
            // A version shape anywhere else is no version.
            ("<type>v3.0.0.88_2306</type>", &["<type><14 bytes></type>"], &["3.0.0"]),
        ];
        for (input, kept, refused) in bodies {
            let out = mask_with(&mask_body(input), &[]).unwrap_or_else(|| panic!("{input}: withheld"));
            for part in *kept {
                assert!(out.contains(part), "{input}: {out} lacks {part}");
            }
            for part in *refused {
                assert!(!out.contains(part), "{input}: {out} holds {part}");
            }
        }
        let models: &[(&str, &str)] = &[
            ("RLC-810A", "RLC-810A"),
            ("Reolink Home Hub Pro", "Reolink Home Hub Pro"),
            ("E1 Zoom+", "E1 Zoom+"),
            (" RLN36 ", "RLN36"),
            ("v192.0.2.9_42", "<13 bytes>"),
            ("cam 10.0.0.7", "<12 bytes>"),
            ("fe80::1", "<7 bytes>"),
            ("a@b", "<3 bytes>"),
            ("", "<0 bytes>"),
            ("9000", "<4 bytes>"),
            ("Model With A Very Long Name Indeed", "<34 bytes>"),
        ];
        for (model, shown) in models {
            assert_eq!(model_word(model), *shown, "{model}");
        }
        // The version exemption holds only right after a version element's tag.
        for text in ["fw v3.0.0.88_2306 ok", "model v192.0.2.9_42"] {
            assert!(residue(text) || has_shapes(text), "{text}");
        }
    }

    /// The reviewers' two probes: the short digitless IPv6 in an element name and the version-shaped IPv4 in an
    /// element name and as the model never reach the file, and the line is kept, not withheld.
    #[test]
    fn the_reviewed_device_text_never_reaches_the_file() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let path = scratch("device-text").join(FILE_NAME);
        set_file(Some(path.clone()));
        let body = mask_body("<peer:::be/><v192.0.2.9_42/><QuuxFox/>");
        masked(format_args!("[conn 999009] DEBUG version body text (as read): {body}"));
        masked_keeping_names(format_args!("[conn 999009] DEBUG identity model: {}", model_word("v192.0.2.9_42")));
        set_file(None);
        let ours: Vec<String> = lines_of(&path).into_iter().filter(|l| l.contains("[conn 999009]")).collect();
        assert_eq!(ours.len(), 2, "{ours:?}");
        for leak in ["::be", "192.0.2.9", "WITHHELD"] {
            assert!(!ours.iter().any(|l| l.contains(leak)), "{leak} in {ours:?}");
        }
        assert!(ours[0].contains("<QuuxFox/>"), "{ours:?}");
    }

    /// A version field that holds exactly four dotted numbers, after an optional `v`, is an address and reaches the
    /// file masked, whether the body went through `mask_body` or the line quotes it as it was read; a version with
    /// a build number or any other suffix is kept.
    #[test]
    fn a_dotted_quad_in_a_version_field_is_masked_in_the_file() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let path = scratch("version-quad").join(FILE_NAME);
        set_file(Some(path.clone()));
        let refused = [
            "<firmwareVersion>192.0.2.9</firmwareVersion>",
            "<hardwareVersion>v192.0.2.10</hardwareVersion>",
            "<firmwareVersion>V192.0.2.11</firmwareVersion>",
        ];
        let kept = [
            "<firmwareVersion>3.0.0.88_2306</firmwareVersion>",
            "<hardwareVersion>v3.0.0.88_2306</hardwareVersion>",
            "<firmwareVersion>v3.0.0.2356_23062000</firmwareVersion>",
            "<firmwareVersion>v3.0.0.88_2306 </firmwareVersion>",
            "<hardwareVersion>  v3.0.0.88_2306 </hardwareVersion>",
            "<hardwareVersion>3.0.0.88.1</hardwareVersion>",
            "<hardwareVersion>3.0.0.88.</hardwareVersion>",
            "<firmwareVersion>3.0.0.88_</firmwareVersion>",
        ];
        for body in refused.iter().chain(&kept) {
            masked(format_args!("[conn 999010] DEBUG version body text (as read): {}", mask_body(body)));
            masked(format_args!("[conn 999010] DEBUG version raw: {body}"));
        }
        set_file(None);
        let ours: Vec<String> = lines_of(&path).into_iter().filter(|l| l.contains("[conn 999010]")).collect();
        assert_eq!(ours.len(), 2 * (refused.len() + kept.len()), "{ours:?}");
        for leak in ["192.0.2", "WITHHELD"] {
            assert!(!ours.iter().any(|l| l.contains(leak)), "{leak} in {ours:?}");
        }
        for body in kept {
            let version = &body[body.find('>').unwrap_or(0)..=body.rfind('<').unwrap_or(0)];
            assert_eq!(ours.iter().filter(|l| l.contains(version)).count(), 2, "{version}: {ours:?}");
        }
    }

    /// The whole-line check withholds a line that still holds a shape the mask knows (a UID, which the coarse check
    /// alone does not see) or one it does not know (the coarse check), and passes a clean one.
    #[test]
    fn the_line_check_withholds_every_shape_left_in_a_line() {
        for text in ["uid TESTCAMUID000001 on", "uid QWERTYUIOP12", "x_TESTCAMUID000001_y"] {
            assert!(!residue(text), "{text}: the coarse check sees it, so this row tests nothing");
            assert!(withholds(text), "{text}");
        }
        for text in ["uuid 123e4567-e89b-12d3-a456-426614174000", "id 0011-2233-4455 x", "mail a@b"] {
            assert!(withholds(text), "{text}");
        }
        for text in ["uid <uid> on", "channels 0x1: mainStream 2560x1440 15 fps", "code 0xdd12 class 0x6614"] {
            assert!(!withholds(text), "{text}");
        }
    }

    /// When the first layer misses a shape it knows, the final check in the masking path withholds the line, also a
    /// shape the coarse check alone does not see (a UID).
    #[test]
    fn the_final_check_withholds_what_a_broken_first_layer_left() {
        let missed_everything: fn(&str) -> String = str::to_string;
        for text in ["uid TESTCAMUID000001 on", "uid QWERTYUIOP12", "at 192.0.2.9 and 2001:db8::7"] {
            assert_eq!(mask_through(text, &[], missed_everything), None, "{text}");
        }
        assert_eq!(mask_through("code 200", &[], missed_everything).as_deref(), Some("code 200"));
    }

    /// The coarse check withholds a line with an identifier in a shape the mask does not know.
    #[test]
    fn a_shape_the_mask_does_not_know_is_withheld() {
        for text in ["uuid 123e4567-e89b-12d3-a456-426614174000", "id 0011-2233-4455 x"] {
            assert_eq!(mask_with(text, &[]), None, "{text}");
        }
    }

    /// The mask reads at most a line's worth, cut back to a space so that no identifier is split by the cut.
    #[test]
    fn the_mask_reads_a_bounded_part_of_its_input() {
        let out = mask_with(&"g".repeat(262_144), &[]).unwrap();
        assert!(out.len() <= LINE_LIMIT + 32, "{}", out.len());
        // A MAC across the cut: none of it shows.
        let text = format!("{}00:11:22:33:44:55 tail", "w ".repeat(LINE_LIMIT / 2 - 3));
        assert!(text.find("00:11").unwrap() < LINE_LIMIT && text.find("44:55").unwrap() > LINE_LIMIT - 17);
        let out = mask_with(&text, &[]).unwrap();
        assert!(!out.contains("00:1") && out.ends_with(" more bytes)"), "{}", &out[out.len() - 40..]);
    }

    /// What a line shows on stderr (`masked`, `always`) and in the file: masked, or the fixed marker.
    #[test]
    fn a_new_line_shows_masked_and_an_unsafe_one_as_the_marker() {
        let mut learned = Names::default();
        learned.learn("Garden Cam");
        let shown = safe_text("[conn 7 +5 ms] DEBUG identity model: TESTCAMUID000001 Garden Cam", &learned);
        assert_eq!(shown, "[conn 7 +5 ms] DEBUG identity model: <uid> <name>");
        assert_eq!(safe_text("peer x10.0.0.7y answered", &learned), "peer x<ipv4 private>y answered");
        // The model line keeps the names (see `masked_keeping_names`), not the token masks.
        let model = safe_text("model Garden Cam Pro on 10.0.0.7", &Names::default());
        assert_eq!(model, "model Garden Cam Pro on <ipv4 private>");
    }

    /// With `REOLING_DEBUG_NEGOTIATION`, stderr gets the new lines (`masked`) masked and the older ones (`line`,
    /// `always`) as they are. Run in a child process of this test binary: the variable is read once per process.
    #[test]
    fn the_env_stderr_path_masks_the_new_lines() {
        if std::env::var_os("REOLING_DIAG_STDERR_CHILD").is_some() {
            masked(format_args!("[conn 1] DEBUG identity model: TESTCAMUID000001"));
            conceal("device");
            always(format_args!("login refused by the device: code 401"));
            line(format_args!("[conn 1] DEBUG older line TESTCAMUID000002"));
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "diag::tests::the_env_stderr_path_masks_the_new_lines", "--nocapture"])
            .env("REOLING_DIAG_STDERR_CHILD", "1")
            .env("REOLING_DEBUG_NEGOTIATION", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains("[conn 1] DEBUG identity model: <uid>"), "{err}");
        assert!(!err.contains("TESTCAMUID000001"), "{err}");
        assert!(err.contains("login refused by the device: code 401"), "as before the log: {err}");
        assert!(err.contains("[conn 1] DEBUG older line TESTCAMUID000002"), "the harness reads these as they are");
    }

    /// Without the debug switch, the lines `always` printed before the diagnostic log reach stderr as they did, also
    /// once the learned names overflowed; the file then withholds every line, its header and the model line too.
    #[test]
    fn past_the_name_cap_stderr_keeps_its_lines_and_the_file_withholds_all() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/diag-tests/overflow");
        if std::env::var_os("REOLING_DIAG_OVERFLOW_CHILD").is_some() {
            for at in 0..=NAME_CAP {
                conceal(&format!("name{at}"));
            }
            set_file(Some(dir.join(FILE_NAME)));
            always(format_args!("connecting to the device failed: timed out"));
            masked_keeping_names(format_args!("[conn 1] DEBUG identity model: TESTMODEL"));
            masked(format_args!("[conn 1] DEBUG version answered"));
            set_file(None);
            return;
        }
        let _ = std::fs::remove_dir_all(&dir);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "diag::tests::past_the_name_cap_stderr_keeps_its_lines_and_the_file_withholds_all"])
            .args(["--nocapture"])
            .env("REOLING_DIAG_OVERFLOW_CHILD", "1")
            .env_remove("REOLING_DEBUG_NEGOTIATION")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains("connecting to the device failed: timed out"), "{err}");
        assert!(!err.contains(WITHHELD), "{err}");
        let file = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert_eq!(file.lines().count(), 5, "{file}");
        assert!(file.lines().all(|l| l.ends_with(WITHHELD)), "{file}");
    }

    /// The lines `always` printed before the diagnostic log keep their stderr text through the mask.
    #[test]
    fn the_earlier_always_lines_keep_their_text() {
        for text in [
            "login refused by the device: code 401",
            "connecting to the device failed: timed out",
            "logging in to the device failed: connection lost",
            "[conn 12 +3456 ms] NEGOTIATION experiment REOLING_TRY_LEGACY_CIPHER active: the session goes on under \
             BCEncrypt",
            "[conn 12 +3456 ms] NEGOTIATION experiment REOLING_TRY_PREVIEW_POLICY active: this Preview carries the \
             policy element",
        ] {
            assert_eq!(safe_text(text, &Names::default()), text);
        }
    }

    /// Each name once whatever its case; past the cap every line is withheld rather than one name left unmasked.
    #[test]
    fn the_name_registry_is_bounded() {
        let mut learned = Names::default();
        for name in ["Hub", "HUB", " hub ", "Grădină", "GRĂDINĂ", ""] {
            learned.learn(name);
        }
        assert_eq!(learned.list.len(), 2, "{:?}", learned.list);
        for at in 0..NAME_CAP {
            learned.learn(&format!("name{at}"));
        }
        assert_eq!(learned.list.len(), NAME_CAP);
        assert!(learned.overflow);
        assert_eq!(safe_text("device online", &learned), WITHHELD);
    }

    /// A huge reply body is read only up to its bound: the mask's work does not grow with it.
    #[test]
    fn a_huge_body_is_read_only_up_to_its_bound() {
        let started = Instant::now();
        for body in ["<".repeat(4_000_000), format!("<a x=\"{}", "y".repeat(4_000_000))] {
            let out = mask_body(&body);
            assert!(out.len() <= BODY_LIMIT + 64, "{}", out.len());
            assert!(out.ends_with(" bytes not read)"), "{}", &out[out.len() - 40..]);
        }
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn an_address_at_the_end_of_a_sentence_is_masked_too() {
        let out = mask_with("talking to 10.0.0.7. Then 127.0.0.1, then 0.0.0.0:0", &[]).unwrap();
        assert_eq!(out, "talking to <ipv4 private>. Then <ipv4 loopback>, then <ipv4 any>:0");
    }

    #[test]
    fn a_line_the_mask_cannot_make_safe_is_dropped() {
        // Joined to other text, an address is masked where it stands (overflow withholds, see
        // `the_name_registry_is_bounded`).
        assert_eq!(mask_with("peer x10.0.0.7y answered", &[]), Some("peer x<ipv4 private>y answered".to_string()));
        assert_eq!(mask_with("from a1@b2.example", &[]), Some("from <email>".to_string()));
        assert!(mask_with("firmware v3.0.0.88_2306 and reoling 1.0.6", &[]).is_some());
    }

    #[test]
    fn line_breaks_in_a_body_stay_on_one_line() {
        let out = mask_with(&mask_body("<?xml version=\"1.0\" ?>\n<body>\n<code>200</code>\n</body>\n"), &[]).unwrap();
        assert!(!out.contains('\n'), "{out}");
        assert!(out.contains("<?xml version=\"1.0\" ?>"), "{out}");
        assert!(out.contains("<code>200</code>"), "{out}");
    }

    #[test]
    fn malformed_markup_is_masked_as_text() {
        let out = mask_body("<body><name>Fro<nt door</body><x a=b>secret");
        for part in ["Fro", "nt door", "secret", "a=b"] {
            assert!(!out.contains(part), "{out} holds {part}");
        }
    }

    #[test]
    fn a_body_is_cut_to_its_limit() {
        let long = format!("<body><code>{}</code></body>", "1".repeat(5000));
        assert!(mask_body(&long).len() <= BODY_LIMIT + 32);
    }

    #[test]
    fn mask_never_panics_and_never_grows_past_the_cap() {
        // A cheap deterministic generator over every byte value, read lossily as the file writer reads a body.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2000 {
            let len = (state % 300) as usize;
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            let body = mask_body(&text);
            if let Some(out) = mask_with(&body, &names(&["Hub", "Garden Cam"])) {
                let line = format_line(Duration::from_millis(5), Some(1), &out);
                assert!(line.len() <= LINE_LIMIT + 64, "{}", line.len());
            }
            let _ = mask_with(&text, &[]);
        }
    }

    #[test]
    fn a_line_carries_the_time_and_its_owner() {
        let at = Duration::from_millis(12_345);
        let race = "[race 4 +180 ms] NEGOTIATION race: x";
        assert_eq!(format_line(at, Some(2), race), "+12.345 [dev 2] [race 4 +180 ms] NEGOTIATION race: x");
        assert_eq!(format_line(at, None, "[conn 6 +555 ms] DEBUG y"), "+12.345 [conn 6 +555 ms] DEBUG y");
        assert_eq!(format_line(at, None, "[race 1 +0 ms] z"), "+12.345 [race 1 +0 ms] z");
        assert_eq!(format_line(at, None, "NEGOTIATION diagnostic build"), "+12.345 [app] NEGOTIATION diagnostic build");
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        let text = format!("[conn 1] {}", "é".repeat(3000));
        let line = format_line(Duration::ZERO, None, &text);
        assert!(line.len() <= LINE_LIMIT + 32, "{}", line.len());
        assert!(line.ends_with(" more bytes)"), "{}", &line[line.len() - 30..]);
    }

    /// A fresh directory for one test under `target/`.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("diag-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lines_of(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_string).collect()
    }

    #[test]
    fn with_the_switch_off_nothing_is_written() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("off");
        set_file(None);
        line(format_args!("[conn 999001] DEBUG identity version: version reply code 200"));
        always(format_args!("[conn 999001] NEGOTIATION experiment policy active"));
        flush();
        assert_eq!(current_path(), None);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn with_the_switch_on_lines_reach_the_file_masked_at_once() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let path = scratch("on").join("reoling-diagnostic.log");
        set_file(Some(path.clone()));
        assert_eq!(current_path(), Some(path.clone()));
        line(format_args!("[conn 999002] DEBUG uid TESTCAMUID000001 at 192.168.1.50:9000"));
        let lines = lines_of(&path);
        let header = "diagnostic log. Masked wherever they stand:";
        assert!(lines[0].contains("[app] Reoling ") && lines[0].contains(header), "{lines:?}");
        assert!(lines[0].ends_with("Never written: passwords, nonces, keys."), "the header is whole: {lines:?}");
        let ours: Vec<&String> = lines.iter().filter(|l| l.contains("[conn 999002]")).collect();
        assert_eq!(ours.len(), 1, "{lines:?}");
        assert!(ours[0].ends_with("[conn 999002] DEBUG uid <uid> at <ipv4 private>:9000"), "{}", ours[0]);
        assert!(ours[0].starts_with('+'), "{}", ours[0]);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        set_file(None);
        line(format_args!("[conn 999002] DEBUG after"));
        let lines = lines_of(&path);
        assert!(lines.last().unwrap().ends_with("NEGOTIATION diagnostics off"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("DEBUG after")), "{lines:?}");
    }

    #[test]
    fn asking_again_for_the_open_file_keeps_it_and_a_new_session_moves_it_aside() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("again");
        let path = dir.join("reoling-diagnostic.log");
        set_file(Some(path.clone()));
        line(format_args!("[conn 999003] first"));
        set_file(Some(path.clone()));
        assert!(lines_of(&path).iter().any(|l| l.contains("[conn 999003] first")));
        assert!(!dir.join("reoling-diagnostic.1.log").exists());
        set_file(None);
        set_file(Some(path.clone()));
        assert!(lines_of(&dir.join("reoling-diagnostic.1.log")).iter().any(|l| l.contains("[conn 999003] first")));
        assert!(!lines_of(&path).iter().any(|l| l.contains("[conn 999003] first")));
        set_file(None);
    }

    #[test]
    fn a_full_file_starts_again_beside_the_previous_one() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("full");
        let path = dir.join("reoling-diagnostic.log");
        set_file(Some(path.clone()));
        let filler = "x".repeat(4000);
        for _ in 0..(FILE_LIMIT as usize / 4000 + 2) {
            line(format_args!("[conn 999004] {filler}"));
        }
        set_file(None);
        assert!(std::fs::metadata(&path).unwrap().len() <= FILE_LIMIT);
        assert!(std::fs::metadata(dir.join("reoling-diagnostic.1.log")).unwrap().len() <= FILE_LIMIT);
        assert!(lines_of(&path)[0].contains("diagnostic log. Masked wherever they stand:"));
    }

    /// A file that cannot be opened is reported, not tried again on every settings edit, and tried again once the
    /// switch is turned off and on.
    #[test]
    fn a_file_that_cannot_be_opened_is_reported_and_not_retried_each_edit() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("unopenable");
        std::fs::write(dir.join("state"), "a file where the directory should be").unwrap();
        let path = dir.join("state").join("reoling-diagnostic.log");
        set_file(None);
        set_file(Some(path.clone()));
        let Recording::Failed(failed) = recording() else { panic!("{:?}", recording()) };
        assert!(failed.opening && failed.path == path, "{failed:?}");
        // The directory appears; a settings edit applies the same path again and changes nothing.
        std::fs::remove_file(dir.join("state")).unwrap();
        set_file(Some(path.clone()));
        assert_eq!(recording(), Recording::Failed(failed));
        set_file(None);
        assert_eq!(recording(), Recording::Off);
        set_file(Some(path.clone()));
        assert_eq!(recording(), Recording::On(path.clone()));
        set_file(None);
    }

    /// A write error closes the file and says so; the lines after it go nowhere.
    #[test]
    fn a_write_error_stops_the_file_and_is_reported() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        set_file(None);
        let full = Path::new("/dev/full");
        let out = BufWriter::new(OpenOptions::new().write(true).open(full).unwrap());
        *file_slot() = Some(LogFile { path: full.to_path_buf(), out, written: 0 });
        FILE_ON.store(true, Relaxed);
        line(format_args!("[conn 999005] DEBUG nowhere to go"));
        assert!(!FILE_ON.load(Relaxed));
        let kind = std::io::ErrorKind::StorageFull;
        assert_eq!(recording(), Recording::Failed(Failure { path: full.to_path_buf(), opening: false, kind }));
        set_file(None);
        assert_eq!(recording(), Recording::Off);
    }

    /// Save As copies the log, and refuses the log itself under its own name or another one.
    #[test]
    fn exporting_refuses_the_log_itself() {
        let _held = TEST_FILE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("export");
        let path = dir.join("reoling-diagnostic.log");
        set_file(Some(path.clone()));
        line(format_args!("[conn 999006] DEBUG kept"));
        assert_eq!(export(&path, &path), Err(ExportError::TheLogItself));
        std::fs::hard_link(&path, dir.join("hard")).unwrap();
        std::os::unix::fs::symlink(&path, dir.join("soft")).unwrap();
        assert_eq!(export(&path, &dir.join("hard")), Err(ExportError::TheLogItself));
        assert_eq!(export(&path, &dir.join("soft")), Err(ExportError::TheLogItself));
        assert!(lines_of(&path).iter().any(|l| l.contains("DEBUG kept")), "the log is untouched");
        let copy = dir.join("copy.log");
        assert!(export(&path, &copy).is_ok());
        assert!(lines_of(&copy).iter().any(|l| l.contains("[conn 999006] DEBUG kept")));
        set_file(None);
    }

    /// No debug line formats a password, a nonce or a key: as a format argument, or captured in its text.
    #[test]
    fn no_line_formats_a_password_a_nonce_or_a_key() {
        let sources = [
            ("client.rs", include_str!("client.rs")),
            ("client/settings.rs", include_str!("client/settings.rs")),
            ("client/replay.rs", include_str!("client/replay.rs")),
            ("transport/discovery.rs", include_str!("transport/discovery.rs")),
            ("ui/bridge.rs", include_str!("ui/bridge.rs")),
            ("ui/main_window.rs", include_str!("ui/main_window.rs")),
        ];
        let calls = [
            "diag::line(format_args!(",
            "diag::masked(format_args!(",
            "diag::masked_keeping_names(format_args!(",
            "diag::line_split(format_args!(",
            "diag::always(format_args!(",
            "diag::always_under(",
            "note(format_args!(",
        ];
        let mut checked = 0;
        for (file, text) in sources {
            for call in calls {
                for (at, _) in text.match_indices(call) {
                    let args = call_arguments(&text[at + call.len()..]);
                    let (code, literals) = split_literals(args);
                    let words = code.split(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                    let captures = literals.split('{').skip(1).map(|c| c.split(['}', ':']).next().unwrap_or(""));
                    for word in words.chain(captures) {
                        assert!(!["password", "nonce", "key"].contains(&word), "{file}: {word} in {args}");
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 60, "{checked} calls checked");
        // The check itself finds both shapes.
        let (code, literals) = split_literals(call_arguments(r#""{at}x {nonce}", key.len())); y"#));
        assert!(code.contains("key") && literals.contains("{nonce}") && !code.contains("; y"));
    }

    /// The text up to the parenthesis that closes the call `text` starts inside.
    fn call_arguments(text: &str) -> &str {
        let mut depth = 1;
        let mut in_string = false;
        let mut escaped = false;
        for (at, c) in text.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' if in_string => escaped = true,
                '"' => in_string = !in_string,
                '(' if !in_string => depth += 1,
                ')' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return &text[..at];
                    }
                }
                _ => {}
            }
        }
        text
    }

    /// The code of `args` without its string literals, and the literals.
    fn split_literals(args: &str) -> (String, String) {
        let (mut code, mut literals) = (String::new(), String::new());
        let mut in_string = false;
        let mut escaped = false;
        for c in args.chars() {
            match c {
                _ if escaped => escaped = false,
                '\\' if in_string => escaped = true,
                '"' => in_string = !in_string,
                c if in_string => literals.push(c),
                c => code.push(c),
            }
        }
        (code, literals)
    }

    #[test]
    fn the_log_lives_in_the_state_directory() {
        let home = Path::new("/home/u");
        let default = Path::new("/home/u/.local/state/reoling/reoling-diagnostic.log");
        assert_eq!(log_path(None, home), default);
        assert_eq!(log_path(Some(OsStr::new("/x")), home), Path::new("/x/reoling/reoling-diagnostic.log"));
        assert_eq!(log_path(Some(OsStr::new("rel")), home), default);
        assert_eq!(log_path(Some(OsStr::new("")), home), default);
    }
}
