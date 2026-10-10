//! Host memory admission: refuse to start a VM the host cannot hold.
//!
//! Guest RAM is a shared memfd; once the guest has touched it, the host cannot
//! take it back (shmem is not reaped by the OOM killer). Starting more guest
//! RAM than the host has free therefore ends in the global OOM killer picking
//! a victim — often the desktop. So before starting, the requested RAM plus a
//! safety margin must fit in what the kernel reports as available, minus what
//! the other running Conduit VMs may still claim (their RAM not faulted in yet).

use crate::ui::{self, oops};
use anyhow::Result;

/// Kept free for the host on top of the VM's RAM and overhead.
pub const MARGIN_MIB: u64 = 2048;

/// Host-side memory a VM uses beyond its guest RAM (backend, runner, virtiofsd).
pub fn overhead_mib(ram_mib: u64) -> u64 {
    (ram_mib / 4).max(1024)
}

/// A field of /proc/meminfo in MiB.
fn meminfo_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb / 1024)
    })
}

/// MemAvailable in MiB.
pub fn available_mib() -> Option<u64> {
    meminfo_field(
        &std::fs::read_to_string("/proc/meminfo").ok()?,
        "MemAvailable",
    )
}

/// Resident shared + anonymous memory of a process, in MiB (guest RAM it has faulted in).
pub fn resident_mib(pid: i32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let f = |k| meminfo_field(&s, k).unwrap_or(0);
    Some(f("RssAnon") + f("RssShmem"))
}

/// How a running VM's guest RAM is backed on the host, in MiB: from the VMM's
/// `/proc/PID/status` (`RssShmem`) and `/proc/PID/smaps_rollup`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GuestPages {
    /// Shared memory resident in the VMM (the guest RAM memfd, mostly).
    pub shmem_mib: u64,
    /// Of it, mapped with 2 MiB pages (`ShmemPmdMapped`): only those can be
    /// 2 MiB pages in the guest's nested page tables too.
    pub pmd_mib: u64,
    /// Reserved huge pages (`Shared_Hugetlb` + `Private_Hugetlb`).
    pub hugetlb_mib: u64,
    /// Swapped out (`Swap`).
    pub swap_mib: u64,
}

/// [`GuestPages`] from the two files' text.
pub fn parse_guest_pages(status: &str, rollup: &str) -> GuestPages {
    let f = |t: &str, k| meminfo_field(t, k).unwrap_or(0);
    GuestPages {
        shmem_mib: f(status, "RssShmem"),
        pmd_mib: f(rollup, "ShmemPmdMapped"),
        hugetlb_mib: f(rollup, "Shared_Hugetlb") + f(rollup, "Private_Hugetlb"),
        swap_mib: f(rollup, "Swap"),
    }
}

/// [`GuestPages`] of the VMM `pid`.
pub fn guest_pages(pid: i32) -> Option<GuestPages> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    Some(parse_guest_pages(&status, &rollup))
}

/// Whether the backing is worth a warning, and what to say. Guest RAM on
/// 4 KiB host pages makes every guest TLB miss a two-level walk through 4 KiB
/// nested tables: measured on the reference host, a CPU-bound game's render
/// thread spent its time on cold loads with the whole 16 GiB on 4 KiB pages
/// (KVM `pages_2m` 0). Swapped guest RAM stalls the guest outright.
pub fn guest_pages_verdict(p: &GuestPages) -> (bool, String) {
    let swap = if p.swap_mib >= 256 {
        format!(", {} MiB swapped out", p.swap_mib)
    } else {
        String::new()
    };
    if p.hugetlb_mib > 0 {
        return (
            !swap.is_empty(),
            format!("{} MiB on reserved huge pages{swap}", p.hugetlb_mib),
        );
    }
    let total = p.shmem_mib.max(1);
    let pct = p.pmd_mib.min(total) * 100 / total;
    (
        pct < 50 || !swap.is_empty(),
        format!(
            "{pct}% of {} MiB on 2 MiB pages, the rest on 4 KiB{swap}",
            p.shmem_mib
        ),
    )
}

/// What the check decided, for the message.
#[derive(Debug, PartialEq, Eq)]
pub struct Budget {
    pub need_mib: u64,
    pub free_mib: u64,
}

