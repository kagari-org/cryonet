use std::time::Duration;

pub(crate) use tokio::time::{Instant, Interval, MissedTickBehavior, interval, timeout};

#[cfg(target_os = "linux")]
pub(crate) fn init_mpls() -> anyhow::Result<()> {
    const PLATFORM_LABELS: &str = "/proc/sys/net/mpls/platform_labels";
    let labels = std::fs::read_to_string(PLATFORM_LABELS)
        .map_err(|err| anyhow::anyhow!("MPLS is not available: {err}"))?;
    if labels.trim() == "0" {
        std::fs::write(PLATFORM_LABELS, "1048575\n")
            .map_err(|err| anyhow::anyhow!("Failed to set net.mpls.platform_labels: {err}"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn init_mpls() -> anyhow::Result<()> {
    Err(anyhow::anyhow!("MPLS is only supported on Linux"))
}

#[cfg(target_os = "linux")]
pub(crate) fn enable_mpls_input(interface: &str) -> anyhow::Result<()> {
    let path = format!("/proc/sys/net/mpls/conf/{interface}/input");
    std::fs::write(&path, "1\n")
        .map_err(|err| anyhow::anyhow!("Failed to enable MPLS input on {interface}: {err}"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn enable_mpls_input(_interface: &str) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("MPLS is only supported on Linux"))
}

pub(crate) fn jitter(period: Duration, ratio: f64) -> Duration {
    let ratio = ratio.clamp(0.0, 1.0);
    period.mul_f64(1.0 - ratio + 2.0 * ratio * rand::random::<f64>())
}

pub(crate) fn jitter_up(period: Duration, ratio: f64) -> Duration {
    let ratio = ratio.clamp(0.0, 1.0);
    period.mul_f64(1.0 + ratio * rand::random::<f64>())
}

pub(crate) fn jitter_down(period: Duration, ratio: f64) -> Duration {
    let ratio = ratio.clamp(0.0, 1.0);
    period.mul_f64(1.0 - ratio * rand::random::<f64>())
}

pub(crate) fn delayed_interval(period: Duration) -> Interval {
    let mut interval = interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

pub(crate) fn random_below(bound: usize) -> usize {
    if bound == 0 {
        0
    } else {
        rand::random_range(0..bound)
    }
}

pub(crate) fn random_mac() -> [u8; 6] {
    let mut mac: [u8; 6] = rand::random();
    mac[0] = (mac[0] & 0xfc) | 0x02;
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_ratio_keeps_the_period() {
        assert_eq!(jitter(Duration::from_secs(3), 0.0), Duration::from_secs(3));
        assert_eq!(
            jitter_up(Duration::from_secs(3), 0.0),
            Duration::from_secs(3)
        );
        assert_eq!(
            jitter_down(Duration::from_secs(3), 0.0),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn jitter_stays_near_the_period() {
        let period = Duration::from_secs(10);
        let mut values = Vec::new();
        for _ in 0..64 {
            let value = jitter(period, 0.2);
            assert!(value >= Duration::from_secs(7) && value <= Duration::from_secs(13));
            values.push(value);

            let value = jitter_up(period, 0.2);
            assert!(value >= period && value <= Duration::from_secs(13));

            let value = jitter_down(period, 0.2);
            assert!(value >= Duration::from_secs(8) && value <= period);
        }
        assert!(values.iter().any(|value| *value != values[0]));
    }
}
