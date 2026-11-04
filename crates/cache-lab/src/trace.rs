//! Stable, deliberately small v1 CSV trace schema. Keys must be pre-hashed if
//! real traces are collected: do not ingest filenames, paths or secret fields.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

pub const HEADER: &str = "timestamp_ns,stream_id,key,charged_bytes,kind,refill_cost_ns,speculative";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Data,
    Metadata,
    Index,
    Directory,
}
impl Kind {
    pub const ALL: [Self; 4] = [Self::Data, Self::Metadata, Self::Index, Self::Directory];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Metadata => "metadata",
            Self::Index => "index",
            Self::Directory => "directory",
        }
    }
    pub fn index(self) -> usize {
        match self {
            Self::Data => 0,
            Self::Metadata => 1,
            Self::Index => 2,
            Self::Directory => 3,
        }
    }
    fn parse(v: &str) -> Result<Self, String> {
        match v {
            "data" => Ok(Self::Data),
            "metadata" => Ok(Self::Metadata),
            "index" => Ok(Self::Index),
            "directory" => Ok(Self::Directory),
            _ => Err(format!("unknown kind '{v}'")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Event {
    pub timestamp_ns: u64,
    pub stream_id: u64,
    pub key: u64,
    pub charged_bytes: u64,
    pub kind: Kind,
    /// An EXOGENOUS modeled penalty for this request, NOT measured demand wait
    /// or physical device latency, and never an assertion of real p99 latency.
    pub refill_cost_ns: u64,
    pub speculative: bool,
}

impl Event {
    pub fn csv(self) -> String {
        format!(
            "{},{},{},{},{},{},{}",
            self.timestamp_ns,
            self.stream_id,
            self.key,
            self.charged_bytes,
            self.kind.as_str(),
            self.refill_cost_ns,
            u8::from(self.speculative)
        )
    }
    pub fn parse(line: &str) -> Result<Self, String> {
        let p: Vec<&str> = line.trim().split(',').collect();
        if p.len() != 7 {
            return Err(format!("expected 7 CSV fields; got {}", p.len()));
        }
        let numeric = |idx: usize| {
            p[idx]
                .parse::<u64>()
                .map_err(|e| format!("field {} ('{}'): {e}", idx + 1, p[idx]))
        };
        let speculative = match p[6] {
            "0" => false,
            "1" => true,
            _ => return Err("speculative must be 0 or 1".into()),
        };
        let event = Self {
            timestamp_ns: numeric(0)?,
            stream_id: numeric(1)?,
            key: numeric(2)?,
            charged_bytes: numeric(3)?,
            kind: Kind::parse(p[4])?,
            refill_cost_ns: numeric(5)?,
            speculative,
        };
        if event.charged_bytes == 0 {
            return Err("charged_bytes must be > 0".into());
        }
        Ok(event)
    }
}

/// Deliberately strict: a key's size and kind are immutable for a replay.
/// For a rewritten block, the collector must assign a new key incorporating
/// its generation, or emit an explicit invalidation in a future trace version.
pub fn validate(events: &[Event]) -> Result<(), String> {
    let mut prev = None;
    let mut shapes: HashMap<u64, (u64, Kind)> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        if e.charged_bytes == 0 {
            return Err(format!("event {} has size zero", i + 1));
        }
        if prev.is_some_and(|p| e.timestamp_ns < p) {
            return Err(format!("timestamp decreased at event {}", i + 1));
        }
        prev = Some(e.timestamp_ns);
        match shapes.insert(e.key, (e.charged_bytes, e.kind)) {
            Some(old) if old != (e.charged_bytes, e.kind) => {
                return Err(format!(
                    "key {} changed size or kind at event {} (use a generation-distinct key)",
                    e.key,
                    i + 1
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn load(path: impl AsRef<Path>, max_events: usize) -> io::Result<Vec<Event>> {
    let reader = BufReader::new(File::open(path)?);
    let mut events = Vec::new();
    let mut header_seen = false;
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !header_seen {
            if line != HEADER {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "line {}: unsupported trace header (expected '{HEADER}')",
                        index + 1
                    ),
                ));
            }
            header_seen = true;
            continue;
        }
        if events.len() >= max_events {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("trace exceeds max-events={max_events}"),
            ));
        }
        let event = Event::parse(line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: {e}", index + 1),
            )
        })?;
        events.push(event);
    }
    if !header_seen {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing CSV header",
        ));
    }
    validate(&events).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(events)
}

