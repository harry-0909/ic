use crate::{
    chart::Chart, collector::RequestInfo, content_length::ContentLength, ChartSize, RequestType,
};
use std::{cmp, collections::HashMap, fmt, time::Duration};

use serde::Serialize;

trait ToMilliseconds {
    fn to_ms(&self) -> f64;
}

impl ToMilliseconds for Duration {
    fn to_ms(&self) -> f64 {
        (self.as_secs() as f64 * 1_000f64) + (f64::from(self.subsec_nanos()) / 1_000_000f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MS(f64);
impl From<Duration> for MS {
    fn from(d: Duration) -> MS {
        let ms = (d.as_secs() as f64 * 1_000f64) + (f64::from(d.subsec_nanos()) / 1_000_000f64);
        MS(ms)
    }
}

impl From<MS> for Duration {
    fn from(ms: MS) -> Duration {
        let MS(ms) = ms;
        let duration = Duration::from_millis(ms.trunc() as u64);
        let nanos = (ms.fract() * 1_000_000f64) as u32;
        duration + Duration::new(0, nanos)
    }
}

/// A single datum or "fact" about the requests
#[derive(Debug)]
pub struct Fact {
    status: u16,
    duration: Duration,
    content_length: ContentLength,
    success: bool,
    request_type: RequestType,
}

impl Fact {
    pub fn record(
        content_length: ContentLength,
        status: u16,
        duration: Duration,
        // For context: some status codes were considered success others not. If
        // we had a single failure the binary will return a non-zero code.
        // Returning a non-zero code is used to determine if the binary crashed
        // or got kill. If there is a problem with the returned requests we
        // should catch it by the metrics or the returned summary.
        success: bool,
        request_type: RequestType,
    ) -> Fact {
        Fact {
            status,
            duration,
            content_length,
            success,
            request_type,
        }
    }
}
impl RequestInfo for Fact {
    fn is_succ(&self) -> bool {
        self.success
    }
}

struct DurationStats {
    sorted: Vec<Duration>,
}

impl DurationStats {
    fn from_facts(facts: &[Fact]) -> DurationStats {
        let mut sorted: Vec<Duration> = facts.iter().map(|f| f.duration).collect();
        sorted.sort();
        Self { sorted }
    }

    fn max(&self) -> Option<Duration> {
        self.sorted.last().cloned()
    }

    fn min(&self) -> Option<Duration> {
        self.sorted.first().cloned()
    }

    fn median(&self) -> Duration {
        let mid = self.sorted.len() / 2;
        if self.sorted.len() % 2 == 0 {
            // even
            (self.sorted[mid - 1] + self.sorted[mid]) / 2
        } else {
            // odd
            self.sorted[mid]
        }
    }

    fn average(&self) -> Duration {
        self.total() / (self.sorted.len() as u32)
    }

    fn stddev(&self) -> Duration {
        let mean = self.average();
        let MS(mean) = mean.into();
        let summed_squares = self.sorted.iter().fold(0f64, |acc, duration| {
            let MS(ms) = (*duration).into();
            acc + (ms - mean).powi(2)
        });
        let ratio = summed_squares / (self.sorted.len() - 1) as f64;
        let std_ms = ratio.sqrt();
        MS(std_ms).into()
    }

    fn latency_histogram(&self) -> Vec<u32> {
        let mut latency_histogram = vec![0; 100];

        if let Some(max) = self.max() {
            let bin_size = max.to_ms() / 100.;

            for duration in &self.sorted {
                let index = (duration.to_ms() / bin_size) as usize;
                latency_histogram[cmp::min(index, 49)] += 1;
            }
        }
        latency_histogram
    }

    fn percentiles(&self) -> Vec<Duration> {
        (0..100)
            .map(|n| {
                let mut index = ((f64::from(n) / 100.0) * (self.sorted.len() as f64)) as usize;
                index = cmp::max(index, 0);
                index = cmp::min(index, self.sorted.len() - 1);
                self.sorted[index]
            })
            .collect()
    }

    fn total(&self) -> Duration {
        self.sorted.iter().sum()
    }
}

/// Represents the statistics around a given set of facts.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    average: Duration,
    median: Duration,
    max: Duration,
    min: Duration,
    stddev: Duration,
    count: u32,
    content_length: ContentLength,
    percentiles: Vec<Duration>,
    latency_histogram: Vec<u32>,
    status_counts: HashMap<u16, u32>,
    #[serde(skip_serializing)]
    chart_size: ChartSize,
}

impl Summary {
    /// From a set of facts, calculate the statistics.
    pub fn from_facts(facts: &[Fact]) -> Summary {
        if facts.is_empty() {
            return Summary::zero();
        }
        let content_length = Self::total_content_length(facts);
        let count = facts.len() as u32;
        let status_counts = facts.iter().fold(
            HashMap::with_capacity(699),
            |mut acc: HashMap<u16, u32>, fact| {
                let count = if let Some(current) = acc.get(&fact.status) {
                    current + 1
                } else {
                    1
                };
                acc.insert(fact.status, count);
                acc
            },
        );

        Summary {
            count,
            content_length,
            status_counts,
            ..Summary::from_durations(&DurationStats::from_facts(facts))
        }
    }

    #[allow(dead_code)]
    pub fn content_length(self) -> ContentLength {
        self.content_length
    }

    pub fn with_chart_size(mut self, size: ChartSize) -> Self {
        self.chart_size = size;
        self
    }

    fn from_durations(stats: &DurationStats) -> Summary {
        let average = stats.average();
        let stddev = stats.stddev();
        let median = stats.median();
        let min = stats.min().expect("Returned early if empty");
        let max = stats.max().expect("Returned early if empty");
        let latency_histogram = stats.latency_histogram();
        let percentiles = stats.percentiles();

        Summary {
            average,
            median,
            max,
            min,
            stddev,
            percentiles,
            latency_histogram,
            ..Summary::zero()
        }
    }

    fn zero() -> Summary {
        Summary {
            average: Duration::new(0, 0),
            stddev: Duration::new(0, 0),
            median: Duration::new(0, 0),
            max: Duration::new(0, 0),
            min: Duration::new(0, 0),
            count: 0,
            content_length: ContentLength::zero(),
            percentiles: vec![Duration::new(0, 0); 100],
            latency_histogram: vec![0; 0],
            status_counts: HashMap::new(),
            chart_size: ChartSize::Medium,
        }
    }

    fn total_content_length(facts: &[Fact]) -> ContentLength {
        facts.iter().fold(ContentLength::zero(), |len, fact| {
            len + &fact.content_length
        })
    }

    fn chart<T>(&self, vec: &[T]) -> String
    where
        T: Copy + Into<f64>,
    {
        let (height, scale) = match self.chart_size {
            ChartSize::None => return String::new(),
            ChartSize::Small => (7, 3),
            ChartSize::Medium => (10, 2),
            ChartSize::Large => (20, 1),
        };
        Chart::new().height(height).make(&scale_array(vec, scale))
    }
}

fn scale_array<T>(vec: &[T], scale_array: usize) -> Vec<T>
where
    T: Copy,
{
    vec.iter()
        .enumerate()
        .filter(|&(i, _)| i % scale_array == 0)
        .map(|(_, v)| *v)
        .collect()
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Summary")?;
        writeln!(
            f,
            "  Average:   {} ms (std: {} ms)",
            self.average.to_ms(),
            self.stddev.to_ms()
        )?;
        writeln!(f, "  Median:    {} ms", self.median.to_ms())?;
        writeln!(f, "  Longest:   {} ms", self.max.to_ms())?;
        writeln!(f, "  Shortest:  {} ms", self.min.to_ms())?;
        writeln!(f, "  Requests:  {}", self.count)?;
        writeln!(f, "  Data:      {}", self.content_length)?;
        writeln!(f)?;
        writeln!(f, "Status codes:")?;
        writeln!(f, "https://gitlab.com/dfinity-lab/core/ic/tree/master/rs/workload_generator#summary-status-counts")?;
        let mut status_counts: Vec<(&u16, &u32)> = self.status_counts.iter().collect();
        status_counts.sort_by(|&(&code_a, _), &(&code_b, _)| code_a.cmp(&code_b));
        for (k, v) in status_counts {
            let desc = match k {
                0 => "workload generator: request not submitted",
                11 => "workload generator: update send failed",
                33 => "workload generator: update request status rejected",
                44 => {
                    "workload generator: timed out before update reuqest status rejected or replied"
                }
                _ => "HTTP status code",
            };
            writeln!(f, "  {:4.}: {:10.}   {}", k, v, desc)?;
        }
        if self.chart_size != ChartSize::None {
            writeln!(f)?;
            writeln!(f, "Latency Percentiles (2% of requests per bar):")?;
            let percentiles: Vec<f64> = self.percentiles.iter().map(|d| d.to_ms()).collect();
            writeln!(f, "{}", self.chart(&percentiles))?;
            writeln!(f)?;
            writeln!(f, "Latency Histogram (each bar is 2% of max latency)")?;
            writeln!(f, "{}", self.chart(&self.latency_histogram))?;
        }
        Ok(())
    }
}
