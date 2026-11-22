// simple tool for DR

use clap::Parser;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP3, CODEC_TYPE_OPUS,
    CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_S16BE, CODEC_TYPE_PCM_S16LE, CODEC_TYPE_PCM_S24BE,
    CODEC_TYPE_PCM_S24LE, CODEC_TYPE_PCM_S32BE, CODEC_TYPE_PCM_S32LE, CODEC_TYPE_VORBIS,
    CodecType, DecoderOptions,
};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;

#[derive(Parser, Debug)]
#[command(author, version, about = "Dynamic Range generator (DR)")]
struct Args {
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct TrackAnalysis {
    dr: i32,
    dr_f64: f64,
    peak_db: f64,
    rms_db: f64,
    duration_secs: u64,
    track_display: String,
    artist: Option<String>,
    album: Option<String>,
    sample_rate: u32,
    channels: usize,
    bits_per_sample: Option<u32>,
    codec: String,
}

fn main() {
    let args = Args::parse();
    let files = collect_audio_files(&args.path);

    if files.is_empty() {
        eprintln!("Audio files not found at this path: {:?}", args.path);
        std::process::exit(1);
    }

    let results: Vec<TrackAnalysis> = files
        .par_iter()
        .filter_map(|path| match analyze_file(path) {
            Ok(res) => Some(res),
            Err(e) => {
                eprintln!("Processing error {:?}: {}", path.file_name().unwrap_or_default(), e);
                None
            }
        })
        .collect();

    if results.is_empty() {
        eprintln!("None of the audio files could be analyzed");
        std::process::exit(1);
    }

    print_report(&results, &args.path);
}

fn collect_audio_files(path: &Path) -> Vec<PathBuf> {
    let valid_exts = ["flac", "wav", "mp3", "ogg", "m4a", "aac", "opus", "wv", "aiff"];
    if path.is_file() {
        return vec![path.to_path_buf()];
    }

    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                    if valid_exts.contains(&ext.to_lowercase().as_str()) {
                        files.push(p);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

fn codec_name(codec: CodecType) -> String {
    match codec {
        CODEC_TYPE_FLAC => "FLAC".to_string(),
        CODEC_TYPE_MP3 => "MP3".to_string(),
        CODEC_TYPE_VORBIS => "Vorbis".to_string(),
        CODEC_TYPE_AAC => "AAC".to_string(),
        CODEC_TYPE_ALAC => "ALAC".to_string(),
        CODEC_TYPE_OPUS => "Opus".to_string(),
        CODEC_TYPE_PCM_S16LE | CODEC_TYPE_PCM_S24LE | CODEC_TYPE_PCM_S32LE
        | CODEC_TYPE_PCM_S16BE | CODEC_TYPE_PCM_S24BE | CODEC_TYPE_PCM_S32BE
        | CODEC_TYPE_PCM_F32LE => "PCM".to_string(),
        _ => "Unknown".to_string(),
    }
}

fn analyze_file(path: &Path) -> Result<TrackAnalysis, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| e.to_string())?;

    let mut artist = None;
    let mut album = None;
    let mut title = None;
    let mut track_num = None;

    let mut extract_tags = |rev: &symphonia::core::meta::MetadataRevision| {
        for tag in rev.tags() {
            match tag.std_key {
                Some(StandardTagKey::Artist) | Some(StandardTagKey::AlbumArtist) => {
                    if artist.is_none() {
                        artist = Some(tag.value.to_string());
                    }
                }
                Some(StandardTagKey::Album) => album = Some(tag.value.to_string()),
                Some(StandardTagKey::TrackTitle) => title = Some(tag.value.to_string()),
                Some(StandardTagKey::TrackNumber) => track_num = Some(tag.value.to_string()),
                _ => {}
            }
        }
    };

    if let Some( mut meta) = probed.metadata.get() {
        if let Some(rev) = meta.skip_to_latest() {
            extract_tags(rev);
        }
    }
    if let Some(rev) = probed.format.metadata().skip_to_latest() {
        extract_tags(rev);
    }

    let track = probed
        .format
        .default_track()
        .ok_or("Audio track not found")?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();

    let sample_rate = codec_params.sample_rate.ok_or("Unknown sampling rate")?;
    let channels = codec_params.channels.map(|c| c.count()).unwrap_or(2);
    let bits_per_sample = codec_params.bits_per_sample;
    let codec_str = codec_name(codec_params.codec);

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;

    let block_size = (sample_rate as f64 * 3.0053).floor() as usize;

    let mut ch_sum_sq = vec![0.0f64; channels];
    let mut ch_peak = vec![0.0f64; channels];
    let mut current_frames = 0usize;
    let mut total_frames = 0u64;

    let mut blocks_rms: Vec<Vec<f64>> = vec![Vec::new(); channels];
    let mut blocks_peak: Vec<Vec<f64>> = vec![Vec::new(); channels];

    let mut sample_buf = None;

    loop {
        let packet = match probed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(ref err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(_) => break,
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                if sample_buf.is_none() {
                    let spec = *audio_buf.spec();
                    let duration = audio_buf.capacity() as u64;
                    sample_buf = Some(SampleBuffer::<f64>::new(duration, spec));
                }

                if let Some(buf) = &mut sample_buf {
                    buf.copy_interleaved_ref(audio_buf);
                    let samples = buf.samples();
                    let frames = samples.len() / channels;

                    for f in 0..frames {
                        for c in 0..channels {
                            let s = samples[f * channels + c];
                            let abs_s = s.abs();
                            ch_sum_sq[c] += s * s;
                            if abs_s > ch_peak[c] {
                                ch_peak[c] = abs_s;
                            }
                        }
                        current_frames += 1;
                        total_frames += 1;

                        if current_frames == block_size {
                            for c in 0..channels {
                                let rms = ((2.0 * ch_sum_sq[c]) / (block_size as f64)).sqrt();
                                blocks_rms[c].push(rms);
                                blocks_peak[c].push(ch_peak[c]);
                                ch_sum_sq[c] = 0.0;
                                ch_peak[c] = 0.0;
                            }
                            current_frames = 0;
                        }
                    }
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if blocks_rms[0].is_empty() && current_frames > 0 {
        for c in 0..channels {
            let rms = ((2.0 * ch_sum_sq[c]) / (current_frames as f64)).sqrt();
            blocks_rms[c].push(rms);
            blocks_peak[c].push(ch_peak[c]);
        }
    }

    let mut ch_dr = Vec::with_capacity(channels);
    let mut overall_peak = 0.0f64;
    let mut overall_rms_sq_sum = 0.0f64;
    let total_blocks = blocks_rms[0].len();

    for c in 0..channels {
        for &p in &blocks_peak[c] {
            if p > overall_peak {
                overall_peak = p;
            }
        }
        let ch_energy: f64 = blocks_rms[c].iter().map(|r| r * r).sum();
        overall_rms_sq_sum += ch_energy / (total_blocks.max(1) as f64);

        // Расчёт TT DR для канала
        let mut sorted_rms = blocks_rms[c].clone();
        sorted_rms.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));

        let n_top = ((total_blocks as f64 * 0.20).floor() as usize).max(1);
        let top_rms_sq_sum: f64 = sorted_rms.iter().take(n_top).map(|r| r * r).sum();
        let dr_rms = (top_rms_sq_sum / (n_top as f64)).sqrt();

        let mut sorted_peaks = blocks_peak[c].clone();
        sorted_peaks.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
        let dr_peak = if sorted_peaks.len() >= 2 {
            sorted_peaks[1]
        } else {
            sorted_peaks.first().copied().unwrap_or(0.0)
        };

        if dr_rms > 0.0 && dr_peak > 0.0 {
            ch_dr.push(20.0 * (dr_peak / dr_rms).log10());
        } else {
            ch_dr.push(0.0);
        }
    }

    let mean_dr = ch_dr.iter().sum::<f64>() / (channels as f64);
    let dr_rounded = mean_dr.round() as i32;

    let peak_db = if overall_peak > 0.0 {
        20.0 * overall_peak.log10()
    } else {
        -100.0
    };

    let total_rms = (overall_rms_sq_sum / (channels as f64)).sqrt();
    let rms_db = if total_rms > 0.0 {
        20.0 * total_rms.log10()
    } else {
        -100.0
    };

    let duration_secs = (total_frames as f64 / sample_rate as f64).round() as u64;

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown Track")
        .to_string();

    let track_display = match (track_num, title) {
        (Some(num), Some(t)) => {
            let clean_num = num.split('/').next().unwrap_or(&num).trim();
            if let Ok(n) = clean_num.parse::<u32>() {
                format!("{:02}-{}", n, t)
            } else {
                format!("{}-{}", clean_num, t)
            }
        }
        (None, Some(t)) => {
            if file_stem.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                file_stem
            } else {
                t
            }
        }
        _ => file_stem,
    };

    Ok(TrackAnalysis {
        dr: dr_rounded,
        dr_f64: mean_dr,
        peak_db,
        rms_db,
        duration_secs,
        track_display,
        artist,
        album,
        sample_rate,
        channels,
        bits_per_sample,
        codec: codec_str,
    })
}

fn format_db(val: f64) -> String {
    let clean_val = if val.abs() < 0.005 { 0.0 } else { val };
    format!("{:.2} dB", clean_val)
}

fn print_report(tracks: &[TrackAnalysis], root_path: &Path) {
    let first = &tracks[0];

    let artist = first.artist.clone().unwrap_or_else(|| "Unknown Artist".to_string());
    let album = first.album.clone().unwrap_or_else(|| {
        root_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown Album")
            .to_string()
    });

    println!("--------------------------------------------------------------------------------");
    println!("Analyzed: {} / {}", artist, album);
    println!("--------------------------------------------------------------------------------\n");
    println!("DR Peak RMS Duration Track");
    println!("--------------------------------------------------------------------------------");

    for t in tracks {
        let mins = t.duration_secs / 60;
        let secs = t.duration_secs % 60;
        println!(
            "DR{} {} {} {}:{:02} {}",
            t.dr,
            format_db(t.peak_db),
            format_db(t.rms_db),
            mins,
            secs,
            t.track_display
        );
    }

    let avg_dr = (tracks.iter().map(|t| t.dr_f64).sum::<f64>() / tracks.len() as f64).round() as i32;
    let bps = first
        .bits_per_sample
        .map(|b| b.to_string())
        .unwrap_or_else(|| "16".to_string());

    println!("--------------------------------------------------------------------------------\n");
    println!("Number of tracks: {}", tracks.len());
    println!("Official DR value: DR{}\n", avg_dr);
    println!("Samplerate: {} Hz", first.sample_rate);
    println!("Channels: {}", first.channels);
    println!("Bits per sample: {}", bps);
    println!("Codec: {}", first.codec);
    println!("================================================================================");
}
