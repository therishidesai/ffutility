# ffutility

ffutility (pronounced futility) is a grab bag of tools to work with video encoding in rust.

## EncoderConfig

`EncoderConfig` has structural fields for buffer allocation and codec setup:

- `input_width`, `input_height` — input frame dimensions
- `output_width`, `output_height` — encoded output dimensions
- `framerate` — frames per second
- `enc_type` — encoder selection (`EncoderType`)
- `input_type` — input pixel format (`InputType` / ffmpeg `Pixel`)

All encoder tuning is configured via `opts: Vec<(String, String)>`, passed directly as an ffmpeg dictionary to `avcodec_open2`. Any option valid for `ffmpeg -option value` works here.

Some options are mutually exclusive or only apply to specific encoders/modes. Invalid or inapplicable options are silently ignored by ffmpeg. Run `ffmpeg -h encoder=<name>` to see all options for a given encoder.

Full option references:
- [Generic codec options](https://ffmpeg.org/ffmpeg-codecs.html#Codec-Options)
- [libx264](https://ffmpeg.org/ffmpeg-codecs.html#libx264_002c-libx264rgb)
- [NVENC](https://docs.nvidia.com/video-technologies/video-codec-sdk/12.0/ffmpeg-with-nvidia-gpu/index.html)
- [SVT-AV1](https://gitlab.com/AOMediaCodec/SVT-AV1/-/blob/master/Docs/Parameters.md)

### Common

| Key | Description | Example |
|-----|-------------|---------|
| `b` | Bitrate in bits/sec. Ignored with `rc=constqp`. | `"7500000"` |
| `g` | GOP size in frames | `"16"` |
| `bf` | Max B-frames. `"0"` to disable. | `"2"` |
| `qp` | Constant QP. Lower = higher quality. | `"20"` |
| `preset` | Speed/quality tradeoff. Values are encoder-specific (see Presets). | `"p5"` |

#### Presets

| Encoder | Values |
|---------|--------|
| `libx264` / `libx265` | `ultrafast`, `superfast`, `veryfast`, `faster`, `fast`, `medium`, `slow`, `slower`, `veryslow` |
| `h264_nvenc` / `av1_nvenc` | `p1` through `p7` |
| `libsvtav1` | `0` (slowest) through `13` (fastest) |

### NVENC Specific

| Key | Description | Example |
|-----|-------------|---------|
| `rc` | Rate control: `constqp`, `vbr`, `cbr` | `"constqp"` |
| `tune` | `hq`, `ll`, `ull`, `lossless` | `"hq"` |
| `rc-lookahead` | Frames to look ahead for rate control | `"10"` |
| `b_ref_mode` | B-frames as references: `disabled`, `each`, `middle` | `"middle"` |
| `spatial-aq` | Spatial adaptive quantization. `"1"` to enable. | `"1"` |
| `temporal-aq` | Temporal adaptive quantization. `"1"` to enable. | `"1"` |
| `aq-strength` | AQ strength 1 (low) to 15 (aggressive). Only with `spatial-aq`. | `"8"` |
| `multipass` | `disabled`, `qres`, `fullres` | `"qres"` |
| `zerolatency` | No reordering delay. `"1"` to enable. | `"1"` |
| `forced-idr` | Force keyframes as IDR frames. `"1"` to enable. | `"1"` |

### H.264 Specific

| Key | Description | Example |
|-----|-------------|---------|
| `profile` | `baseline`, `main`, `high` | `"high"` |
| `tune` | `film`, `animation`, `grain`, `stillimage`, `zerolatency`, etc. (libx264 only) | `"zerolatency"` |
| `x264-params` | Raw params as `key=value:key=value` | `"repeat-headers=1"` |
| `crf` | Constant rate factor, 0-51 (libx264 only) | `"23"` |

### AV1 Specific

| Key | Description | Example |
|-----|-------------|---------|
| `tier` | `main` or `high` (av1_nvenc) | `"high"` |
| `highbitdepth` | 10-bit encode for 8-bit input (av1_nvenc). `"1"` to enable. | `"1"` |
| `crf` | Constant rate factor, 0-63 (libsvtav1 only) | `"30"` |
| `svtav1-params` | Raw params as `key=value:key=value` (libsvtav1 only) | `"film-grain=8"` |

### Example

```rust
let ec = EncoderConfig {
    input_width: 1920,
    input_height: 1080,
    output_width: 1920,
    output_height: 1080,
    enc_type: EncoderType::Av1Nvenc,
    input_type: InputType::BGR24,
    opts: vec![
        ("preset".into(), "p5".into()),
        ("rc".into(), "constqp".into()),
        ("qp".into(), "20".into()),
        ("g".into(), "16".into()),
        ("bf".into(), "2".into()),
        ("b_ref_mode".into(), "middle".into()),
        ("rc-lookahead".into(), "10".into()),
    ],
};

let encoder = VideoEncoder::new(ec).expect("failed to create encoder");
```
