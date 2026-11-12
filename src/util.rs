//! Small helpers shared by the commands: output formats, JSON text, a seeded
//! generator.

use std::fmt::Write as _;

use xxhash_rust::xxh3::xxh3_64;

/// The output formats of the commands that report (diff, hotspots, validate).
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Json,
    Text,
}

/// `s` as a JSON string.
pub fn json_text(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).expect("write to String"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `v` as a JSON number, `null` when missing or not finite.
pub fn json_number(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => format!("{v}"),
        _ => "null".into(),
    }
}

/// A small deterministic generator (SplitMix64), seeded from text.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: &str) -> Rng {
        Rng(xxh3_64(seed.as_bytes()))
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A number in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// A number in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}
