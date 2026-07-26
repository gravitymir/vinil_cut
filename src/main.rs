use axum::extract::{DefaultBodyLimit, Multipart, Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

const PORT: u16 = 3400;

// Static light reflection: a wide, soft transparent->white->transparent band along
// the bottom-left -> top-right diagonal, CENTERED on the disc (peak at X+Y=842, i.e.
// through the middle). Masked to the disc but with a hole at the very center (radius
// < 26) so the spindle hole stays black — light falls into the hole, doesn't reflect.
// Overlaid AFTER rotation, so the glare stays put while the record spins under it.
const SHINE_FILTER: &str = "format=rgba,geq=\
r='255':g='255':b='255':\
a='if(between(hypot(X-421,Y-421),26,418),clip(30*exp(-pow((X+Y-842)/430,2))+110*exp(-pow((X+Y-842)/144,2)),0,150),0)'";
// Circular label mask (full disc, no cutout — the center is solid label color,
// no metal spindle; the shine just avoids the very center so it reads as a hole).
const MASK_FILTER: &str = "format=gray,geq=lum='if(lte(hypot(X-140,Y-140),140),255,0)'";

// The disc is a real (AI-generated) vinyl saved once as a GRAYSCALE groove map,
// assets/disc_base.png (the grooves/imperfections of an actual record). Any color
// is produced by tinting that gray map: out_c = clip(color_c * gray / 140). One
// high-quality base -> any vinyl color, keeping the real grooves. To swap the look
// entirely, just replace assets/disc_base.png with another grayscale masked disc.
// The disc rotates; the static shine (SHINE_FILTER) is a separate layer on top.

// "#rrggbb" / "rrggbb" -> (r,g,b). Falls back to a dark cool near-black.
fn parse_hex(s: &str) -> (u8, u8, u8) {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return (r, g, b);
        }
    }
    (24, 24, 30)
}

// Cache/generate a colored disc by tinting the gray base; returns its path.
async fn ensure_disc(cr: u8, cg: u8, cb: u8) -> Result<PathBuf, String> {
    let path = PathBuf::from("work").join(format!("disc_{:02x}{:02x}{:02x}.png", cr, cg, cb));
    if !path.exists() {
        if !Path::new("assets/disc_base.png").exists() {
            return Err("assets/disc_base.png not found — start VinilCut from its project folder.".into());
        }
        let tint = format!(
            "geq=r='clip({cr}*r(X,Y)/140,0,255)':g='clip({cg}*g(X,Y)/140,0,255)':b='clip({cb}*b(X,Y)/140,0,255)':a='alpha(X,Y)'"
        );
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i", "assets/disc_base.png", "-vf", &tint,
                   "-frames:v", "1", "-update", "1", path.to_str().unwrap()])
            .status().await.map(|s| s.success()).unwrap_or(false);
        if !ok || !path.exists() {
            return Err("failed to build the colored disc from assets/disc_base.png.".into());
        }
    }
    Ok(path)
}
// Inputs: 0=label, 1=disc, 2=audio, [3=cover if any], then shine (last).
// Order per frame: bg -> spinning disc(+label) -> STATIC shine on top.
const GRAPH_COVER: &str = "\
[3:v]scale=848:848:force_original_aspect_ratio=increase,crop=848:848,boxblur=30:1,eq=brightness=-0.18[bg];\
[1:v][0:v]overlay=(W-w)/2:(H-h)/2[rec];\
[rec]rotate=a='2*PI*t/3':c=none[spun];\
[bg][spun]overlay=3:3[disc];\
[disc][4:v]overlay=3:3[outv]";
const GRAPH_SOLID: &str = "\
color=c=0x141018:s=848x848:r=30[bg];\
[1:v][0:v]overlay=(W-w)/2:(H-h)/2[rec];\
[rec]rotate=a='2*PI*t/3':c=none[spun];\
[bg][spun]overlay=3:3[disc];\
[disc][3:v]overlay=3:3[outv]";

