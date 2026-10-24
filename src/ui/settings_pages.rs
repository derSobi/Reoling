//! What the read-only settings tabs show. Each page a device answered is
//! turned into plain groups of rows — a title with a text, a fill level, a
//! weekly schedule — with the device's own words and numbers put into the
//! app's. Nothing here needs GTK, so every translation can be tested; the
//! settings windows only render the groups.

use reoling::protocol::bc::xml::VersionInfo;
use reoling::{
    AccessUserList, AiCfg, AiDetectCfg, AutoReboot, CameraDetectionPage, CameraDisplayPage, CameraImagePage,
    CameraInfoPage, CameraLightSoundPage, CameraSchedulesPage, ChannelAbilities, ChannelInfo, ChannelSwitch,
    DayNightThreshold, DeviceAlertsPage, DeviceAudioCfg, DeviceInfoPage, DeviceLightSoundPage,
    DeviceMaintenancePage, DeviceNetworkPage, DeviceStoragePage, DeviceTimePage, DeviceUsersPage, Dst, Enable,
    FloodlightTask, HddInfo, HourSpan, InputAdvanceCfg, KitApCfg, LedState, MinMaxCur, MotionDetect, NetworkRows,
    Ntp, OsdDatetime, OsdName, PowerLineFrequency, Read, ReadFailure, RecordCfg, ScheduleTable, ScheduledTask,
    ServicePort, SettingsPage, SystemGeneral, UserList, CivilTime,
};

/// What a row says.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A plain text ("On", "50 (0–100)", "—").
    Text(String),
    /// How full something is, with the text beside it.
    Level { text: String, fraction: f64 },
    /// Weekly schedules, one per type label (an all-off one is listed as
    /// text by the grid widget).
    Schedules(Vec<(String, ScheduleTable)>),
    /// A text with lines that unfold under it.
    Lines { text: String, lines: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub title: String,
    pub value: Value,
}

impl Row {
    fn text(title: &str, value: impl Into<String>) -> Self {
        Row { title: title.to_string(), value: Value::Text(value.into()) }
    }
}

/// A heading with its rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub title: String,
    pub rows: Vec<Row>,
}

impl Group {
    fn new(title: &str, rows: Vec<Row>) -> Self {
        Group { title: title.to_string(), rows }
    }

    /// A group whose read failed: one row saying so.
    fn failed(title: &str, failure: ReadFailure) -> Self {
        Group::new(title, vec![Row::text("", failure_text(failure))])
    }
}

/// A tab's content.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tab {
    /// A line above the groups: why there is nothing to show, or empty.
    pub status: String,
    pub groups: Vec<Group>,
}

/// What the rows depend on besides the page itself.
#[derive(Debug, Clone, Default)]
pub struct Context<'a> {
    /// The channel's abilities (a camera page); `None` until the device has
    /// said, which shows every row the reply allows.
    pub abilities: Option<ChannelAbilities>,
    /// The device's channels (the Alerts tab names them).
    pub channels: &'a [ChannelInfo],
    /// The masked UID the device Info tab shows: the app's own, never read
    /// from the device.
    pub uid_hint: Option<String>,
    /// The local time now as civil seconds (Unix time plus the local UTC
    /// offset), for dates computed from a duration.
    pub local_now: Option<i64>,
}

pub const NOT_AVAILABLE: &str = "Not available on this device";
pub const NO_ANSWER: &str = "The device did not answer";
const DASH: &str = "—";

/// Builds the tab for one page the device answered.
pub fn build(page: &SettingsPage, ctx: &Context) -> Tab {
    match page {
        SettingsPage::CameraInfo { page, .. } => camera_info(page),
        SettingsPage::CameraImage { page, .. } => camera_image(page, ctx.abilities),
        SettingsPage::CameraDisplay { page, .. } => camera_display(page),
        SettingsPage::CameraDetection { page, .. } => camera_detection(page, ctx.abilities),
        SettingsPage::CameraLightSound { page, .. } => camera_light_sound(page, ctx.abilities),
        SettingsPage::CameraSchedules { page, .. } => camera_schedules(page, ctx.abilities),
        SettingsPage::CameraNetwork { page, .. } => {
            let mut tab = Tab {
                status: status(&[outcome(&page.dhcp), outcome(&page.auto_dns), outcome(&page.ip), outcome(&page.dns)]),
                groups: Vec::new(),
            };
            if let Some(group) = network_group(page) {
                tab.groups.push(group);
            }
            tab
        }
        SettingsPage::DeviceInfo(page) => device_info(page, ctx.uid_hint.as_deref()),
        SettingsPage::DeviceStorage(page) => device_storage(page),
        SettingsPage::DeviceNetwork(page) => device_network(page),
        SettingsPage::DeviceTime(page) => device_time(page),
        SettingsPage::DeviceLightSound(page) => device_light_sound(page),
        SettingsPage::DeviceAlerts(page) => device_alerts(page, ctx.channels),
        SettingsPage::DeviceUsers(page) => device_users(page, ctx.local_now),
        SettingsPage::DeviceMaintenance(page) => device_maintenance(page),
    }
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// `None` for a value, the failure otherwise.
fn outcome<T>(read: &Read<T>) -> Option<ReadFailure> {
    read.as_ref().err().copied()
}

/// The line above a tab's groups, from every read it sent: empty as soon as
/// one answered; otherwise why none did.
fn status(outcomes: &[Option<ReadFailure>]) -> String {
    let sent: Vec<ReadFailure> = outcomes.iter().flatten().copied().filter(|f| *f != ReadFailure::NotSent).collect();
    let answered = outcomes.iter().any(Option::is_none);
    if answered || sent.is_empty() {
        return String::new();
    }
    if sent.iter().all(|f| *f == ReadFailure::NoReply) {
        NO_ANSWER.to_string()
    } else {
        NOT_AVAILABLE.to_string()
    }
}

/// What a row or group says instead of a value its read did not bring.
pub fn failure_text(failure: ReadFailure) -> &'static str {
    match failure {
        ReadFailure::NoReply => NO_ANSWER,
        ReadFailure::Refused(_) | ReadFailure::Unreadable | ReadFailure::Missing | ReadFailure::NotSent => NOT_AVAILABLE,
    }
}

/// A group for one read: its rows, one row saying why not, or nothing at
/// all when the read was never sent.
fn group_for<T>(title: &str, read: &Read<T>, rows: impl FnOnce(&T) -> Vec<Row>) -> Option<Group> {
    match read {
        Ok(value) => Some(Group::new(title, rows(value))),
        Err(ReadFailure::NotSent) => None,
        Err(failure) => Some(Group::failed(title, *failure)),
    }
}

/// A row for one read inside a group of several: the value, or why not;
/// nothing when the read was never sent.
fn row_for<T>(title: &str, read: &Read<T>, value: impl FnOnce(&T) -> String) -> Option<Row> {
    match read {
        Ok(v) => Some(Row::text(title, value(v))),
        Err(ReadFailure::NotSent) => None,
        Err(failure) => Some(Row::text(title, failure_text(*failure))),
    }
}

// ---------------------------------------------------------------------------
// Words and numbers
// ---------------------------------------------------------------------------

fn num(value: Option<u32>) -> String {
    value.map_or_else(|| DASH.to_string(), |v| v.to_string())
}

fn text(value: &Option<String>) -> String {
    match value.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => DASH.to_string(),
    }
}

/// `0` is off, any other number on.
pub fn on_off(value: Option<u32>) -> String {
    match value {
        Some(0) => "Off".to_string(),
        Some(_) => "On".to_string(),
        None => DASH.to_string(),
    }
}

fn yes_no(value: Option<u32>) -> String {
    match value {
        Some(0) => "No".to_string(),
        Some(_) => "Yes".to_string(),
        None => DASH.to_string(),
    }
}

fn seconds(value: Option<u32>) -> String {
    value.map_or_else(|| DASH.to_string(), |v| format!("{v} s"))
}

/// The device's lower-case words ("auto", "night") with a capital.
pub fn capitalised(word: &str) -> String {
    let word = word.trim();
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => DASH.to_string(),
    }
}

fn capitalised_opt(value: &Option<String>) -> String {
    value.as_deref().map_or_else(|| DASH.to_string(), capitalised)
}

/// The anti-flicker setting: off, or its mode ("50hz" → "50 Hz").
pub fn anti_flicker(cfg: &PowerLineFrequency) -> String {
    if cfg.enable == Some(0) {
        return "Off".to_string();
    }
    let mode = cfg.mode.as_deref().map(str::trim).unwrap_or("");
    match mode.strip_suffix("hz").or_else(|| mode.strip_suffix("Hz")).or_else(|| mode.strip_suffix("HZ")) {
        Some(number) => format!("{} Hz", number.trim()),
        None if mode.is_empty() => DASH.to_string(),
        None => capitalised(mode),
    }
}

/// Where an overlay sits, from its two coordinates: each axis is at the
/// start edge (0 or 1), the far edge (65536) or the centre (65537).
pub fn position_label(x: Option<u32>, y: Option<u32>) -> String {
    #[derive(PartialEq)]
    enum Axis {
        Start,
        Centre,
        Far,
    }
    let axis = |v: u32| match v {
        0 | 1 => Some(Axis::Start),
        65537 => Some(Axis::Centre),
        65536 => Some(Axis::Far),
        _ => None,
    };
    let (Some(x), Some(y)) = (x, y) else { return DASH.to_string() };
    match (axis(x), axis(y)) {
        (Some(Axis::Start), Some(Axis::Start)) => "Top left",
        (Some(Axis::Centre), Some(Axis::Start)) => "Top centre",
        (Some(Axis::Far), Some(Axis::Start)) => "Top right",
        (Some(Axis::Start), Some(Axis::Far)) => "Bottom left",
        (Some(Axis::Centre), Some(Axis::Far)) => "Bottom centre",
        (Some(Axis::Far), Some(Axis::Far)) => "Bottom right",
        (Some(Axis::Centre), Some(Axis::Centre)) => "Centre",
        _ => return format!("{x} / {y}"),
    }
    .to_string()
}

/// The app's name for a detection or schedule type the device names.
/// `None` for the type that means nothing at all.
pub fn type_label(kind: &str, pet: bool) -> Option<String> {
    let label = match kind.trim() {
        "none" | "" => return None,
        "MD" => "Any motion",
        "people" => "Person",
        "vehicle" => "Vehicle",
        "dog_cat" if pet => "Pet",
        "dog_cat" => "Animal",
        "Normal" => "Continuous",
        other => other,
    };
    Some(label.to_string())
}

/// A comma list of types ("people, vehicle") as labels.
pub fn type_list(csv: &str, pet: bool) -> String {
    let labels: Vec<String> = csv.split(',').filter_map(|k| type_label(k, pet)).collect();
    if labels.is_empty() {
        DASH.to_string()
    } else {
        labels.join(", ")
    }
}

/// A task's schedules by type label. Types without a name of their own are
/// listed under the device's word only when they have a set hour; a table
/// that is not 168 cells is left out.
pub fn schedules(task: &ScheduledTask, pet: bool) -> Vec<(String, ScheduleTable)> {
    let mut out = Vec::new();
    for (kind, table) in task.items() {
        let Some(table) = table else { continue };
        let Some(label) = type_label(&kind, pet) else { continue };
        let named = matches!(kind.trim(), "MD" | "people" | "vehicle" | "dog_cat" | "Normal");
        if named || table.any() {
            out.push((label, table));
        }
    }
    out
}

