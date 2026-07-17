//! Service-agnostic humanized browser interaction primitives.
//!
//! Used by auto-register (ChatGPT today) and reusable for other signup/login
//! flows: delays, mouse paths, and Markov typing timelines.

use rand::RngExt;
use std::time::Duration;

use crate::human_typing::{MarkovTyper, TypingAction, TypingEvent};

/// Profile for how "human" an interaction should feel.
#[derive(Debug, Clone, Copy)]
pub struct HumanProfile {
  /// Target typing speed (words per minute). `None` = Markov default (~80).
  pub wpm: Option<f64>,
  /// Min pre-action think delay (ms).
  pub think_min_ms: u64,
  /// Max pre-action think delay (ms).
  pub think_max_ms: u64,
  /// Mouse path steps for a normal move (more = smoother).
  pub mouse_steps: u32,
  /// Extra pause after typing a field (ms range).
  pub post_type_min_ms: u64,
  pub post_type_max_ms: u64,
}

impl Default for HumanProfile {
  fn default() -> Self {
    Self {
      wpm: Some(62.0),
      think_min_ms: 120,
      think_max_ms: 480,
      mouse_steps: 12,
      post_type_min_ms: 80,
      post_type_max_ms: 320,
    }
  }
}

impl HumanProfile {
  /// Slightly faster, still human — good for short fields (OTP, password).
  pub fn form_fill() -> Self {
    Self {
      wpm: Some(55.0),
      think_min_ms: 90,
      think_max_ms: 350,
      mouse_steps: 10,
      post_type_min_ms: 60,
      post_type_max_ms: 220,
    }
  }

  /// Slower / more cautious — clicks and navigation.
  pub fn careful() -> Self {
    Self {
      wpm: Some(48.0),
      think_min_ms: 180,
      think_max_ms: 700,
      mouse_steps: 16,
      post_type_min_ms: 120,
      post_type_max_ms: 400,
    }
  }
}

/// Random delay in [min_ms, max_ms].
pub fn jitter_ms(min_ms: u64, max_ms: u64) -> Duration {
  let (lo, hi) = if min_ms <= max_ms {
    (min_ms, max_ms)
  } else {
    (max_ms, min_ms)
  };
  let mut rng = rand::rng();
  let v = if lo == hi {
    lo
  } else {
    rng.random_range(lo..=hi)
  };
  Duration::from_millis(v)
}

pub fn think_delay(profile: &HumanProfile) -> Duration {
  jitter_ms(profile.think_min_ms, profile.think_max_ms)
}

pub fn post_type_delay(profile: &HumanProfile) -> Duration {
  jitter_ms(profile.post_type_min_ms, profile.post_type_max_ms)
}

/// Build a curved mouse path from `from` → `to` with slight human noise.
pub fn mouse_path(from: (f64, f64), to: (f64, f64), steps: u32) -> Vec<(f64, f64)> {
  let steps = steps.max(2);
  let mut rng = rand::rng();
  // Control point offset for a mild bezier curve.
  let mid_x = (from.0 + to.0) / 2.0 + rng.random_range(-40.0..40.0);
  let mid_y = (from.1 + to.1) / 2.0 + rng.random_range(-30.0..30.0);

  let mut points = Vec::with_capacity(steps as usize + 1);
  for i in 0..=steps {
    let t = i as f64 / steps as f64;
    // Quadratic Bezier: (1-t)^2 P0 + 2(1-t)t P1 + t^2 P2
    let u = 1.0 - t;
    let x = u * u * from.0 + 2.0 * u * t * mid_x + t * t * to.0;
    let y = u * u * from.1 + 2.0 * u * t * mid_y + t * t * to.1;
    let noise_x = if i == 0 || i == steps {
      0.0
    } else {
      rng.random_range(-1.2..1.2)
    };
    let noise_y = if i == 0 || i == steps {
      0.0
    } else {
      rng.random_range(-1.2..1.2)
    };
    points.push((x + noise_x, y + noise_y));
  }
  points
}

/// Click target offset inside an element rect (not always dead-center).
pub fn click_point_in_rect(x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
  let mut rng = rand::rng();
  // Prefer center-ish with Gaussian-ish jitter; clamp to 20–80% of box.
  let jx = rng.random_range(0.22..0.78);
  let jy = rng.random_range(0.25..0.75);
  let px = if w > 2.0 {
    x + w * jx
  } else {
    x + rng.random_range(-2.0..2.0)
  };
  let py = if h > 2.0 {
    y + h * jy
  } else {
    y + rng.random_range(-2.0..2.0)
  };
  (px, py)
}

/// Markov typing timeline with inter-event delays.
pub fn typing_events(text: &str, wpm: Option<f64>) -> Vec<TypingEvent> {
  MarkovTyper::new(text, wpm).run()
}

/// Convert absolute event times into per-step delays.
pub fn typing_step_delays(events: &[TypingEvent]) -> Vec<(Duration, TypingAction)> {
  let mut out = Vec::with_capacity(events.len());
  let mut last = 0.0_f64;
  for ev in events {
    let delta = (ev.time - last).max(0.0);
    // Cap absurd gaps; floor tiny delays so we don't spin.
    let secs = delta.clamp(0.015, 0.85);
    out.push((Duration::from_secs_f64(secs), ev.action.clone()));
    last = ev.time;
  }
  out
}

/// Key name for Playwright / CDP special keys.
pub fn key_name_for_char(ch: char) -> String {
  match ch {
    '\n' | '\r' => "Enter".into(),
    '\t' => "Tab".into(),
    ' ' => " ".into(),
    c => c.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mouse_path_starts_and_ends_near_targets() {
    let path = mouse_path((10.0, 10.0), (200.0, 150.0), 8);
    assert!(path.len() >= 3);
    let first = path.first().unwrap();
    let last = path.last().unwrap();
    assert!((first.0 - 10.0).abs() < 0.01);
    assert!((first.1 - 10.0).abs() < 0.01);
    assert!((last.0 - 200.0).abs() < 0.01);
    assert!((last.1 - 150.0).abs() < 0.01);
  }

  #[test]
  fn click_point_stays_in_rect() {
    for _ in 0..20 {
      let (px, py) = click_point_in_rect(100.0, 50.0, 80.0, 40.0);
      assert!((100.0..=180.0).contains(&px));
      assert!((50.0..=90.0).contains(&py));
    }
  }

  #[test]
  fn typing_events_replay_to_text() {
    let events = typing_events("hello", Some(80.0));
    let mut s = String::new();
    for e in &events {
      match &e.action {
        TypingAction::Char(c) => s.push(*c),
        TypingAction::Backspace => {
          s.pop();
        }
      }
    }
    assert_eq!(s, "hello");
  }

  #[test]
  fn typing_delays_non_empty() {
    let events = typing_events("ab", Some(70.0));
    let steps = typing_step_delays(&events);
    assert!(!steps.is_empty());
    assert!(steps.iter().all(|(d, _)| *d > Duration::ZERO));
  }

  #[test]
  fn jitter_in_range() {
    for _ in 0..30 {
      let d = jitter_ms(100, 200);
      let ms = d.as_millis() as u64;
      assert!((100..=200).contains(&ms));
    }
  }
}
