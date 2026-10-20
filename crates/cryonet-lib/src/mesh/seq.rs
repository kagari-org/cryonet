use std::{
    cmp::Ordering,
    fmt::Display,
    ops::{Add, AddAssign, Sub, SubAssign},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seq(pub u16);

impl Display for Seq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Add for Seq {
    type Output = Seq;

    fn add(self, rhs: Self) -> Self::Output {
        Seq(self.0.wrapping_add(rhs.0))
    }
}

impl AddAssign for Seq {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Seq {
    type Output = Seq;

    fn sub(self, rhs: Self) -> Self::Output {
        Seq(self.0.wrapping_sub(rhs.0))
    }
}

impl SubAssign for Seq {
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl PartialOrd for Seq {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Seq {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.0 == other.0 {
            Ordering::Equal
        } else if other.0.wrapping_sub(self.0) & 0x8000 == 0 {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqMetric {
    pub seq: Seq,
    pub metric: u32,
}

impl SeqMetric {
    pub fn feasible(&self, rhs: &Self) -> bool {
        self.metric == u32::MAX || self.better(rhs)
    }

    pub fn better(&self, rhs: &Self) -> bool {
        self.seq > rhs.seq || (self.seq == rhs.seq && self.metric < rhs.metric)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feasibility_requires_strictly_better_metric() {
        let fd = SeqMetric {
            seq: Seq(10),
            metric: 100,
        };
        assert!(
            SeqMetric {
                seq: Seq(10),
                metric: 99
            }
            .feasible(&fd)
        );
        assert!(
            !SeqMetric {
                seq: Seq(10),
                metric: 100
            }
            .feasible(&fd)
        );
        assert!(
            !SeqMetric {
                seq: Seq(10),
                metric: 101
            }
            .feasible(&fd)
        );
    }

    #[test]
    fn newer_seq_is_feasible_even_with_worse_metric() {
        let fd = SeqMetric {
            seq: Seq(10),
            metric: 100,
        };
        assert!(
            SeqMetric {
                seq: Seq(11),
                metric: 1000
            }
            .feasible(&fd)
        );
        assert!(
            !SeqMetric {
                seq: Seq(9),
                metric: 1
            }
            .feasible(&fd)
        );
    }

    #[test]
    fn retraction_is_always_feasible() {
        let fd = SeqMetric {
            seq: Seq(10),
            metric: 100,
        };
        assert!(
            SeqMetric {
                seq: Seq(1),
                metric: u32::MAX
            }
            .feasible(&fd)
        );
    }

    #[test]
    fn better_orders_by_seq_then_metric() {
        let fd = SeqMetric {
            seq: Seq(10),
            metric: 100,
        };
        assert!(
            SeqMetric {
                seq: Seq(11),
                metric: 1000
            }
            .better(&fd)
        );
        assert!(
            SeqMetric {
                seq: Seq(10),
                metric: 99
            }
            .better(&fd)
        );
        assert!(
            !SeqMetric {
                seq: Seq(10),
                metric: 100
            }
            .better(&fd)
        );
        assert!(
            !SeqMetric {
                seq: Seq(9),
                metric: 1
            }
            .better(&fd)
        );
    }
}
