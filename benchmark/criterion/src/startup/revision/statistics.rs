use super::super::observer::Observation;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

// Join Criterion's saved statistical samples to exact callback durations rather
// than guessing which callbacks were calibration from their position/count.
pub(super) fn formal(
    samples: &Value,
    callbacks: &[Value],
    observations: &[Observation],
    metric: &str,
) -> Result<Value> {
    let iters = samples["iters"]
        .as_array()
        .context("Criterion sample iterations")?;
    let times = samples["times"]
        .as_array()
        .context("Criterion sample times")?;
    ensure!(
        iters.len() == 60 && times.len() == iters.len(),
        "require 60 Criterion samples"
    );
    let mut used = BTreeSet::new();
    let mut values = Vec::new();
    let mut mapping = Vec::new();
    for (iterations, time) in iters.iter().zip(times) {
        let candidates: Vec<_> = callbacks
            .iter()
            .enumerate()
            .filter(|(_, value)| {
                value["iterations"]
                    .as_u64()
                    .is_some_and(|n| Some(n as f64) == iterations.as_f64())
                    && value["nanoseconds"]
                        .as_u64()
                        .is_some_and(|n| Some(n as f64) == time.as_f64())
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "ambiguous or absent Criterion callback mapping"
        );
        let (callback, value) = candidates[0];
        ensure!(used.insert(callback), "duplicate Criterion callback");
        let first = value["first"].as_u64().context("first trial")? as usize;
        let end = value["end"].as_u64().context("last trial")? as usize;
        ensure!(
            end > first
                && end <= observations.len()
                && end - first == value["iterations"].as_u64().unwrap() as usize,
            "invalid callback observation range"
        );
        mapping.push(json!({"callback":callback,"first":first,"end":end}));
        values.extend(observations[first..end].iter().map(|o| {
            if metric == "ready" {
                o.ready_ns
            } else {
                o.exit_ns
            }
        }));
    }
    let mut sorted = values.clone();
    sorted.sort_unstable();
    let count = sorted.len();
    let mean = sorted.iter().map(|v| *v as f64).sum::<f64>() / count as f64;
    let median = (sorted[(count - 1) / 2] as f64 + sorted[count / 2] as f64) / 2.0;
    let deviation = (sorted
        .iter()
        .map(|v| (*v as f64 - mean).powi(2))
        .sum::<f64>()
        / (count - 1) as f64)
        .sqrt();
    Ok(
        json!({"metric":metric,"criterion_samples":iters.len(),"operations":count,
        "median_ms":median/1e6,"mean_ms":mean/1e6,"sample_stddev_ms":deviation/1e6,
        "min_ms":sorted[0] as f64/1e6,"max_ms":sorted[count-1] as f64/1e6,
        "observations_ns":values,"mapping":mapping,"excludes_calibration":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formal_samples_exclude_calibration_and_keep_slow_operations() {
        let observations: Vec<_> = (0..61)
            .map(|i| Observation {
                ready_ns: i + 1,
                exit_ns: i + 2,
                exit_code: 0,
                events: vec![],
            })
            .collect();
        let callbacks: Vec<_> = (0..61)
            .map(|i| json!({"first":i,"end":i+1,"iterations":1,"nanoseconds":i+1}))
            .collect();
        let samples = json!({"iters":vec![1;60],"times":(2..=61).collect::<Vec<_>>()});
        let result = formal(&samples, &callbacks, &observations, "ready").unwrap();
        assert_eq!(result["operations"], 60);
        assert_eq!(result["observations_ns"][0], 2);
        assert_eq!(result["observations_ns"][59], 61);
        assert_eq!(result["median_ms"], 31.5 / 1e6);
        let mut ambiguous = callbacks;
        ambiguous.push(ambiguous[1].clone());
        assert!(formal(&samples, &ambiguous, &observations, "ready").is_err());
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    #[test]
    fn multi_operation_samples_use_individual_clocks_instead_of_batch_averages() {
        let observations: Vec<_> = (0..120)
            .map(|i| Observation {
                ready_ns: if i % 2 == 0 { 1 } else { 99 },
                exit_ns: 200,
                exit_code: 0,
                events: vec![],
            })
            .collect();
        // Distinct durations make the saved sample/callback join unambiguous;
        // use Exit clocks for this sample stream and retain varied Ready clocks.
        let mut observations = observations;
        for (i, o) in observations.iter_mut().enumerate() {
            o.exit_ns += i as u64;
        }
        let callbacks: Vec<_> = (0..60)
            .map(|i| {
                json!({"first":i*2,"end":i*2+2,"iterations":2,
            "nanoseconds":observations[i*2].exit_ns+observations[i*2+1].exit_ns})
            })
            .collect();
        let samples = json!({"iters":vec![2;60],"times":callbacks.iter().map(|v|v["nanoseconds"].clone()).collect::<Vec<_>>()});
        let result = formal(&samples, &callbacks, &observations, "exit").unwrap();
        assert_eq!(result["operations"], 120);
        assert_eq!(result["min_ms"], 200.0 / 1e6);
        assert_eq!(result["max_ms"], 319.0 / 1e6);
        assert_eq!(result["median_ms"], 259.5 / 1e6);
        let mut missing = samples;
        missing["times"][0] = 999999.into();
        assert!(formal(&missing, &callbacks, &observations, "exit").is_err());
    }
}
