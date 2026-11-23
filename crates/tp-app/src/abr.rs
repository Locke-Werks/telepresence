//! Sender-side bitrate control driven by the receiver's `Stats` reports.
//!
//! Multiplicative decrease on loss, slow additive-ish increase after a run of
//! clean reports. Deliberately simple: on a LAN loss usually means Wi-Fi
//! contention, and backing off quickly matters more than finding the exact
//! ceiling.

#[derive(Debug, Clone)]
pub struct BitrateController {
    min: u32,
    max: u32,
    current: u32,
    clean_reports: u32,
}

const LOSS_HIGH: f32 = 0.02;
const LOSS_CLEAN: f32 = 0.005;
/// Clean reports (one per second) needed before stepping up, so a brief lull
/// in contention does not immediately push back into loss.
const CLEAN_TO_INCREASE: u32 = 3;

impl BitrateController {
    pub fn new(start: u32, min: u32, max: u32) -> Self {
        let min = min.min(max);
        Self { min, max, current: start.clamp(min, max), clean_reports: 0 }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// Feeds one loss report; returns the new target when it changed.
    pub fn on_report(&mut self, loss: f32) -> Option<u32> {
        let before = self.current;
        if loss > LOSS_HIGH {
            self.clean_reports = 0;
            self.current = ((self.current as f32 * 0.8) as u32).max(self.min);
        } else if loss < LOSS_CLEAN {
            self.clean_reports += 1;
            if self.clean_reports >= CLEAN_TO_INCREASE {
                self.clean_reports = 0;
                let step = (self.current / 20).max(50);
                self.current = (self.current + step).min(self.max);
            }
        } else {
            self.clean_reports = 0;
        }
        (self.current != before).then_some(self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_down_on_loss_and_respects_floor() {
        let mut c = BitrateController::new(1000, 600, 2000);
        assert_eq!(c.on_report(0.05), Some(800));
        assert_eq!(c.on_report(0.05), Some(640));
        assert_eq!(c.on_report(0.05), Some(600));
        assert_eq!(c.on_report(0.05), None);
    }

    #[test]
    fn creeps_up_only_after_clean_run() {
        let mut c = BitrateController::new(1000, 100, 1100);
        assert_eq!(c.on_report(0.0), None);
        assert_eq!(c.on_report(0.0), None);
        assert_eq!(c.on_report(0.0), Some(1050));
        // Moderate loss resets the run without stepping down.
        assert_eq!(c.on_report(0.01), None);
        assert_eq!(c.on_report(0.0), None);
        assert_eq!(c.on_report(0.0), None);
        assert_eq!(c.on_report(0.0), Some(1100));
        for _ in 0..6 {
            assert_eq!(c.on_report(0.0), None);
        }
    }
}
