//! End-of-speech detection on 16 kHz mono PCM: wait for speech, then stop after a pause.
//!
//! Energy-based and calibrated to the room: the first frames set the noise floor, and speech
//! is anything well above it. Good enough for push-to-talk on a laptop mic; the wake-word
//! loop uses it to cut segments.

pub const RATE: usize = 16_000;
/// 30 ms frames.
pub const FRAME: usize = RATE * 30 / 1000;

#[derive(Debug, Clone)]
pub struct VadCfg {
    /// Frames used to measure the noise floor before listening.
    pub calibrate_ms: usize,
    /// Speech = RMS above floor × this.
    pub ratio: f32,
    /// ...and above this absolute level (so a silent room doesn't make breathing "speech").
    pub min_rms: f32,
    /// This much quiet after speech ends the utterance.
    pub silence_ms: usize,
    /// Shorter bursts (a click, a cough) don't count as speech.
    pub min_speech_ms: usize,
    /// Give up if nobody starts talking within this.
    pub start_timeout_ms: usize,
    /// Hard cap on one utterance.
    pub max_ms: usize,
}

impl Default for VadCfg {
    fn default() -> Self {
        Self { calibrate_ms: 300, ratio: 3.0, min_rms: 300.0, silence_ms: 800, min_speech_ms: 240, start_timeout_ms: 6000, max_ms: 20_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Keep feeding audio.
    Listening,
    /// Speech started (e.g. to stop playback: barge-in).
    Started,
    /// The utterance is over: transcribe what was recorded.
    Done,
    /// Nobody spoke before the timeout.
    NoSpeech,
}

pub struct Endpointer {
    cfg: VadCfg,
    pending: Vec<i16>,
    frames: usize,
    floor_sum: f32,
    floor_n: usize,
    voiced_run: usize,
    quiet_run: usize,
    speaking: bool,
    ended: Option<Event>,
}

fn ms(frames: usize) -> usize {
    frames * 30
}

pub fn rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    (frame.iter().map(|&s| (s as f32) * (s as f32)).sum::<f32>() / frame.len() as f32).sqrt()
}

impl Endpointer {
    pub fn new(cfg: VadCfg) -> Self {
        Self { cfg, pending: vec![], frames: 0, floor_sum: 0.0, floor_n: 0, voiced_run: 0, quiet_run: 0, speaking: false, ended: None }
    }

    fn threshold(&self) -> f32 {
        let floor = if self.floor_n > 0 { self.floor_sum / self.floor_n as f32 } else { 0.0 };
        (floor * self.cfg.ratio).max(self.cfg.min_rms)
    }

    /// Feed samples (any length). Returns the most important event they caused.
    pub fn push(&mut self, samples: &[i16]) -> Event {
        if let Some(e) = self.ended {
            return e;
        }
        self.pending.extend_from_slice(samples);
        let mut event = Event::Listening;
        while self.pending.len() >= FRAME {
            let frame: Vec<i16> = self.pending.drain(..FRAME).collect();
            let e = self.frame(rms(&frame));
            if e != Event::Listening {
                event = e;
            }
            if let Some(end) = self.ended {
                return end;
            }
        }
        event
    }

    fn frame(&mut self, level: f32) -> Event {
        self.frames += 1;
        if ms(self.frames) <= self.cfg.calibrate_ms {
            self.floor_sum += level;
            self.floor_n += 1;
            return Event::Listening;
        }
        let voiced = level > self.threshold();
        if voiced {
            self.voiced_run += 1;
            self.quiet_run = 0;
        } else {
            self.quiet_run += 1;
            if !self.speaking {
                self.voiced_run = 0;
            }
        }
        if !self.speaking && ms(self.voiced_run) >= self.cfg.min_speech_ms {
            self.speaking = true;
            return Event::Started;
        }
        if self.speaking && ms(self.quiet_run) >= self.cfg.silence_ms || ms(self.frames) >= self.cfg.max_ms {
            self.ended = Some(if self.speaking { Event::Done } else { Event::NoSpeech });
            return self.ended.unwrap();
        }
        if !self.speaking && ms(self.frames) >= self.cfg.start_timeout_ms {
            self.ended = Some(Event::NoSpeech);
            return Event::NoSpeech;
        }
        Event::Listening
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ms` of a tone at `amp` (or near-silence with amp 0).
    fn sound(ms: usize, amp: f32) -> Vec<i16> {
        (0..RATE * ms / 1000).map(|i| ((i as f32 * 0.07).sin() * amp + (i % 7) as f32 * 3.0) as i16).collect()
    }

    fn run(cfg: VadCfg, parts: &[(usize, f32)]) -> Vec<Event> {
        let mut e = Endpointer::new(cfg);
        let mut out = vec![];
        for (ms, amp) in parts {
            for chunk in sound(*ms, *amp).chunks(1024) {
                let ev = e.push(chunk);
                if ev != Event::Listening && out.last() != Some(&ev) {
                    out.push(ev);
                }
            }
        }
        out
    }

    #[test]
    fn speech_then_pause_ends_utterance() {
        assert_eq!(run(VadCfg::default(), &[(400, 0.0), (1200, 4000.0), (1000, 0.0)]), vec![Event::Started, Event::Done]);
    }

    #[test]
    fn short_pause_mid_sentence_keeps_listening() {
        let ev = run(VadCfg::default(), &[(400, 0.0), (600, 4000.0), (300, 0.0), (600, 4000.0), (300, 0.0)]);
        assert_eq!(ev, vec![Event::Started]);
    }

    #[test]
    fn clicks_and_silence_time_out() {
        let cfg = VadCfg { start_timeout_ms: 2000, ..Default::default() };
        assert_eq!(run(cfg, &[(400, 0.0), (60, 6000.0), (2000, 0.0)]), vec![Event::NoSpeech]);
    }

    #[test]
    fn noisy_room_raises_the_bar() {
        // Calibrated on a fan at 1500: the same fan isn't speech; a louder voice is.
        let ev = run(VadCfg { start_timeout_ms: 3000, ..Default::default() }, &[(400, 1500.0), (1000, 1500.0), (800, 9000.0), (1000, 1500.0)]);
        assert_eq!(ev, vec![Event::Started, Event::Done]);
    }
}
