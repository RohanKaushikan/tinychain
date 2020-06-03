use std::convert::TryInto;
use std::ops;
use std::time;

#[derive(Clone)]
pub struct NetworkTime {
    nanos: u128,
}

impl NetworkTime {
    pub fn now() -> NetworkTime {
        NetworkTime::from_nanos(
            time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
    }

    pub fn as_millis(&self) -> u64 {
        const MILLIS_PER_NANO: u128 = 1_000_000;
        (self.nanos / MILLIS_PER_NANO).try_into().unwrap()
    }

    pub fn as_nanos(&self) -> u128 {
        self.nanos
    }

    pub fn from_nanos(nanos: u128) -> NetworkTime {
        NetworkTime { nanos }
    }
}

impl ops::Add<time::Duration> for NetworkTime {
    type Output = Self;

    fn add(self, other: time::Duration) -> Self {
        NetworkTime {
            nanos: self.nanos + other.as_nanos(),
        }
    }
}
