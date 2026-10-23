//! System metrics for the METRICS module of the bar (`render::ItemId::Metrics`): CPU % from
//! `GetSystemTimes` deltas, RAM % from `GlobalMemoryStatusEx`.
//!
//! COST: two kernel32 calls per sample, no allocation, no thread. The UI owns a `Sampler` only while
//! `show_metrics` is on and samples it from the sysmon timer, which runs only while the bar is
//! visible (`ui::sysmon_sync`). Disabled = the `Sampler` does not exist.
//!
//! THREADING: UI thread (any thread would do: no state outside the `Sampler`).

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

/// CPU and RAM usage, percent 0..=100.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu: u8,
    pub ram: u8,
}

/// CPU sampler: remembers the previous `GetSystemTimes` totals.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sampler {
    idle: u64,
    total: u64,
    /// Last computed values (returned again if a sample fails).
    last: Usage,
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// CPU busy percentage between two `(idle, kernel + user)` totals (kernel time includes idle).
fn cpu_percent(d_idle: u64, d_total: u64) -> u8 {
    if d_total == 0 {
        return 0;
    }
    let busy = d_total.saturating_sub(d_idle);
    ((busy * 100 + d_total / 2) / d_total).min(100) as u8
}

impl Sampler {
    /// New sampler primed with the current totals (the first `sample` gives a real delta).
    pub fn new() -> Sampler {
        let mut s = Sampler::default();
        s.sample();
        s
    }

    /// Values of the last sample (CPU 0 right after `new`: no interval yet).
    pub fn last(&self) -> Usage {
        self.last
    }

    /// Takes a sample: CPU since the previous call, current RAM load.
    pub fn sample(&mut self) -> Usage {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        // SAFETY: out pointers to locals.
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_ok() {
            let (i, t) = (ft(idle), ft(kernel) + ft(user));
            if self.total != 0 {
                self.last.cpu = cpu_percent(i.saturating_sub(self.idle), t.saturating_sub(self.total));
            }
            self.idle = i;
            self.total = t;
        }
        let mut ms = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        // SAFETY: `ms` is a valid MEMORYSTATUSEX with `dwLength` set.
        if unsafe { GlobalMemoryStatusEx(&mut ms) }.is_ok() {
            self.last.ram = ms.dwMemoryLoad.min(100) as u8;
        }
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_math() {
        assert_eq!(cpu_percent(0, 0), 0);
        assert_eq!(cpu_percent(75, 100), 25);
        assert_eq!(cpu_percent(0, 100), 100);
        assert_eq!(cpu_percent(200, 100), 0);
    }
}
