//! Aligns captured audio to the recorder's monotonic, pause-free timeline.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MAX_BUFFERED_SECONDS: u64 = 30;

#[derive(Clone, Copy)]
pub(super) enum Track {
    System,
    Microphone,
}

struct Segment {
    start: Instant,
    end: Option<Instant>,
    elapsed_before: Duration,
}

pub(super) struct TimedMixer {
    rate: u32,
    segments: Vec<Segment>,
    written: u64,
    system: VecDeque<Option<[i16; 2]>>,
    microphone: VecDeque<Option<[i16; 2]>>,
}

fn frames(duration: Duration, rate: u32, round_up: bool) -> Result<u64, String> {
    let scaled = duration.as_nanos() * u128::from(rate);
    let adjustment = if round_up { NANOS_PER_SECOND - 1 } else { NANOS_PER_SECOND / 2 };
    u64::try_from((scaled + adjustment) / NANOS_PER_SECOND).map_err(|_| "audio timeline is too long".into())
}

fn frame_duration(frame: usize, rate: u32) -> Result<Duration, String> {
    let nanos = frame as u128 * NANOS_PER_SECOND / u128::from(rate);
    Ok(Duration::from_nanos(u64::try_from(nanos).map_err(|_| "audio block is too long")?))
}

impl TimedMixer {
    pub(super) fn new(start: Instant, rate: u32) -> TimedMixer {
        assert!(rate > 0);
        TimedMixer {
            rate,
            segments: vec![Segment { start, end: None, elapsed_before: Duration::ZERO }],
            written: 0,
            system: VecDeque::new(),
            microphone: VecDeque::new(),
        }
    }

