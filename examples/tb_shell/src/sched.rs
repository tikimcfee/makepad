//! Where the UI and GL threads run, by env from the unit (off unless set: the
//! cpu numbers are the phone's):
//!   TB_SHELL_CPUS=0-3         sched_setaffinity for them (on the phone, the fast cluster)
//!   TB_SHELL_UCLAMP_MIN=768   a uclamp minimum (sched_setattr, SCHED_FLAG_UTIL_CLAMP_MIN):
//!                             per task, so it acts only while they run and costs nothing at rest
//! Either call refused: one log line, and the shell goes on without it.

use makepad_widgets::*;

extern "C" {
    fn gettid() -> i32;
    fn sched_setaffinity(pid: i32, size: usize, mask: *const u64) -> i32;
    fn syscall(n: i64, ...) -> i64;
}

#[cfg(target_arch = "aarch64")]
const SYS_SCHED_SETATTR: i64 = 274;
#[cfg(target_arch = "x86_64")]
const SYS_SCHED_SETATTR: i64 = 314;

const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;

#[repr(C)]
#[derive(Default)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

#[derive(Default)]
pub struct Placement {
    cpus: Option<u64>,
    uclamp_min: Option<u32>,
    done: Vec<i32>,
    pub ui_tid: i32,
}

/// "0-3" or "0,1,4-5" into a mask.
fn parse_cpus(list: &str) -> Option<u64> {
    let mut mask = 0u64;
    for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        for c in a..=b.min(63) {
            mask |= 1 << c;
        }
    }
    (mask != 0).then_some(mask)
}

impl Placement {
    pub fn from_env() -> Placement {
        let cpus = std::env::var("TB_SHELL_CPUS").ok().and_then(|v| parse_cpus(&v));
        let uclamp_min = std::env::var("TB_SHELL_UCLAMP_MIN").ok().and_then(|v| v.trim().parse().ok()).filter(|v| *v > 0);
        let ui_tid = unsafe { gettid() };
        log!("tb-shell: ui thread tid {ui_tid}");
        Placement { cpus, uclamp_min, done: Vec::new(), ui_tid }
    }

    /// Places the UI thread and every GL thread not placed yet (call again when
    /// more may have started: the driver starts some with the first frame).
    pub fn apply(&mut self) {
        if self.cpus.is_none() && self.uclamp_min.is_none() {
            return;
        }
        let mut tids = vec![self.ui_tid];
        for entry in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            if comm.starts_with("makepad-gl-rend") {
                if let Ok(tid) = entry.file_name().to_string_lossy().parse() {
                    tids.push(tid);
                }
            }
        }
        for tid in tids {
            if self.done.contains(&tid) {
                continue;
            }
            self.done.push(tid);
            if let Some(mask) = self.cpus {
                if unsafe { sched_setaffinity(tid, std::mem::size_of::<u64>(), &mask) } != 0 {
                    log!("tb-shell: affinity {mask:#x} for tid {tid} refused: {}; going on without it", std::io::Error::last_os_error());
                    self.cpus = None;
                }
            }
            if let Some(min) = self.uclamp_min {
                let attr = SchedAttr {
                    size: std::mem::size_of::<SchedAttr>() as u32,
                    sched_flags: SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS | SCHED_FLAG_UTIL_CLAMP_MIN,
                    sched_util_min: min,
                    ..Default::default()
                };
                if unsafe { syscall(SYS_SCHED_SETATTR, tid as i64, &attr as *const SchedAttr, 0i64) } != 0 {
                    log!("tb-shell: uclamp min {min} for tid {tid} refused: {}; going on without it", std::io::Error::last_os_error());
                    self.uclamp_min = None;
                }
            }
        }
        log!("tb-shell: placed {} threads (cpus {:?}, uclamp min {:?})", self.done.len(), self.cpus.map(|m| format!("{m:#x}")), self.uclamp_min);
    }
}
