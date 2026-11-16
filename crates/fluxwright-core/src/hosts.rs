//! Per-host limits for `goto`: how many pages may be on one host at once, and how far apart
//! navigations to it start. What a polite crawler needs, kept for the whole fleet.
//! shortcut: only `goto` is counted; a click or redirect that leaves for another host is not.
//! Count requests in the route layer if that starts to matter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::netguard::url_host;

/// The host a lease is on and its place there; dropped with the lease.
pub(crate) type HostSlot = Mutex<Option<(String, OwnedSemaphorePermit)>>;

struct Host {
    slots: Arc<Semaphore>,
    /// When the next navigation to this host may start. The mutex is held across `sleep_until`
    /// so a cancelled `enter` never pushes the next navigation's start time forward.
    next: Arc<tokio::sync::Mutex<Instant>>,
}

pub(crate) struct HostLimits {
    max: Option<usize>,
    min_delay: Option<Duration>,
    hosts: Mutex<HashMap<String, Host>>,
}

/// Idle hosts are forgotten past this many, so a crawl over millions of hosts stays small.
const KEEP: usize = 4096;

impl HostLimits {
    /// 0 pages per host would wait forever, so it means no limit, like a zero delay.
    pub fn new(max: Option<usize>, min_delay: Option<Duration>) -> Self {
        Self {
            max: max.filter(|n| *n > 0),
            min_delay: min_delay.filter(|d| !d.is_zero()),
            hosts: Mutex::default(),
        }
    }

    /// Waits until the lease holding `slot` may navigate to `url`. A lease already on that host
    /// keeps its place; one moving to another host gives its old place up first.
    pub async fn enter(&self, url: &str, slot: &HostSlot) {
        if self.max.is_none() && self.min_delay.is_none() {
            return;
        }
        let Some(host) = url_host(url).filter(|h| !h.is_empty()) else {
            // about:blank, data: and the like: the page is on no host any more.
            *slot.lock().unwrap() = None;
            return;
        };
        if let Some(max) = self.max {
            let here = slot.lock().unwrap().as_ref().is_some_and(|(h, _)| *h == host);
            if !here {
                *slot.lock().unwrap() = None;
                let slots = entry(&mut self.hosts.lock().unwrap(), &host, max).slots.clone();
                // Never closed, so this only ever waits.
                if let Ok(permit) = slots.acquire_owned().await {
                    *slot.lock().unwrap() = Some((host.clone(), permit));
                }
            }
        }
        if let Some(delay) = self.min_delay {
            let next = {
                let mut hosts = self.hosts.lock().unwrap();
                entry(&mut hosts, &host, self.max.unwrap_or(1)).next.clone()
            };
            let mut next = next.lock().await;
            let start = (*next).max(Instant::now());
            tokio::time::sleep_until(start).await;
            *next = Instant::now() + delay;
        }
    }
}

fn entry<'a>(hosts: &'a mut HashMap<String, Host>, host: &str, max: usize) -> &'a mut Host {
    if hosts.len() >= KEEP && !hosts.contains_key(host) {
        let now = Instant::now();
        // In use (a permit or delay lock held or waited for keeps a second reference) or still spacing.
        hosts.retain(|_, h| {
            Arc::strong_count(&h.slots) > 1
                || Arc::strong_count(&h.next) > 1
                || h.next.try_lock().is_ok_and(|t| *t > now)
        });
    }
    hosts.entry(host.to_string()).or_insert_with(|| Host {
        slots: Arc::new(Semaphore::new(max)),
        next: Arc::new(tokio::sync::Mutex::new(Instant::now())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_page_per_host_waits_for_the_place() {
        let limits = Arc::new(HostLimits::new(Some(1), None));
        let first = HostSlot::default();
        limits.enter("https://a.example/1", &first).await;
        // The same lease navigating again on its host, and another host, do not wait.
        limits.enter("https://A.example:443/2", &first).await;
        limits.enter("https://b.example/", &HostSlot::default()).await;

        let second = Arc::new(HostSlot::default());
        let (l, s) = (limits.clone(), second.clone());
        let waiting = tokio::spawn(async move { l.enter("http://a.example/3", &s).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!waiting.is_finished(), "a second page got onto a host limited to one");

        // Leaving the host (here for about:blank) frees the place.
        limits.enter("about:blank", &first).await;
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("the freed place was never taken")
            .unwrap();
        assert_eq!(second.lock().unwrap().as_ref().map(|(h, _)| h.as_str()), Some("a.example"));
    }

    #[tokio::test]
    async fn navigations_to_a_host_start_a_delay_apart() {
        let limits = HostLimits::new(None, Some(Duration::from_millis(80)));
        let start = Instant::now();
        for _ in 0..3 {
            limits.enter("https://a.example/", &HostSlot::default()).await;
        }
        assert!(start.elapsed() >= Duration::from_millis(160), "{:?}", start.elapsed());
        let other = Instant::now();
        limits.enter("https://b.example/", &HostSlot::default()).await;
        assert!(other.elapsed() < Duration::from_millis(60), "{:?}", other.elapsed());

        // Cancelled waits must not push the next navigation's start time forward.
        let limits = Arc::new(HostLimits::new(None, Some(Duration::from_millis(200))));
        limits.enter("https://c.example/", &HostSlot::default()).await;
        let cancelled: Vec<_> = (0..10)
            .map(|_| {
                let l = limits.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(10),
                        l.enter("https://c.example/", &HostSlot::default()),
                    )
                    .await;
                })
            })
            .collect();
        for h in cancelled {
            h.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(210)).await;
        let after = Instant::now();
        limits.enter("https://c.example/", &HostSlot::default()).await;
        assert!(after.elapsed() < Duration::from_millis(100), "after={:?}", after.elapsed());
    }

    #[tokio::test]
    async fn no_limits_and_zero_limits_never_wait() {
        for limits in [HostLimits::new(None, None), HostLimits::new(Some(0), Some(Duration::ZERO))] {
            let slot = HostSlot::default();
            for _ in 0..3 {
                limits.enter("https://a.example/", &HostSlot::default()).await;
            }
            limits.enter("https://a.example/", &slot).await;
            assert!(slot.lock().unwrap().is_none());
        }
    }
}
