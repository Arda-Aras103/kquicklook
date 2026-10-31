use crate::preview::PreviewContent;
use eframe::egui;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

const VIDEO_FPS: u32 = 24;
const MAX_VIDEO_W: u32 = 854;
const MAX_VIDEO_H: u32 = 480;

pub fn load_media_preview(path: &Path, is_video: bool) -> PreviewContent {
    let kind_title = if is_video {
        "Video Preview".to_string()
    } else {
        "Audio File Preview".to_string()
    };

    let (mut metadata_pairs, duration_secs, video_dims) = extract_ffprobe_metadata(path);

    let thumbnail = if is_video {
        extract_video_frame(path)
    } else {
        extract_audio_cover_or_waveform(path)
    };

    if metadata_pairs.is_empty() {
        let hint = "Install 'ffmpeg-free' for media playback, metadata, and thumbnail extraction.";
        metadata_pairs.push(("Info".to_string(), hint.to_string()));
    }

    PreviewContent::Media {
        kind_title,
        is_video,
        duration_secs,
        video_dims,
        thumbnail,
        metadata_pairs,
    }
}

/// Real-time streaming video + audio player backed by `ffmpeg` pipes.
/// Keeps only the current frame in memory and cleans up child processes on Drop.
pub struct VideoPlayer {
    path: PathBuf,
    ctx: egui::Context,
    pub is_video: bool,
    pub width: usize,
    pub height: usize,
    pub duration: f64,
    pub position: f64,
    pub paused: bool,
    pub muted: bool,
    pub finished: bool,
    frame_rx: Option<Receiver<(Vec<u8>, f64)>>,
    stop_flag: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Child>>>,
}

impl VideoPlayer {
    pub fn new(
        path: PathBuf,
        is_video: bool,
        duration: f64,
        orig_dims: Option<(u32, u32)>,
        ctx: egui::Context,
    ) -> Self {
        let (ow, oh) = orig_dims.unwrap_or((640, 360));
        let (w, h) = fit_even_dims(ow.max(2), oh.max(2), MAX_VIDEO_W, MAX_VIDEO_H);

        let mut player = Self {
            path,
            ctx,
            is_video,
            width: w as usize,
            height: h as usize,
            duration: duration.max(0.0),
            position: 0.0,
            paused: false,
            muted: false,
            finished: false,
            frame_rx: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            children: Arc::new(Mutex::new(Vec::new())),
        };
        player.start_stream(0.0);
        player
    }

    pub fn poll_frame(&mut self) -> Option<Vec<u8>> {
        let rx = self.frame_rx.as_ref()?;
        let mut latest_frame = None;
        while let Ok((rgba, pos)) = rx.try_recv() {
            if pos < 0.0 {
                self.finished = true;
                self.paused = true;
            } else {
                self.position = pos;
                latest_frame = Some(rgba);
            }
        }
        latest_frame
    }

    pub fn toggle_pause(&mut self) {
        if self.paused {
            if self.finished || (self.duration > 0.0 && self.position >= self.duration - 0.3) {
                self.position = 0.0;
            }
            self.paused = false;
            self.finished = false;
            self.start_stream(self.position);
        } else {
            self.paused = true;
            self.stop_stream();
        }
    }

    pub fn seek(&mut self, target_secs: f64) {
        let clamped = if self.duration > 0.0 {
            target_secs.clamp(0.0, self.duration)
        } else {
            target_secs.max(0.0)
        };
        self.position = clamped;
        self.finished = false;
        if !self.paused {
            self.start_stream(clamped);
        }
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if !self.paused {
            self.start_stream(self.position);
        }
    }