impl Budget {
    /// `others` = (RAM, resident) of the other running VMs: what they have
    /// not faulted in yet they may still take, so it is not free for us.
    pub fn new(ram_mib: u64, available_mib: u64, others: &[(u64, u64)]) -> Budget {
        let pending: u64 = others.iter().map(|&(r, res)| r.saturating_sub(res)).sum();
        Budget {
            need_mib: ram_mib + overhead_mib(ram_mib) + MARGIN_MIB,
            free_mib: available_mib.saturating_sub(pending),
        }
    }
    pub fn fits(&self) -> bool {
        self.need_mib <= self.free_mib
    }
}

/// Refuse to start `name` with `ram_mib` when the host cannot hold it.
/// `others`: (RAM, vm pid) of the other running Conduit VMs.
pub fn admit(name: &str, ram_mib: u64, others: &[(u64, i32)], skip: bool) -> Result<()> {
    let Some(avail) = available_mib() else {
        return Ok(());
    };
    let others: Vec<(u64, u64)> = others
        .iter()
        .map(|&(r, pid)| (r, resident_mib(pid).unwrap_or(0)))
        .collect();
    let b = Budget::new(ram_mib, avail, &others);
    if b.fits() {
        return Ok(());
    }
    let gib = |m: u64| ui::human_bytes(m << 20);
    let msg = format!(
        "not enough free memory to start {name}: it needs about {} ({} for the VM, plus room for the host), but only {} is available{}",
        gib(b.need_mib),
        gib(ram_mib),
        gib(b.free_mib),
        if others.is_empty() {
            String::new()
        } else {
            format!(" (other running VMs: {})", others.len())
        }
    );
    if skip {
        ui::warn(format!("{msg}; starting anyway (--no-mem-check)"));
        return Ok(());
    }
    Err(oops(
        msg,
        format!(
            "Close programs or stop another VM (`conduit status`), or give {name} less memory: set \"ram_mib\" in its vm.json (e.g. 4096). `--no-mem-check` starts it anyway, at the risk of the host running out of memory."
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_meminfo() {
        let t = "MemTotal:       65536000 kB\nMemFree:  100 kB\nMemAvailable:   8388608 kB\n";
        assert_eq!(meminfo_field(t, "MemAvailable"), Some(8192));
        assert_eq!(meminfo_field(t, "MemTotal"), Some(64000));
        assert_eq!(meminfo_field(t, "Mem"), None);
    }

    #[test]
    fn guest_pages_read_the_vmm_and_warn_on_small_pages() {
        let status = "Name:\tqemu\nRssShmem:\t15863532 kB\n";
        let rollup = "Rss:  15863532 kB\nShmemPmdMapped:        0 kB\n\
                      Shared_Hugetlb:        0 kB\nPrivate_Hugetlb:        0 kB\n\
                      Swap:            1304456 kB\n";
        let p = parse_guest_pages(status, rollup);
        assert_eq!(
            p,
            GuestPages {
                shmem_mib: 15491,
                pmd_mib: 0,
                hugetlb_mib: 0,
                swap_mib: 1273,
            }
        );
        let (warn, text) = guest_pages_verdict(&p);
        assert!(warn);
        assert_eq!(
            text,
            "0% of 15491 MiB on 2 MiB pages, the rest on 4 KiB, 1273 MiB swapped out"
        );
        // Mostly on 2 MiB pages, nothing swapped: fine.
        let thp = GuestPages {
            shmem_mib: 16384,
            pmd_mib: 15000,
            ..Default::default()
        };
        assert!(!guest_pages_verdict(&thp).0);
        // Reserved huge pages.
        let huge = GuestPages {
            hugetlb_mib: 16384,
            ..Default::default()
        };
        assert_eq!(
            guest_pages_verdict(&huge),
            (false, "16384 MiB on reserved huge pages".to_string())
        );
    }

    #[test]
    fn budget_counts_ram_overhead_margin_and_other_vms() {
        // 4 GiB VM: 4096 + 1024 + 2048.
        let b = Budget::new(4096, 8000, &[]);
        assert_eq!(b.need_mib, 7168);
        assert!(b.fits());
        // A running 8 GiB VM with only 2 GiB faulted in may still take 6 GiB.
        let b = Budget::new(4096, 16000, &[(8192, 2048)]);
        assert_eq!(b.free_mib, 16000 - 6144);
        assert!(b.fits());
        assert!(!Budget::new(4096, 12000, &[(8192, 2048)]).fits());
        assert!(!Budget::new(16384, 16000, &[]).fits());
        // Overhead grows with big guests.
        assert_eq!(overhead_mib(32768), 8192);
    }
}