    pub(super) fn set_paused(&mut self, paused: bool, at: Instant) -> Result<(), String> {
        let last = self.segments.last_mut().expect("the recording has an initial segment");
        match (paused, last.end) {
            (true, None) => {
                if at < last.start {
                    return Err("audio pause precedes its recording segment".into());
                }
                let boundary = frames(last.elapsed_before + (at - last.start), self.rate, false)?;
                if boundary < self.written {
                    return Err("audio pause precedes already written samples".into());
                }
                last.end = Some(at);
                let keep = usize::try_from(boundary - self.written).unwrap_or(usize::MAX);
                self.system.truncate(keep);
                self.microphone.truncate(keep);
            }
            (false, Some(end)) => {
                if at < end {
                    return Err("audio resume precedes its pause".into());
                }
                let elapsed_before = last.elapsed_before + (end - last.start);
                self.segments.push(Segment { start: at, end: None, elapsed_before });
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn frame_at(&self, at: Instant) -> Result<u64, String> {
        let mut elapsed = Duration::ZERO;
        for segment in &self.segments {
            if at <= segment.start {
                break;
            }
            let end = segment.end.map_or(at, |end| end.min(at));
            elapsed = segment.elapsed_before + (end - segment.start);
            if segment.end.is_none_or(|end| at < end) {
                break;
            }
        }
        frames(elapsed, self.rate, false)
    }

    pub(super) fn written(&self) -> u64 {
        self.written
    }

    pub(super) fn end_track(&mut self, track: Track, at: Instant) -> Result<(), String> {
        let boundary = self.frame_at(at)?;
        if boundary < self.written {
            return Err("audio source stopped before already written samples".into());
        }
        let keep = usize::try_from(boundary - self.written).unwrap_or(usize::MAX);
        match track {
            Track::System => self.system.truncate(keep),
            Track::Microphone => self.microphone.truncate(keep),
        }
        Ok(())
    }

    /// Returns the number of frames that arrived after their output position.
    pub(super) fn push(&mut self, track: Track, captured_at: Instant, pcm: &[i16]) -> Result<u64, String> {
        if pcm.len() % 2 != 0 {
            return Err("audio capture supplied an incomplete stereo frame".into());
        }
        let count = pcm.len() / 2;
        if count == 0 {
            return Ok(0);
        }
        let mut pieces = Vec::new();
        for segment in &self.segments {
            if segment.end.is_some_and(|end| captured_at >= end) {
                continue;
            }
            let first = if segment.start > captured_at {
                usize::try_from(frames(segment.start - captured_at, self.rate, true)?).unwrap_or(usize::MAX).min(count)
            } else {
                0
            };
            let end = match segment.end {
                Some(end) if end > captured_at => usize::try_from(frames(end - captured_at, self.rate, true)?).unwrap_or(usize::MAX).min(count),
                Some(_) => 0,
                None => count,
            };
            if first >= end {
                continue;
            }
            let first_time = captured_at.checked_add(frame_duration(first, self.rate)?).ok_or("audio timestamp is out of range")?;
            let elapsed = segment.elapsed_before + first_time.saturating_duration_since(segment.start);
            let start = frames(elapsed, self.rate, false)?;
            pieces.push((start, first, end));
        }

        let pending_limit = self.written.saturating_add(u64::from(self.rate) * MAX_BUFFERED_SECONDS);
        let output = match track {
            Track::System => &mut self.system,
            Track::Microphone => &mut self.microphone,
        };
        let mut late = 0u64;
        for (start, first, end) in pieces {
            let piece_end = start.checked_add((end - first) as u64).ok_or("audio timeline is too long")?;
            late += self.written.saturating_sub(start).min(piece_end - start);
            if piece_end <= self.written {
                continue;
            }
            if piece_end > pending_limit {
                return Err("audio timestamps are too far ahead of the recording".into());
            }
            let skip = self.written.saturating_sub(start) as usize;
            let relative = start.saturating_sub(self.written) as usize;
            let needed = relative + end - first - skip;
            if output.len() < needed {
                output.resize(needed, None);
            }
            for (slot, sample) in output.iter_mut().skip(relative).zip(pcm[(first + skip) * 2..end * 2].chunks_exact(2)) {
                if slot.is_none() {
                    *slot = Some([sample[0], sample[1]]);
                }
            }
        }
        Ok(late)
    }

    /// Returns at most one second, inserting silence for missing or late data.
    pub(super) fn take_to(&mut self, target: u64) -> Vec<i16> {
        let end = target.min(self.written.saturating_add(u64::from(self.rate)));
        let count = end.saturating_sub(self.written) as usize;
        let mut result = Vec::with_capacity(count * 2);
        for _ in 0..count {
            let a = self.system.pop_front().flatten().unwrap_or([0; 2]);
            let b = self.microphone.pop_front().flatten().unwrap_or([0; 2]);
            result.extend([a[0].saturating_add(b[0]), a[1].saturating_add(b[1])]);
        }
        self.written += count as u64;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(values: impl IntoIterator<Item = i16>) -> Vec<i16> {
        values.into_iter().flat_map(|value| [value, value]).collect()
    }

    fn mono(pcm: &[i16]) -> Vec<i16> {
        pcm.chunks_exact(2).map(|frame| frame[0]).collect()
    }

    #[test]
    fn aligns_sources_by_capture_time_not_callback_order() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        mixer.push(Track::Microphone, start + Duration::from_millis(30), &stereo([200; 100])).unwrap();
        mixer.push(Track::System, start + Duration::from_millis(20), &stereo([100; 100])).unwrap();

        let samples = mono(&mixer.take_to(mixer.frame_at(start + Duration::from_millis(150)).unwrap()));
        assert_eq!(&samples[..20], &[0; 20]);
        assert_eq!(&samples[20..30], &[100; 10]);
        assert_eq!(&samples[30..120], &[300; 90]);
        assert_eq!(&samples[120..130], &[200; 10]);
        assert_eq!(&samples[130..], &[0; 20]);
    }

    #[test]
    fn pause_boundaries_trim_blocks_and_remove_paused_time() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        mixer.set_paused(true, start + Duration::from_millis(40)).unwrap();
        assert_eq!(mixer.frame_at(start + Duration::from_millis(80)).unwrap(), 40);
        mixer.set_paused(false, start + Duration::from_millis(90)).unwrap();
        mixer.push(Track::System, start, &stereo(1..=160)).unwrap();

        let samples = mono(&mixer.take_to(mixer.frame_at(start + Duration::from_millis(160)).unwrap()));
        assert_eq!(samples, (1..=40).chain(91..=160).collect::<Vec<_>>());
    }

    #[test]
    fn pause_discards_already_buffered_samples_past_its_boundary() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        mixer.push(Track::System, start, &stereo(1..=100)).unwrap();
        mixer.set_paused(true, start + Duration::from_millis(40)).unwrap();
        mixer.set_paused(false, start + Duration::from_millis(90)).unwrap();
        mixer.push(Track::System, start + Duration::from_millis(90), &stereo([200; 10])).unwrap();
        let samples = mono(&mixer.take_to(50));
        assert_eq!(samples, (1..=40).chain([200; 10]).collect::<Vec<_>>());
    }

    #[test]
    fn trims_pre_start_and_late_samples_and_does_not_double_overlaps() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        mixer.push(Track::System, start - Duration::from_millis(5), &stereo(1..=10)).unwrap();
        assert_eq!(mono(&mixer.take_to(5)), vec![6, 7, 8, 9, 10]);
        assert_eq!(mixer.push(Track::System, start + Duration::from_millis(3), &stereo([20; 5])).unwrap(), 2);
        mixer.push(Track::System, start + Duration::from_millis(6), &stereo([30; 4])).unwrap();
        assert_eq!(mono(&mixer.take_to(10)), vec![20, 20, 20, 30, 30]);
    }

