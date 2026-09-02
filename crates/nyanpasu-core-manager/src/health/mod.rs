//! Health-check policy and transition tracking.

pub mod probe;

use std::{num::NonZeroU32, time::Duration};

use crate::Error;

pub(crate) const MAX_LAST_ERROR_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthPolicy {
    interval: Duration,
    timeout: Duration,
    failure_threshold: NonZeroU32,
    success_threshold: NonZeroU32,
    start_period: Duration,
}

impl HealthPolicy {
    pub fn new(
        interval: Duration,
        timeout: Duration,
        failure_threshold: NonZeroU32,
        success_threshold: NonZeroU32,
        start_period: Duration,
    ) -> Result<Self, Error> {
        if interval.is_zero() {
            return Err(Error::InvalidHealthPolicy(
                "interval must be greater than zero".into(),
            ));
        }
        if timeout.is_zero() {
            return Err(Error::InvalidHealthPolicy(
                "timeout must be greater than zero".into(),
            ));
        }
        Ok(Self {
            interval,
            timeout,
            failure_threshold,
            success_threshold,
            start_period,
        })
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn failure_threshold(&self) -> NonZeroU32 {
        self.failure_threshold
    }

    pub fn success_threshold(&self) -> NonZeroU32 {
        self.success_threshold
    }

    pub fn start_period(&self) -> Duration {
        self.start_period
    }
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(250),
            timeout: Duration::from_secs(1),
            failure_threshold: NonZeroU32::new(3).expect("non-zero"),
            success_threshold: NonZeroU32::MIN,
            start_period: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackerState {
    Starting,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackerUpdate {
    pub(crate) state: TrackerState,
    pub(crate) transitioned: bool,
    pub(crate) consecutive_failures: u32,
    pub(crate) last_error: Option<String>,
}

pub(crate) struct HealthTracker {
    policy: HealthPolicy,
    started_at: std::time::Instant,
    grace_ended: bool,
    state: TrackerState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    last_error: Option<String>,
}

impl HealthTracker {
    pub(crate) fn new(policy: HealthPolicy, started_at: std::time::Instant) -> Self {
        Self {
            policy,
            started_at,
            grace_ended: false,
            state: TrackerState::Starting,
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_error: None,
        }
    }

    pub(crate) fn observe(
        &mut self,
        now: std::time::Instant,
        result: &probe::ProbeResult,
    ) -> TrackerUpdate {
        let previous = self.state;
        match result {
            probe::ProbeResult::Healthy => {
                self.grace_ended = true;
                self.consecutive_failures = 0;
                self.last_error = None;
                if self.state == TrackerState::Healthy {
                    self.consecutive_successes = 0;
                } else {
                    self.consecutive_successes = self.consecutive_successes.saturating_add(1);
                    if self.consecutive_successes >= self.policy.success_threshold.get() {
                        self.state = TrackerState::Healthy;
                        self.consecutive_successes = 0;
                    }
                }
            }
            probe::ProbeResult::Unhealthy { detail } => {
                self.consecutive_successes = 0;
                self.last_error = detail.as_deref().map(cap_detail);
                let grace_active =
                    !self.grace_ended && now < self.started_at + self.policy.start_period;
                if !grace_active {
                    self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    if self.state != TrackerState::Unhealthy
                        && self.consecutive_failures >= self.policy.failure_threshold.get()
                    {
                        self.state = TrackerState::Unhealthy;
                    }
                }
            }
        }
        TrackerUpdate {
            state: self.state,
            transitioned: previous != self.state,
            consecutive_failures: self.consecutive_failures,
            last_error: self.last_error.clone(),
        }
    }
}

fn cap_detail(detail: &str) -> String {
    if detail.len() <= MAX_LAST_ERROR_BYTES {
        return detail.to_owned();
    }
    let mut end = MAX_LAST_ERROR_BYTES;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_manager_design() {
        let policy = HealthPolicy::default();
        assert_eq!(policy.interval(), Duration::from_millis(250));
        assert_eq!(policy.timeout(), Duration::from_secs(1));
        assert_eq!(policy.failure_threshold().get(), 3);
        assert_eq!(policy.success_threshold().get(), 1);
        assert_eq!(policy.start_period(), Duration::ZERO);
    }

    #[test]
    fn zero_interval_or_timeout_is_rejected() {
        assert!(HealthPolicy::new(
            Duration::ZERO,
            Duration::from_secs(1),
            NonZeroU32::MIN,
            NonZeroU32::MIN,
            Duration::ZERO,
        )
        .is_err());
        assert!(HealthPolicy::new(
            Duration::from_secs(1),
            Duration::ZERO,
            NonZeroU32::MIN,
            NonZeroU32::MIN,
            Duration::ZERO,
        )
        .is_err());
    }

    fn policy(failures: u32, successes: u32, start_period: Duration) -> HealthPolicy {
        HealthPolicy::new(
            Duration::from_millis(10),
            Duration::from_millis(20),
            NonZeroU32::new(failures).unwrap(),
            NonZeroU32::new(successes).unwrap(),
            start_period,
        )
        .unwrap()
    }

    #[test]
    fn thresholds_require_consecutive_results() {
        let started = std::time::Instant::now();
        let mut tracker = HealthTracker::new(policy(2, 2, Duration::ZERO), started);
        let unhealthy = probe::ProbeResult::Unhealthy {
            detail: Some("down".into()),
        };
        let first = tracker.observe(started, &unhealthy);
        assert_eq!(first.state, TrackerState::Starting);
        assert_eq!(first.consecutive_failures, 1);
        let second = tracker.observe(started, &unhealthy);
        assert_eq!(second.state, TrackerState::Unhealthy);
        assert!(second.transitioned);

        let first_success = tracker.observe(started, &probe::ProbeResult::Healthy);
        assert_eq!(first_success.state, TrackerState::Unhealthy);
        let second_success = tracker.observe(started, &probe::ProbeResult::Healthy);
        assert_eq!(second_success.state, TrackerState::Healthy);
        assert!(second_success.transitioned);
    }

    #[test]
    fn start_period_ignores_failures_until_it_expires() {
        let started = std::time::Instant::now();
        let mut tracker = HealthTracker::new(policy(1, 1, Duration::from_secs(5)), started);
        let failure = probe::ProbeResult::Unhealthy {
            detail: Some("warming up".into()),
        };
        let during_grace = tracker.observe(started + Duration::from_secs(4), &failure);
        assert_eq!(during_grace.state, TrackerState::Starting);
        assert_eq!(during_grace.consecutive_failures, 0);
        let after_grace = tracker.observe(started + Duration::from_secs(5), &failure);
        assert_eq!(after_grace.state, TrackerState::Unhealthy);
    }

    #[test]
    fn healthy_result_ends_the_start_period() {
        let started = std::time::Instant::now();
        let mut tracker = HealthTracker::new(policy(1, 1, Duration::from_secs(30)), started);
        assert_eq!(
            tracker
                .observe(started, &probe::ProbeResult::Healthy)
                .state,
            TrackerState::Healthy
        );
        let failure = tracker.observe(
            started + Duration::from_secs(1),
            &probe::ProbeResult::Unhealthy {
                detail: Some("down".into()),
            },
        );
        assert_eq!(failure.state, TrackerState::Unhealthy);
    }

    #[test]
    fn error_detail_is_capped_on_a_utf8_boundary() {
        let detail = "é".repeat(MAX_LAST_ERROR_BYTES);
        let capped = cap_detail(&detail);
        assert!(capped.len() <= MAX_LAST_ERROR_BYTES);
        assert!(capped.is_char_boundary(capped.len()));
    }
}
