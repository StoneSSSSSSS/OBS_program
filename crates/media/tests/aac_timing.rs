use cv_media::{
    audioread::{TrackPcm, OUT_RATE},
    mf::aac::{AacDecoder, AacEncoder, FRAME},
    mp4::{flat::FlatWriter, frag::{FragWriter, TrackRun}, reader::Mp4Reader, Codec, SampleRef, TrackSpec},
};

fn encoded(rate: u32, channels: u16) -> anyhow::Result<(TrackSpec, Vec<Vec<u8>>)> {
    let mut encoder = AacEncoder::new(rate, channels, 96)?;
    let pcm: Vec<f32> = (0..12 * FRAME).flat_map(|frame| {
        let t = frame as f32 / rate as f32;
        (0..channels).map(move |channel| {
            let phase = (340.0 + 411.0 * channel as f32) * t + 790.0 * t * t;
            (std::f32::consts::TAU * phase).sin() * (0.35 - 0.12 * channel as f32)
        })
    }).collect();
    let mut packets = encoder.encode(&pcm)?;
    packets.extend(encoder.finish()?);
    let track = TrackSpec {
        id: 1,
        timescale: rate,
        codec: Codec::Aac { asc: encoder.asc, sample_rate: rate, channels, avg_bitrate: 96_000 },
        width: 0,
        height: 0,
        name: "Test audio".into(),
        enabled: true,
        default_duration: FRAME as u32,
    };
    Ok((track, packets))
}

fn packet(data: &[u8], duration: usize) -> SampleRef<'_> {
    SampleRef { data, duration: duration as u32, sync: true, cts: 0 }
}

// Compare to PCM in codec output order, with no MP4 timestamps supplied to
// the direct decoder. Feed one extra packet to release its last buffered PCM.
fn decode_reference(track: &TrackSpec, packets: &[Vec<u8>]) -> anyhow::Result<Vec<f32>> {
    let Codec::Aac { asc, .. } = &track.codec else { unreachable!() };
    let mut decoder = AacDecoder::new(asc)?;
    let mut pcm = Vec::new();
    for data in packets {
        pcm.extend(decoder.decode(data)?);
    }
    Ok(pcm)
}

fn assert_close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    let max_error = actual.iter().zip(expected).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
    assert!(max_error < 0.0001, "maximum PCM error {max_error}");
}

#[test]
fn stereo_fragmented_audio_keeps_its_samples_across_gaps_and_seeks() -> anyhow::Result<()> {
    let (track, packets) = encoded(OUT_RATE, 2)?;
    let mut unused = track.clone();
    unused.id = 2;
    let packet_count = 5;
    let final_duration = FRAME - 317;
    let mut samples: Vec<_> = packets[..packet_count].iter().map(|data| packet(data, FRAME)).collect();
    samples[packet_count - 1].duration = final_duration as u32;
    let reference = decode_reference(&track, &packets[..=packet_count])?;
    let segment_end = (packet_count - 1) * FRAME + final_duration;
    let expected = &reference[..segment_end * 2];
    assert!(expected[expected.len() - 512..].iter().any(|s| s.abs() > 0.1));
    assert!(expected.chunks_exact(2).any(|frame| (frame[0] - frame[1]).abs() > 0.1));

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("sparse.mp4");
    let second = 5 * OUT_RATE as usize + 237;
    let mut writer = FragWriter::create(&path, &[track, unused], 0, None)?;
    for start in [0, second] {
        writer.write_fragment(&[TrackRun { track_id: 1, base_time: start as u64, samples: &samples }])?;
    }
    writer.finish()?;

    let reader = Mp4Reader::open(&path)?;
    let mut pcm = TrackPcm::new(&reader, reader.audio()[0])?;
    let mut actual = vec![0.0; 2 * (second + segment_end + 257)];
    let mut pos = 0;
    for block in actual.chunks_mut(2 * 777) {
        pcm.read(&reader, pos, block)?;
        pos += (block.len() / 2) as i64;
    }
    assert_close(&actual[..segment_end * 2], expected);
    assert!(actual[segment_end * 2..second * 2].iter().all(|v| *v == 0.0), "AAC padding spilled into the gap");
    assert_close(&actual[second * 2..(second + segment_end) * 2], expected);
    assert!(actual[(second + segment_end) * 2..].iter().all(|v| *v == 0.0));

    let inside = 3 * FRAME + 117;
    let off = (second + inside) as i64;
    let mut window = vec![0.0; (segment_end - inside + 257) * 2];
    pcm.seek(&reader, off)?;
    pcm.read(&reader, off, &mut window)?;
    assert_close(&window, &actual[off as usize * 2..]);

    let off = (2 * FRAME + 33) as i64;
    let expected = &expected[off as usize * 2..];
    pcm.seek(&reader, off)?;
    let mut window = vec![0.0; expected.len()];
    pcm.read(&reader, off, &mut window)?;
    assert_close(&window, expected);

    let unused = reader.track_index(2).unwrap();
    let mut pcm = TrackPcm::new(&reader, unused)?;
    pcm.read(&reader, second as i64, &mut window)?;
    assert!(window.iter().all(|s| *s == 0.0));

    let waveform = cv_media::wave::generate_archive(&path, |_| Ok(()))?;
    assert!(waveform.get(0, 0) > 0);
    assert_eq!(waveform.get(20, 0), 0);
    assert_eq!(waveform.get(50, 0), waveform.get(0, 0));
    assert!(waveform.peaks.iter().skip(1).step_by(2).all(|peak| *peak == 0));
    Ok(())
}

