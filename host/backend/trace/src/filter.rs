//! `--filter`: which records to show.
//!
//! A filter is a comma-separated list of terms. Call terms (`alloc`,
//! `control`, `uvm`, `nvkms`, `drm`, ...) select by what a request is; a
//! record passes if it matches any of them, or if none were given. The other
//! terms narrow further and must all hold:
//!
//! * `slow:>1ms` -- total latency over a threshold (`ns`, `us`, `ms`, `s`)
//! * `errors` -- the guest saw a failure (errno, RM status or refusal)
//! * `refused` -- the backend refused it
//!
//! `rm` is every ioctl on an NVIDIA node (alloc, control, free, dup, map,
//! unmap and the other escapes).

use crate::{Call, Kind, Record, Refusal};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    calls: Vec<Call>,
    min_ns: Option<u64>,
    errors: bool,
    refused: bool,
}

const RM: [Call; 7] = [
    Call::Alloc,
    Call::Control,
    Call::Free,
    Call::Dup,
    Call::Map,
    Call::Unmap,
    Call::Rm,
];

/// `1ms`, `250us`, `2s`, `500ns`, `1.5ms` as nanoseconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let v: f64 = num.parse().ok()?;
    let scale = match unit.trim() {
        "ns" => 1.0,
        "us" | "µs" => 1e3,
        "ms" | "" => 1e6,
        "s" => 1e9,
        _ => return None,
    };
    Some((v * scale).round() as u64)
}

impl Filter {
    /// Parse a list of terms. Several `--filter` options can be joined with
    /// commas first; the result is the same.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut f = Filter::default();
        for term in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(rest) = term.strip_prefix("slow:") {
                let rest = rest.trim_start_matches(['>', '=']);
                f.min_ns =
                    Some(parse_duration(rest).ok_or_else(|| {
                        format!("{term}: expected e.g. slow:>1ms or slow:>250us")
                    })?);
                continue;
            }
            match term {
                "errors" => f.errors = true,
                "refused" => f.refused = true,
                "rm" => f.calls.extend(RM),
                _ => match Call::parse(term) {
                    Some(c) if c != Call::Dropped => f.calls.push(c),
                    _ => {
                        return Err(format!(
                            "unknown filter {term:?}; try alloc, control, free, map, rm, uvm, \
                             nvkms, drm, open, close, mmap, munmap, event, errors, refused, \
                             slow:>1ms"
                        ));
                    }
                },
            }
        }
        Ok(f)
    }

    pub fn is_empty(&self) -> bool {
        *self == Filter::default()
    }

    pub fn matches(&self, r: &Record) -> bool {
        // Lost records are always worth knowing about.
        if r.kind == Kind::Dropped {
            return true;
        }
        (self.calls.is_empty() || self.calls.contains(&r.call))
            && self.min_ns.is_none_or(|m| r.total_ns() > m)
            && (!self.errors || r.failed())
            && (!self.refused || !matches!(r.refusal, Refusal::None | Refusal::Local))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::sample;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("1ms"), Some(1_000_000));
        assert_eq!(parse_duration("250us"), Some(250_000));
        assert_eq!(parse_duration("1.5ms"), Some(1_500_000));
        assert_eq!(parse_duration("2s"), Some(2_000_000_000));
        assert_eq!(parse_duration("10"), Some(10_000_000));
        assert_eq!(parse_duration("ms"), None);
        assert_eq!(parse_duration("3h"), None);
    }

    #[test]
    fn call_terms_are_alternatives_and_the_rest_narrow() {
        let control = sample();
        let alloc = Record {
            call: Call::Alloc,
            ..sample()
        };
        let uvm = Record {
            call: Call::Uvm,
            ..sample()
        };
        let f = Filter::parse("alloc,control").unwrap();
        assert!(f.matches(&control) && f.matches(&alloc) && !f.matches(&uvm));

        let slow = Filter::parse("control,slow:>10us").unwrap();
        assert!(slow.matches(&control), "17.3us > 10us");
        let slower = Filter::parse("slow:>1ms").unwrap();
        assert!(!slower.matches(&control));

        let rm = Filter::parse("rm").unwrap();
        assert!(rm.matches(&control) && rm.matches(&alloc) && !rm.matches(&uvm));
    }

    #[test]
    fn errors_and_refusals() {
        let ok = sample();
        let bad_status = Record {
            nv_status: Some(0x56),
            ..sample()
        };
        let refused = Record {
            refusal: Refusal::Allowlist,
            ..sample()
        };
        let errors = Filter::parse("errors").unwrap();
        assert!(!errors.matches(&ok) && errors.matches(&bad_status) && errors.matches(&refused));
        let refusals = Filter::parse("refused").unwrap();
        assert!(!refusals.matches(&bad_status) && refusals.matches(&refused));
    }

    #[test]
    fn nonsense_is_refused_with_a_hint() {
        assert!(Filter::parse("allocs").unwrap_err().contains("try"));
        assert!(Filter::parse("slow:>fast").is_err());
        assert!(Filter::parse("").unwrap().is_empty());
    }

    #[test]
    fn dropped_markers_always_show() {
        let f = Filter::parse("uvm,slow:>1s").unwrap();
        assert!(f.matches(&Record::dropped(1, 5)));
    }
}
