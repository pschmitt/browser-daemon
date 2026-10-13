//! Live view as H.264 video: a session's own X display is captured with ffmpeg
//! (x11grab), unchanged frames are dropped (mpdecimate), and the stream is encoded
//! on the GPU (VAAPI) or with x264, tuned for latency (no B-frames, one access
//! unit at a time). Far cheaper than a JPEG screencast: on fnuc an animated page
//! went from ~147% of a core to ~13% (VAAPI) or ~67% (x264).

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::{Child, Command};

#[derive(Clone, Debug)]
pub enum Encoder {
    /// Hardware encoding through VAAPI on this render node.
    Vaapi(PathBuf),
    /// Software encoding.
    X264,
}

#[derive(Clone, Debug)]
pub struct VideoConfig {
    /// An ffmpeg with x11grab (ffmpeg-full).
    pub ffmpeg: PathBuf,
    pub encoder: Encoder,
}

/// Part of the display to capture, in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// Clamp into a `width`x`height` screen; encoders need even sizes.
    pub fn clamp(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width.saturating_sub(16));
        let y = self.y.min(height.saturating_sub(16));
        let w = self.w.min(width - x) & !1;
        let h = self.h.min(height - y) & !1;
        (w >= 16 && h >= 16).then_some(Self { x, y, w, h })
    }
}

pub fn encoder(cfg: &VideoConfig, display: &str, r: Region) -> std::io::Result<Child> {
    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-f", "x11grab", "-draw_mouse", "0", "-framerate", "30"])
        .args(["-video_size", &format!("{}x{}", r.w, r.h)])
        .arg("-i")
        .arg(format!("{display}+{},{}", r.x, r.y));
    // Drop frames that did not change, so a still page costs next to nothing, but
    // keep ~2 fps when still, so a client that lost a frame catches up.
    let decimate = "mpdecimate=hi=768:lo=320:frac=0.33:max=15";
    match cfg.encoder {
        Encoder::Vaapi(ref device) => {
            cmd.arg("-vaapi_device")
                .arg(device)
                // Converted on the GPU with BT.601 and tagged as such: left alone, the
                // stream says "RGB matrix, full range" and colours wash out. Primaries
                // and transfer (sRGB, as captured) are only tagged: given to
                // scale_vaapi, they make it convert the gamut as well.
                .args([
                    "-vf",
                    &format!(
                        "{decimate},hwupload,scale_vaapi=format=nv12:out_range=limited\
                         :out_color_matrix=bt470bg,\
                         setparams=color_primaries=bt709:color_trc=iec61966-2-1"
                    ),
                ])
                .args(["-fps_mode", "vfr"])
                .args(["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", "22"])
                // One frame in flight: the default depth held each frame ~270 ms.
                .args(["-async_depth", "1"])
                .args(["-bf", "0", "-g", "120", "-aud", "1"]);
        }
        Encoder::X264 => {
            cmd.args(["-vf", decimate, "-fps_mode", "vfr"])
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                ])
                .args(["-crf", "23", "-pix_fmt", "yuv420p", "-bf", "0", "-g", "120"])
                // swscale converts with BT.601; say so rather than leave it to guess.
                .args([
                    "-x264-params",
                    "aud=1:repeat-headers=1:range=tv:colormatrix=smpte170m\
                     :colorprim=bt709:transfer=iec61966-2-1",
                ]);
        }
    }
    cmd.args(["-f", "h264", "pipe:1"])
        .env("DISPLAY", display)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Splits an Annex B byte stream into access units at access unit delimiters
/// (NAL type 9), so each WebSocket message is one decodable frame.
#[derive(Default)]
pub struct AccessUnits {
    buf: Vec<u8>,
}

/// Start of each NAL unit (index of its header byte) in `data`.
fn nal_starts(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            out.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

impl AccessUnits {
    /// Feed bytes; returns complete access units with whether each is a keyframe.
    pub fn push(&mut self, data: &[u8]) -> Vec<(bool, Vec<u8>)> {
        self.buf.extend_from_slice(data);
        let starts = nal_starts(&self.buf);
        // AUD positions: the start code (3 or 4 bytes) before each type-9 NAL.
        let auds: Vec<usize> = starts
            .iter()
            .filter(|&&h| self.buf[h] & 0x1f == 9)
            .map(|&h| {
                if h >= 4 && self.buf[h - 4] == 0 {
                    h - 4
                } else {
                    h - 3
                }
            })
            .collect();
        let mut out = Vec::new();
        // Everything between two delimiters is complete; the tail waits for the next.
        for pair in auds.windows(2) {
            let au = &self.buf[pair[0]..pair[1]];
            let key = nal_starts(au).iter().any(|&h| au[h] & 0x1f == 5);
            out.push((key, au.to_vec()));
        }
        if let Some(&last) = auds.last() {
            self.buf.drain(..last);
        }
        out
    }

    /// The buffered access unit, once the encoder has gone quiet: ffmpeg writes
    /// whole packets, so the tail is complete. Without this, the last frame before
    /// a still period (or the only frame of a still page) would wait for the next.
    pub fn flush(&mut self) -> Option<(bool, Vec<u8>)> {
        let starts = nal_starts(&self.buf);
        if starts.is_empty() {
            return None;
        }
        let au = std::mem::take(&mut self.buf);
        let key = starts.iter().any(|&h| au[h] & 0x1f == 5);
        Some((key, au))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_delimiters_and_flags_keyframes() {
        let aud = [0u8, 0, 0, 1, 0x09, 0xf0];
        let idr = [0u8, 0, 0, 1, 0x65, 1, 2, 3];
        let p = [0u8, 0, 1, 0x41, 9, 9];
        let mut s = AccessUnits::default();
        let mut stream = Vec::new();
        stream.extend(aud);
        stream.extend(idr);
        stream.extend(aud);
        stream.extend(p);
        // Split mid-NAL to check buffering.
        let (a, b) = stream.split_at(9);
        assert!(s.push(a).is_empty());
        let out = s.push(b);
        assert_eq!(out.len(), 1);
        assert!(out[0].0);
        assert_eq!(out[0].1, [&aud[..], &idr[..]].concat());
        // The P frame completes with the next delimiter.
        let out = s.push(&aud);
        assert_eq!(out.len(), 1);
        assert!(!out[0].0);
        assert_eq!(out[0].1, [&aud[..], &p[..]].concat());
        // Or by flushing once the encoder is quiet.
        s.push(&idr);
        let (key, au) = s.flush().unwrap();
        assert!(key);
        assert_eq!(au, [&aud[..], &idr[..]].concat());
        assert!(s.flush().is_none());
    }

    #[test]
    fn regions_are_clamped_and_even() {
        let r = Region {
            x: 0,
            y: 87,
            w: 1601,
            h: 2000,
        }
        .clamp(1664, 1064)
        .unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0, 87, 1600, 976));
        assert!(Region {
            x: 1660,
            y: 0,
            w: 100,
            h: 100
        }
        .clamp(1664, 1064)
        .is_some());
    }
}