// Video note (Telegram circle): square 512x512, record fills the frame.
const GRAPH_NOTE_COVER: &str = "\
[3:v]scale=512:512:force_original_aspect_ratio=increase,crop=512:512,boxblur=20:1,eq=brightness=-0.18[bg];\
[1:v]scale=512:512[d];[0:v]scale=170:170[l];\
[d][l]overlay=(W-w)/2:(H-h)/2[rec];\
[rec]rotate=a='2*PI*t/3':c=none[spun];\
[4:v]scale=512:512[shine];\
[bg][spun]overlay=0:0[disc];\
[disc][shine]overlay=0:0[outv]";
const GRAPH_NOTE_SOLID: &str = "\
color=c=black:s=512x512:r=30[bg];\
[1:v]scale=512:512[d];[0:v]scale=170:170[l];\
[d][l]overlay=(W-w)/2:(H-h)/2[rec];\
[rec]rotate=a='2*PI*t/3':c=none[spun];\
[3:v]scale=512:512[shine];\
[bg][spun]overlay=0:0[disc];\
[disc][shine]overlay=0:0[outv]";

struct Job {
    percent: f64,
    done: bool,
    ok: bool,
    error: String,
    out: PathBuf,
    dir: PathBuf,
}
type Jobs = Arc<Mutex<HashMap<String, Job>>>;

fn now_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

async fn gen_png(size: &str, filter: &str, out: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", &format!("color=c=black:s={size}"),
               "-vf", filter, "-frames:v", "1", out])
        .status().await.map(|s| s.success()).unwrap_or(false)
}

async fn ensure_assets() {
    let _ = std::fs::create_dir_all("assets");
    let _ = std::fs::create_dir_all("work");
    if !Path::new("assets/shine.png").exists() {
        println!("shine.png: {}", if gen_png("842x842", SHINE_FILTER, "assets/shine.png").await { "ok" } else { "FAILED" });
    }
    if !Path::new("assets/mask.png").exists() {
        println!("mask.png: {}", if gen_png("280x280", MASK_FILTER, "assets/mask.png").await { "ok" } else { "FAILED" });
    }
    if !Path::new("assets/font.ttf").exists() {
        for src in ["C:/Windows/Fonts/arialbd.ttf", "C:/Windows/Fonts/arial.ttf", "C:/Windows/Fonts/segoeui.ttf"] {
            if std::fs::copy(src, "assets/font.ttf").is_ok() { break; }
        }
    }
    // Gallery thumbnails for the disc/reel picker.
    if !Path::new("assets/thumb_vinyl.png").exists() {
        let tint = "geq=r='clip(46*r(X,Y)/140,0,255)':g='clip(46*g(X,Y)/140,0,255)':b='clip(52*b(X,Y)/140,0,255)':a='alpha(X,Y)',scale=170:170";
        let _ = Command::new("ffmpeg").args(["-y", "-v", "error", "-i", "assets/disc_base.png",
            "-vf", tint, "-frames:v", "1", "-update", "1", "assets/thumb_vinyl.png"]).status().await;
    }
    if !Path::new("assets/thumb_reel.png").exists() {
        let _ = Command::new("ffmpeg").args(["-y", "-v", "error", "-i", "assets/reel_base.png",
            "-vf", "scale=170:170", "-frames:v", "1", "-update", "1", "assets/thumb_reel.png"]).status().await;
    }
}

// Serve a whitelisted gallery thumbnail from assets/.
async fn media(AxPath(name): AxPath<String>) -> Result<Response, (StatusCode, String)> {
    if !matches!(name.as_str(), "thumb_vinyl.png" | "thumb_reel.png") {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }
    let bytes = std::fs::read(format!("assets/{name}"))
        .map_err(|_| (StatusCode::NOT_FOUND, "not found".into()))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], bytes).into_response())
}