    #[test]
    fn source_toggles_preserve_gaps_and_other_track() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        mixer.push(Track::Microphone, start, &stereo([1; 20])).unwrap();
        mixer.push(Track::System, start, &stereo([3; 5])).unwrap();
        mixer.push(Track::System, start + Duration::from_millis(15), &stereo([7; 5])).unwrap();
        assert_eq!(mono(&mixer.take_to(20)), [vec![4; 5], vec![1; 10], vec![8; 5]].concat());
    }

    #[test]
    fn disabling_a_source_removes_only_its_buffered_future() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        mixer.push(Track::System, start, &stereo([3; 20])).unwrap();
        mixer.push(Track::Microphone, start, &stereo([1; 20])).unwrap();
        mixer.end_track(Track::System, start + Duration::from_millis(5)).unwrap();
        assert_eq!(mono(&mixer.take_to(20)), [vec![4; 5], vec![1; 15]].concat());
    }

    #[test]
    fn timestamp_drift_does_not_accumulate_as_a_queue_offset() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        for block in 0..10 {
            mixer.push(Track::System, start + Duration::from_millis(block * 101), &stereo([7; 100])).unwrap();
        }
        let mut samples = mixer.take_to(1_010);
        samples.extend(mixer.take_to(1_010));
        let samples = mono(&samples);
        assert_eq!(samples.len(), 1_010);
        assert_eq!(samples.iter().filter(|&&sample| sample == 7).count(), 1_000);
        assert_eq!(samples.iter().filter(|&&sample| sample == 0).count(), 10);
    }

    #[test]
    fn rejects_invalid_blocks_and_implausible_future_timestamps() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 1_000);
        assert!(mixer.push(Track::System, start, &[1]).is_err());
        assert!(mixer.push(Track::System, start + Duration::from_secs(31), &[1, 1]).is_err());
        assert!(mixer.set_paused(true, start - Duration::from_millis(1)).is_err());
        mixer.set_paused(true, start + Duration::from_millis(10)).unwrap();
        assert!(mixer.set_paused(false, start + Duration::from_millis(9)).is_err());
    }

    #[test]
    fn mix_clips_and_output_is_bounded_per_write() {
        let start = Instant::now();
        let mut mixer = TimedMixer::new(start, 10);
        mixer.push(Track::System, start, &stereo([20_000; 15])).unwrap();
        mixer.push(Track::Microphone, start, &stereo([20_000; 15])).unwrap();
        assert_eq!(mono(&mixer.take_to(15)), vec![i16::MAX; 10]);
        assert_eq!(mixer.written(), 10);
        assert_eq!(mono(&mixer.take_to(15)), vec![i16::MAX; 5]);
    }
}
