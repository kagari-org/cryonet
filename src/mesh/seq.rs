use std::{cmp::Ordering, ops::{Add, AddAssign, Sub, SubAssign}};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seq(pub(crate) u16);

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
pub(crate) struct SeqMetric {
    pub(crate) seq: Seq,
    pub(crate) metric: u32,
}

impl SeqMetric {
    pub(crate) fn feasible(&self, &rhs: &Self) -> bool {
        if self.seq > rhs.seq {
            return true
        }
        if self.seq == rhs.seq && (self.metric <= rhs.metric || self.metric == u32::MAX) {
            return true
        }
        false
    }
}