fn default_label_graph(date: &str, cr: u8, cg: u8, cb: u8) -> String {
    let c = 140.0_f64;
    let r = 98.0_f64;
    let arc = 210.0_f64.to_radians();
    let half = 22.0_f64;
    let chars: Vec<char> = date.chars().collect();
    let n = chars.len();
    let mut g = format!(
        "format=rgba,geq=r='{cr}':g='{cg}':b='{cb}':a='if(lte(hypot(X-140,Y-140),138),255,0)'[t0];",
    );
    for (i, ch) in chars.iter().enumerate() {
        let frac = if n > 1 { (i as f64 - (n as f64 - 1.0) / 2.0) / (n as f64 - 1.0) } else { 0.0 };
        let th = frac * arc;
        let ox = (c + r * th.sin() - half).round() as i64;
        let oy = (c - r * th.cos() - half).round() as i64;
        g.push_str(&format!(
            "color=c=#00000000:s=44x44,format=rgba,drawtext=fontfile=assets/font.ttf:text='{ch}':fontsize=26:fontcolor=white:x=(w-text_w)/2:y=(h-text_h)/2,rotate=a={th:.4}:c=none[g{n1}];[t{i}][g{n1}]overlay={ox}:{oy}[t{n1}];",
            n1 = i + 1
        ));
    }
    g.push_str(&format!("[t{n}]null[out]"));
    g
}

// Bake `text` curved along the reel's outer rim (radius ~R on the 842x842 reel),
// starting from the reel image itself ([0:v]). Each glyph is rotated tangentially
// and sits near the top of the rim; the whole ring rotates with the reel. A black
// outline keeps it legible over both the clear flange and the dark tape.
fn reel_text_graph(text: &str) -> String {
    let cx = 421.0_f64;
    let cy = 421.0_f64;
    let r = 372.0_f64; // on the rim, just inside the outer edge
    let step = 46.0 / r; // angular spacing per glyph (~arc length 46px)
    let half = 30.0_f64; // half of the 60px glyph box
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut g = String::from("[0:v]format=rgba[t0];");
    for (i, ch) in chars.iter().enumerate() {
        let th = (i as f64 - (n as f64 - 1.0) / 2.0) * step;
        let ox = (cx + r * th.sin() - half).round() as i64;
        let oy = (cy - r * th.cos() - half).round() as i64;
        g.push_str(&format!(
            "color=c=#00000000:s=60x60,format=rgba,drawtext=fontfile=assets/font.ttf:text='{ch}':fontsize=40:fontcolor=black:x=(w-text_w)/2:y=(h-text_h)/2,rotate=a={th:.4}:c=none[g{n1}];[t{i}][g{n1}]overlay={ox}:{oy}[t{n1}];",
            n1 = i + 1
        ));
    }
    g.push_str(&format!("[t{n}]null[out]"));
    g
}

// Short caption (e.g. the date): placed on the reel's right rib — rotated ~20°
// counter-clockwise (negative angle) and shifted right of the hub. Baked onto the
// reel via [0:v], so it rotates with it.
fn reel_center_text_graph(text: &str) -> String {
    let ang = -0.297_f64; // ~17° counter-clockwise
    let (dx, dy) = (238_i64, -83_i64); // on the right rib
    let ox = 421 + dx - 210;
    let oy = 421 + dy - 50;
    format!(
        "[0:v]format=rgba[base];\
color=c=#00000000:s=420x100,format=rgba,drawtext=fontfile=assets/font.ttf:text='{text}':\
fontsize=46:fontcolor=black:x=(w-text_w)/2:y=(h-text_h)/2,rotate={ang:.4}:c=none:ow=420:oh=100[t];\
[base][t]overlay={ox}:{oy}[out]"
    )
}

// Strip characters that would break an ffmpeg drawtext expression, and cap length.
fn safe_caption(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\'' | '\\' | ':' | '%' | '{' | '}' | '\n' | '\r'))
        .take(40)
        .collect::<String>()
        .trim()
        .to_string()
}