/// "18:00–06:00".
pub fn hour_span(span: &HourSpan) -> String {
    match (span.start_hour, span.start_min, span.end_hour, span.end_min) {
        (Some(sh), Some(sm), Some(eh), Some(em)) => format!("{sh:02}:{sm:02}–{eh:02}:{em:02}"),
        _ => DASH.to_string(),
    }
}

fn clock(hour: Option<u32>, minute: Option<u32>) -> String {
    match (hour, minute) {
        (Some(h), Some(m)) => format!("{h:02}:{m:02}"),
        _ => DASH.to_string(),
    }
}

pub fn frame_rate_mode(value: Option<u32>) -> String {
    match value {
        Some(0) => "Auto".to_string(),
        Some(2) => "Gradual".to_string(),
        other => num(other),
    }
}

pub fn bitrate_mode(value: Option<u32>) -> String {
    match value {
        Some(0) => "Variable bitrate".to_string(),
        other => num(other),
    }
}

fn binning_mode(value: Option<u32>) -> String {
    match value {
        Some(1) => "Auto".to_string(),
        other => num(other),
    }
}

/// "50 (0–100)".
pub fn range_value(v: &MinMaxCur) -> String {
    match (v.cur, v.min, v.max) {
        (Some(cur), Some(min), Some(max)) => format!("{cur} ({min}–{max})"),
        (cur, _, _) => num(cur),
    }
}

/// What the device does when its disk is full. The meaning of a negative
/// value is unconfirmed, so the device's number is shown with the guess.
pub fn cycle_label(cycle: Option<i64>) -> String {
    match cycle {
        Some(1) => "Overwrite oldest files".to_string(),
        Some(0) => "Off".to_string(),
        Some(n) if n < 0 => format!("Keep {} days (device value {n})", n.unsigned_abs()),
        Some(n) => n.to_string(),
        None => DASH.to_string(),
    }
}

pub fn gib(value: f64) -> String {
    format!("{value:.2} GiB")
}

/// "On, port 80" / "Off (port 80)". The basic service port has no switch.
pub fn service_label(port: &ServicePort, switchable: bool) -> String {
    let p = num(port.port());
    if !switchable {
        return p;
    }
    match port.enable {
        Some(0) => format!("Off (port {p})"),
        Some(_) => format!("On, port {p}"),
        None => p,
    }
}

/// The zone from the device's seconds west of UTC: "UTC+02:00".
pub fn time_zone_label(seconds_west: Option<i64>) -> String {
    let Some(west) = seconds_west else { return DASH.to_string() };
    // The sign read off the device's value, the magnitude unsigned: no
    // negation, so no overflow whatever number the reply carried.
    let sign = if west > 0 { '−' } else { '+' };
    let abs = west.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60)
}

/// "2026-10-24 12:34:56" as the device read its clock.
pub fn clock_label(g: &SystemGeneral) -> String {
    match (g.year, g.month, g.day, g.hour, g.minute, g.second) {
        (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) => {
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }
        _ => DASH.to_string(),
    }
}

fn month_name(month: Option<u32>) -> String {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    match month {
        Some(m @ 1..=12) => NAMES[m as usize - 1].to_string(),
        other => num(other),
    }
}

/// "the last Sunday of March" / "the 2nd Sunday of March".
fn week_of_month(index: Option<u32>, weekday: &Option<String>, month: Option<u32>) -> String {
    let day = capitalised_opt(weekday);
    let which = match index {
        Some(5) => "last".to_string(),
        Some(1) => "1st".to_string(),
        Some(2) => "2nd".to_string(),
        Some(3) => "3rd".to_string(),
        Some(4) => "4th".to_string(),
        other => num(other),
    };
    format!("the {which} {day} of {}", month_name(month))
}

/// The daylight-saving line: off, or the rule the device follows.
pub fn dst_label(is_dst: Option<u32>, dst: &Read<Dst>) -> String {
    let on = match dst {
        Ok(d) => d.enable.map_or(is_dst != Some(0), |e| e != 0),
        Err(_) => is_dst.is_some_and(|v| v != 0),
    };
    if !on {
        return "Off".to_string();
    }
    match dst {
        Ok(d) => format!(
            "On, +{} h, from {} {} to {} {}",
            num(d.offset),
            week_of_month(d.start_week_index, &d.start_weekday, d.start_month),
            clock(d.start_hour, d.start_minute),
            week_of_month(d.end_week_index, &d.end_weekday, d.end_month),
            clock(d.end_hour, d.end_minute),
        ),
        Err(failure) => format!("On ({})", failure_text(*failure).to_lowercase()),
    }
}

pub fn date_format_label(format: &Option<String>) -> String {
    match format.as_deref().map(str::trim) {
        Some("YMD") => "YYYY/MM/DD".to_string(),
        Some("DMY") => "DD/MM/YYYY".to_string(),
        Some("MDY") => "MM/DD/YYYY".to_string(),
        _ => text(format),
    }
}

pub fn time_format_label(value: Option<u32>) -> String {
    match value {
        Some(0) => "24-hour".to_string(),
        Some(_) => "12-hour".to_string(),
        None => DASH.to_string(),
    }
}

fn certificate_label(cert: &Enable) -> String {
    match cert.enable {
        Some(0) => "Default (self-signed)".to_string(),
        Some(_) => "Custom certificate installed".to_string(),
        None => DASH.to_string(),
    }
}

/// "2.4 GHz" / "5 GHz" from the device's band word, or the word itself.
pub fn band_label(freq: &Option<String>) -> String {
    let word = text(freq);
    let lower = word.to_lowercase();
    if lower.starts_with("2.4") {
        "2.4 GHz".to_string()
    } else if lower.starts_with('5') {
        "5 GHz".to_string()
    } else {
        word
    }
}

fn user_level(level: Option<u32>) -> String {
    match level {
        Some(1) => "Administrator".to_string(),
        Some(0) => "User".to_string(),
        other => num(other),
    }
}

/// The channel's name as the sidebar shows it.
pub fn channel_label(channel: &ChannelInfo) -> String {
    if channel.name.is_empty() {
        format!("Channel {}", u16::from(channel.channel_id) + 1)
    } else {
        channel.name.clone()
    }
}

/// A device-wide alert switch over the connected channels: the main switch
/// and how many channels it covers, with one line per channel.
pub fn channel_switch(sw: &ChannelSwitch, channels: &[ChannelInfo]) -> Value {
    let online: Vec<&ChannelInfo> = channels.iter().filter(|c| c.online).collect();
    let on = online.iter().filter(|c| sw.channel_on(c.channel_id)).count();
    let text = match sw.main_switch {
        Some(0) => "Off".to_string(),
        Some(_) if online.is_empty() => "On".to_string(),
        Some(_) => format!("On · {on} of {} channels", online.len()),
        None => DASH.to_string(),
    };
    let lines = online
        .iter()
        .map(|c| format!("{}: {}", channel_label(c), if sw.channel_on(c.channel_id) { "on" } else { "off" }))
        .collect();
    Value::Lines { text, lines }
}

/// "Authorisation ends in 36 h (on 2026-10-25 at 18:00)"; a negative
/// number of hours is the device's word for a share that does not end.
pub fn ends_label(valid_hours: Option<i64>, local_now: Option<i64>) -> String {
    let Some(hours) = valid_hours else { return DASH.to_string() };
    if hours < 0 {
        return "No time limit".to_string();
    }
    let end = local_now.and_then(|now| hours.checked_mul(3600).and_then(|seconds| now.checked_add(seconds)));
    match end {
        Some(end) => {
            let end = CivilTime::from_epoch_seconds(end);
            format!(
                "Authorisation ends in {hours} h (on {:04}-{:02}-{:02} at {:02}:{:02})",
                end.year, end.month, end.day, end.hour, end.minute
            )
        }
        None => format!("Authorisation ends in {hours} h"),
    }
}

/// The first four characters of a UID and an ellipsis.
pub fn mask_uid(uid: &str) -> String {
    let shown: String = uid.chars().take(4).collect();
    format!("{shown}…")
}

/// "W×H – W×H", or "not set" when the device gives no size at all.
pub fn object_size(cfg: &AiDetectCfg) -> String {
    let sizes = [cfg.min_target_width, cfg.min_target_height, cfg.max_target_width, cfg.max_target_height];
    if sizes.iter().all(|s| s.unwrap_or(0) == 0) {
        return "not set".to_string();
    }
    format!("{}×{} – {}×{}", num(sizes[0]), num(sizes[1]), num(sizes[2]), num(sizes[3]))
}

fn day_night_state(stat: &Option<String>) -> String {
    match stat.as_deref().map(str::trim) {
        Some("day") => "Day".to_string(),
        Some("night") => "Night".to_string(),
        _ => capitalised_opt(stat),
    }
}

fn led_state(word: &Option<String>) -> String {
    match word.as_deref().map(str::trim) {
        Some("open") => "On".to_string(),
        Some("close") => "Off".to_string(),
        Some("auto") => "Auto".to_string(),
        _ => capitalised_opt(word),
    }
}

/// Whether a row gated by an ability is shown: yes until the device has
/// said what the channel can do.
fn allows(abilities: Option<ChannelAbilities>, ability: impl Fn(&ChannelAbilities) -> bool) -> bool {
    abilities.as_ref().map_or(true, ability)
}

fn pet_label(abilities: Option<ChannelAbilities>) -> bool {
    abilities.is_some_and(|a| a.pet_label)
}

// ---------------------------------------------------------------------------
// Camera pages
// ---------------------------------------------------------------------------

fn version_rows(v: &VersionInfo) -> Vec<Row> {
    let mut rows = vec![
        Row::text("Name", text(&v.name)),
        Row::text("Model", text(&v.model)),
        Row::text("Build", text(&v.build_day)),
        Row::text("Hardware", text(&v.hardware_version)),
        Row::text("Config version", text(&v.cfg_version)),
        Row::text("Firmware", text(&v.firmware_version)),
        Row::text("Details", text(&v.detail)),
    ];
    if v.ai_version.is_some() {
        rows.push(Row::text("AI version", text(&v.ai_version)));
    }
    rows
}

fn camera_info(page: &CameraInfoPage) -> Tab {
    Tab { status: status(&[outcome(&page.version)]), groups: group_for("Camera", &page.version, version_rows).into_iter().collect() }
}

