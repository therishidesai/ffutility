use ffutility::{encoders::FfmpegOptions, streams::{V4lH264Stream, V4lH264Config}};
use h264_parser::AnnexBParser;
use std::fs::File;
use std::io::{Read, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let v4l_config = V4lH264Config {
        output_width: 736,
        output_height: 414,
        bitrate: 300000, // bitrate
        video_dev: String::from("/dev/video0"),
    };

    let mut ffmpeg_opts = FfmpegOptions::new();
    ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

    let mut rx = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

    let mut parser = AnnexBParser::new();

    let mut frame_count = 0;
    let mut keyframe_count = 0;

    let mut emit_au = |au: h264_parser::AccessUnit| -> std::io::Result<()> {
        frame_count += 1;

        if au.is_keyframe() {
            keyframe_count += 1;
            eprintln!("Frame {}: KEYFRAME", frame_count);
        } else {
            eprintln!("Frame {}: Regular frame", frame_count);
        }

        if let Some(ref sps) = au.sps {
            let constraint_flags: u8 =
                ((sps.constraint_set0_flag as u8) << 7) |
            ((sps.constraint_set1_flag as u8) << 6) |
            ((sps.constraint_set2_flag as u8) << 5) |
            ((sps.constraint_set3_flag as u8) << 4) |
            ((sps.constraint_set4_flag as u8) << 3) |
            ((sps.constraint_set5_flag as u8) << 2);
            eprintln!("  Resolution: {}x{}", sps.width, sps.height);
            eprintln!("  Profile: {}, Level: {}", sps.profile_idc, sps.level_idc);
            eprintln!("  CF1: {}", sps.constraint_set1_flag);
            eprintln!("  CF2: {}", sps.constraint_set2_flag);
            eprintln!("  CF3: {}", sps.constraint_set3_flag);
            eprintln!("  CF4: {}", sps.constraint_set4_flag);
            eprintln!("  CF5: {}", sps.constraint_set5_flag);
            println!("   Constraint Flags:{:08b}", constraint_flags);
        }

        eprintln!("  NAL units in frame: {}", au.nals.len());
        for nal in au.nals() {
            eprintln!("    - {:?}", nal.nal_type);
        }

        Ok(())
    };

    while let Ok(au_opt) = parser.next_access_unit() {
        match au_opt {
            Some(au) => {
                emit_au(au)?;
            }
            None => {
                let buf = rx.recv()?;
                parser.push(&buf);
            }
        }
    }


    Ok(())
}
