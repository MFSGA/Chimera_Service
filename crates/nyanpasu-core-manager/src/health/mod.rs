//! Health-check policy and transition tracking.

use std::{num::NonZeroU32, time::Duration};

use crate::Error;

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
}
