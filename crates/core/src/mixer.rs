//! Audio-track mixing filter graphs for mpv (live playback) and ffmpeg (export).
//!
//! mpv can only play one audio track at a time, unless the tracks are combined
//! in `lavfi-complex`. Filters inside lavfi-complex can't be changed at runtime,
//! so we `join` all tracks into one multichannel stream there (each track's
//! left/right on its own named speaker pair), and do the per-track volume in
//! the regular `af` chain, where `af-command` can change gains live.
//!
//! Verified against libmpv with real OBS recordings (6 stereo tracks):
//! * the joined layout must list speakers in FFmpeg's canonical order, or mpv
//!   rejects the frames ("could not use filter output");
//! * remapping with `pan` into 2-channel side/top layouts fails in swresample,
//!   which is why `join` is used instead;
//! * volume changes via af-command are instant and survive seeks as long as
//!   the `af` option string is also kept current.

/// Speaker pairs used to carry each track through the joined stream. None of
/// them is LFE (which may be low-pass filtered or dropped on conversion).
pub const CHANNEL_PAIRS: [(&str, &str); 6] =
    [("FL", "FR"), ("FC", "BC"), ("BL", "BR"), ("FLC", "FRC"), ("SL", "SR"), ("TFL", "TFR")];

/// FFmpeg's canonical speaker order (AV_CH_* bit order) for the names above.
const CANONICAL: [&str; 14] = ["FL", "FR", "FC", "LFE", "BL", "BR", "FLC", "FRC", "BC", "SL", "SR", "TC", "TFL", "TFR"];

fn joined_layout(n: usize) -> String {
    let used: Vec<&str> = CHANNEL_PAIRS[..n].iter().flat_map(|(a, b)| [*a, *b]).collect();
    CANONICAL.iter().filter(|c| used.contains(c)).copied().collect::<Vec<_>>().join("+")
}

pub const MAX_MIX_TRACKS: usize = CHANNEL_PAIRS.len();
pub const MIX_FILTER_LABEL: &str = "cvmix";

/// lavfi-complex graph merging `n` audio tracks (mpv aid 1..=n) into [ao].
/// Returns `None` when there is nothing to merge (0 or 1 track).
pub fn mpv_lavfi_complex(n: usize) -> Option<String> {
    let n = n.min(MAX_MIX_TRACKS);
    if n < 2 {
        return None;
    }
    let mut g = String::new();
    for i in 0..n {
        g.push_str(&format!("[aid{}]aformat=sample_rates=48000:channel_layouts=stereo[cvt{i}];", i + 1));
    }
    for i in 0..n {
        g.push_str(&format!("[cvt{i}]"));
    }
    let map: Vec<String> = (0..n)
        .map(|i| {
            let (a, b) = CHANNEL_PAIRS[i];
            format!("{i}.0-{a}|{i}.1-{b}")
        })
        .collect();
    g.push_str(&format!("join=inputs={n}:channel_layout={}:map={}[ao]", joined_layout(n), map.join("|")));
    Some(g)
}

/// The `af` chain that splits the merged stream back into tracks, applies a
/// gain per track (`volume@gN`), and mixes them, with a limiter so boosting
/// tracks can't clip.
pub fn mpv_af(gains: &[f32]) -> String {
    let n = gains.len().min(MAX_MIX_TRACKS);
    if n < 2 {
        let g = gains.first().copied().unwrap_or(1.0);
        return format!("@{MIX_FILTER_LABEL}:lavfi=[volume@g0={}]", fmt_gain(g));
    }
    let splits: String = (0..n).map(|i| format!("[s{i}]")).collect();
    let mut graph = format!("asplit={n}{splits}");
    for i in 0..n {
        let (a, b) = CHANNEL_PAIRS[i];
        graph.push_str(&format!(
            ";[s{i}]pan=stereo|c0={a}|c1={b},volume@g{i}={}[m{i}]",
            fmt_gain(gains[i])
        ));
    }
    let ms: String = (0..n).map(|i| format!("[m{i}]")).collect();
    graph.push_str(&format!(";{ms}amix=inputs={n}:normalize=0,alimiter=limit=0.98:level=disabled"));
    format!("@{MIX_FILTER_LABEL}:lavfi=[{graph}]")
}

/// Arguments for mpv's `af-command` to change one track's gain live.
pub fn mpv_gain_command(track_index: usize, gain: f32) -> [String; 5] {
    [
        "af-command".into(),
        MIX_FILTER_LABEL.into(),
        "volume".into(),
        fmt_gain(gain),
        format!("volume@g{track_index}"),
    ]
}

fn fmt_gain(g: f32) -> String {
    let g = if g.is_finite() { g.clamp(0.0, 4.0) } else { 1.0 };
    format!("{g:.3}")
}

