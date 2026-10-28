use std::{cell::Cell, time::Duration};

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{Instant, Interval, MissedTickBehavior, interval, timeout};
#[cfg(target_arch = "wasm32")]
pub use wasmtimer::{
    std::Instant,
    tokio::{Interval, MissedTickBehavior, interval, timeout},
};

pub fn jitter(period: Duration, ratio: f64) -> Duration {
    let ratio = ratio.clamp(0.0, 1.0);
    period.mul_f64(1.0 - ratio + 2.0 * ratio * random_unit())
}

pub fn jitter_up(period: Duration, ratio: f64) -> Duration {
    let ratio = ratio.clamp(0.0, 1.0);
    period.mul_f64(1.0 + ratio * random_unit())
}

pub fn jitter_down(period: Duration, ratio: f64) -> Duration {
    let ratio = ratio.clamp(0.0, 1.0);
    period.mul_f64(1.0 - ratio * random_unit())
}

pub fn delayed_interval(period: Duration) -> Interval {
    let mut interval = interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn now_ms() -> u64 {
    web_sys::js_sys::Date::now() as u64
}

pub(crate) fn random_u64() -> u64 {
    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }
    STATE.with(|state| {
        let mut value = state.get();
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        state.set(value);
        value
    })
}

fn random_unit() -> f64 {
    (random_u64() >> 11) as f64 / (1u64 << 53) as f64
}

pub(crate) fn random_below(bound: usize) -> usize {
    if bound == 0 {
        0
    } else {
        (random_unit() * bound as f64) as usize
    }
}

pub(crate) fn random_mac() -> [u8; 6] {
    let bytes = random_u64().to_be_bytes();
    let mut mac = [bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]];
    mac[0] = (mac[0] & 0xfc) | 0x02;
    mac
}

fn seed() -> u64 {
    let value = uuid::Uuid::new_v4().as_u128();
    let seed = (value as u64) ^ ((value >> 64) as u64);
    if seed == 0 {
        0x9e37_79b9_7f4a_7c15
    } else {
        seed
    }
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