fn camera_image(page: &CameraImagePage, abilities: Option<ChannelAbilities>) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.video_input), outcome(&page.advanced), outcome(&page.threshold)]), groups: Vec::new() };
    tab.groups.extend(group_for("Picture", &page.video_input, |v| {
        let mut rows = vec![
            Row::text("Brightness", num(v.bright)),
            Row::text("Contrast", num(v.contrast)),
            Row::text("Saturation", num(v.saturation)),
            Row::text("Sharpness", num(v.sharpen)),
            Row::text("Hue", num(v.hue)),
        ];
        if v.corridor_ability == Some(1) {
            rows.push(Row::text("Corridor mode", capitalised_opt(&v.corridor_mode)));
        }
        rows
    }));
    tab.groups.extend(group_for("Advanced", &page.advanced, |a| advanced_rows(a, abilities)));
    tab.groups.extend(group_for("Day and night", &page.advanced, |a| match &a.day_night {
        Some(dn) => vec![
            Row::text("Mode", capitalised_opt(&dn.mode)),
            Row::text("IR cut", capitalised_opt(&dn.ircut_mode)),
            Row::text("Threshold", capitalised_opt(&dn.threshold)),
        ],
        None => vec![Row::text("Mode", DASH)],
    }));
    tab.groups.extend(group_for("Day/night switching", &page.threshold, threshold_rows));
    tab
}

fn advanced_rows(a: &InputAdvanceCfg, abilities: Option<ChannelAbilities>) -> Vec<Row> {
    let mut rows = vec![
        Row::text("Flip", on_off(a.flip)),
        Row::text("Mirror", on_off(a.mirror)),
        Row::text("Anti-flicker", a.power_line_frequency.as_ref().map_or_else(|| DASH.to_string(), anti_flicker)),
        Row::text("Colour day mode", a.bd_day_color.as_ref().map_or_else(|| DASH.to_string(), |m| capitalised_opt(&m.mode))),
        Row::text("Black & white", a.bd_night.as_ref().map_or_else(|| DASH.to_string(), |m| capitalised_opt(&m.mode))),
    ];
    if let Some(m) = &a.bd_night_color {
        rows.push(Row::text("Colour night mode", capitalised_opt(&m.mode)));
    }
    if a.binning_mode.is_some() && allows(abilities, |ab| ab.binning) {
        rows.push(Row::text("Binning mode", binning_mode(a.binning_mode)));
    }
    rows.push(Row::text("Frame rate mode", frame_rate_mode(a.constant_frame_rate)));
    if a.enc_type.is_some() && a.enc_type_ability.unwrap_or(0) > 0 {
        rows.push(Row::text("Bitrate mode", bitrate_mode(a.enc_type)));
    }
    rows.push(Row::text(
        "3D noise reduction",
        match &a.nr3d {
            Some(nr) if nr.enable == Some(0) => "Off".to_string(),
            Some(nr) => format!("On, {}", nr.value.as_deref().map_or_else(|| DASH.to_string(), |v| v.trim().to_lowercase())),
            None => DASH.to_string(),
        },
    ));
    rows.push(Row::text("Backlight compensation", a.blc.as_ref().map_or_else(|| DASH.to_string(), |b| on_off(b.enable))));
    rows
}

fn threshold_rows(t: &DayNightThreshold) -> Vec<Row> {
    vec![
        Row::text("Switching threshold", t.threshold_value.as_ref().map_or_else(|| DASH.to_string(), range_value)),
        Row::text("Threshold mode", capitalised_opt(&t.threshold)),
        Row::text("Current state", day_night_state(&t.stat)),
    ]
}

fn camera_display(page: &CameraDisplayPage) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.datetime), outcome(&page.name)]), groups: Vec::new() };
    tab.groups.extend(group_for("Date and time", &page.datetime, |d: &OsdDatetime| {
        vec![
            Row::text("Shown", on_off(d.enable)),
            Row::text("Position", position_label(d.top_left_x, d.top_left_y)),
            Row::text("Language", text(&d.language)),
        ]
    }));
    tab.groups.extend(group_for("Camera name", &page.name, |n: &OsdName| {
        vec![
            Row::text("Name", text(&n.name)),
            Row::text("Shown", on_off(n.enable)),
            Row::text("Position", position_label(n.top_left_x, n.top_left_y)),
            Row::text("Watermark", on_off(n.watermark)),
            Row::text("Background", on_off(n.background)),
        ]
    }));
    tab
}

fn camera_detection(page: &CameraDetectionPage, abilities: Option<ChannelAbilities>) -> Tab {
    let pet = pet_label(abilities);
    let mut tab = Tab {
        status: status(&[
            outcome(&page.ai),
            outcome(&page.motion),
            outcome(&page.people),
            outcome(&page.vehicle),
            outcome(&page.animal),
            outcome(&page.track_limit),
            outcome(&page.track_schedule),
        ]),
        groups: Vec::new(),
    };
    tab.groups.extend(group_for("Motion detection", &page.motion, motion_rows));
    tab.groups.extend(group_for("Smart detection", &page.ai, |ai: &AiCfg| {
        vec![Row::text("Detects", ai.detect_type.as_deref().map_or_else(|| DASH.to_string(), |t| type_list(t, pet)))]
    }));
    for (title, read) in [("Person", &page.people), ("Vehicle", &page.vehicle), (if pet { "Pet" } else { "Animal" }, &page.animal)] {
        tab.groups.extend(group_for(title, read, |cfg: &AiDetectCfg| {
            vec![
                Row::text("Sensitivity", num(cfg.sensitivity)),
                Row::text("Alarm delay", seconds(cfg.stay_time)),
                Row::text("Object size", object_size(cfg)),
            ]
        }));
    }
    // The tracking rows: for cameras that can follow an object, or that say
    // so themselves.
    let tracks = page.ai.as_ref().is_ok_and(|ai| ai.smart_track_mode_ability.unwrap_or(0) > 0)
        || abilities.is_some_and(|a| a.tracking);
    if tracks {
        tab.groups.extend(group_for("Auto tracking", &page.ai, |ai: &AiCfg| {
            let mut rows = vec![
                Row::text("Auto tracking", on_off(ai.smart_track)),
                Row::text("Tracking type", ai.smart_track_type.as_deref().map_or_else(|| DASH.to_string(), |t| type_list(t, pet))),
            ];
            if ai.smart_track_object_stop_delay.is_some() {
                rows.push(Row::text("Stop tracking when the object stops", seconds(ai.smart_track_object_stop_delay)));
            }
            if ai.smart_track_object_disappear_delay.is_some() {
                rows.push(Row::text("Stop tracking when the object disappears", seconds(ai.smart_track_object_disappear_delay)));
            }
            rows
        }));
    }
    tab.groups.extend(group_for("Horizontal tracking range", &page.track_limit, |l| {
        vec![Row::text("Left", num(l.left_limit)), Row::text("Right", num(l.right_limit))]
    }));
    tab.groups.extend(group_for("Tracking schedule", &page.track_schedule, |s| match s.table() {
        Some(table) => vec![Row { title: String::new(), value: Value::Schedules(vec![("Tracking".to_string(), table)]) }],
        None => vec![Row::text("Schedule", DASH)],
    }));
    tab
}

fn motion_rows(md: &MotionDetect) -> Vec<Row> {
    let default = md.sens_info_new.as_ref().and_then(|s| s.sensitivity_default);
    let mut rows = vec![
        Row::text("Motion detection", on_off(md.enable)),
        Row::text("Sensitivity", default.map_or_else(|| DASH.to_string(), |d| format!("{d} (device scale)"))),
    ];
    // The four legacy time slots, only when one of them differs.
    let periods = md.periods();
    if periods.iter().any(|p| p.sensitivity.is_some() && p.sensitivity != default) {
        for p in periods {
            rows.push(Row::text(
                &format!("{}–{}", clock(p.begin_hour, p.begin_minute), clock(p.end_hour, p.end_minute)),
                num(p.sensitivity),
            ));
        }
    }
    if md.width.is_some() || md.height.is_some() {
        rows.push(Row::text("Detection grid", format!("{} × {}", num(md.width), num(md.height))));
    }
    rows
}

fn camera_light_sound(page: &CameraLightSoundPage, abilities: Option<ChannelAbilities>) -> Tab {
    let pet = pet_label(abilities);
    let mut tab = Tab {
        status: status(&[
            outcome(&page.led),
            outcome(&page.spotlight),
            outcome(&page.audio),
            outcome(&page.denoise),
            outcome(&page.record_audio),
        ]),
        groups: Vec::new(),
    };
    tab.groups.extend(group_for("Lights", &page.led, |led: &LedState| {
        let mut rows = Vec::new();
        if allows(abilities, |a| a.status_led) {
            rows.push(Row::text("Status LED", led_state(&led.light_state)));
        }
        rows.push(Row::text("Infrared lights", led_state(&led.state)));
        if led.ir_led_brightness.unwrap_or(0) > 0 {
            rows.push(Row::text("IR brightness", num(led.ir_led_brightness)));
        }
        rows
    }));
    tab.groups.extend(group_for("Spotlight", &page.spotlight, |f: &FloodlightTask| {
        vec![
            Row::text(
                "Mode",
                match f.alarm_mode {
                    Some(1) => "Night smart mode".to_string(),
                    other => num(other),
                },
            ),
            Row::text(
                "Brightness",
                range_value(&MinMaxCur { min: f.brightness_min, max: f.brightness_max, cur: f.brightness_cur }),
            ),
            // The unit of the duration is not known for sure.
            Row::text("Duration", num(f.duration)),
            Row::text("Schedule", f.schedule.as_ref().map_or_else(|| DASH.to_string(), hour_span)),
            Row::text("Alarm schedule", f.light_alarm_schedule.as_ref().map_or_else(|| DASH.to_string(), hour_span)),
            Row::text("Triggers", f.detect_type.as_deref().map_or_else(|| DASH.to_string(), |t| type_list(t, pet))),
        ]
    }));
    let mut sound = Vec::new();
    sound.extend(row_for("Volume", &page.audio, |a| num(a.volume)));
    // No ability in the support table says which cameras have audio noise
    // reduction, so every camera is asked: one without it refuses the read
    // and the row is left out; one with it shows the value it reports.
    if let Ok(d) = &page.denoise {
        sound.push(Row::text(
            "Noise reduction",
            match d.enable {
                Some(0) => "Off".to_string(),
                Some(_) => format!("On, level {}", num(d.level)),
                None => DASH.to_string(),
            },
        ));
    }
    sound.extend(row_for("Record audio", &page.record_audio, |c| {
        on_off(c.main_stream.as_ref().and_then(|s| s.audio))
    }));
    if !sound.is_empty() {
        tab.groups.push(Group::new("Sound", sound));
    }
    tab
}

/// One scheduled task's group: its switch, extra rows, then the grids.
fn task_group(title: &str, task: &Read<ScheduledTask>, extra: Vec<Row>, pet: bool) -> Option<Group> {
    group_for(title, task, |t| {
        let mut rows = vec![Row::text("Enabled", on_off(t.enable))];
        rows.extend(extra);
        let tables = schedules(t, pet);
        if !tables.is_empty() {
            rows.push(Row { title: String::new(), value: Value::Schedules(tables) });
        }
        rows
    })
}

