use chrono::{DateTime, Duration as ChronoDuration, Utc};
use khive_pack_schedule::repeat::{parse_repeat, Repeat};
use std::hint::black_box;
use std::time::{Duration, Instant};

pub const STEPS: usize = 1_000;

pub struct Fixture {
    pub base: DateTime<Utc>,
    repeat: Repeat,
    expected: Vec<DateTime<Utc>>,
}

impl Fixture {
    pub fn new() -> Self {
        let base = "2100-01-01T00:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("fixed UTC base");
        let repeat = parse_repeat("0 * * * *").expect("public hourly UTC cron parser");
        assert!(matches!(&repeat, Repeat::Cron(_)));
        let expected = (1..=STEPS)
            .map(|index| base + ChronoDuration::hours(index as i64))
            .collect();
        Self {
            base,
            repeat,
            expected,
        }
    }

    pub fn sample(&self, first_after: bool, output: &mut [DateTime<Utc>]) -> Duration {
        assert_eq!(output.len(), STEPS);
        let mut previous = self.base;
        let start = Instant::now();
        if first_after {
            for slot in output.iter_mut() {
                let now = previous + ChronoDuration::minutes(30);
                previous = self
                    .repeat
                    .first_after(black_box(previous), black_box(now))
                    .expect("next cron occurrence after now");
                *slot = previous;
            }
        } else {
            for slot in output.iter_mut() {
                previous = self
                    .repeat
                    .next_after(black_box(previous))
                    .expect("next cron occurrence");
                *slot = previous;
            }
        }
        let elapsed = start.elapsed();
        assert_eq!(output, self.expected.as_slice());
        assert_eq!(previous, self.base + ChronoDuration::hours(STEPS as i64));
        assert!(output.windows(2).all(|pair| pair[0] < pair[1]));
        elapsed
    }
}
