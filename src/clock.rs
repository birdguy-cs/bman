use jiff::{tz::TimeZone, Timestamp};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[derive(Clone)]
pub struct Clock {
    tz: TimeZone,
    offset: i64,     // seconds from UTC, including DST
    refresh_at: i64, // next time to re-check the offset
}

impl Clock {
    pub fn new() -> Self {
        let mut c = Clock { tz: TimeZone::system(), offset: 0, refresh_at: 0 };
        c.refresh(now_secs());
        c
    }

    // Slow path: runs once every 10 minutes, so DST changes are picked up
    pub fn refresh(&mut self, now: i64) {
        let ts = Timestamp::from_second(now).unwrap();
        self.offset = self.tz.to_offset(ts).seconds() as i64;
        self.refresh_at = now + 600;
    }

    // Fast path: call this on every render
    pub fn now(&mut self) -> (i64, i64, &'static str) {
        let t = now_secs();
        if t >= self.refresh_at {
            self.refresh(t);
        }
        let local = t + self.offset;
        let h = (local / 3600).rem_euclid(24);
        let m = (local / 60).rem_euclid(60);
        match h {
            0 => (12, m, "AM"),
            1..=11 => (h, m, "AM"),
            12 => (12, m, "PM"),
            _ => (h - 12, m, "PM"),
        }
    }
}