fn camera_schedules(page: &CameraSchedulesPage, abilities: Option<ChannelAbilities>) -> Tab {
    let pet = pet_label(abilities);
    let mut tab = Tab {
        status: status(&[
            outcome(&page.record),
            outcome(&page.record_cfg),
            outcome(&page.email),
            outcome(&page.push),
            outcome(&page.push_cfg),
            outcome(&page.ftp),
            outcome(&page.siren),
            outcome(&page.hub_siren),
        ]),
        groups: Vec::new(),
    };
    let mut record_rows = Vec::new();
    record_rows.extend(row_for("Post-motion recording", &page.record_cfg, |c: &RecordCfg| seconds(c.record_delay_time)));
    // The units of the pre-recording time and the pack duration are not
    // known for sure; the numbers are shown as the device sends them.
    record_rows.extend(row_for("Pre-recording", &page.record_cfg, |c: &RecordCfg| num(c.pre_record_time)));
    record_rows.extend(row_for("Pack duration", &page.record_cfg, |c: &RecordCfg| num(c.package_time)));
    tab.groups.extend(task_group("Recording", &page.record, record_rows, pet));
    tab.groups.extend(task_group("Email", &page.email, Vec::new(), pet));
    let push_rows = row_for("Interval", &page.push_cfg, |c| seconds(c.interval)).into_iter().collect();
    tab.groups.extend(task_group("Push notifications", &page.push, push_rows, pet));
    tab.groups.extend(task_group("FTP upload", &page.ftp, Vec::new(), pet));
    tab.groups.extend(task_group("Siren", &page.siren, Vec::new(), pet));
    tab.groups.extend(task_group("Home Hub siren", &page.hub_siren, Vec::new(), pet));
    tab
}

/// The address rows a camera and a device share (one reply, four roots).
fn network_group(n: &NetworkRows) -> Option<Group> {
    let mut rows = Vec::new();
    rows.extend(row_for("Address mode", &n.dhcp, |d| match d.enable {
        Some(0) => "Static".to_string(),
        Some(_) => "DHCP".to_string(),
        None => DASH.to_string(),
    }));
    rows.extend(row_for("IP address", &n.ip, |ip| text(&ip.ip)));
    rows.extend(row_for("Subnet mask", &n.ip, |ip| text(&ip.mask)));
    rows.extend(row_for("Gateway", &n.ip, |ip| text(&ip.gateway)));
    rows.extend(row_for("MAC address", &n.ip, |ip| {
        ip.mac.as_ref().filter(|m| !m.is_empty()).map_or_else(|| DASH.to_string(), |m| m.as_str().trim().to_string())
    }));
    rows.extend(row_for("DNS", &n.auto_dns, |d| match d.enable {
        Some(0) => "Manual".to_string(),
        Some(_) => "Automatic".to_string(),
        None => DASH.to_string(),
    }));
    rows.extend(row_for("DNS server 1", &n.dns, |d| text(&d.dns1)));
    rows.extend(row_for("DNS server 2", &n.dns, |d| text(&d.dns2)));
    (!rows.is_empty()).then(|| Group::new("Network", rows))
}

// ---------------------------------------------------------------------------
// Device pages
// ---------------------------------------------------------------------------

fn device_info(page: &DeviceInfoPage, uid_hint: Option<&str>) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.version)]), groups: Vec::new() };
    tab.groups.extend(group_for("Device", &page.version, |v| {
        let mut rows = version_rows(v);
        if let Some(uid) = uid_hint {
            rows.push(Row::text("UID", uid));
        }
        rows
    }));
    tab
}

fn disk_rows(disk: &HddInfo) -> Vec<Row> {
    let (total, free) = disk.gib();
    let used = (total - free).max(0.0);
    let fraction = if total > 0.0 { (used / total).clamp(0.0, 1.0) } else { 0.0 };
    vec![
        Row::text("Capacity", gib(total)),
        Row { title: "Used".to_string(), value: Value::Level { text: gib(used), fraction } },
        Row::text("Free", gib(free)),
        Row::text("Formatted", yes_no(disk.format)),
        Row::text("Mounted", yes_no(disk.mount)),
    ]
}

fn device_storage(page: &DeviceStoragePage) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.disks), outcome(&page.record_cfg), outcome(&page.encryption)]), groups: Vec::new() };
    match &page.disks {
        Ok(list) if list.disks.is_empty() => tab.groups.push(Group::new("Storage", vec![Row::text("", "No disk")])),
        Ok(list) => {
            for disk in &list.disks {
                let title = match disk.number {
                    Some(0) | None => "HDD".to_string(),
                    Some(n) => format!("HDD{n}"),
                };
                tab.groups.push(Group::new(&title, disk_rows(disk)));
            }
        }
        Err(ReadFailure::NotSent) => {}
        Err(failure) => tab.groups.push(Group::failed("Storage", *failure)),
    }
    let mut rows = Vec::new();
    rows.extend(row_for("When full", &page.record_cfg, |c| cycle_label(c.cycle)));
    rows.extend(row_for("Recording encryption", &page.encryption, |e| on_off(e.enable)));
    if !rows.is_empty() {
        tab.groups.push(Group::new("Recording", rows));
    }
    tab
}

fn ntp_rows(ntp: &Ntp) -> Vec<Row> {
    vec![
        Row::text("Enabled", on_off(ntp.enable)),
        Row::text("Server", text(&ntp.server)),
        Row::text("Port", num(ntp.port)),
        // The unit of the interval is not known for sure.
        Row::text("Sync interval", num(ntp.synchronize_interval)),
    ]
}

fn device_network(page: &DeviceNetworkPage) -> Tab {
    let n = &page.network;
    let mut tab = Tab {
        status: status(&[
            outcome(&n.dhcp),
            outcome(&n.auto_dns),
            outcome(&n.ip),
            outcome(&n.dns),
            outcome(&page.server),
            outcome(&page.http),
            outcome(&page.https),
            outcome(&page.rtsp),
            outcome(&page.rtmp),
            outcome(&page.onvif),
            outcome(&page.upnp),
            outcome(&page.p2p),
            outcome(&page.ddns),
            outcome(&page.ntp),
            outcome(&page.certificate),
            outcome(&page.hub_ap),
        ]),
        groups: Vec::new(),
    };
    tab.groups.extend(network_group(n));
    let mut services = Vec::new();
    services.extend(row_for("Basic service port", &page.server, |p| service_label(p, false)));
    services.extend(row_for("HTTP", &page.http, |p| service_label(p, true)));
    services.extend(row_for("HTTPS", &page.https, |p| service_label(p, true)));
    services.extend(row_for("RTSP", &page.rtsp, |p| service_label(p, true)));
    services.extend(row_for("RTMP", &page.rtmp, |p| service_label(p, true)));
    services.extend(row_for("ONVIF", &page.onvif, |p| service_label(p, true)));
    services.extend(row_for("UPnP", &page.upnp, |e| on_off(e.enable)));
    services.extend(row_for("Access by UID (P2P)", &page.p2p, |e| on_off(e.enable)));
    services.extend(row_for("HTTPS certificate", &page.certificate, certificate_label));
    if !services.is_empty() {
        tab.groups.push(Group::new("Services", services));
    }
    tab.groups.extend(group_for("DDNS", &page.ddns, |d| {
        vec![Row::text("Enabled", on_off(d.enable)), Row::text("Provider", text(&d.ddns_type))]
    }));
    tab.groups.extend(group_for("NTP", &page.ntp, ntp_rows));
    match &page.hub_ap {
        Ok(ap) => tab.groups.extend(wifi_groups(ap)),
        Err(ReadFailure::NotSent) => {}
        Err(failure) => tab.groups.push(Group::failed("Wi-Fi for cameras", *failure)),
    }
    tab
}

/// One group per band of the Hub's own Wi-Fi for its cameras.
fn wifi_groups(ap: &KitApCfg) -> Vec<Group> {
    if ap.bands.is_empty() {
        return vec![Group::new("Wi-Fi for cameras", vec![Row::text("", DASH)])];
    }
    ap.bands
        .iter()
        .map(|band| {
            let mut rows = vec![
                Row::text("Network name", band.ssid.as_ref().filter(|s| !s.is_empty()).map_or_else(|| DASH.to_string(), |s| s.as_str().trim().to_string())),
                Row::text(
                    "Channel",
                    match band.channel {
                        Some(0) => "Auto".to_string(),
                        other => num(other),
                    },
                ),
                Row::text("Hidden", on_off(band.hide_ssid)),
            ];
            if band.country_code.is_some() {
                rows.push(Row::text("Country", text(&band.country_code)));
            }
            // `enableWifi` is parsed but not shown: what it means is not
            // known (both bands are listed with it at 0).
            Group::new(&format!("Wi-Fi for cameras — {}", band_label(&band.freq)), rows)
        })
        .collect()
}

fn device_time(page: &DeviceTimePage) -> Tab {
    let mut tab = Tab {
        status: status(&[outcome(&page.general), outcome(&page.norm), outcome(&page.dst), outcome(&page.ntp)]),
        groups: Vec::new(),
    };
    let mut rows = Vec::new();
    rows.extend(row_for("Device clock (at the time of reading)", &page.general, clock_label));
    rows.extend(row_for("Time zone", &page.general, |g| time_zone_label(g.time_zone)));
    if let Ok(g) = &page.general {
        rows.push(Row::text("Daylight saving", dst_label(g.is_dst, &page.dst)));
    } else if let Err(failure) = &page.dst {
        if *failure != ReadFailure::NotSent {
            rows.push(Row::text("Daylight saving", failure_text(*failure)));
        }
    }
    rows.extend(row_for("Date format", &page.general, |g| date_format_label(&g.osd_format)));
    rows.extend(row_for("Time format", &page.general, |g| time_format_label(g.time_format)));
    rows.extend(row_for("Video standard", &page.norm, |n| text(&n.norm)));
    if !rows.is_empty() {
        tab.groups.push(Group::new("Clock", rows));
    }
    tab.groups.extend(group_for("NTP", &page.ntp, ntp_rows));
    tab
}

fn device_light_sound(page: &DeviceLightSoundPage) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.led), outcome(&page.audio)]), groups: Vec::new() };
    tab.groups.extend(group_for("Lights", &page.led, |led: &LedState| vec![Row::text("Status LED", led_state(&led.light_state))]));
    tab.groups.extend(group_for("Sound", &page.audio, |a: &DeviceAudioCfg| {
        vec![
            Row::text("Notification volume", num(a.cues_volume)),
            Row::text("Alarm volume", num(a.alarm_volume)),
            // The unit of the pause is not known for sure.
            Row::text("Alarm pause", num(a.alarm_pause_time)),
        ]
    }));
    tab
}

fn device_alerts(page: &DeviceAlertsPage, channels: &[ChannelInfo]) -> Tab {
    let mut tab = Tab {
        status: status(&[outcome(&page.record), outcome(&page.email), outcome(&page.push), outcome(&page.buzzer)]),
        groups: Vec::new(),
    };
    let mut rows = Vec::new();
    for (title, read) in [("Recording", &page.record), ("Email", &page.email), ("Push notifications", &page.push), ("Buzzer", &page.buzzer)] {
        match read {
            Ok(sw) => rows.push(Row { title: title.to_string(), value: channel_switch(sw, channels) }),
            Err(ReadFailure::NotSent) => {}
            Err(failure) => rows.push(Row::text(title, failure_text(*failure))),
        }
    }
    if !rows.is_empty() {
        tab.groups.push(Group::new("Alerts", rows));
    }
    tab
}