async fn audio_duration(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nk=1:nw=1"])
        .arg(path)
        .output().await;
    if let Ok(o) = out {
        if let Ok(s) = String::from_utf8(o.stdout) {
            if let Ok(d) = s.trim().parse::<f64>() { return d; }
        }
    }
    0.0
}

// Parse "out_time=HH:MM:SS.micro" -> seconds.
fn parse_out_time(line: &str) -> Option<f64> {
    let v = line.strip_prefix("out_time=")?;
    let mut it = v.trim().split(':');
    let h: f64 = it.next()?.parse().ok()?;
    let m: f64 = it.next()?.parse().ok()?;
    let s: f64 = it.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

// Assets are referenced by relative paths, so the working dir must contain them.
// When the .exe is launched directly (cwd != project root), walk up from the
// executable's location until we find assets/disc_base.png and chdir there.
fn locate_project_root() {
    if Path::new("assets/disc_base.png").exists() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            if d.join("assets/disc_base.png").exists() {
                let _ = std::env::set_current_dir(&d);
                return;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
}

#[tokio::main]
async fn main() {
    locate_project_root();
    ensure_assets().await;
    let jobs: Jobs = Arc::new(Mutex::new(HashMap::new()));
    let app = Router::new()
        .route("/", get(index))
        .route("/media/:name", get(media))
        .route("/render", post(render))
        .route("/progress/:id", get(progress))
        .route("/result/:id", get(result))
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
        .with_state(jobs);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await.expect("bind");
    println!("VinilCut running at http://localhost:{PORT}");
    axum::serve(listener, app).await.expect("serve");
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn render(State(jobs): State<Jobs>, mut mp: Multipart) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let id = format!("{}", now_ns());
    let dir = PathBuf::from("work").join(format!("r-{id}"));
    std::fs::create_dir_all(&dir).map_err(ise)?;
    let cover = dir.join("cover_input");
    let cover_png = dir.join("cover.png");
    let audio = dir.join("audio_input");
    let label = dir.join("label.png");
    let out = dir.join("out.mp4");

    let mut have_cover = false;
    let mut have_audio = false;
    let mut note = false;
    let mut disc_hex = String::from("#2e2e34");
    let mut label_hex = String::from("#000000");
    let mut disc_type = String::from("vinyl");
    let mut caption = String::new();
    while let Some(field) = mp.next_field().await.map_err(bad)? {
        let name = field.name().unwrap_or("").to_string();
        let data = field.bytes().await.map_err(bad)?;
        match name.as_str() {
            "image" if !data.is_empty() => { std::fs::write(&cover, &data).map_err(ise)?; have_cover = true; }
            "audio" if !data.is_empty() => { std::fs::write(&audio, &data).map_err(ise)?; have_audio = true; }
            "mode" => { note = data.as_ref() == b"note"; }
            "disc_color" if !data.is_empty() => { disc_hex = String::from_utf8_lossy(&data).into_owned(); }
            "label_color" if !data.is_empty() => { label_hex = String::from_utf8_lossy(&data).into_owned(); }
            "disc_type" if !data.is_empty() => { disc_type = String::from_utf8_lossy(&data).trim().to_string(); }
            "text" => { caption = String::from_utf8_lossy(&data).into_owned(); }
            _ => {}
        }
    }
    let (dr, dg, db) = parse_hex(&disc_hex);
    let (lr, lg, lb) = parse_hex(&label_hex);
    // The reel has no center label/cover — text is baked along its rim instead.
    let is_reel = disc_type == "reel";
    let use_cover = have_cover && !is_reel;
    // Caption: user text, or today's date when empty. Sanitized for drawtext.
    let caption = {
        let c = safe_caption(&caption);
        if c.is_empty() { chrono::Local::now().format("%d.%m.%Y").to_string() } else { c }
    };
    if !have_audio {
        let _ = std::fs::remove_dir_all(&dir);
        return Err((StatusCode::BAD_REQUEST, "An audio file is required.".into()));
    }

    // Normalize any cover format (png/jpg/webp/avif/heif/…) to a plain PNG first.
    if use_cover {
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-i", cover.to_str().unwrap(), "-frames:v", "1", cover_png.to_str().unwrap()])
            .status().await.map(|s| s.success()).unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return Err((StatusCode::BAD_REQUEST, "Could not read the cover image (unsupported format?).".into()));
        }
    }

    // Build the center label + pick the spinning base.
    //  - Reel: no center sticker; the date is baked curved along the outer rim, and
    //    a fully transparent placeholder label makes the centered overlay a no-op.
    //  - Vinyl: a 280 colored sticker (cover or date) with a same-color center hole,
    //    over the tinted disc.
    let label_col = format!("0x{:02x}{:02x}{:02x}", lr, lg, lb);
    let disc_png: PathBuf = if is_reel {
        if !Path::new("assets/reel_base.png").exists() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ise("assets/reel_base.png not found — start VinilCut from its project folder."));
        }
        let placeholder = Command::new("ffmpeg").args([
            "-y", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=16x16",
            "-vf", "format=rgba,geq=r=0:g=0:b=0:a=0", "-frames:v", "1", "-update", "1",
            label.to_str().unwrap(),
        ]).status().await.map(|s| s.success()).unwrap_or(false);
        let reel_text = dir.join("reel_text.png");
        // Short captions (<= 10 chars, e.g. a date) sit in the central part;
        // longer ones curve along the outer rim.
        let reel_graph = if caption.chars().count() <= 10 {
            reel_center_text_graph(&caption)
        } else {
            reel_text_graph(&caption)
        };
        let baked = Command::new("ffmpeg").args([
            "-y", "-v", "error", "-i", "assets/reel_base.png",
            "-filter_complex", &reel_graph,
            "-map", "[out]", "-frames:v", "1", "-update", "1", reel_text.to_str().unwrap(),
        ]).status().await.map(|s| s.success()).unwrap_or(false);
        if !placeholder || !baked {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ise("failed to build the reel."));
        }
        reel_text
    } else {
        let lbl_ok = if use_cover {
            Command::new("ffmpeg").args([
                "-y", "-v", "error",
                "-i", cover_png.to_str().unwrap(), "-i", "assets/mask.png",
                "-f", "lavfi", "-i", &format!("color=c={label_col}:s=280x280"),
                "-f", "lavfi", "-i", &format!("color=c={label_col}:s=24x24"),
                "-filter_complex",
                "[2:v]format=rgba[base];\
[0:v]scale=236:236:force_original_aspect_ratio=increase,crop=236:236[c];\
[base][c]overlay=(W-w)/2:(H-h)/2[m];\
[3:v]format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lte(hypot(X-12,Y-12),9),255,0)'[hole];\
[m][hole]overlay=(W-w)/2:(H-h)/2[mh];[mh][1:v]alphamerge[out]",
                "-map", "[out]", "-frames:v", "1", label.to_str().unwrap(),
            ]).status().await.map(|s| s.success()).unwrap_or(false)
        } else {
            Command::new("ffmpeg").args([
                "-y", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=280x280",
                "-filter_complex", &default_label_graph(&caption, lr, lg, lb),
                "-map", "[out]", "-frames:v", "1", label.to_str().unwrap(),
            ]).status().await.map(|s| s.success()).unwrap_or(false)
        };
        if !lbl_ok {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(ise("failed to build label"));
        }
        match ensure_disc(dr, dg, db).await {
            Ok(p) => p,
            Err(e) => { let _ = std::fs::remove_dir_all(&dir); return Err(ise(e)); }
        }
    };

    let duration = audio_duration(&audio).await.max(0.1);
    // Video notes are capped at 60s; progress % is against the effective length.
    let eff = if note { duration.min(60.0) } else { duration };

    let graph = match (note, use_cover) {
        (true, true) => GRAPH_NOTE_COVER,
        (true, false) => GRAPH_NOTE_SOLID,
        (false, true) => GRAPH_COVER,
        (false, false) => GRAPH_SOLID,
    };

    // Build the final render args, with -progress on stdout.
    let mut args: Vec<String> = vec![
        "-y".into(), "-v".into(), "error".into(), "-nostats".into(), "-progress".into(), "pipe:1".into(),
        "-loop".into(), "1".into(), "-i".into(), label.to_str().unwrap().into(),
        "-loop".into(), "1".into(), "-i".into(), disc_png.to_str().unwrap().into(),
        "-i".into(), audio.to_str().unwrap().into(),
    ];
    if use_cover {
        args.extend(["-loop".into(), "1".into(), "-i".into(), cover_png.to_str().unwrap().into()]);
    }
    args.extend(["-loop".into(), "1".into(), "-i".into(), "assets/shine.png".into()]);
    args.extend(["-filter_complex".into(), graph.into()]);
    args.extend(["-map".into(), "[outv]".into(), "-map".into(), "2:a".into(),
                 "-r".into(), "30".into(), "-shortest".into()]);
    if note {
        args.extend(["-t".into(), "60".into()]);
    }
    args.extend([
        "-c:v".into(), "libx264".into(), "-preset".into(), "veryfast".into(), "-crf".into(), "20".into(),
        "-pix_fmt".into(), "yuv420p".into(),
        "-c:a".into(), "aac".into(), "-b:a".into(), "192k".into(), "-movflags".into(), "+faststart".into(),
        out.to_str().unwrap().into(),
    ]);

    // Register the job.
    jobs.lock().unwrap().insert(id.clone(), Job {
        percent: 0.0, done: false, ok: false, error: String::new(),
        out: out.clone(), dir: dir.clone(),
    });

    // Spawn ffmpeg + a task that streams progress into the job.
    let jobs2 = jobs.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        let mut child = match Command::new("ffmpeg").args(&args)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => { finish(&jobs2, &id2, false, e.to_string()); return; }
        };
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let err_handle = tokio::spawn(async move {
            let mut s = String::new(); let _ = stderr.read_to_string(&mut s).await; s
        });
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(t) = parse_out_time(&line) {
                let pct = (t / eff * 100.0).clamp(0.0, 99.5);
                if let Some(j) = jobs2.lock().unwrap().get_mut(&id2) { j.percent = pct; }
            }
        }
        let status = child.wait().await;
        let ok = status.map(|s| s.success()).unwrap_or(false);
        let err = if ok { String::new() } else { err_handle.await.unwrap_or_default() };
        finish(&jobs2, &id2, ok, err);
    });

    Ok(Json(json!({ "id": id, "duration": duration })))
}