    pub fn stop_stream(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.frame_rx = None;
        if let Ok(mut list) = self.children.lock() {
            for mut child in list.drain(..) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn start_stream(&mut self, start_sec: f64) {
        self.stop_stream();

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = stop_flag.clone();

        let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, f64)>(2);
        self.frame_rx = Some(rx);

        let seek_str = format!("{start_sec:.2}");

        if !self.is_video {
            if !self.muted {
                if let Ok(mut list) = self.children.lock() {
                    if let Some(aud) = spawn_audio_process(&self.path, &seek_str) {
                        list.extend(aud);
                    }
                }
            }
            let ctx = self.ctx.clone();
            let dur = self.duration;
            let start_instant = std::time::Instant::now();
            thread::spawn(move || {
                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(std::time::Duration::from_millis(120));
                    let pos = start_sec + start_instant.elapsed().as_secs_f64();
                    if dur > 0.0 && pos >= dur {
                        let _ = tx.send((Vec::new(), -1.0));
                        ctx.request_repaint();
                        break;
                    }
                    if tx.send((Vec::new(), pos)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
            return;
        }

        let w = self.width;
        let h = self.height;

        let vid_child = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "quiet",
                "-hwaccel",
                "auto",
                "-threads",
                "2",
                "-ss",
                &seek_str,
                "-re",
                "-i",
            ])
            .arg(&self.path)
            .args([
                "-an",
                "-vf",
                &format!("scale={w}:{h}:flags=fast_bilinear"),
                "-r",
                &VIDEO_FPS.to_string(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        let Ok(mut vid_proc) = vid_child else {
            return;
        };
        let Some(mut stdout) = vid_proc.stdout.take() else {
            let _ = vid_proc.kill();
            return;
        };

        if let Ok(mut list) = self.children.lock() {
            list.push(vid_proc);
            if !self.muted {
                if let Some(aud) = spawn_audio_process(&self.path, &seek_str) {
                    list.extend(aud);
                }
            }
        }

        let ctx = self.ctx.clone();
        let frame_bytes = w * h * 4;
        thread::spawn(move || {
            let mut frame_idx = 0u64;
            loop {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let mut buf = vec![0u8; frame_bytes];
                if stdout.read_exact(&mut buf).is_err() {
                    let _ = tx.send((Vec::new(), -1.0));
                    ctx.request_repaint();
                    break;
                }
                frame_idx += 1;
                let pos = start_sec + (frame_idx as f64 / VIDEO_FPS as f64);
                if tx.send((buf, pos)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.stop_stream();
    }
}

fn spawn_audio_process(path: &Path, seek_str: &str) -> Option<Vec<Child>> {
    // 1. Try ffplay -nodisp first
    if let Ok(child) = Command::new("ffplay")
        .args([
            "-nodisp",
            "-autoexit",
            "-loglevel",
            "quiet",
            "-vn",
            "-ss",
            seek_str,
            "-i",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        return Some(vec![child]);
    }

    // 2. Fallback: ffmpeg PCM piped to pw-play (PipeWire) or paplay (PulseAudio)
    let mut ff = Command::new("ffmpeg")
        .args(["-loglevel", "quiet", "-ss", seek_str, "-i"])
        .arg(path)
        .args(["-vn", "-f", "s16le", "-ar", "48000", "-ac", "2", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let ff_out = ff.stdout.take()?;

    let player = Command::new("pw-play")
        .args(["--rate", "48000", "--channels", "2", "--format", "s16", "-"])
        .stdin(Stdio::from(ff_out))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();

    if let Some(p) = player {
        return Some(vec![ff, p]);
    }

    let _ = ff.kill();
    let _ = ff.wait();
    None
}

fn fit_even_dims(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f32 / w as f32)
        .min(max_h as f32 / h as f32)
        .min(1.0);
    let mut nw = ((w as f32 * scale) as u32).max(2);
    let mut nh = ((h as f32 * scale) as u32).max(2);
    nw &= !1;
    nh &= !1;
    (nw.max(2), nh.max(2))
}

pub fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let hrs = total / 3600;
    let mins = (total % 3600) / 60;
    let rem = total % 60;
    if hrs > 0 {
        format!("{hrs:02}:{mins:02}:{rem:02}")
    } else {
        format!("{mins:02}:{rem:02}")
    }
}

fn extract_video_frame(path: &Path) -> Option<(Vec<u8>, usize, usize)> {
    let out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-ss", "00:00:01", "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale='min(720,iw)':-1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ])
        .output()
        .ok()?;

    if out.status.success() && !out.stdout.is_empty() {
        if let Ok(img) = image::load_from_memory(&out.stdout) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            return Some((rgba.into_raw(), w as usize, h as usize));
        }
    }
    None
}

fn extract_audio_cover_or_waveform(path: &Path) -> Option<(Vec<u8>, usize, usize)> {
    let cover_out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-an",
            "-vcodec",
            "png",
            "-vf",
            "scale='min(400,iw)':-1",
            "-f",
            "image2pipe",
            "-",
        ])
        .output()
        .ok();

    if let Some(out) = cover_out {
        if out.status.success() && !out.stdout.is_empty() {
            if let Ok(img) = image::load_from_memory(&out.stdout) {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                return Some((rgba.into_raw(), w as usize, h as usize));
            }
        }
    }

    let wave_out = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-t", "60", "-i"])
        .arg(path)
        .args([
            "-filter_complex",
            "showwavespic=s=560x140:colors=#3daee9",
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ])
        .output()
        .ok()?;

    if wave_out.status.success() && !wave_out.stdout.is_empty() {
        if let Ok(img) = image::load_from_memory(&wave_out.stdout) {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            return Some((rgba.into_raw(), w as usize, h as usize));
        }
    }

    None
}

fn extract_ffprobe_metadata(
    path: &Path,
) -> (Vec<(String, String)>, f64, Option<(u32, u32)>) {
    let Ok(out) = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
    else {
        return (Vec::new(), 0.0, None);
    };

    if !out.status.success() {
        return (Vec::new(), 0.0, None);
    }

    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return (Vec::new(), 0.0, None);
    };

    let mut pairs = Vec::new();
    let mut duration_secs = 0.0;
    let mut video_dims = None;

    if let Some(format) = json.get("format") {
        if let Some(dur_str) = format.get("duration").and_then(|v| v.as_str()) {
            if let Ok(secs) = dur_str.parse::<f64>() {
                duration_secs = secs;
                pairs.push(("Duration".to_string(), format_duration(secs)));
            }
        }
        if let Some(br_str) = format.get("bit_rate").and_then(|v| v.as_str()) {
            if let Ok(br) = br_str.parse::<u64>() {
                pairs.push(("Bitrate".to_string(), format!("{} kbps", br / 1000)));
            }
        }
        if let Some(tags) = format.get("tags").and_then(|v| v.as_object()) {
            for (tag_key, label) in [
                ("title", "Title"),
                ("artist", "Artist"),
                ("album", "Album"),
            ] {
                if let Some(val) = tags.get(tag_key).and_then(|v| v.as_str()) {
                    pairs.push((label.to_string(), val.to_string()));
                }
            }
        }
    }

    if let Some(streams) = json.get("streams").and_then(|v| v.as_array()) {
        for stream in streams {
            let codec_type = stream
                .get("codec_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let codec_name = stream
                .get("codec_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            if codec_type == "video" {
                let w = stream.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
                let h = stream.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                if w > 0 && h > 0 {
                    if video_dims.is_none() {
                        video_dims = Some((w as u32, h as u32));
                    }
                    pairs.push((
                        "Video Stream".to_string(),
                        format!("{} ({}×{})", codec_name.to_uppercase(), w, h),
                    ));
                }
            } else if codec_type == "audio" {
                let sr = stream
                    .get("sample_rate")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                pairs.push((
                    "Audio Stream".to_string(),
                    format!("{} ({} Hz)", codec_name.to_uppercase(), sr),
                ));
            }
        }
    }

    (pairs, duration_secs, video_dims)
}
