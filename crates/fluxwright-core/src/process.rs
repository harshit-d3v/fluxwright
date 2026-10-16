use std::collections::{HashMap, HashSet};

use sysinfo::{Pid, ProcessesToUpdate, System};

/// Memory of `root` plus every descendant, counted so that a tree sums without counting
/// shared pages once per process: see [`footprint`].
#[allow(dead_code)]
pub fn process_tree_memory_bytes(root: u32) -> u64 {
    process_trees_memory_bytes(&[root])
}

/// One sysinfo refresh covering every root. Calling this per-pid on the Tokio
/// runtime stalls the reactor under load.
pub fn process_trees_memory_bytes(roots: &[u32]) -> u64 {
    process_tree_memory_map(roots).values().copied().sum()
}

pub fn process_tree_memory_map(roots: &[u32]) -> HashMap<u32, u64> {
    if roots.is_empty() {
        return HashMap::new();
    }
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    roots
        .iter()
        .copied()
        .map(|r| (r, walk(&sys, r)))
        .collect()
}

fn walk(sys: &System, root: u32) -> u64 {
    // Linux lists threads as processes: parent() is their thread-group leader
    // and memory() is the whole process RSS, so counting them multiplies a
    // browser's footprint by its thread count.
    let procs: Vec<(u32, Option<u32>, u64)> = sys
        .processes()
        .values()
        .filter(|p| p.thread_kind().is_none())
        .map(|p| (p.pid().as_u32(), p.parent().map(|q| q.as_u32()), p.start_time()))
        .collect();
    tree_sum(&procs, root, |pid| sys.process(Pid::from_u32(pid)).map_or(0, |p| footprint(pid, p.memory())))
}

/// `cost` summed over `root` and its descendants; `procs` holds (pid, parent, start time).
/// Windows keeps a dead parent's pid on its children, and a new process can reuse that pid:
/// such a link points at a process younger than the child, and can close a cycle (the old
/// recursive walk overflowed the stack on one). So a child must not predate its parent, and
/// each process counts once.
fn tree_sum(procs: &[(u32, Option<u32>, u64)], root: u32, cost: impl Fn(u32) -> u64) -> u64 {
    let start = |pid: u32| procs.iter().find(|p| p.0 == pid).map_or(0, |p| p.2);
    let mut seen = HashSet::new();
    let mut todo = vec![root];
    let mut sum = 0;
    while let Some(pid) = todo.pop() {
        if !seen.insert(pid) {
            continue;
        }
        sum += cost(pid);
        let born = start(pid);
        todo.extend(procs.iter().filter(|p| p.1 == Some(pid) && p.2 >= born).map(|p| p.0));
    }
    sum
}

/// What one process costs by itself, the measure Chrome calls its memory footprint.
/// Summed RSS counts pages shared between Chrome's processes once per process, which
/// made the memory ceiling and RSS recycling trigger early.
/// Linux: PSS, shared pages split between the processes mapping them.
#[cfg(target_os = "linux")]
fn footprint(pid: u32, rss: u64) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Pss:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map_or(rss, |kb| kb * 1024)
}

/// Windows: private bytes (commit charge), which no other process shares.
#[cfg(windows)]
fn footprint(pid: u32, rss: u64) -> u64 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            return rss;
        }
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = K32GetProcessMemoryInfo(h, &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
        CloseHandle(h);
        if ok == 0 {
            rss
        } else {
            c.PrivateUsage as u64
        }
    }
}

/// ponytail: macOS keeps RSS. Its footprint (phys_footprint) needs task ports Chrome's
/// children do not grant.
#[cfg(not(any(target_os = "linux", windows)))]
fn footprint(_pid: u32, rss: u64) -> u64 {
    rss
}

pub fn current_process_rss_bytes() -> u64 {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.process(Pid::from_u32(std::process::id()))
        .map(|p| p.memory())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn tree_sum_survives_reused_pids() {
        // 1 is the root, 2 and 3 its children, 4 a grandchild. 9 claims parent 1 but started
        // before it (its real parent died and 1 reused the pid). 5 is 2's child and also 1's
        // recorded parent, which closes a cycle.
        let procs = [(1, Some(5), 100), (2, Some(1), 100), (3, Some(1), 120), (4, Some(3), 130),
            (9, Some(1), 50), (5, Some(2), 100)];
        assert_eq!(tree_sum(&procs, 1, |pid| 1 << pid), 0b111110);
    }

    #[test]
    fn tree_rss_does_not_multiply_by_thread_count() {
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
            })
            .collect();

        let me = std::process::id();
        let own = footprint(me, current_process_rss_bytes());
        let tree = process_tree_memory_bytes(me);

        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }

        assert!(tree > 0);
        // No child processes, so the tree is this process alone. With threads
        // miscounted as children it would be at least 9x.
        assert!(
            tree <= own * 2,
            "tree {tree} vs own {own}: threads counted as child processes"
        );
    }
}