fn device_users(page: &DeviceUsersPage, local_now: Option<i64>) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.users), outcome(&page.shared), outcome(&page.general)]), groups: Vec::new() };
    tab.groups.extend(group_for("Accounts", &page.users, |list: &UserList| {
        if list.users.is_empty() {
            return vec![Row::text("", DASH)];
        }
        list.users
            .iter()
            .map(|u| {
                let name = u.user_name.as_ref().filter(|n| !n.is_empty()).map_or(DASH, |n| n.as_str().trim());
                let mut value = user_level(u.user_level);
                if u.login_state == Some(1) {
                    value.push_str(" · signed in");
                }
                Row::text(name, value)
            })
            .collect()
    }));
    tab.groups.extend(group_for("Shared access", &page.shared, |list: &AccessUserList| {
        let mut rows = vec![Row::text(
            "Shared users",
            match list.max_access_user_num {
                Some(max) => format!("{} of {max}", list.users.len()),
                None => list.users.len().to_string(),
            },
        )];
        for (i, u) in list.users.iter().enumerate() {
            let channels = u.channel_ability.map_or_else(|| DASH.to_string(), |mask| mask.count_ones().to_string());
            rows.push(Row::text(
                &format!("Shared user {}", i + 1),
                format!("{}, {channels} channels, {}", user_level(u.user_level), ends_label(u.valid_hours, local_now)),
            ));
        }
        rows
    }));
    tab.groups.extend(group_for("Login lockout", &page.general, |g: &SystemGeneral| {
        let mut rows = vec![Row::text("Lock after failed logins", on_off(g.login_lock))];
        if g.login_lock.is_some_and(|v| v != 0) {
            rows.push(Row::text("Rule", format!("after {} failed attempts, for {}", num(g.allowed_times), seconds(g.lock_time))));
        }
        rows
    }));
    tab
}

fn device_maintenance(page: &DeviceMaintenancePage) -> Tab {
    let mut tab = Tab { status: status(&[outcome(&page.auto_reboot)]), groups: Vec::new() };
    tab.groups.extend(group_for("Auto reboot", &page.auto_reboot, |r: &AutoReboot| {
        vec![
            Row::text("Enabled", on_off(r.enable)),
            Row::text(
                "When",
                format!(
                    "Every {} at {}:{:02}",
                    capitalised_opt(&r.week_day),
                    clock(r.hour, r.minute),
                    r.second.unwrap_or(0)
                ),
            ),
        ]
    }));
    tab
}

#[cfg(test)]
mod tests {
    use super::*;
    use reoling::{AccessUser, ApCfg, DnsConfig, IpConfig, MacAddress, ScheduleItem, Ssid, TypeScheduleList, UserEntry};

    fn table(pattern: &str) -> ScheduleTable {
        ScheduleTable::parse(pattern).expect("168 cells")
    }

    fn ones() -> ScheduleTable {
        table(&"1".repeat(168))
    }

    fn zeros() -> ScheduleTable {
        table(&"0".repeat(168))
    }

    #[test]
    fn words_for_switches_and_numbers() {
        assert_eq!(on_off(Some(0)), "Off");
        assert_eq!(on_off(Some(1)), "On");
        assert_eq!(on_off(Some(7)), "On");
        assert_eq!(on_off(None), "—");
        assert_eq!(yes_no(Some(1)), "Yes");
        assert_eq!(yes_no(Some(0)), "No");
        assert_eq!(seconds(Some(10)), "10 s");
        assert_eq!(seconds(None), "—");
        assert_eq!(num(Some(128)), "128");
        assert_eq!(text(&Some("  ".into())), "—");
        assert_eq!(text(&Some(" v3.1 ".into())), "v3.1");
        assert_eq!(capitalised("auto"), "Auto");
        assert_eq!(capitalised("manual"), "Manual");
        assert_eq!(capitalised(""), "—");
    }

    #[test]
    fn anti_flicker_words() {
        let off = PowerLineFrequency { mode: Some("50hz".into()), enable: Some(0) };
        assert_eq!(anti_flicker(&off), "Off");
        let fifty = PowerLineFrequency { mode: Some("50hz".into()), enable: Some(1) };
        assert_eq!(anti_flicker(&fifty), "50 Hz");
        let sixty = PowerLineFrequency { mode: Some("60HZ".into()), enable: Some(1) };
        assert_eq!(anti_flicker(&sixty), "60 Hz");
        let other = PowerLineFrequency { mode: Some("outdoor".into()), enable: Some(1) };
        assert_eq!(anti_flicker(&other), "Outdoor");
        let none = PowerLineFrequency { mode: None, enable: Some(1) };
        assert_eq!(anti_flicker(&none), "—");
    }

    #[test]
    fn overlay_positions() {
        assert_eq!(position_label(Some(0), Some(0)), "Top left");
        assert_eq!(position_label(Some(1), Some(1)), "Top left");
        assert_eq!(position_label(Some(65537), Some(0)), "Top centre");
        assert_eq!(position_label(Some(65536), Some(0)), "Top right");
        assert_eq!(position_label(Some(0), Some(65536)), "Bottom left");
        assert_eq!(position_label(Some(65537), Some(65536)), "Bottom centre");
        assert_eq!(position_label(Some(65536), Some(65536)), "Bottom right");
        assert_eq!(position_label(Some(65537), Some(65537)), "Centre");
        assert_eq!(position_label(Some(120), Some(40)), "120 / 40");
        assert_eq!(position_label(Some(0), Some(65537)), "0 / 65537");
        assert_eq!(position_label(None, Some(0)), "—");
    }

    #[test]
    fn type_vocabulary() {
        assert_eq!(type_label("MD", false).as_deref(), Some("Any motion"));
        assert_eq!(type_label("people", false).as_deref(), Some("Person"));
        assert_eq!(type_label("vehicle", false).as_deref(), Some("Vehicle"));
        assert_eq!(type_label("dog_cat", false).as_deref(), Some("Animal"));
        assert_eq!(type_label("dog_cat", true).as_deref(), Some("Pet"));
        assert_eq!(type_label("Normal", false).as_deref(), Some("Continuous"));
        assert_eq!(type_label("none", false), None);
        assert_eq!(type_label("crossline", false).as_deref(), Some("crossline"));
        assert_eq!(type_list("people, vehicle,dog_cat", false), "Person, Vehicle, Animal");
        assert_eq!(type_list("people,none", true), "Person");
        assert_eq!(type_list("none", false), "—");
        assert_eq!(type_list("", false), "—");
    }

    #[test]
    fn schedules_keep_named_types_and_set_strangers() {
        let item = |kind: &str, table: &str| ScheduleItem { kind: Some(kind.into()), value_table: Some(table.into()) };
        let mut set_monday = "0".repeat(168);
        set_monday.replace_range(24..25, "1");
        let task = ScheduledTask {
            channel_id: Some(0),
            enable: Some(1),
            type_schedule_list: Some(TypeScheduleList {
                items: vec![
                    item("MD", &"0".repeat(168)),
                    item("people", &"1".repeat(168)),
                    item("none", &"1".repeat(168)),
                    item("crossline", &"0".repeat(168)),
                    item("intrude", &set_monday),
                    item("legacy", &"1".repeat(10)),
                    item("dog_cat", &"1".repeat(168)),
                ],
            }),
        };
        let out = schedules(&task, true);
        let labels: Vec<&str> = out.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Any motion", "Person", "intrude", "Pet"]);
        assert!(!out[0].1.any());
        assert!(out[1].1.all());
        assert!(out[2].1.on(1, 0));
    }

    #[test]
    fn spans_and_clocks() {
        let span = HourSpan { start_hour: Some(18), start_min: Some(0), end_hour: Some(6), end_min: Some(30) };
        assert_eq!(hour_span(&span), "18:00–06:30");
        assert_eq!(hour_span(&HourSpan::default()), "—");
        assert_eq!(clock(Some(3), Some(5)), "03:05");
        let g = SystemGeneral {
            year: Some(2026),
            month: Some(10),
            day: Some(24),
            hour: Some(9),
            minute: Some(7),
            second: Some(3),
            ..Default::default()
        };
        assert_eq!(clock_label(&g), "2026-10-24 09:07:03");
        assert_eq!(clock_label(&SystemGeneral::default()), "—");
    }

    #[test]
    fn modes_and_ranges() {
        assert_eq!(frame_rate_mode(Some(0)), "Auto");
        assert_eq!(frame_rate_mode(Some(2)), "Gradual");
        assert_eq!(frame_rate_mode(Some(1)), "1");
        assert_eq!(bitrate_mode(Some(0)), "Variable bitrate");
        assert_eq!(bitrate_mode(Some(1)), "1");
        assert_eq!(binning_mode(Some(1)), "Auto");
        assert_eq!(binning_mode(Some(0)), "0");
        assert_eq!(range_value(&MinMaxCur { min: Some(0), max: Some(100), cur: Some(50) }), "50 (0–100)");
        assert_eq!(range_value(&MinMaxCur { min: None, max: None, cur: Some(50) }), "50");
        assert_eq!(cycle_label(Some(1)), "Overwrite oldest files");
        assert_eq!(cycle_label(Some(0)), "Off");
        assert_eq!(cycle_label(Some(-7)), "Keep 7 days (device value -7)");
        assert_eq!(cycle_label(Some(3)), "3");
        assert_eq!(cycle_label(None), "—");
        // The device's number, whatever it is, never overflows the label.
        assert_eq!(cycle_label(Some(i64::MIN)), "Keep 9223372036854775808 days (device value -9223372036854775808)");
        assert_eq!(gib(1803.41), "1803.41 GiB");
        assert_eq!(gib(71.876), "71.88 GiB");
    }

    #[test]
    fn time_zones() {
        assert_eq!(time_zone_label(Some(-7200)), "UTC+02:00");
        assert_eq!(time_zone_label(Some(18000)), "UTC−05:00");
        assert_eq!(time_zone_label(Some(-19800)), "UTC+05:30");
        assert_eq!(time_zone_label(Some(0)), "UTC+00:00");
        assert_eq!(time_zone_label(None), "—");
        // The device's number, whatever it is, never overflows the label.
        assert_eq!(time_zone_label(Some(i64::MIN)), "UTC+2562047788015215:30");
        assert_eq!(time_zone_label(Some(i64::MAX)), "UTC−2562047788015215:30");
    }

    #[test]
    fn service_ports() {
        let http = ServicePort { http_port: Some(80), enable: Some(1), ..Default::default() };
        assert_eq!(service_label(&http, true), "On, port 80");
        let rtsp = ServicePort { rtsp_port: Some(554), enable: Some(0), ..Default::default() };
        assert_eq!(service_label(&rtsp, true), "Off (port 554)");
        let server = ServicePort { server_port: Some(9000), enable: Some(0), ..Default::default() };
        assert_eq!(service_label(&server, false), "9000");
        assert_eq!(service_label(&ServicePort::default(), true), "—");
    }

