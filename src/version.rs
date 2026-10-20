//! Version parsing and comparison.
//!
//! Mirrors the practical semantics Scoop users expect:
//!   * dotted releases compared segment-wise, numerically when both sides are
//!     numeric ("1.2.10" > "1.2.9", "2024.01.11" > "2024.1.9")
//!   * prereleases rank below release: alpha < beta < pre/preview < rc < release
//!   * rolling tags (nightly, dev, snapshot, canary, build) rank above release
//!     and are then compared by their trailing identifier, so consecutive
//!     nightlies keep showing up as updates
//!   * a leading "v" is ignored; build metadata after "+" is ignored
//!   * numeric segments beat non-numeric ones at the same position

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Version {
    raw: String,
    release: Vec<Chunk>,
    /// 1..=4 = prerelease kinds, 5 = plain release, 6 = rolling/post tag
    rank: u8,
    /// remainder after the prerelease/rolling tag, tokenized
    tag_rest: Vec<Chunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Chunk {
    Num(u64),
    Str(String),
}

impl Chunk {
    fn cmp_chunks(a: &Chunk, b: &Chunk) -> Ordering {
        match (a, b) {
            (Chunk::Num(x), Chunk::Num(y)) => x.cmp(y),
            (Chunk::Num(_), Chunk::Str(_)) => Ordering::Greater,
            (Chunk::Str(_), Chunk::Num(_)) => Ordering::Less,
            (Chunk::Str(x), Chunk::Str(y)) => x.cmp(y),
        }
    }
}

fn tokenize(s: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    // 0 = separator, 1 = digit run, 2 = alpha run
    let mut mode = 0u8;
    fn flush(chunks: &mut Vec<Chunk>, cur: &mut String, mode: u8) {
        if cur.is_empty() {
            return;
        }
        match mode {
            1 => chunks.push(Chunk::Num(cur.parse::<u64>().unwrap_or(0))),
            _ => chunks.push(Chunk::Str(cur.to_lowercase())),
        }
        cur.clear();
    }
    for c in s.chars() {
        let m = if c.is_ascii_digit() {
            1u8
        } else if c.is_alphanumeric() {
            2u8
        } else {
            0u8
        };
        if m != mode {
            flush(&mut chunks, &mut cur, mode);
        }
        mode = m;
        if m != 0 {
            cur.push(c);
        }
    }
    flush(&mut chunks, &mut cur, mode);
    chunks
}

fn pad_to(a: &mut Vec<Chunk>, b: &[Chunk]) {
    let need = b.len().saturating_sub(a.len());
    for _ in 0..need {
        a.push(Chunk::Num(0));
    }
}

impl Version {
    pub fn new(raw: &str) -> Version {
        let raw = raw.trim();
        let base = raw.trim_start_matches(['v', 'V']);
        // drop build metadata
        let base = base.split('+').next().unwrap_or(base);
        let (release_part, tag_part) = match base.find('-') {
            Some(i) => (&base[..i], Some(&base[i + 1..])),
            None => (base, None),
        };
        let release = tokenize(release_part);

        let (rank, tag_rest) = match tag_part {
            None => (5u8, Vec::new()),
            Some(tag) => {
                let t = tag.trim().to_ascii_lowercase();
                let rolling = [
                    "nightly", "dev", "snapshot", "canary", "master", "build", "latest",
                ]
                .iter()
                .any(|k| {
                    t == *k
                        || t.starts_with(&format!("{k}."))
                        || t.starts_with(&format!("{k}-"))
                        || t.starts_with(*k)
                });
                if rolling {
                    (6u8, tokenize(tag))
                } else {
                    let lower = t.as_str();
                    let (rk, used) = if lower.starts_with("alpha")
                        || lower.starts_with("a.")
                        || lower == "a"
                    {
                        (1u8, 5usize)
                    } else if lower.starts_with("beta") || lower.starts_with("b.") || lower == "b" {
                        (2u8, 4usize)
                    } else if lower.starts_with("pre") {
                        (3u8, 3usize)
                    } else if lower.starts_with("rc") || lower.starts_with("candidate") {
                        (4u8, 2usize)
                    } else {
                        // unknown tags behave like an extra plain segment:
                        // treat as slightly older than release but newer than
                        // a prerelease, and still comparable
                        (4u8, 0usize)
                    };
                    let rest = if used > 0 && used < lower.len() {
                        tokenize(&lower[used..])
                    } else {
                        tokenize(tag)
                    };
                    (rk, rest)
                }
            }
        };
        Version {
            raw: raw.to_string(),
            release,
            rank,
            tag_rest,
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut a = self.release.clone();
        let mut b = other.release.clone();
        pad_to(&mut a, &b);
        pad_to(&mut b, &a);
        for (x, y) in a.iter().zip(b.iter()) {
            let ord = Chunk::cmp_chunks(x, y);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        match self.rank.cmp(&other.rank) {
            Ordering::Equal => {}
            ord => return ord,
        }
        let mut x = self.tag_rest.clone();
        let mut y = other.tag_rest.clone();
        pad_to(&mut x, &y);
        pad_to(&mut y, &x);
        for (u, v) in x.iter().zip(y.iter()) {
            let ord = Chunk::cmp_chunks(u, v);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    fn v(s: &str) -> Version {
        Version::new(s)
    }

    #[test]
    fn numeric_segment_compare() {
        assert!(v("1.2.10") > v("1.2.9"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("2024.01.11") > v("2024.1.9"));
        assert!(v("2.0") < v("2.0.1"));
        assert_eq!(v("1.2.0"), v("1.2"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
    }

    #[test]
    fn prerelease_ordering() {
        assert!(v("1.2.3-beta1") < v("1.2.3"));
        assert!(v("1.2.3-alpha") < v("1.2.3-beta1"));
        assert!(v("1.2.3-beta1") < v("1.2.3-beta2"));
        assert!(v("1.2.3-beta2") < v("1.2.3-rc1"));
        assert!(v("1.2.3-rc1") < v("1.2.3"));
        assert!(v("1.2.3-rc2") > v("1.2.3-rc1"));
    }

    #[test]
    fn rolling_tags_are_newer_and_comparable() {
        assert!(v("5.4.1-nightly.20240102") > v("5.4.1"));
        assert!(v("5.4.1-nightly.20240102") > v("5.4.1-nightly.20240101"));
        assert!(v("1.0-dev.2") > v("1.0"));
        assert!(v("1.0-dev.2") > v("1.0-dev.1"));
    }

    #[test]
    fn unknown_tag_is_older_than_release() {
        assert!(v("1.0-experimental") < v("1.0"));
        assert!(v("1.0-experimental.2") > v("1.0-experimental.1"));
    }

    #[test]
    fn mixed_segments() {
        assert!(v("20.08.2.0") > v("20.08.1"));
        assert!(v("1.0.0-beta.2") > v("1.0.0-beta.1"));
        // prerelease of a newer major still beats the older major
        // (segments are compared before pre-release rank, like scoop/semver)
        assert!(v("10.0-beta") > v("9.9.9"));
        assert!(v("1.0.0-beta") < v("1.0.0"));
    }
}