fn finish(jobs: &Jobs, id: &str, ok: bool, error: String) {
    if let Some(j) = jobs.lock().unwrap().get_mut(id) {
        j.done = true; j.ok = ok; j.error = error;
        if ok { j.percent = 100.0; }
    }
}

async fn progress(State(jobs): State<Jobs>, AxPath(id): AxPath<String>) -> Json<serde_json::Value> {
    let g = jobs.lock().unwrap();
    match g.get(&id) {
        Some(j) => Json(json!({ "percent": j.percent, "done": j.done, "ok": j.ok, "error": j.error })),
        None => Json(json!({ "percent": 0, "done": true, "ok": false, "error": "unknown job" })),
    }
}

async fn result(State(jobs): State<Jobs>, AxPath(id): AxPath<String>) -> Result<Response, (StatusCode, String)> {
    let (out, dir, ready, ok, error) = {
        let g = jobs.lock().unwrap();
        match g.get(&id) {
            Some(j) => (j.out.clone(), j.dir.clone(), j.done, j.ok, j.error.clone()),
            None => return Err((StatusCode::NOT_FOUND, "unknown job".into())),
        }
    };
    if !ready { return Err((StatusCode::ACCEPTED, "not ready".into())); }
    if !ok { return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("ffmpeg failed:\n{error}"))); }
    let bytes = std::fs::read(&out).map_err(ise)?;
    jobs.lock().unwrap().remove(&id);
    let _ = std::fs::remove_dir_all(&dir);
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"vinyl.mp4\""),
        ],
        bytes,
    ).into_response())
}

