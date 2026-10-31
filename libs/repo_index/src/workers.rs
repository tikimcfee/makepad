//! The crate's one way to run work on several cores: a process-wide set of worker threads,
//! created on first use and fed jobs over a channel (AGENTS.md: a long-lived worker fed by a
//! channel, never a thread per job), and [`fan_out`] over indices on top of it.
//!
//! `fan_out(len, f)` returns `f(0), .., f(len - 1)` in index order, so its result is exactly
//! what a serial loop gives. The calling thread claims indices like any worker, so a fan-out
//! finishes even when every worker is busy, and a fan-out nested inside another (a worker
//! calling it) cannot deadlock. A worker that picks up a fan-out after its indices are all
//! claimed does nothing; the caller returns only once no worker is still inside `f`, which is
//! what keeps `f`'s borrows sound. A panic in `f`, on any thread, resumes in the caller.

use std::any::Any;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

struct Pool {
    jobs: Mutex<Sender<Job>>,
    workers: usize,
}

static POOL: OnceLock<Pool> = OnceLock::new();

fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let cores = match std::thread::available_parallelism() {
            Ok(n) => n.get(),
            Err(e) => {
                eprintln!("makepad index: the core count is unknown ({e}); one worker thread");
                1
            }
        };
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = 0;
        for n in 0..cores {
            let rx: Arc<Mutex<Receiver<Job>>> = Arc::clone(&rx);
            let spawned = std::thread::Builder::new().name(format!("index-worker-{n}")).spawn(move || loop {
                let job = match rx.lock() {
                    Ok(r) => r.recv(),
                    Err(_) => return,
                };
                match job {
                    Ok(job) => job(),
                    Err(_) => return,
                }
            });
            match spawned {
                Ok(_) => workers += 1,
                Err(e) => eprintln!("makepad index: worker thread {n} not started ({e}); {workers} workers"),
            }
        }
        Pool { jobs: Mutex::new(tx), workers }
    })
}

/// The worker threads there are (the calling thread of a [`fan_out`] helps besides).
pub fn workers() -> usize {
    pool().workers
}

/// Who is inside a fan-out's `f`, and whether its indices are all claimed.
#[derive(Default)]
struct Gate {
    active: usize,
    closed: bool,
}

struct Shared {
    /// The fan-out's body, its borrows erased; valid while the gate is open or a worker is
    /// active (see [`fan_out`]).
    task: &'static (dyn Fn(usize) + Sync),
    len: usize,
    next: AtomicUsize,
    gate: Mutex<Gate>,
    idle: Condvar,
    panic: Mutex<Option<Box<dyn Any + Send>>>,
}

impl Shared {
    fn claim_all(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            if i >= self.len {
                return;
            }
            (self.task)(i);
        }
    }
    fn help(&self) {
        {
            let mut g = self.gate.lock().unwrap_or_else(|p| p.into_inner());
            if g.closed {
                return;
            }
            g.active += 1;
        }
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| self.claim_all())) {
            // stop the others early; the caller resumes this panic
            self.next.store(self.len, Ordering::Relaxed);
            self.panic.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(p);
        }
        let mut g = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        g.active -= 1;
        self.idle.notify_all();
    }
    /// Closes the gate and waits until no worker is inside the task.
    fn close_and_wait(&self) {
        let mut g = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        g.closed = true;
        while g.active > 0 {
            g = self.idle.wait(g).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Closes the gate even when the caller's own share panics, before the task's borrows end.
struct CloseOnDrop<'a>(&'a Shared);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close_and_wait();
    }
}

/// `f(0), .., f(len - 1)`, run on the workers and the calling thread, in index order.
pub fn fan_out<T, F>(len: usize, f: F) -> Vec<T>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    if len <= 1 {
        return (0..len).map(f).collect();
    }
    let slots: Vec<Mutex<Option<T>>> = (0..len).map(|_| Mutex::new(None)).collect();
    let body = |i: usize| {
        let v = f(i);
        *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
    };
    let task: &(dyn Fn(usize) + Sync) = &body;
    // SAFETY: workers reach `task` only through `Shared::help`, which enters only while the
    // gate is open and counts itself active; `CloseOnDrop` closes the gate and waits until no
    // worker is active before this frame (and `body`'s borrows) ends, also on unwind.
    let task: &'static (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(task) };
    let shared = Arc::new(Shared {
        task,
        len,
        next: AtomicUsize::new(0),
        gate: Mutex::new(Gate::default()),
        idle: Condvar::new(),
        panic: Mutex::new(None),
    });
    let p = pool();
    {
        let jobs = p.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..p.workers.min(len - 1) {
            let s = Arc::clone(&shared);
            // a send fails only when every worker is gone; the caller then does it all
            if jobs.send(Box::new(move || s.help())).is_err() {
                break;
            }
        }
    }
    {
        let _close = CloseOnDrop(&shared);
        shared.claim_all();
    }
    if let Some(p) = shared.panic.lock().unwrap_or_else(|p| p.into_inner()).take() {
        resume_unwind(p);
    }
    slots.into_iter().map(|s| s.into_inner().unwrap_or_else(|p| p.into_inner()).expect("every index ran")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_are_in_order_and_equal_serial() {
        let data: Vec<u64> = (0..20_000).collect();
        let par = fan_out(data.len(), |i| data[i] * data[i] + 1);
        let ser: Vec<u64> = data.iter().map(|x| x * x + 1).collect();
        assert_eq!(par, ser);
        assert_eq!(fan_out(0, |i| i), Vec::<usize>::new());
        assert_eq!(fan_out(1, |i| i + 7), [7]);
    }

    #[test]
    fn nested_fan_outs_finish() {
        let sums = fan_out(64, |i| fan_out(100, |j| i * j).into_iter().sum::<usize>());
        assert_eq!(sums, (0..64).map(|i| i * 4950).collect::<Vec<_>>());
    }

    #[test]
    fn a_panic_resumes_in_the_caller_and_the_pool_keeps_working() {
        let r = std::panic::catch_unwind(|| fan_out(1000, |i| if i == 517 { panic!("index 517") } else { i }));
        let msg = r.unwrap_err();
        assert_eq!(msg.downcast_ref::<&str>().copied(), Some("index 517"));
        assert_eq!(fan_out(10, |i| i * 2), (0..10).map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn workers_are_made_once() {
        let a = workers();
        for _ in 0..50 {
            fan_out(100, |i| i);
        }
        assert_eq!(workers(), a);
        assert!(a >= 1);
    }
}
