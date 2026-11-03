//! Cordata's two sounds, synthesised here (no audio files). Each has one job, and plays only for something you could
//! not see happen:
//!
//! - `NeedsYou`: two notes rising a fourth — a question. An approval arrived, or the terminal's agent is waiting.
//! - `Done`: two notes falling a major third — a resolution. A thread finished while you were elsewhere.
//!
//! Never while you are looking at the thing, never for a turn you cancelled, at most one every few seconds, and only
//! with Settings → General → Sounds on. The timbre is a sine with a faint second harmonic and a soft attack: glassy,
//! quiet, short (under half a second), so it reads as Cordata's and not as an alert.

use std::time::{Duration, Instant};

const RATE: u32 = 44_100;
/// Loudest sample, as a share of full scale: a cue, not an alarm.
const PEAK: f32 = 0.22;
/// A second cue this soon after the last is dropped: a burst of events is one sound.
pub const QUIET_FOR: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    NeedsYou,
    Done,
}

/// One note: frequency (Hz), start and length (s), and its level relative to `PEAK`.
struct Note(f32, f32, f32, f32);

fn notes(cue: Cue) -> [Note; 2] {
    match cue {
        // A5 then D6 (a fourth up), the second held a little: it asks.
        Cue::NeedsYou => [Note(880.0, 0.0, 0.16, 0.85), Note(1174.66, 0.11, 0.30, 1.0)],
        // E6 then C6 (a major third down), the second longer and softer: it settles.
        Cue::Done => [Note(1318.51, 0.0, 0.14, 0.9), Note(1046.50, 0.09, 0.36, 0.75)],
    }
}

/// The cue as 16-bit mono PCM samples.
pub fn samples(cue: Cue) -> Vec<i16> {
    let notes = notes(cue);
    let end = notes.iter().map(|n| n.1 + n.2).fold(0.0, f32::max);
    let len = (end * RATE as f32).ceil() as usize;
    (0..len)
        .map(|i| {
            let time = i as f32 / RATE as f32;
            let mix: f32 = notes
                .iter()
                .filter(|n| time >= n.1 && time < n.1 + n.2)
                .map(|&Note(freq, start, length, level)| {
                    let t = time - start;
                    // 6 ms attack, then an exponential fall to silence by the note's end.
                    let attack = (t / 0.006).min(1.0);
                    let decay = (-5.0 * t / length).exp() * (1.0 - t / length);
                    let phase = std::f32::consts::TAU * freq * t;
                    level * attack * decay * (phase.sin() + 0.15 * (2.0 * phase).sin()) / 1.15
                })
                .sum();
            (mix.clamp(-1.0, 1.0) * PEAK * f32::from(i16::MAX)) as i16
        })
        .collect()
}

/// The cue as a WAV file in memory.
pub fn wav(cue: Cue) -> Vec<u8> {
    let pcm = samples(cue);
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Remembers the last cue so a burst of events makes one sound.
#[derive(Debug, Default)]
pub struct Gate {
    last: Option<Instant>,
}

impl Gate {
    /// Plays `cue` unless one played within `QUIET_FOR`. Returns whether it played.
    pub fn play(&mut self, cue: Cue) -> bool {
        let now = Instant::now();
        if self.last.is_some_and(|last| now.duration_since(last) < QUIET_FOR) {
            return false;
        }
        self.last = Some(now);
        play(cue);
        true
    }
}

#[cfg(target_os = "macos")]
fn play(cue: Cue) {
    use objc2::AllocAnyThread as _;
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;
    let data = NSData::from_vec(wav(cue));
    if let Some(sound) = NSSound::initWithData(NSSound::alloc(), &data) {
        sound.play();
    }
}

#[cfg(not(target_os = "macos"))]
fn play(_cue: Cue) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_cue_is_short_quiet_and_ends_in_silence() {
        for cue in [Cue::NeedsYou, Cue::Done] {
            let pcm = samples(cue);
            let seconds = pcm.len() as f32 / RATE as f32;
            assert!(seconds > 0.2 && seconds < 0.5, "{cue:?} lasts {seconds:.2}s");
            let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
            assert!(f32::from(peak) <= PEAK * f32::from(i16::MAX) + 1.0 && peak > 1000, "{cue:?} peak {peak}");
            assert!(pcm.last().unwrap().unsigned_abs() < 50, "{cue:?} ends without a click");
            assert!(pcm[0].unsigned_abs() < 50, "{cue:?} starts without a click");
        }
        assert_ne!(samples(Cue::NeedsYou), samples(Cue::Done));
    }

    #[test]
    fn the_rising_cue_asks_and_the_falling_one_settles() {
        let [a, b] = notes(Cue::NeedsYou);
        assert!(b.0 > a.0, "needs-you rises");
        let [c, d] = notes(Cue::Done);
        assert!(d.0 < c.0, "done falls");
    }

    #[test]
    fn the_wav_is_a_well_formed_pcm_file() {
        let bytes = wav(Cue::Done);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 44 + data_len);
        assert_eq!(data_len, samples(Cue::Done).len() * 2);
    }

    #[test]
    fn a_burst_makes_one_sound() {
        // A cue just played: the next one inside the quiet period is dropped (and nothing is played by the test).
        let mut gate = Gate { last: Some(Instant::now()) };
        assert!(!gate.play(Cue::Done));
        assert!(!gate.play(Cue::NeedsYou), "whatever the cue");
    }
}
