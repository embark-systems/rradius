//! The tokio runtime the server runs on, and `[runtime] pin_threads`: each worker thread bound to
//! one CPU. The runtime has to exist before the configuration is read, since SIGHUP is handled
//! from the very start, so workers are pinned after they started, by the thread IDs they
//! recorded then. Threads started afterwards get back the CPUs the process had.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// Worker threads of the runtime: one per CPU the process may use.
pub fn worker_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Threads started so far; the first `worker_threads()` are the workers. `build` waits for them
/// before it returns, so no code of ours can start another thread ahead of them.
static STARTED: AtomicUsize = AtomicUsize::new(0);
/// The workers' thread IDs, for pinning them later, and the condition `build` waits on.
static WORKERS: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static WORKERS_STARTED: Condvar = Condvar::new();
/// Set once workers are pinned: the CPUs the process had, for the threads started after.
static ORIGINAL_CPUS: OnceLock<Vec<usize>> = OnceLock::new();

/// The multi-threaded runtime, with `worker_threads()` workers that record themselves. Returns
/// once every worker has.
pub fn build() -> std::io::Result<tokio::runtime::Runtime> {
    let workers = worker_threads();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .on_thread_start(move || {
            if STARTED.fetch_add(1, Ordering::SeqCst) < workers {
                let mut recorded = WORKERS.lock().unwrap_or_else(PoisonError::into_inner);
                // Recorded even without an ID, so that `build` stops waiting.
                recorded.push(os::thread_id().unwrap_or(-1));
                WORKERS_STARTED.notify_all();
            } else if let Some(cpus) = ORIGINAL_CPUS.get() {
                // A new thread takes the CPUs of the one starting it, which may be a pinned worker.
                let _ = os::set_cpus(0, cpus);
            }
        })
        .build()?;
    let recorded = WORKERS.lock().unwrap_or_else(PoisonError::into_inner);
    // Workers start at once; the limit only keeps a stuck start from hanging the server.
    let _ = WORKERS_STARTED.wait_timeout_while(recorded, Duration::from_secs(10), |recorded| recorded.len() < workers);
    Ok(runtime)
}

/// Which CPU each of `workers` gets, in turn over `allowed`.
pub fn pin_plan(allowed: &[usize], workers: usize) -> Vec<usize> {
    if allowed.is_empty() {
        return Vec::new();
    }
    (0..workers).map(|i| allowed[i % allowed.len()]).collect()
}

/// Binds each recorded worker thread to one of the CPUs the process may use. Returns the CPUs
/// in the workers' order.
/// If one worker cannot be pinned, none stays pinned.
pub fn pin_workers() -> Result<Vec<usize>, String> {
    let allowed = os::cpus(0).ok_or("cannot read which CPUs the process may use")?;
    let workers = WORKERS.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if workers.len() != worker_threads() || workers.contains(&-1) {
        return Err(format!("{} of {} worker threads were recorded", workers.iter().filter(|id| **id != -1).count(), worker_threads()));
    }
    let _ = ORIGINAL_CPUS.set(allowed.clone());
    let plan = pin_plan(&allowed, workers.len());
    for (i, (worker, cpu)) in workers.iter().zip(&plan).enumerate() {
        if let Err(e) = os::set_cpus(*worker, &[*cpu]) {
            for pinned in &workers[..i] {
                let _ = os::set_cpus(*pinned, &allowed);
            }
            return Err(format!("cannot pin worker thread {worker} to CPU {cpu}: {e}"));
        }
    }
    Ok(plan)
}

#[cfg(target_os = "linux")]
mod os {
    /// The calling thread's ID, as the kernel knows it.
    pub fn thread_id() -> Option<i32> {
        // SAFETY: gettid has no arguments and cannot fail.
        Some(unsafe { libc::gettid() })
    }

    /// The CPUs thread `id` may use; 0 is the calling thread.
    pub fn cpus(id: i32) -> Option<Vec<usize>> {
        // SAFETY: sched_getaffinity writes a cpu_set_t we own, of the size we give.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(id, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
                return None;
            }
            Some((0..libc::CPU_SETSIZE as usize).filter(|cpu| libc::CPU_ISSET(*cpu, &set)).collect())
        }
    }

    pub fn set_cpus(id: i32, cpus: &[usize]) -> std::io::Result<()> {
        // SAFETY: sched_setaffinity reads a cpu_set_t we own, of the size we give.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            for cpu in cpus {
                libc::CPU_SET(*cpu, &mut set);
            }
            if libc::sched_setaffinity(id, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Elsewhere there is nothing to pin with; the configuration refuses `pin_threads` there.
#[cfg(not(target_os = "linux"))]
mod os {
    pub fn thread_id() -> Option<i32> {
        None
    }

    pub fn cpus(_id: i32) -> Option<Vec<usize>> {
        None
    }

    pub fn set_cpus(_id: i32, _cpus: &[usize]) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "pinning threads needs Linux"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pin_plan() {
        assert_eq!(pin_plan(&[0, 1, 2, 3], 4), vec![0, 1, 2, 3]);
        assert_eq!(pin_plan(&[2, 5, 7], 5), vec![2, 5, 7, 2, 5], "more workers than CPUs: in turn");
        assert_eq!(pin_plan(&[4, 6], 1), vec![4]);
        assert!(pin_plan(&[], 3).is_empty());
    }
}
