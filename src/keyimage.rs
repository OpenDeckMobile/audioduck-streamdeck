//! Key images in Foreground Boost's design: flat themed tile ("default" Stream Deck look or "duotone" Duotone Essentials
//! colours). Top to bottom: the apps that trigger the ducking, a pill with the volume the others go down to, and the
//! apps that go down. Drawn as SVG, sent as PNG data URIs.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::{HSTRING, PWSTR};

use crate::engine::Rule;
use crate::streamdeck::base64;

const ICON: u32 = 64;
const RETRY: Duration = Duration::from_secs(3);

/// Same palettes as Foreground Boost / Volume Controller NEO: (background, secondary, accent).
const PRESETS: [(&str, [&str; 3]); 4] = [
    ("blue", ["#3b4c66", "#4c70a3", "#67a4ff"]),
    ("orange", ["#6a5542", "#a67b50", "#ffb367"]),
    ("purple", ["#4e3d66", "#774ea3", "#b367ff"]),
    ("red", ["#623d46", "#a14e5d", "#ff6783"]),
];

pub struct Theme {
    duo: bool,
    bg: String,
    fg: String,
    sec: String, // in "default": the muted grey
}

fn rgb(h: &str) -> [u8; 3] {
    let n = u32::from_str_radix(h.trim_start_matches('#'), 16).unwrap_or(0);
    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
}