    #[test]
    fn time_zones_and_formats() {
        assert_eq!(time_zone_label(Some(-7200)), "UTC+02:00");
        assert_eq!(time_zone_label(Some(0)), "UTC+00:00");
        assert_eq!(time_zone_label(Some(18000)), "UTC−05:00");
        assert_eq!(time_zone_label(Some(-19800)), "UTC+05:30");
        assert_eq!(time_zone_label(None), "—");
        assert_eq!(date_format_label(&Some("YMD".into())), "YYYY/MM/DD");
        assert_eq!(date_format_label(&Some("DMY".into())), "DD/MM/YYYY");
        assert_eq!(date_format_label(&Some("MDY".into())), "MM/DD/YYYY");
        assert_eq!(date_format_label(&Some("odd".into())), "odd");
        assert_eq!(time_format_label(Some(0)), "24-hour");
        assert_eq!(time_format_label(Some(1)), "12-hour");
    }

    #[test]
    fn daylight_saving_rule() {
        let dst = Dst {
            enable: Some(1),
            offset: Some(1),
            start_month: Some(3),
            start_week_index: Some(5),
            start_weekday: Some("Sunday".into()),
            start_hour: Some(2),
            start_minute: Some(0),
            end_month: Some(10),
            end_week_index: Some(2),
            end_weekday: Some("sunday".into()),
            end_hour: Some(3),
            end_minute: Some(0),
            ..Default::default()
        };
        assert_eq!(
            dst_label(Some(1), &Ok(dst.clone())),
            "On, +1 h, from the last Sunday of March 02:00 to the 2nd Sunday of October 03:00"
        );
        let off = Dst { enable: Some(0), ..dst };
        assert_eq!(dst_label(Some(1), &Ok(off)), "Off");
        assert_eq!(dst_label(Some(0), &Err(ReadFailure::Refused(400))), "Off");
        assert_eq!(dst_label(Some(1), &Err(ReadFailure::NoReply)), "On (the device did not answer)");
    }

    #[test]
    fn bands_users_and_uids() {
        assert_eq!(band_label(&Some("2.4G".into())), "2.4 GHz");
        assert_eq!(band_label(&Some("5G".into())), "5 GHz");
        assert_eq!(band_label(&Some("other".into())), "other");
        assert_eq!(band_label(&None), "—");
        assert_eq!(user_level(Some(1)), "Administrator");
        assert_eq!(user_level(Some(0)), "User");
        assert_eq!(user_level(Some(4)), "4");
        assert_eq!(mask_uid("ABCDEF123456"), "ABCD…");
        assert_eq!(mask_uid("AB"), "AB…");
        // 2026-10-24 12:00 local, plus 36 h.
        let now = CivilTime::new(2026, 10, 24, 12, 0, 0).to_epoch_seconds();
        assert_eq!(ends_label(Some(36), Some(now)), "Authorisation ends in 36 h (on 2026-10-26 at 00:00)");
        assert_eq!(ends_label(Some(5), None), "Authorisation ends in 5 h");
        assert_eq!(ends_label(None, Some(now)), "—");
        assert_eq!(ends_label(Some(-1), Some(now)), "No time limit");
        assert_eq!(ends_label(Some(i64::MAX), Some(now)), format!("Authorisation ends in {} h", i64::MAX));
    }

    #[test]
    fn object_sizes() {
        let unset = AiDetectCfg { min_target_width: Some(0), max_target_width: Some(0), ..Default::default() };
        assert_eq!(object_size(&unset), "not set");
        let set = AiDetectCfg {
            min_target_width: Some(10),
            min_target_height: Some(20),
            max_target_width: Some(300),
            max_target_height: Some(400),
            ..Default::default()
        };
        assert_eq!(object_size(&set), "10×20 – 300×400");
    }

    fn channels() -> Vec<ChannelInfo> {
        let mut c = Vec::new();
        for (id, name, online) in [(0u8, "Front", true), (2, "", true), (5, "Garage", false)] {
            c.push(ChannelInfo { channel_id: id, name: name.into(), online, streams: Vec::new(), uid: None });
        }
        c
    }

    #[test]
    fn channel_switches_count_connected_channels() {
        let sw = ChannelSwitch { enable: Some(0b100001), main_switch: Some(1) };
        match channel_switch(&sw, &channels()) {
            Value::Lines { text, lines } => {
                assert_eq!(text, "On · 1 of 2 channels");
                assert_eq!(lines, ["Front: on", "Channel 3: off"]);
            }
            other => panic!("{other:?}"),
        }
        let off = ChannelSwitch { enable: Some(0b1), main_switch: Some(0) };
        assert!(matches!(channel_switch(&off, &channels()), Value::Lines { text, .. } if text == "Off"));
        let alone = ChannelSwitch { enable: Some(1), main_switch: Some(1) };
        assert!(matches!(channel_switch(&alone, &[]), Value::Lines { text, lines } if text == "On" && lines.is_empty()));
    }

    #[test]
    fn tab_status_follows_the_outcomes() {
        assert_eq!(status(&[None, Some(ReadFailure::Refused(400))]), "");
        assert_eq!(status(&[Some(ReadFailure::Refused(400)), Some(ReadFailure::Missing)]), NOT_AVAILABLE);
        assert_eq!(status(&[Some(ReadFailure::NoReply), Some(ReadFailure::NoReply)]), NO_ANSWER);
        assert_eq!(status(&[Some(ReadFailure::NoReply), Some(ReadFailure::Refused(400))]), NOT_AVAILABLE);
        assert_eq!(status(&[Some(ReadFailure::NotSent)]), "");
        assert_eq!(status(&[]), "");
        assert_eq!(status(&[Some(ReadFailure::NotSent), Some(ReadFailure::NoReply)]), NO_ANSWER);
    }

    fn version() -> VersionInfo {
        VersionInfo {
            channel_id: Some(2),
            name: Some("Camera1".into()),
            model: Some("E1 Outdoor PoE".into()),
            firmware_version: Some("v3.1.0.5223_2510172100".into()),
            build_day: Some("build 2510172100".into()),
            hardware_version: Some("IPC_560SD88MP".into()),
            cfg_version: Some("v3.1.0.0".into()),
            detail: Some("IPC_560SD88MPS16A1T60E1W0110000".into()),
            ai_version: None,
        }
    }

    #[test]
    fn info_tabs_list_the_version_and_the_masked_uid() {
        let tab = build(&SettingsPage::CameraInfo { channel: 2, page: CameraInfoPage { version: Ok(version()) } }, &Context::default());
        assert_eq!(tab.status, "");
        assert_eq!(tab.groups.len(), 1);
        let titles: Vec<&str> = tab.groups[0].rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["Name", "Model", "Build", "Hardware", "Config version", "Firmware", "Details"]);
        assert_eq!(tab.groups[0].rows[5].value, Value::Text("v3.1.0.5223_2510172100".into()));

        let with_ai = VersionInfo { ai_version: Some("1.2".into()), ..version() };
        let ctx = Context { uid_hint: Some("ABCD…".into()), ..Context::default() };
        let tab = build(&SettingsPage::DeviceInfo(DeviceInfoPage { version: Ok(with_ai) }), &ctx);
        let rows = &tab.groups[0].rows;
        assert_eq!(rows[7], Row::text("AI version", "1.2"));
        assert_eq!(rows[8], Row::text("UID", "ABCD…"));