pub fn save(path: impl AsRef<Path>, events: &[Event]) -> io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    writeln!(out, "{HEADER}")?;
    for e in events {
        writeln!(out, "{}", e.csv())?;
    }
    out.flush()
}

#[derive(Debug, Clone, Copy)]
pub enum Pattern {
    Hotset,
    Scan,
    Mixed,
    Phase,
}
impl Pattern {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "hotset" => Ok(Self::Hotset),
            "scan" => Ok(Self::Scan),
            "mixed" => Ok(Self::Mixed),
            "phase" => Ok(Self::Phase),
            _ => Err(format!(
                "unknown pattern '{s}', expected hotset|scan|mixed|phase"
            )),
        }
    }
}

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545f4914f6cdd1d)
    }
    fn bounded(&mut self, range: u64) -> u64 {
        self.next() % range
    }
}

fn shape(key: u64) -> (u64, Kind, u64) {
    match key % 8 {
        0..=2 => (4096, Kind::Metadata, 110_000),
        3 => (4096, Kind::Index, 130_000),
        4 => (4096, Kind::Directory, 100_000),
        5 | 6 => (16_384, Kind::Data, 160_000),
        _ => (65_536, Kind::Data, 200_000),
    }
}

/// Fixed-seed, synthetic traces allow policy regression tests, not claims
/// about real-world latency or cache-hit rates.
pub fn generate(pattern: Pattern, count: usize, seed: u64) -> Vec<Event> {
    let mut rng = Rng::new(seed);
    let mut result = Vec::with_capacity(count);
    let mut cold = 10_000_000_u64;
    for i in 0..count {
        let dice = rng.bounded(100);
        let key = match pattern {
            Pattern::Hotset => {
                if dice < 83 {
                    rng.bounded(64)
                } else {
                    cold += 1;
                    cold
                }
            }
            Pattern::Scan => {
                if dice < 5 {
                    rng.bounded(16)
                } else {
                    cold += 1;
                    cold
                }
            }
            Pattern::Mixed => {
                if dice < 55 {
                    rng.bounded(64)
                } else if dice < 80 {
                    cold += 1;
                    cold
                } else {
                    1_000 + rng.bounded(2_048)
                }
            }
            Pattern::Phase => {
                if dice < 75 {
                    // New hot set every 10,000 requests, eventually revisiting.
                    let phase = (i / 10_000) % 4;
                    100_000 + (phase as u64) * 128 + rng.bounded(64)
                } else {
                    cold += 1;
                    cold
                }
            }
        };
        let (charged_bytes, kind, refill_cost_ns) = shape(key);
        result.push(Event {
            timestamp_ns: (i as u64).saturating_mul(1000),
            stream_id: (i % 4) as u64,
            key,
            charged_bytes,
            kind,
            refill_cost_ns,
            speculative: false,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let ev = generate(Pattern::Mixed, 1000, 42);
        validate(&ev).unwrap();
        for e in ev {
            let parsed = Event::parse(&e.csv()).unwrap();
            assert_eq!(parsed.csv(), e.csv());
        }
    }
    #[test]
    fn key_size_change_is_rejected() {
        let mut ev = generate(Pattern::Hotset, 10, 1);
        let mut edited = ev[0];
        edited.charged_bytes += 1;
        edited.timestamp_ns = ev.last().unwrap().timestamp_ns + 1;
        ev.push(edited);
        assert!(validate(&ev).is_err());
    }
}