fn is_hex(h: &str) -> bool {
    h.len() == 7 && h.starts_with('#') && h[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Key settings: `style` ("default" | "duotone"), `duoPreset` (a preset name or "custom"), `duoBg`, `duoAccent`.
pub fn theme(s: &Value) -> Theme {
    let get = |k: &str| s[k].as_str().unwrap_or_default();
    if get("style") != "duotone" {
        return Theme { duo: false, bg: "#000000".into(), fg: "#ffffff".into(), sec: "#8a8a8a".into() };
    }
    let (bg, accent, sec) = if get("duoPreset") == "custom" && is_hex(get("duoBg")) && is_hex(get("duoAccent")) {
        // Secondary: accent at 40% over the background.
        let (b, a) = (rgb(get("duoBg")), rgb(get("duoAccent")));
        let mix: Vec<String> = (0..3).map(|i| format!("{:02x}", (b[i] as f32 + (a[i] as f32 - b[i] as f32) * 0.4).round() as u8)).collect();
        (get("duoBg").to_string(), get("duoAccent").to_string(), format!("#{}", mix.concat()))
    } else {
        let p = PRESETS.iter().find(|(n, _)| *n == get("duoPreset")).unwrap_or(&PRESETS[0]).1;
        (p[0].into(), p[2].into(), p[1].into())
    };
    Theme { duo: true, bg, fg: accent, sec }
}

/// Duotone Essentials look: split the icon into its two best-separated luminance groups (Otsu) and paint them in the
/// secondary and accent tones. Port of Foreground Boost's `duoIconRgba`.
fn duo_icon(rgba: &mut [u8], sec: [u8; 3], pri: [u8; 3]) {
    let lum = |p: &[u8]| (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 255.0;
    let solid = || rgba.chunks(4).filter(|p| p[3] > 128);
    let (lo, hi) = solid().fold((1f32, 0f32), |(lo, hi), p| (lo.min(lum(p)), hi.max(lum(p))));
    let span = (hi - lo).max(0.05);
    let norm = |p: &[u8]| (lum(p) - lo) / span;
    let mut hist = [0u32; 64];
    let mut n = 0;
    for p in solid() {
        hist[((64.0 * norm(p)) as usize).min(63)] += 1;
        n += 1;
    }
    let tot: f64 = hist.iter().enumerate().map(|(k, &c)| c as f64 * k as f64).sum();
    let (mut best, mut cut, mut w0, mut s0) = (0f64, 0.5f32, 0u32, 0f64);
    for (k, &h) in hist.iter().enumerate().take(63) {
        w0 += h;
        s0 += h as f64 * k as f64;
        let w1 = n - w0;
        if w0 == 0 || w1 == 0 {
            continue;
        }
        let v = w0 as f64 * w1 as f64 * (s0 / w0 as f64 - (tot - s0) / w1 as f64).powi(2);
        if v > best {
            best = v;
            cut = (k + 1) as f32 / 64.0;
        }
    }
    for p in rgba.chunks_mut(4) {
        let t = ((norm(p) - cut) / 0.08 + 0.5).clamp(0.0, 1.0); // soft edge at the cut
        for k in 0..3 {
            p[k] = (sec[k] as f32 + (pri[k] as f32 - sec[k] as f32) * t).round() as u8;
        }
    }
}

fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// ICON×ICON RGBA of the exe's main icon.
fn exe_icon(path: &str) -> Option<Vec<u8>> {
    unsafe {
        let mut icon = HICON::default();
        SHDefExtractIconW(&HSTRING::from(path), 0, 0, Some(&mut icon), None, ICON).ok().ok()?;
        let mut info = ICONINFO::default();
        let got = GetIconInfo(icon, &mut info);
        let _ = DestroyIcon(icon);
        got.ok()?;
        let dc = CreateCompatibleDC(None);
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ICON as i32,
                biHeight: -(ICON as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (ICON * ICON * 4) as usize];
        let rows = GetDIBits(dc, info.hbmColor, 0, ICON, Some(px.as_mut_ptr() as _), &mut bmi, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        if rows == 0 {
            return None;
        }
        let has_alpha = px.chunks(4).any(|p| p[3] != 0);
        for p in px.chunks_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
            if !has_alpha {
                p[3] = 255; // shortcut: old icons without alpha come out as an opaque square
            }
        }
        Some(px)
    }
}

/// Where an app's icon is kept once seen, so a key can show it while the app isn't running. None for names that
/// can't be a plain file name (they come from key settings).
fn cache_path(exe: &str) -> Option<std::path::PathBuf> {
    let plain = !exe.is_empty() && exe.len() <= 64 && exe.chars().all(|c| c.is_ascii_alphanumeric() || " ._-".contains(c)) && !exe.starts_with('.');
    plain.then(|| crate::app_dir().join("icons").join(format!("{exe}.png")))
}

fn load_cached(exe: &str) -> Option<Vec<u8>> {
    let file = std::fs::File::open(cache_path(exe)?).ok()?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().ok()?;
    let info = reader.info();
    if (info.width, info.height, info.color_type, info.bit_depth) != (ICON, ICON, png::ColorType::Rgba, png::BitDepth::Eight) {
        return None;
    }
    let mut px = vec![0u8; reader.output_buffer_size()?];
    reader.next_frame(&mut px).ok()?;
    Some(px)
}

fn save_cached(exe: &str, rgba: &[u8]) {
    let Some(path) = cache_path(exe) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, crate::icon::encode(rgba, ICON));
}

/// App icons by exe name: from the disk cache, else from a running process (an app that isn't running and was never
/// seen is retried every RETRY).
#[derive(Default)]
pub struct Icons {
    rgba: HashMap<String, Option<Vec<u8>>>,
    tried: HashMap<String, Instant>,
    png: HashMap<(String, bool, String), String>, // (exe, duo, colours) -> base64 PNG
}

impl Icons {
    fn png_b64(&mut self, exe: &str, t: &Theme) -> Option<String> {
        let key = (exe.to_string(), t.duo, format!("{}{}", t.sec, t.fg));
        if let Some(p) = self.png.get(&key) {
            return Some(p.clone());
        }
        if !self.rgba.contains_key(exe) {
            if let Some(px) = load_cached(exe) {
                self.rgba.insert(exe.to_string(), Some(px));
            } else {
                if self.tried.get(exe).is_some_and(|t| t.elapsed() < RETRY) {
                    return None;
                }
                self.tried.insert(exe.to_string(), Instant::now());
                let pid = crate::audio::processes().into_iter().find(|(_, (n, _))| n == exe)?.0;
                let px = image_path(pid).and_then(|p| exe_icon(&p));
                if let Some(px) = &px {
                    save_cached(exe, px);
                }
                self.rgba.insert(exe.to_string(), px);
            }
        }
        let mut px = self.rgba.get(exe)?.clone()?;
        if t.duo {
            duo_icon(&mut px, rgb(&t.sec), rgb(&t.fg));
        }
        let b = base64(&crate::icon::encode(&px, ICON));
        self.png.insert(key, b.clone());
        Some(b)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum State {
    SetUp, // nothing set up on this key yet
    Off,
    Listening,
    Ducking,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

const FONT: &str = r#"font-family="Segoe UI,Arial,sans-serif" font-weight="700""#;

/// Up to three apps side by side, centred, `size` px each, top edge at `y`: their icons, or a tile with the app's
/// initial while it has never been seen running. More than three show the first two and a "+n" tile. Returns where
/// each shown app starts.
fn app_row(p: &mut Vec<String>, icons: &mut Icons, t: &Theme, apps: &[String], y: u32, size: u32, opacity: &str) -> Vec<u32> {
    let shown = if apps.len() <= 3 { apps.len() } else { 2 };
    let more = apps.len() - shown;
    let slots = (shown + usize::from(more > 0)) as u32;
    let gap = size / 7;
    let mut x = (144 - (slots * size + slots.saturating_sub(1) * gap)) / 2;
    let tile = |p: &mut Vec<String>, x: u32, text: &str| {
        p.push(format!(r#"<rect x="{x}" y="{y}" width="{size}" height="{size}" rx="{}" fill="{}" opacity="{opacity}"/>"#, size / 4, t.sec));
        let font = if text.chars().count() > 1 { size * 2 / 5 } else { size / 2 };
        p.push(format!(r#"<text x="{}" y="{}" text-anchor="middle" {FONT} font-size="{font}" fill="{}" opacity="{opacity}">{}</text>"#, x + size / 2, y + size / 2 + font * 7 / 20, t.fg, esc(text)));
    };
    let mut starts = Vec::new();
    for app in &apps[..shown] {
        starts.push(x);
        match icons.png_b64(app, t) {
            Some(png) => p.push(format!(r#"<image x="{x}" y="{y}" width="{size}" height="{size}" opacity="{opacity}" href="data:image/png;base64,{png}"/>"#)),
            None => tile(p, x, &app.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
        }
        x += size + gap;
    }
    if more > 0 {
        tile(p, x, &format!("+{more}"));
    }
    starts
}

/// The key for one ducking setup (`r`): the apps whose sound triggers it on top, the volume the others go down to in
/// the middle, and the apps that go down at the bottom ("all others" with `all_others`, its exceptions crossed out).
pub fn key_image(icons: &mut Icons, settings: &Value, state: State, r: &Rule) -> String {
    let svg = key_svg(icons, settings, state, r);
    match to_png(&svg) {
        Some(png) => format!("data:image/png;base64,{}", base64(&png)),
        None => format!("data:image/svg+xml;base64,{}", base64(svg.as_bytes())),
    }
}

/// Stream Deck rasterizing an SVG with an embedded icon held back key events from a phone deck by up to 0.6 s;
/// a ready PNG costs it almost nothing.
fn to_png(svg: &str) -> Option<Vec<u8>> {
    static OPT: std::sync::OnceLock<resvg::usvg::Options> = std::sync::OnceLock::new();
    let opt = OPT.get_or_init(|| {
        let mut o = resvg::usvg::Options::default();
        if let Ok(f) = std::fs::read(r"C:\Windows\Fonts\segoeuib.ttf") { o.fontdb_mut().load_font_data(f) } // the only face the keys use
        o.fontdb_mut().set_sans_serif_family("Segoe UI");
        o
    });
    let tree = resvg::usvg::Tree::from_str(svg, opt).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(144, 144)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

fn key_svg(icons: &mut Icons, settings: &Value, state: State, r: &Rule) -> String {
    let t = theme(settings);
    let mut p = vec![r#"<svg xmlns="http://www.w3.org/2000/svg" width="144" height="144" viewBox="0 0 144 144">"#.to_string()];
    p.push(format!(r#"<rect width="144" height="144" fill="{}"/>"#, t.bg));
    if state == State::SetUp {
        // AudioDuck's equalizer glyph.
        let bar = if t.duo { &t.sec } else { &t.fg };
        for (x, h) in [(46.0, 22.0), (66.0, 40.0), (86.0, 30.0)] {
            p.push(format!(r#"<rect x="{x}" y="{}" width="12" height="{h}" rx="6" fill="{bar}" opacity="0.45"/>"#, 46.0 - h / 2.0));
        }
        p.push(format!(r#"<text x="72" y="121" text-anchor="middle" {FONT} font-size="22" fill="{}">Set up</text>"#, t.sec));
        p.push("</svg>".into());
        return p.concat();
    }
    let off = state == State::Off;
    let opacity = if off { "0.45" } else { "1" };
    // Top: the apps whose sound starts the ducking.
    app_row(&mut p, icons, &t, &r.sources, 10, 40, opacity);
    // Middle: down to what volume. Fixed width like Foreground Boost's pill: Stream Deck draws Segoe UI wider than
    // browsers, so never size to the text. Filled while ducking, outlined while waiting, "OFF" when switched off.
    let ink = match state {
        State::Ducking => {
            p.push(format!(r#"<rect x="26" y="57" width="92" height="30" rx="15" fill="{}"/>"#, t.fg));
            &t.bg
        }
        State::Listening => {
            p.push(format!(r#"<rect x="27.5" y="58.5" width="89" height="27" rx="13.5" fill="none" stroke="{}" stroke-width="3"/>"#, t.sec));
            &t.fg
        }
        _ => {
            p.push(format!(r#"<rect x="27.5" y="58.5" width="89" height="27" rx="13.5" fill="none" stroke="{}" stroke-width="3" opacity="{opacity}"/>"#, t.sec));
            &t.sec
        }
    };
    if off {
        p.push(format!(r#"<text x="72" y="80" text-anchor="middle" {FONT} font-size="19" fill="{ink}">OFF</text>"#));
    } else {
        p.push(format!(r#"<path d="M44 64v14m-6-6l6 6 6-6" fill="none" stroke="{ink}" stroke-width="3.5" stroke-linecap="round" stroke-linejoin="round"/>"#));
        p.push(format!(r#"<text x="81" y="80" text-anchor="middle" {FONT} font-size="19" fill="{ink}">{}%</text>"#, (r.level * 100.0).round()));
    }
    // Bottom: the apps that go down, dimmed while they are down.
    let low = if state == State::Ducking { "0.55" } else { opacity };
    if !r.all_others {
        app_row(&mut p, icons, &t, &r.targets, 95, 40, low);
    } else if r.targets.is_empty() {
        p.push(format!(r#"<text x="72" y="111" text-anchor="middle" {FONT} font-size="17" fill="{}" opacity="{low}">ALL</text>"#, t.fg));
        p.push(format!(r#"<text x="72" y="131" text-anchor="middle" {FONT} font-size="17" fill="{}" opacity="{low}">OTHERS</text>"#, t.fg));
    } else {
        // Everything else except these, which are crossed out.
        p.push(format!(r#"<text x="72" y="107" text-anchor="middle" {FONT} font-size="14" fill="{}" opacity="{low}">ALL OTHERS</text>"#, t.fg));
        for x in app_row(&mut p, icons, &t, &r.targets, 112, 24, low) {
            let d = format!("M{} 138L{} 110", x - 2, x + 26);
            p.push(format!(r#"<path d="{d}" stroke="{}" stroke-width="6" stroke-linecap="round" opacity="{low}"/><path d="{d}" stroke="{}" stroke-width="2.5" stroke-linecap="round" opacity="{low}"/>"#, t.bg, t.fg));
        }
    }
    p.push("</svg>".into());
    p.concat()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn themes() {
        assert_eq!(theme(&json!({})).bg, "#000000");
        let t = theme(&json!({ "style": "duotone", "duoPreset": "red" }));
        assert_eq!((t.bg.as_str(), t.sec.as_str(), t.fg.as_str()), ("#623d46", "#a14e5d", "#ff6783"));
        let t = theme(&json!({ "style": "duotone", "duoPreset": "custom", "duoBg": "#000000", "duoAccent": "#ffffff" }));
        assert_eq!(t.sec, "#666666");
        assert_eq!(theme(&json!({ "style": "duotone", "duoPreset": "custom", "duoBg": "bad" })).bg, "#3b4c66"); // falls back to blue
    }

    fn rule(sources: &[&str], targets: &[&str], all_others: bool) -> Rule {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
        Rule { sources: v(sources), targets: v(targets), all_others, level: 0.1, enabled: true }
    }

    #[test]
    fn key_shows_every_trigger_above_every_target() {
        let svg = |r: &Rule, state| key_svg(&mut Icons::default(), &json!({}), state, r);
        // Apps never seen running show as tiles with their initial: sources above the pill, targets below it.
        let two = svg(&rule(&["qqnoapp.exe", "wwnoapp.exe"], &["zznoapp.exe"], false), State::Listening);
        let (q, w, pill, z) = (two.find(">Q<").unwrap(), two.find(">W<").unwrap(), two.find(">10%<").unwrap(), two.find(">Z<").unwrap());
        assert!(q < w && w < pill && pill < z, "{two}");
        // Four apps: the first two and "+2".
        let four = svg(&rule(&["anoapp.exe", "bnoapp.exe", "cnoapp.exe", "dnoapp.exe"], &["znoapp.exe"], false), State::Listening);
        assert!(four.contains(">A<") && four.contains(">B<") && four.contains(">+2<") && !four.contains(">C<"), "{four}");
        // Everything else, then everything else except one app, crossed out.
        let all = svg(&rule(&["qqnoapp.exe"], &[], true), State::Listening);
        assert!(all.contains(">ALL<") && all.contains(">OTHERS<"), "{all}");
        let except = svg(&rule(&["qqnoapp.exe"], &["zznoapp.exe"], true), State::Listening);
        assert!(except.contains(">ALL OTHERS<") && except.contains(">Z<") && except.contains("M58 138L86 110"), "{except}");
        // Off says so instead of the level; ducking fills the pill.
        let off = svg(&rule(&["qqnoapp.exe"], &[], true), State::Off);
        assert!(off.contains(">OFF<") && !off.contains("%<"), "{off}");
        let ducking = svg(&rule(&["qqnoapp.exe"], &[], true), State::Ducking);
        assert!(ducking.contains(r#"<rect x="26" y="57" width="92" height="30" rx="15""#), "{ducking}");
        let url = key_image(&mut Icons::default(), &json!({}), State::Listening, &rule(&["qqnoapp.exe"], &[], true));
        assert!(url.starts_with("data:image/png;base64,"), "{url:.40}");
    }

    #[test]
    fn cached_icons_only_under_plain_names() {
        assert!(cache_path("discord.exe").is_some_and(|p| p.ends_with(r"icons\discord.exe.png")));
        for bad in [r"..\x.exe", "a/b.exe", "", ".hidden", "c:x.exe"] {
            assert!(cache_path(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn duo_icon_maps_dark_and_light_to_the_two_tones() {
        let mut px = [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 255, 255, 255, 128].to_vec();
        duo_icon(&mut px, [1, 2, 3], [9, 8, 7]);
        // Light -> accent; dark -> mostly secondary (the soft edge at the cut blends it a little, as in Foreground Boost).
        assert_eq!(&px[4..8], [9, 8, 7, 255]);
        assert!(px[0] < 5 && px[3] == 255 && px[11] == 0 && px[15] == 128, "{px:?}");
    }
}