        let refused = build(&SettingsPage::DeviceInfo(DeviceInfoPage { version: Err(ReadFailure::Refused(400)) }), &ctx);
        assert_eq!(refused.status, NOT_AVAILABLE);
        assert_eq!(refused.groups[0].rows, vec![Row::text("", NOT_AVAILABLE)]);
    }

    #[test]
    fn image_tab_gates_its_rows() {
        let advanced = InputAdvanceCfg {
            flip: Some(1),
            mirror: Some(0),
            power_line_frequency: Some(PowerLineFrequency { mode: Some("50hz".into()), enable: Some(1) }),
            bd_day_color: Some(reoling::ModeOnly { mode: Some("auto".into()) }),
            bd_night: Some(reoling::ModeOnly { mode: Some("manual".into()) }),
            bd_night_color: None,
            binning_mode: Some(1),
            constant_frame_rate: Some(2),
            enc_type: Some(0),
            enc_type_ability: Some(0),
            nr3d: Some(reoling::NoiseReduction { value: Some("High".into()), enable: Some(1) }),
            blc: Some(Enable { enable: Some(0) }),
            ..Default::default()
        };
        let page = CameraImagePage {
            video_input: Err(ReadFailure::NoReply),
            advanced: Ok(advanced.clone()),
            threshold: Ok(DayNightThreshold {
                channel_id: Some(0),
                threshold: Some("custom".into()),
                stat: Some("night".into()),
                threshold_value: Some(MinMaxCur { min: Some(0), max: Some(100), cur: Some(50) }),
            }),
        };
        let no_binning = ChannelAbilities { binning: false, ..Default::default() };
        let tab = build(&SettingsPage::CameraImage { channel: 0, page: page.clone() }, &Context { abilities: Some(no_binning), ..Default::default() });
        assert_eq!(tab.status, "");
        assert_eq!(tab.groups[0].title, "Picture");
        assert_eq!(tab.groups[0].rows, vec![Row::text("", NO_ANSWER)]);
        let advanced_rows: Vec<(&str, &Value)> = tab.groups[1].rows.iter().map(|r| (r.title.as_str(), &r.value)).collect();
        let titles: Vec<&str> = advanced_rows.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            titles,
            ["Flip", "Mirror", "Anti-flicker", "Colour day mode", "Black & white", "Frame rate mode", "3D noise reduction", "Backlight compensation"]
        );
        assert_eq!(advanced_rows[2].1, &Value::Text("50 Hz".into()));
        assert_eq!(advanced_rows[5].1, &Value::Text("Gradual".into()));
        assert_eq!(advanced_rows[6].1, &Value::Text("On, high".into()));
        assert_eq!(tab.groups[3].rows[0], Row::text("Switching threshold", "50 (0–100)"));
        assert_eq!(tab.groups[3].rows[2], Row::text("Current state", "Night"));

        // Binning allowed, bitrate mode offered, colour night mode present.
        let richer = InputAdvanceCfg {
            enc_type_ability: Some(1),
            bd_night_color: Some(reoling::ModeOnly { mode: Some("auto".into()) }),
            ..advanced
        };
        let page = CameraImagePage { advanced: Ok(richer), ..page };
        let tab = build(&SettingsPage::CameraImage { channel: 0, page }, &Context::default());
        let titles: Vec<&str> = tab.groups[1].rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"Binning mode"));
        assert!(titles.contains(&"Bitrate mode"));
        assert!(titles.contains(&"Colour night mode"));
        assert_eq!(tab.groups[1].rows.iter().find(|r| r.title == "Bitrate mode").unwrap().value, Value::Text("Variable bitrate".into()));
    }

    #[test]
    fn detection_tab_follows_the_tracking_gate_and_pet_label() {
        let ai = AiCfg {
            smart_track: Some(1),
            smart_track_mode_ability: Some(0),
            detect_type: Some("people,vehicle,dog_cat".into()),
            smart_track_type: Some("people".into()),
            smart_track_object_stop_delay: Some(10),
            smart_track_object_disappear_delay: None,
            ..Default::default()
        };
        let motion = MotionDetect {
            enable: Some(1),
            width: Some(60),
            height: Some(33),
            sens_info_new: Some(reoling::SensitivityNew { sensitivity_default: Some(10) }),
            sensitivity_info_list: Some(reoling::SensitivityInfoList {
                items: vec![reoling::SensitivityInfo {
                    id: Some(0),
                    sensitivity: Some(10),
                    begin_hour: Some(0),
                    begin_minute: Some(0),
                    end_hour: Some(6),
                    end_minute: Some(0),
                }],
            }),
            ..Default::default()
        };
        let cfg = |sens: u32| AiDetectCfg { sensitivity: Some(sens), stay_time: Some(3), ..Default::default() };
        let page = CameraDetectionPage {
            ai: Ok(ai.clone()),
            motion: Ok(motion.clone()),
            people: Ok(cfg(50)),
            vehicle: Ok(cfg(60)),
            animal: Ok(cfg(70)),
            track_limit: Err(ReadFailure::NotSent),
            track_schedule: Err(ReadFailure::NotSent),
        };
        let fixed = ChannelAbilities { tracking: false, pet_label: true, ..Default::default() };
        let tab = build(&SettingsPage::CameraDetection { channel: 0, page: page.clone() }, &Context { abilities: Some(fixed), ..Default::default() });
        let titles: Vec<&str> = tab.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, ["Motion detection", "Smart detection", "Person", "Vehicle", "Pet"]);
        // Equal period sensitivities are not listed; the grid is.
        let motion_titles: Vec<&str> = tab.groups[0].rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(motion_titles, ["Motion detection", "Sensitivity", "Detection grid"]);
        assert_eq!(tab.groups[0].rows[1].value, Value::Text("10 (device scale)".into()));
        assert_eq!(tab.groups[0].rows[2].value, Value::Text("60 × 33".into()));
        assert_eq!(tab.groups[1].rows[0], Row::text("Detects", "Person, Vehicle, Pet"));
        assert_eq!(tab.groups[4].rows[2], Row::text("Object size", "not set"));

        // A camera that can track: the tracking group, the range and the grid.
        let differing = MotionDetect {
            sensitivity_info_list: Some(reoling::SensitivityInfoList {
                items: vec![reoling::SensitivityInfo {
                    id: Some(0),
                    sensitivity: Some(20),
                    begin_hour: Some(0),
                    begin_minute: Some(0),
                    end_hour: Some(6),
                    end_minute: Some(0),
                }],
            }),
            ..motion
        };
        let page = CameraDetectionPage {
            motion: Ok(differing),
            track_limit: Ok(reoling::TrackLimit { channel_id: Some(0), left_limit: Some(100), right_limit: Some(2000) }),
            track_schedule: Ok(reoling::TrackSchedule { channel_id: Some(0), time_table: Some("1".repeat(168)) }),
            ..page
        };
        let ptz = ChannelAbilities { tracking: true, ..Default::default() };
        let tab = build(&SettingsPage::CameraDetection { channel: 0, page }, &Context { abilities: Some(ptz), ..Default::default() });
        let titles: Vec<&str> = tab.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Motion detection", "Smart detection", "Person", "Vehicle", "Animal", "Auto tracking", "Horizontal tracking range", "Tracking schedule"]
        );
        assert_eq!(tab.groups[0].rows[2], Row::text("00:00–06:00", "20"));
        let tracking: Vec<&str> = tab.groups[5].rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(tracking, ["Auto tracking", "Tracking type", "Stop tracking when the object stops"]);
        assert_eq!(tab.groups[6].rows[1], Row::text("Right", "2000"));
        assert!(matches!(&tab.groups[7].rows[0].value, Value::Schedules(t) if t.len() == 1 && t[0].0 == "Tracking" && t[0].1.all()));

        // The device itself can say it tracks.
        let says_so = AiCfg { smart_track_mode_ability: Some(1), ..ai };
        let page = CameraDetectionPage {
            ai: Ok(says_so),
            motion: Err(ReadFailure::Refused(400)),
            people: Err(ReadFailure::Refused(400)),
            vehicle: Err(ReadFailure::Refused(400)),
            animal: Err(ReadFailure::Refused(400)),
            track_limit: Err(ReadFailure::NotSent),
            track_schedule: Err(ReadFailure::NotSent),
        };
        let tab = build(&SettingsPage::CameraDetection { channel: 0, page }, &Context::default());
        assert!(tab.groups.iter().any(|g| g.title == "Auto tracking"));
        assert_eq!(tab.groups[0].rows, vec![Row::text("", NOT_AVAILABLE)]);
    }

    #[test]
    fn light_and_sound_tab_hides_what_the_camera_lacks() {
        let led = LedState {
            channel_id: Some(0),
            led_version: Some(2),
            ir_led_brightness: Some(0),
            state: Some("auto".into()),
            light_state: Some("close".into()),
            doorbell_light_state: None,
        };
        let page = CameraLightSoundPage {
            led: Ok(led.clone()),
            spotlight: Err(ReadFailure::NotSent),
            audio: Ok(reoling::AudioCfg { channel_id: Some(0), volume: Some(80) }),
            denoise: Err(ReadFailure::Refused(400)),
            record_audio: Ok(reoling::CompressionAudio { channel_id: Some(0), main_stream: Some(reoling::StreamAudio { audio: Some(1) }) }),
        };
        let no_led = ChannelAbilities { status_led: false, ..Default::default() };
        let tab = build(&SettingsPage::CameraLightSound { channel: 0, page: page.clone() }, &Context { abilities: Some(no_led), ..Default::default() });
        let titles: Vec<&str> = tab.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, ["Lights", "Sound"]);
        assert_eq!(tab.groups[0].rows, vec![Row::text("Infrared lights", "Auto")]);
        assert_eq!(tab.groups[1].rows, vec![Row::text("Volume", "80"), Row::text("Record audio", "On")]);

        let bright = LedState { ir_led_brightness: Some(60), ..led };
        let page = CameraLightSoundPage {
            led: Ok(bright),
            spotlight: Ok(FloodlightTask {
                channel: Some(0),
                alarm_mode: Some(1),
                duration: Some(30),
                brightness_cur: Some(50),
                brightness_min: Some(1),
                brightness_max: Some(100),
                schedule: Some(HourSpan { start_hour: Some(18), start_min: Some(0), end_hour: Some(6), end_min: Some(0) }),
                light_alarm_schedule: None,
                detect_type: Some("people,vehicle".into()),
            }),
            denoise: Ok(reoling::AiDenoise { channel_id: Some(0), enable: Some(1), level: Some(3) }),
            audio: Err(ReadFailure::NotSent),
            ..page
        };
        let tab = build(&SettingsPage::CameraLightSound { channel: 0, page }, &Context::default());
        let lights: Vec<&str> = tab.groups[0].rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(lights, ["Status LED", "Infrared lights", "IR brightness"]);
        assert_eq!(tab.groups[0].rows[0].value, Value::Text("Off".into()));
        assert_eq!(tab.groups[1].title, "Spotlight");
        assert_eq!(tab.groups[1].rows[0], Row::text("Mode", "Night smart mode"));
        assert_eq!(tab.groups[1].rows[1], Row::text("Brightness", "50 (1–100)"));
        assert_eq!(tab.groups[1].rows[3], Row::text("Schedule", "18:00–06:00"));
        assert_eq!(tab.groups[1].rows[4], Row::text("Alarm schedule", "—"));
        assert_eq!(tab.groups[1].rows[5], Row::text("Triggers", "Person, Vehicle"));
        assert_eq!(tab.groups[2].rows, vec![Row::text("Noise reduction", "On, level 3"), Row::text("Record audio", "On")]);
    }

    #[test]
    fn schedules_tab_lists_one_group_per_task() {
        let task = |enable: u32, items: Vec<(&str, ScheduleTable)>| ScheduledTask {
            channel_id: Some(0),
            enable: Some(enable),
            type_schedule_list: Some(TypeScheduleList {
                items: items
                    .into_iter()
                    .map(|(k, t)| ScheduleItem { kind: Some(k.into()), value_table: Some(t.cells().iter().map(|c| if *c { '1' } else { '0' }).collect()) })
                    .collect(),
            }),
        };
        let page = CameraSchedulesPage {
            record: Ok(task(1, vec![("MD", ones()), ("Normal", zeros())])),
            record_cfg: Ok(RecordCfg { record_delay_time: Some(15), pre_record_time: Some(1), package_time: Some(30), ..Default::default() }),
            email: Ok(task(0, vec![("people", ones())])),
            push: Ok(task(1, vec![("MD", ones())])),
            push_cfg: Ok(reoling::PushCfg { interval: Some(60), ..Default::default() }),
            ftp: Err(ReadFailure::NotSent),
            siren: Err(ReadFailure::Refused(400)),
            hub_siren: Ok(task(1, Vec::new())),
        };
        let tab = build(&SettingsPage::CameraSchedules { channel: 0, page }, &Context::default());
        let titles: Vec<&str> = tab.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, ["Recording", "Email", "Push notifications", "Siren", "Home Hub siren"]);
        let record: Vec<&str> = tab.groups[0].rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(record, ["Enabled", "Post-motion recording", "Pre-recording", "Pack duration", ""]);
        assert_eq!(tab.groups[0].rows[1].value, Value::Text("15 s".into()));
        assert_eq!(tab.groups[0].rows[3].value, Value::Text("30".into()));
        match &tab.groups[0].rows[4].value {
            Value::Schedules(t) => assert_eq!(t.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), ["Any motion", "Continuous"]),
            other => panic!("{other:?}"),
        }
        assert_eq!(tab.groups[1].rows[0], Row::text("Enabled", "Off"));
        assert_eq!(tab.groups[2].rows[1], Row::text("Interval", "60 s"));
        assert_eq!(tab.groups[3].rows, vec![Row::text("", NOT_AVAILABLE)]);
        assert_eq!(tab.groups[4].rows, vec![Row::text("Enabled", "On")]);
    }

    fn network_rows() -> NetworkRows {
        NetworkRows {
            dhcp: Ok(Enable { enable: Some(1) }),
            auto_dns: Ok(Enable { enable: Some(0) }),
            ip: Ok(IpConfig {
                ip: Some("192.168.1.50".into()),
                mask: Some("255.255.255.0".into()),
                gateway: Some("192.168.1.1".into()),
                mac: Some(MacAddress::new("00:00:00:00:00:00")),
            }),
            dns: Ok(DnsConfig { dns1: Some("192.168.1.1".into()), dns2: Some("".into()) }),
        }
    }

    #[test]
    fn network_rows_translate_the_switches() {
        let tab = build(&SettingsPage::CameraNetwork { channel: 0, page: network_rows() }, &Context::default());
        assert_eq!(tab.groups.len(), 1);
        let rows: Vec<(&str, &Value)> = tab.groups[0].rows.iter().map(|r| (r.title.as_str(), &r.value)).collect();
        assert_eq!(rows[0], ("Address mode", &Value::Text("DHCP".into())));
        assert_eq!(rows[4], ("MAC address", &Value::Text("00:00:00:00:00:00".into())));
        assert_eq!(rows[5], ("DNS", &Value::Text("Manual".into())));
        assert_eq!(rows[7], ("DNS server 2", &Value::Text("—".into())));

        let not_sent = NetworkRows {
            dhcp: Err(ReadFailure::NotSent),
            auto_dns: Err(ReadFailure::NotSent),
            ip: Err(ReadFailure::NotSent),
            dns: Err(ReadFailure::NotSent),
        };
        let tab = build(&SettingsPage::CameraNetwork { channel: 0, page: not_sent }, &Context::default());
        assert!(tab.groups.is_empty());
        assert_eq!(tab.status, "");
    }

    #[test]
    fn storage_tab_sizes_the_disks() {
        let disk = HddInfo {
            number: Some(2),
            capacity: Some(14872),
            capacity_m: Some(201),
            remain_size: Some(1000),
            remain_size_m: Some(0),
            format: Some(1),
            mount: Some(1),
            ..Default::default()
        };
        let page = DeviceStoragePage {
            disks: Ok(reoling::HddInfoList { disks: vec![disk, HddInfo { number: Some(0), ..Default::default() }] }),
            record_cfg: Ok(RecordCfg { cycle: Some(1), ..Default::default() }),
            encryption: Ok(Enable { enable: Some(0) }),
        };
        let tab = build(&SettingsPage::DeviceStorage(page), &Context::default());
        let titles: Vec<&str> = tab.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, ["HDD2", "HDD", "Recording"]);
        assert_eq!(tab.groups[0].rows[0], Row::text("Capacity", "14872.20 GiB"));
        match &tab.groups[0].rows[1].value {
            Value::Level { text, fraction } => {
                assert_eq!(text, "13872.20 GiB");
                assert!((fraction - 13872.2 / 14872.2).abs() < 1e-6);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(tab.groups[0].rows[3], Row::text("Formatted", "Yes"));
        assert!(matches!(&tab.groups[1].rows[1].value, Value::Level { fraction, .. } if *fraction == 0.0));
        assert_eq!(tab.groups[2].rows, vec![Row::text("When full", "Overwrite oldest files"), Row::text("Recording encryption", "Off")]);

        let empty = DeviceStoragePage {
            disks: Ok(reoling::HddInfoList { disks: Vec::new() }),
            record_cfg: Err(ReadFailure::NoReply),
            encryption: Err(ReadFailure::NoReply),
        };
        let tab = build(&SettingsPage::DeviceStorage(empty), &Context::default());
        assert_eq!(tab.groups[0].rows, vec![Row::text("", "No disk")]);
        assert_eq!(tab.groups[1].rows[0], Row::text("When full", NO_ANSWER));
    }

    #[test]
    fn device_network_tab_groups_services_and_wifi() {
        let port = |p: ServicePort| Ok(p);
        let page = DeviceNetworkPage {
            network: network_rows(),
            server: port(ServicePort { server_port: Some(9000), enable: Some(0), ..Default::default() }),
            http: port(ServicePort { http_port: Some(80), enable: Some(1), ..Default::default() }),
            https: port(ServicePort { https_port: Some(443), enable: Some(1), ..Default::default() }),
            rtsp: port(ServicePort { rtsp_port: Some(554), enable: Some(0), ..Default::default() }),
            rtmp: port(ServicePort { rtmp_port: Some(1935), enable: Some(0), ..Default::default() }),
            onvif: port(ServicePort { onvif_port: Some(8000), enable: Some(1), ..Default::default() }),
            upnp: Ok(Enable { enable: Some(0) }),
            p2p: Ok(Enable { enable: Some(1) }),
            ddns: Ok(reoling::Ddns { enable: Some(0), ddns_type: Some("reolink".into()) }),
            ntp: Ok(Ntp { enable: Some(1), server: Some("ntp.example".into()), port: Some(123), synchronize_interval: Some(1440) }),
            certificate: Ok(Enable { enable: Some(0) }),
            hub_ap: Ok(KitApCfg {
                bands: vec![
                    ApCfg { freq: Some("2.4G".into()), channel: Some(0), hide_ssid: Some(0), ssid: Some(Ssid::new("net")), country_code: Some("DE".into()), enable_wifi: Some(1) },
                    ApCfg { freq: Some("5G".into()), channel: Some(36), hide_ssid: Some(1), ssid: Some(Ssid::new("net5")), country_code: None, enable_wifi: Some(1) },
                ],
            }),
        };
        let tab = build(&SettingsPage::DeviceNetwork(page), &Context::default());
        let titles: Vec<&str> = tab.groups.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, ["Network", "Services", "DDNS", "NTP", "Wi-Fi for cameras — 2.4 GHz", "Wi-Fi for cameras — 5 GHz"]);
        let services: Vec<(&str, &Value)> = tab.groups[1].rows.iter().map(|r| (r.title.as_str(), &r.value)).collect();
        assert_eq!(services[0], ("Basic service port", &Value::Text("9000".into())));
        assert_eq!(services[1], ("HTTP", &Value::Text("On, port 80".into())));
        assert_eq!(services[3], ("RTSP", &Value::Text("Off (port 554)".into())));
        assert_eq!(services[7], ("Access by UID (P2P)", &Value::Text("On".into())));
        assert_eq!(services[8], ("HTTPS certificate", &Value::Text("Default (self-signed)".into())));
        assert_eq!(tab.groups[2].rows[1], Row::text("Provider", "reolink"));
        assert_eq!(tab.groups[3].rows[3], Row::text("Sync interval", "1440"));
        let band: Vec<(&str, &Value)> = tab.groups[4].rows.iter().map(|r| (r.title.as_str(), &r.value)).collect();
        assert_eq!(band[0], ("Network name", &Value::Text("net".into())));
        assert_eq!(band[1], ("Channel", &Value::Text("Auto".into())));
        assert_eq!(band[3], ("Country", &Value::Text("DE".into())));
        assert_eq!(band.len(), 4);
        let five: Vec<&str> = tab.groups[5].rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(five, ["Network name", "Channel", "Hidden"]);
    }

    #[test]
    fn time_tab_reads_the_clock() {
        let general = SystemGeneral {
            time_zone: Some(-7200),
            osd_format: Some("DMY".into()),
            year: Some(2026),
            month: Some(10),
            day: Some(24),
            hour: Some(9),
            minute: Some(7),
            second: Some(3),
            time_format: Some(0),
            is_dst: Some(0),
            ..Default::default()
        };
        let page = DeviceTimePage {
            general: Ok(general),
            norm: Ok(reoling::Norm { norm: Some("PAL".into()) }),
            dst: Ok(Dst { enable: Some(0), ..Default::default() }),
            ntp: Err(ReadFailure::Refused(400)),
        };
        let tab = build(&SettingsPage::DeviceTime(page), &Context::default());
        let rows: Vec<(&str, &Value)> = tab.groups[0].rows.iter().map(|r| (r.title.as_str(), &r.value)).collect();
        assert_eq!(rows[0], ("Device clock (at the time of reading)", &Value::Text("2026-10-24 09:07:03".into())));
        assert_eq!(rows[1], ("Time zone", &Value::Text("UTC+02:00".into())));
        assert_eq!(rows[2], ("Daylight saving", &Value::Text("Off".into())));
        assert_eq!(rows[3], ("Date format", &Value::Text("DD/MM/YYYY".into())));
        assert_eq!(rows[4], ("Time format", &Value::Text("24-hour".into())));
        assert_eq!(rows[5], ("Video standard", &Value::Text("PAL".into())));
        assert_eq!(tab.groups[1].title, "NTP");
        assert_eq!(tab.groups[1].rows, vec![Row::text("", NOT_AVAILABLE)]);
    }

    #[test]
    fn hub_light_sound_and_nvr_alerts() {
        let page = DeviceLightSoundPage {
            led: Ok(LedState { light_state: Some("open".into()), ..Default::default() }),
            audio: Ok(DeviceAudioCfg { alarm_volume: Some(80), cues_volume: Some(50), alarm_pause_time: Some(600) }),
        };
        let tab = build(&SettingsPage::DeviceLightSound(page), &Context::default());
        assert_eq!(tab.groups[0].rows, vec![Row::text("Status LED", "On")]);
        assert_eq!(
            tab.groups[1].rows,
            vec![Row::text("Notification volume", "50"), Row::text("Alarm volume", "80"), Row::text("Alarm pause", "600")]
        );

        let sw = |mask: u64, main: u32| Ok(ChannelSwitch { enable: Some(mask), main_switch: Some(main) });
        let page = DeviceAlertsPage { record: sw(0b101, 1), email: sw(0, 0), push: Err(ReadFailure::Refused(400)), buzzer: Err(ReadFailure::NotSent) };
        let chans = channels();
        let tab = build(&SettingsPage::DeviceAlerts(page), &Context { channels: &chans, ..Default::default() });
        assert_eq!(tab.status, "");
        let rows = &tab.groups[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "Recording");
        assert!(matches!(&rows[0].value, Value::Lines { text, lines } if text == "On · 2 of 2 channels" && lines.len() == 2));
        assert!(matches!(&rows[1].value, Value::Lines { text, .. } if text == "Off"));
        assert_eq!(rows[2], Row::text("Push notifications", NOT_AVAILABLE));
    }

    #[test]
    fn users_tab_masks_nothing_it_should_not_show() {
        let page = DeviceUsersPage {
            users: Ok(UserList {
                users: vec![
                    UserEntry { user_name: Some(reoling::AccountName::new("admin")), user_level: Some(1), login_state: Some(1) },
                    UserEntry { user_name: Some(reoling::AccountName::new("guest")), user_level: Some(0), login_state: Some(0) },
                ],
            }),
            shared: Ok(AccessUserList {
                max_access_user_num: Some(20),
                available_ability: Some(3),
                users: vec![AccessUser { user_level: Some(0), valid_hours: Some(5), login_state: Some(0), ability: Some(1), channel_ability: Some(0b1011) }],
            }),
            general: Ok(SystemGeneral { login_lock: Some(1), lock_time: Some(60), allowed_times: Some(5), ..Default::default() }),
        };
        let tab = build(&SettingsPage::DeviceUsers(page), &Context::default());
        assert_eq!(tab.groups[0].rows, vec![Row::text("admin", "Administrator · signed in"), Row::text("guest", "User")]);
        assert_eq!(tab.groups[1].rows[0], Row::text("Shared users", "1 of 20"));
        assert_eq!(tab.groups[1].rows[1], Row::text("Shared user 1", "User, 3 channels, Authorisation ends in 5 h"));
        assert_eq!(
            tab.groups[2].rows,
            vec![Row::text("Lock after failed logins", "On"), Row::text("Rule", "after 5 failed attempts, for 60 s")]
        );
    }

    #[test]
    fn maintenance_tab_words_the_reboot() {
        let page = DeviceMaintenancePage {
            auto_reboot: Ok(AutoReboot { enable: Some(1), week_day: Some("Monday".into()), hour: Some(3), minute: Some(0), second: Some(0) }),
        };
        let tab = build(&SettingsPage::DeviceMaintenance(page), &Context::default());
        assert_eq!(tab.groups[0].rows, vec![Row::text("Enabled", "On"), Row::text("When", "Every Monday at 03:00:00")]);
        let none = DeviceMaintenancePage { auto_reboot: Err(ReadFailure::NoReply) };
        let tab = build(&SettingsPage::DeviceMaintenance(none), &Context::default());
        assert_eq!(tab.status, NO_ANSWER);
        assert_eq!(tab.groups[0].rows, vec![Row::text("", NO_ANSWER)]);
    }
}
