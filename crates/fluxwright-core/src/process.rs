use std::collections::HashMap;

use sysinfo::{Pid, ProcessesToUpdate, System};

/// RSS of `root` plus every descendant. Shared mappings may be counted more
/// than once; treat the result as an estimate.
#[allow(dead_code)]
pub fn process_tree_rss_bytes(root: u32) -> u64 {
    process_trees_rss_bytes(&[root])
}

/// One sysinfo refresh covering every root. Calling this per-pid on the Tokio
/// runtime stalls the reactor under load.
pub fn process_trees_rss_bytes(roots: &[u32]) -> u64 {
    process_tree_rss_map(roots).values().copied().sum()
}

pub fn process_tree_rss_map(roots: &[u32]) -> HashMap<u32, u64> {
    if roots.is_empty() {
        return HashMap::new();
    }
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    roots
        .iter()
        .copied()
        .map(|r| (r, walk(&sys, Pid::from_u32(r))))
        .collect()
}

fn walk(sys: &System, pid: Pid) -> u64 {
    let mut sum = 0u64;
    if let Some(p) = sys.process(pid) {
        sum += p.memory();
    }
    // Linux lists threads as processes: parent() is their thread-group leader
    // and memory() is the whole process RSS, so counting them multiplies a
    // browser's footprint by its thread count.
    let children: Vec<Pid> = sys
        .processes()
        .iter()
        .filter(|(_, proc)| proc.thread_kind().is_none() && proc.parent() == Some(pid))
        .map(|(id, _)| *id)
        .collect();
    for c in children {
        sum += walk(sys, c);
    }
    sum
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

        let own = current_process_rss_bytes();
        let tree = process_tree_rss_bytes(std::process::id());

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