fn ise<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn bad<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>VinilCut</title>
<style>
  * { box-sizing: border-box; }
  body { font-family: system-ui, "Segoe UI", sans-serif; margin: 0; padding: 24px;
         background: #14161a; color: #eee; display: flex; flex-direction: column;
         align-items: center; min-height: 100vh; }
  h1 { margin: 0 0 4px; font-weight: 800; }
  .sub { color: #8a94a6; margin-bottom: 20px; }
  .card { background: #1c2028; border: 1px solid #2a2f3a; border-radius: 12px;
          padding: 20px; width: 100%; max-width: 460px; }
  label { display:block; font-size: 13px; color:#9aa4b2; margin: 12px 0 6px; }
  .hint { font-size: 12px; color:#6b7280; margin-top: 4px; }
  input[type=file], input[type=text] { width: 100%; padding: 10px; background:#0f1116;
          color:#ddd; border:1px solid #2a2f3a; border-radius:8px; font-size: 13px; }
  input[type=color] { width: 56px; height: 36px; padding: 2px; background:#0f1116;
          border:1px solid #2a2f3a; border-radius:8px; cursor: pointer; }
  .gallery { display:flex; gap:12px; }
  .tile { margin:0; width:auto; flex:1; background:#0f1116; border:2px solid #2a2f3a;
          border-radius:10px; padding:8px; cursor:pointer; color:#9aa4b2;
          display:flex; flex-direction:column; align-items:center; gap:6px;
          font-size:13px; font-weight:600; transition: border-color .15s, color .15s; }
  .tile img { width:100%; border-radius:8px; display:block; background:#000; }
  .tile.selected { border-color:#3275ac; color:#e6ebf2; }
  button { margin-top: 18px; width: 100%; padding: 12px; font-size: 15px; font-weight: 700;
           border: none; border-radius: 8px; background: #3275ac; color: #fff; cursor: pointer; }
  button:disabled { opacity: .5; cursor: not-allowed; }
  .bar { margin-top: 14px; height: 10px; background:#0f1116; border:1px solid #2a2f3a;
         border-radius: 6px; overflow: hidden; display: none; }
  .bar > div { height: 100%; width: 0%; background: #3275ac; transition: width .2s; }
  .status { margin-top: 10px; font-size: 13px; color: #9aa4b2; min-height: 18px; white-space: pre-wrap; }
  .status.err { color: #ff6b6b; }
  video { margin-top: 18px; width: 100%; max-width: 460px; border-radius: 12px; background:#000; display:none; }
  .dl { display:none; margin-top: 10px; }
  .dl a { color:#6cb6ff; }
</style></head>
<body>
  <h1>VinilCut</h1>
  <div class="sub">Cover + audio &rarr; spinning vinyl video (Telegram round note by default)</div>
  <div class="card">
    <label>Spinning object</label>
    <div class="gallery" id="gallery">
      <button type="button" class="tile selected" data-type="vinyl">
        <img src="/media/thumb_vinyl.png" alt="Vinyl record"><span>Vinyl</span>
      </button>
      <button type="button" class="tile" data-type="reel">
        <img src="/media/thumb_reel.png" alt="Tape reel"><span>Reel</span>
      </button>
    </div>
    <label>Cover image (optional)</label>
    <input type="file" id="image" accept="image/*">
    <div class="hint">Leave empty &rarr; a default label with today's date is used.</div>
    <label>Audio file</label>
    <input type="file" id="audio" accept="audio/*">
    <label>Caption (optional)</label>
    <input type="text" id="title" maxlength="40" placeholder="Empty = today's date">
    <div class="hint">&le;10 chars sit in the center; longer curves along the rim (reel) / label (vinyl).</div>
    <div id="colorRow" style="display:flex;gap:24px;margin-top:12px;">
      <div>
        <label style="margin:0 0 6px;">Vinyl color</label>
        <input type="color" id="discColor" value="#2e2e34">
      </div>
      <div>
        <label style="margin:0 0 6px;">Label color</label>
        <input type="color" id="labelColor" value="#000000">
      </div>
    </div>
    <div style="margin-top:14px;">
      <label style="display:inline-flex;align-items:center;gap:8px;margin:0;cursor:pointer;color:#cdd3dc;">
        <input type="checkbox" id="note" style="width:auto;" checked> Video note (round, &le;60s)
      </label>
      <div class="hint">On &rarr; square 512&times;512 round message (&le;60s). Off &rarr; full 848&times;848 video.</div>
    </div>
    <button id="go">Render vinyl</button>
    <div class="bar" id="bar"><div id="barfill"></div></div>
    <div class="status" id="status"></div>
  </div>
  <video id="preview" controls loop></video>
  <div class="dl" id="dl"><a id="dllink" download="vinyl.mp4">Download vinyl.mp4</a></div>

  <script>
    const $ = id => document.getElementById(id);
    const status = $("status"), go = $("go"), video = $("preview"), dl = $("dl"), dllink = $("dllink");
    const bar = $("bar"), barfill = $("barfill");
    function setStatus(m, err){ status.textContent = m || ""; status.className = "status" + (err ? " err" : ""); }
    const sleep = ms => new Promise(r => setTimeout(r, ms));

    let discType = "vinyl";
    document.querySelectorAll("#gallery .tile").forEach(t => {
      t.addEventListener("click", () => {
        document.querySelectorAll("#gallery .tile").forEach(x => x.classList.remove("selected"));
        t.classList.add("selected");
        discType = t.dataset.type;
        $("colorRow").style.display = (discType === "reel") ? "none" : "flex";
      });
    });

    go.addEventListener("click", async () => {
      const img = $("image").files[0], aud = $("audio").files[0];
      if (!aud) { setStatus("Choose an audio file.", true); return; }
      const fd = new FormData();
      if (img) fd.append("image", img);
      fd.append("audio", aud);
      if ($("note").checked) fd.append("mode", "note");
      fd.append("disc_type", discType);
      fd.append("text", $("title").value);
      fd.append("disc_color", $("discColor").value);
      fd.append("label_color", $("labelColor").value);
      go.disabled = true; video.style.display = "none"; dl.style.display = "none";
      bar.style.display = "block"; barfill.style.width = "0%";
      setStatus("Rendering… 0%");
      try {
        const res = await fetch("/render", { method: "POST", body: fd });
        if (!res.ok) throw new Error(await res.text());
        const { id } = await res.json();
        // poll progress
        while (true) {
          await sleep(300);
          const p = await (await fetch("/progress/" + id)).json();
          const pct = Math.round(p.percent || 0);
          barfill.style.width = pct + "%";
          setStatus("Rendering… " + pct + "%");
          if (p.done) {
            if (!p.ok) throw new Error(p.error || "render failed");
            break;
          }
        }
        barfill.style.width = "100%";
        setStatus("Fetching result…");
        const rr = await fetch("/result/" + id);
        if (!rr.ok) throw new Error(await rr.text());
        const blob = await rr.blob();
        const url = URL.createObjectURL(blob);
        video.src = url; video.style.display = "block"; video.play().catch(()=>{});
        dllink.href = url; dl.style.display = "block";
        setStatus("Done — preview below, or download and send to Telegram.");
      } catch (e) {
        setStatus("Error: " + e.message, true);
      } finally { go.disabled = false; bar.style.display = "none"; }
    });
  </script>
</body></html>"##;
