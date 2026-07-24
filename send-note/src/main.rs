use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::media::Attribute;
use grammers_client::message::InputMessage;
use grammers_client::session::storages::SqliteSession;
use grammers_client::{Client, SenderPool, SignInError};

// Telegram video-note (round message) rules: square + ≤ 60 s.
const NOTE_SIZE: i32 = 512;
const MAX_SECS: f64 = 60.0;

fn prompt(m: &str) -> String {
    print!("{m}");
    io::stdout().flush().unwrap();
    let mut s = String::new();
    io::stdin().read_line(&mut s).unwrap();
    s.trim().to_string()
}

/// (width, height, duration_secs) of a video file, via ffprobe.
fn probe(path: &str) -> Result<(i32, i32, f64), Box<dyn std::error::Error>> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
            path,
        ])
        .output()
        .map_err(|_| "ffprobe not found — install FFmpeg and add it to PATH")?;
    if !out.status.success() {
        return Err(format!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let v: Vec<&str> = s.split_whitespace().collect();
    let w = v.first().ok_or("no width")?.parse()?;
    let h = v.get(1).ok_or("no height")?.parse()?;
    let dur = v.get(2).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    Ok((w, h, dur))
}

/// Re-encode any video into a valid note: center-cropped square, scaled to
/// NOTE_SIZE, capped at 60 s, h264/yuv420p + faststart. Returns (path, dur).
fn make_note(input: &str) -> Result<(PathBuf, f64), Box<dyn std::error::Error>> {
    let out = env::temp_dir().join("vinilcut_note.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-i", input,
            "-t", "60",
            "-vf", "crop='min(iw,ih)':'min(iw,ih)',scale=512:512,setsar=1",
            "-c:v", "libx264",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "128k",
            "-movflags", "+faststart",
        ])
        .arg(&out)
        .status()
        .map_err(|_| "ffmpeg not found — install FFmpeg and add it to PATH")?;
    if !status.success() {
        return Err("ffmpeg failed to build the note".into());
    }
    let (_, _, dur) = probe(out.to_str().ok_or("bad temp path")?)?;
    Ok((out, dur))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_id: i32 = env::var("TG_API_ID")
        .map_err(|_| "TG_API_ID is not set. Get it at https://my.telegram.org, then: setx TG_API_ID <id> (and open a NEW terminal).")?
        .trim()
        .parse()
        .map_err(|_| "TG_API_ID must be a number")?;
    let api_hash = env::var("TG_API_HASH")
        .map_err(|_| "TG_API_HASH is not set. Get it at https://my.telegram.org, then: setx TG_API_HASH <hash> (and open a NEW terminal).")?;
    let api_hash = api_hash.trim().to_string();
    let video = env::args().nth(1).unwrap_or_else(|| "vinyl.mp4".into());
    let target = env::args().nth(2).unwrap_or_else(|| "me".into());

    let session = Arc::new(SqliteSession::open("vinilcut.session").await?);
    let SenderPool { runner, updates: _, handle } = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(handle.clone());
    let pool_task = tokio::spawn(runner.run());

    if !client.is_authorized().await? {
        let phone = prompt("Phone (intl, e.g. +3538...): ");
        let token = client.request_login_code(&phone, &api_hash).await?;
        let code = prompt("Login code: ");
        match client.sign_in(&token, &code).await {
            Ok(_) => {}
            Err(SignInError::PasswordRequired(pt)) => {
                let pw = prompt("2FA password: ");
                client.check_password(pt, pw).await?;
            }
            Err(e) => return Err(e.into()),
        }
        println!("Signed in.");
    }

    // Make sure the file actually qualifies as a round message, otherwise
    // Telegram silently shows it as a normal video.
    let (w0, h0, dur0) = probe(&video)?;
    let (send_path, side, dur) = if w0 == h0 && dur0 <= MAX_SECS {
        println!("Already a valid note ({w0}x{h0}, {dur0:.1}s) — sending as is.");
        (PathBuf::from(&video), w0, dur0)
    } else {
        println!(
            "Source is {w0}x{h0}, {dur0:.1}s — normalizing to {NOTE_SIZE}x{NOTE_SIZE} square, ≤60s…"
        );
        let (p, d) = make_note(&video)?;
        (p, NOTE_SIZE, d)
    };

    let uploaded = client.upload_file(&send_path).await?;

    let peer = if target == "me" {
        client.get_me().await?.to_ref().await.ok_or("cannot ref self")?
    } else {
        client
            .resolve_username(target.trim_start_matches('@'))
            .await?
            .ok_or("username not found")?
            .to_ref()
            .await
            .ok_or("cannot ref target")?
    };

    let msg = InputMessage::new()
        .document(uploaded)
        .mime_type("video/mp4")
        .attribute(Attribute::Video {
            round_message: true,
            supports_streaming: false,
            duration: Duration::from_secs_f64(dur),
            w: side,
            h: side,
        });
    client.send_message(peer, msg).await?;
    println!("Sent '{video}' as a {side}x{side} video note ({dur:.1}s) to {target}.");

    handle.quit();
    let _ = pool_task.await;
    Ok(())
}
