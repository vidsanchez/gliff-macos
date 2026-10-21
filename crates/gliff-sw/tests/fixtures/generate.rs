// Offline, one-time fixture generator. Not a workspace target or shipped codec.
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, Profile, QpRange,
    RateControlMode, UsageType, VuiConfig};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
fn main() {
    let output = std::env::args().nth(1).expect("fixture output directory");
    for (w, h) in [(64, 48), (80, 64), (34, 18), (16, 16), (128,96)] {
        let config = EncoderConfig::new().debug(false).num_threads(1)
            .max_slice_len(512).usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate).qp(QpRange::new(12, 35))
            .bitrate(BitRate::from_bps(1_000_000)).max_frame_rate(FrameRate::from_hz(60.0))
            .skip_frames(false).scene_change_detect(false).profile(Profile::Baseline).vui(VuiConfig::bt709());
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
        for frame in 0..3 {
            // Owned red image; the center quarter changes blue then green.
            // Keep most pixels unchanged so ScreenContentRealTime emits P frames.
            let colors = [(63, 102, 240), (32, 240, 118), (173, 42, 26)];
            let mut pixels = vec![0; w * h * 3 / 2];
            for row in 0..h { for col in 0..w {
                let patch = col >= ((w/4)&!1) && col < ((3*w/4)&!1)
                    && row >= ((h/4)&!1) && row < ((3*h/4)&!1);
                let color = if w == 128 { ((row/8 + col/8 + frame) % 3) as usize } else if patch { frame } else { 0 };
                let (y,u,v) = colors[color];
                pixels[row*w+col] = y;
                pixels[w*h+(row/2)*(w/2)+col/2] = u;
                pixels[w*h*5/4+(row/2)*(w/2)+col/2] = v;
            }}
            let picture = YUVBuffer::from_vec(pixels, w, h);
            let bytes = encoder.encode(&picture).unwrap().to_vec();
            std::fs::write(format!("{output}/{w}x{h}-{frame}.h264"), bytes).unwrap();
        }
    }
}