/// ffmpeg `-filter_complex` that mixes the given (track index, gain) pairs of
/// input 0 into `[aout]`. Tracks with gain 0 are skipped. Returns `None` when
/// every track is muted.
pub fn ffmpeg_mix_filter(tracks: &[(usize, f32)]) -> Option<String> {
    let active: Vec<(usize, f32)> = tracks.iter().copied().filter(|(_, g)| *g > 0.0005).collect();
    if active.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for (k, (idx, g)) in active.iter().enumerate() {
        parts.push(format!(
            "[0:a:{idx}]aformat=sample_rates=48000:channel_layouts=stereo,volume={}[x{k}]",
            fmt_gain(*g)
        ));
    }
    let n = active.len();
    let ins: String = (0..n).map(|k| format!("[x{k}]")).collect();
    if n == 1 {
        parts.push(format!("{ins}alimiter=limit=0.98:level=disabled[aout]"));
    } else {
        parts.push(format!(
            "{ins}amix=inputs={n}:normalize=0:duration=longest,alimiter=limit=0.98:level=disabled[aout]"
        ));
    }
    Some(parts.join(";"))
}

/// Default gains for a file with `n` tracks.
///
/// "full" mode plays only track 1 (the full mix). "separate" plays every
/// isolated stem except "Desktop" (track 6, which already contains game, voice
/// and music and would double them up).
pub fn default_gains(n: usize, mode: &str, multitrack_layout: bool) -> Vec<f32> {
    let mut g = vec![0.0; n];
    if n == 0 {
        return g;
    }
    if n == 1 || mode != "separate" || !multitrack_layout {
        g[0] = 1.0;
        return g;
    }
    for (i, v) in g.iter_mut().enumerate().skip(1) {
        *v = if i == crate::config::TRACK_DESKTOP - 1 { 0.0 } else { 1.0 };
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_track_needs_no_complex_graph() {
        assert_eq!(mpv_lavfi_complex(0), None);
        assert_eq!(mpv_lavfi_complex(1), None);
        assert_eq!(mpv_af(&[0.5]), "@cvmix:lavfi=[volume@g0=0.500]");
    }

    #[test]
    fn three_track_graph_matches_verified_shape() {
        let lc = mpv_lavfi_complex(3).unwrap();
        assert!(lc.starts_with("[aid1]aformat=sample_rates=48000:channel_layouts=stereo[cvt0];"));
        assert!(lc.ends_with("[cvt0][cvt1][cvt2]join=inputs=3:channel_layout=FL+FR+FC+BL+BR+BC:map=0.0-FL|0.1-FR|1.0-FC|1.1-BC|2.0-BL|2.1-BR[ao]"));
        let af = mpv_af(&[1.0, 0.0, 0.25]);
        assert!(af.starts_with("@cvmix:lavfi=[asplit=3[s0][s1][s2]"));
        assert!(af.contains("[s1]pan=stereo|c0=FC|c1=BC,volume@g1=0.000[m1]"));
        assert!(af.contains("volume@g2=0.250"));
        assert!(af.ends_with("amix=inputs=3:normalize=0,alimiter=limit=0.98:level=disabled]"));
    }

    #[test]
    fn joined_layout_is_in_canonical_order() {
        // Canonical (AV_CH bit) order is required by mpv.
        assert_eq!(joined_layout(6), "FL+FR+FC+BL+BR+FLC+FRC+BC+SL+SR+TFL+TFR");
        let lc = mpv_lavfi_complex(6).unwrap();
        assert!(lc.contains("5.0-TFL|5.1-TFR"));
        let pos = |c: &str| CANONICAL.iter().position(|x| *x == c).unwrap();
        let order: Vec<usize> = joined_layout(6).split('+').map(pos).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn caps_at_six_tracks_and_clamps_gain() {
        let lc = mpv_lavfi_complex(9).unwrap();
        assert!(lc.contains("join=inputs=6"));
        assert!(!lc.contains("aid7"));
        let cmd = mpv_gain_command(2, 9.0);
        assert_eq!(cmd[3], "4.000");
        assert_eq!(cmd[4], "volume@g2");
        assert_eq!(mpv_gain_command(0, f32::NAN)[3], "1.000");
    }

    #[test]
    fn ffmpeg_filters() {
        assert_eq!(ffmpeg_mix_filter(&[(0, 0.0), (1, 0.0)]), None);
        let one = ffmpeg_mix_filter(&[(0, 0.0), (2, 1.5)]).unwrap();
        assert_eq!(
            one,
            "[0:a:2]aformat=sample_rates=48000:channel_layouts=stereo,volume=1.500[x0];[x0]alimiter=limit=0.98:level=disabled[aout]"
        );
        let two = ffmpeg_mix_filter(&[(1, 1.0), (3, 0.3)]).unwrap();
        assert!(two.contains("[0:a:1]"));
        assert!(two.contains("[0:a:3]"));
        assert!(two.contains("amix=inputs=2:normalize=0:duration=longest"));
    }

    #[test]
    fn default_gain_modes() {
        assert_eq!(default_gains(1, "separate", true), vec![1.0]);
        assert_eq!(default_gains(6, "full", true), vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(default_gains(6, "separate", true), vec![0.0, 1.0, 1.0, 1.0, 1.0, 0.0]);
        assert_eq!(default_gains(3, "separate", false), vec![1.0, 0.0, 0.0]);
        assert!(default_gains(0, "full", true).is_empty());
    }
}
