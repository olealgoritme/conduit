//! `--cpus LIST`: keep a process on a set of host CPUs. See
//! docs/research/host-roundtrip-latency.md, "Placement".
//!
//! The list is the kernel's format (`0-7,16-23`). Set on the calling thread
//! before others start, every thread made afterwards inherits it.

use std::io;

/// CPU numbers of a list such as `0-7,16-23`; refused when empty, malformed
/// or past the kernel's `CPU_SETSIZE`.
pub fn parse_cpu_list(list: &str) -> io::Result<Vec<usize>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("not a CPU list: {list:?}"));
    let mut out = Vec::new();
    for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => {
                (a.trim().parse::<usize>().map_err(|_| bad())?, b.trim().parse::<usize>().map_err(|_| bad())?)
            }
            None => {
                let n = part.parse::<usize>().map_err(|_| bad())?;
                (n, n)
            }
        };
        if a > b || b >= libc::CPU_SETSIZE as usize {
            return Err(bad());
        }
        out.extend(a..=b);
    }
    if out.is_empty() {
        return Err(bad());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Pin the calling thread (and so every thread it starts later) to `list`.
/// Returns how many CPUs that is.
pub fn pin_process(list: &str) -> io::Result<usize> {
    let cpus = parse_cpu_list(list)?;
    // SAFETY: cpu_set_t is plain data; CPU_SET stays inside it (checked
    // against CPU_SETSIZE above); sched_setaffinity reads it for the call.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in &cpus {
            libc::CPU_SET(c, &mut set);
        }
        if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(cpus.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists() {
        assert_eq!(parse_cpu_list("0-3,8").unwrap(), vec![0, 1, 2, 3, 8]);
        assert_eq!(parse_cpu_list(" 16-17 , 1 ").unwrap(), vec![1, 16, 17]);
        assert_eq!(parse_cpu_list("2,2,1-2").unwrap(), vec![1, 2]);
        for bad in ["", ",", "3-1", "a", "1-", "-1", "99999"] {
            assert!(parse_cpu_list(bad).is_err(), "{bad}");
        }
    }
}