#[test]
fn mono_44100_keeps_edit_preroll_and_the_final_partial_packet() -> anyhow::Result<()> {
    let source_rate = 44_100;
    let (track, packets) = encoded(source_rate, 1)?;
    let reference = decode_reference(&track, &packets[..=4])?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mono.m4a");
    let mut writer = FlatWriter::new(&path, &[track], 0)?;
    let last_duration = 637;
    for (i, data) in packets[..4].iter().enumerate() {
        writer.add(0, &packet(data, if i == 3 { last_duration } else { FRAME }))?;
    }
    let native_end = 3 * FRAME + last_duration;
    writer.set_edit(0, FRAME as u64, (native_end - FRAME) as u64);
    writer.finish(&path, None)?;

    let reader = Mp4Reader::open(&path)?;
    assert_eq!(reader.tracks[0].edit.map(|e| e.0), Some(FRAME as i64));
    let mut pcm = TrackPcm::new(&reader, reader.audio()[0])?;
    assert_eq!(pcm.sample_rate(), source_rate);
    let ratio = source_rate as f64 / OUT_RATE as f64;
    let frames = (native_end as f64 / ratio).ceil() as usize + 19;
    let mut actual = vec![0.0; frames * 2];
    pcm.read(&reader, 0, &mut actual)?;
    let mut expected: Vec<f32> = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let native = i as f64 * ratio;
        let k = native.floor() as usize;
        let frac = (native - k as f64) as f32;
        let at = |frame| if frame < native_end { reference[frame] } else { 0.0 };
        let v = at(k) * (1.0 - frac) + at(k + 1) * frac;
        expected.extend([v, v]);
    }
    assert!(expected[(frames - 300) * 2..].iter().any(|v| v.abs() > 0.1));
    assert_close(&actual, &expected);

    let start = (2 * FRAME) as i64;
    pcm.seek(&reader, start)?;
    let mut window = vec![0.0; expected.len() - start as usize * 2];
    pcm.read(&reader, start, &mut window)?;
    assert_close(&window, &expected[start as usize * 2..]);
    Ok(())
}

#[test]
fn a_single_packet_is_available_before_eof_silence() -> anyhow::Result<()> {
    let (track, packets) = encoded(OUT_RATE, 2)?;
    let reference = decode_reference(&track, &packets[3..=4])?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("one-packet.m4a");
    let mut writer = FlatWriter::new(&path, &[track], 0)?;
    writer.add(0, &packet(&packets[3], FRAME))?;
    writer.finish(&path, None)?;

    let reader = Mp4Reader::open(&path)?;
    let mut pcm = TrackPcm::new(&reader, reader.audio()[0])?;
    let mut actual = vec![0.0; (FRAME + 53) * 2];
    pcm.read(&reader, 0, &mut actual)?;
    assert!(reference[..FRAME * 2].iter().any(|v| v.abs() > 0.1));
    assert_close(&actual[..FRAME * 2], &reference[..FRAME * 2]);
    assert!(actual[FRAME * 2..].iter().all(|v| *v == 0.0));
    Ok(())
}
