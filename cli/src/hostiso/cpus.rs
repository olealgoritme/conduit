//! A set of host CPUs, in the kernel's list format (`0-7,16-23`) and as the
//! hex mask `/sys/devices/virtual/workqueue/cpumask` and
//! `/proc/irq/default_smp_affinity` take.

use std::collections::BTreeSet;
use std::fmt;

/// The highest CPU number accepted from a list (the kernel's own limit is
/// larger; no desktop or workstation comes near this).
pub const MAX_CPU: u32 = 4095;

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CpuSet(BTreeSet<u32>);

impl CpuSet {
    pub fn new() -> CpuSet {
        CpuSet::default()
    }

    /// A kernel CPU list: `0-7,16-23`, `3`, or empty (no CPUs). `None` when
    /// it is not one.
    pub fn parse(s: &str) -> Option<CpuSet> {
        let s = s.trim();
        let mut set = BTreeSet::new();
        if s.is_empty() {
            return Some(CpuSet(set));
        }
        for part in s.split(',') {
            let part = part.trim();
            let (a, b) = match part.split_once('-') {
                Some((a, b)) => (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?),
                None => {
                    let n = part.parse::<u32>().ok()?;
                    (n, n)
                }
            };
            if a > b || b > MAX_CPU {
                return None;
            }
            set.extend(a..=b);
        }
        Some(CpuSet(set))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, cpu: u32) -> bool {
        self.0.contains(&cpu)
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().copied()
    }

    pub fn first(&self) -> Option<u32> {
        self.0.first().copied()
    }

    pub fn union(&self, o: &CpuSet) -> CpuSet {
        CpuSet(self.0.union(&o.0).copied().collect())
    }

    pub fn minus(&self, o: &CpuSet) -> CpuSet {
        CpuSet(self.0.difference(&o.0).copied().collect())
    }

    pub fn intersect(&self, o: &CpuSet) -> CpuSet {
        CpuSet(self.0.intersection(&o.0).copied().collect())
    }

    pub fn overlaps(&self, o: &CpuSet) -> bool {
        self.0.intersection(&o.0).next().is_some()
    }

    pub fn is_subset(&self, o: &CpuSet) -> bool {
        self.0.is_subset(&o.0)
    }

    /// The kernel's hex mask: 32-bit words, most significant first, comma
    /// separated (`00ff00ff`; `0000000f,00ff00ff` past CPU 31).
    pub fn mask(&self) -> String {
        let words = self.0.last().map_or(1, |m| (*m as usize) / 32 + 1);
        let mut w = vec![0u32; words];
        for c in &self.0 {
            w[(*c as usize) / 32] |= 1 << (c % 32);
        }
        w.iter()
            .rev()
            .map(|x| format!("{x:08x}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    #[cfg(test)]
    /// A hex mask as the kernel prints one (`ffffffff`, `00000000,00ff00ff`).
    pub fn parse_mask(s: &str) -> Option<CpuSet> {
        let mut set = BTreeSet::new();
        let words: Vec<&str> = s.trim().split(',').collect();
        for (i, word) in words.iter().rev().enumerate() {
            let v = u32::from_str_radix(word.trim(), 16).ok()?;
            for b in 0..32 {
                if v & (1 << b) != 0 {
                    set.insert(i as u32 * 32 + b);
                }
            }
        }
        Some(CpuSet(set))
    }

    /// The set as `_`-separated ranges, for a systemd instance name
    /// (`8-15_24-31`): a comma would need escaping there.
    pub fn instance_form(&self) -> String {
        self.to_string().replace(',', "_")
    }
}

impl FromIterator<u32> for CpuSet {
    fn from_iter<I: IntoIterator<Item = u32>>(it: I) -> CpuSet {
        CpuSet(it.into_iter().collect())
    }
}

impl fmt::Display for CpuSet {
    /// The kernel's list format, with ranges: `0-7,16-23`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = Vec::new();
        let mut it = self.0.iter().copied().peekable();
        while let Some(a) = it.next() {
            let mut b = a;
            while it.peek() == Some(&(b + 1)) {
                b = it.next().unwrap();
            }
            out.push(if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            });
        }
        write!(f, "{}", out.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_round_trip() {
        for s in ["0-7,16-23", "3", "0-1,4,6-9", ""] {
            assert_eq!(CpuSet::parse(s).unwrap().to_string(), s);
        }
        assert_eq!(CpuSet::parse(" 1, 2 ,3").unwrap().to_string(), "1-3");
        for bad in ["7-0", "a", "1,", "1-2-3", "0-5000", "-1"] {
            assert!(CpuSet::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn masks_match_the_kernel_format() {
        let host = CpuSet::parse("0-7,16-23").unwrap();
        assert_eq!(host.mask(), "00ff00ff");
        assert_eq!(CpuSet::parse_mask("00ff00ff").unwrap(), host);
        let wide = CpuSet::parse("0-3,32-35").unwrap();
        assert_eq!(wide.mask(), "0000000f,0000000f");
        assert_eq!(CpuSet::parse_mask("0000000f,0000000f").unwrap(), wide);
        assert_eq!(CpuSet::new().mask(), "00000000");
        assert_eq!(
            CpuSet::parse_mask("ffffffff").unwrap(),
            CpuSet::parse("0-31").unwrap()
        );
    }

    #[test]
    fn set_operations() {
        let all = CpuSet::parse("0-31").unwrap();
        let guest = CpuSet::parse("8-15,24-31").unwrap();
        assert_eq!(all.minus(&guest).to_string(), "0-7,16-23");
        assert!(guest.is_subset(&all));
        assert!(guest.overlaps(&CpuSet::parse("15-16").unwrap()));
        assert_eq!(guest.instance_form(), "8-15_24-31");
    }
}
