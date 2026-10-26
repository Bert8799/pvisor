//! Diagnostic observations of one native pause/resume, never admission credit.

use super::*;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sample {
    current_bytes: u64,
    swap_bytes: u64,
    stat: BTreeMap<String, u64>,
    events: BTreeMap<String, u64>,
}

impl Sample {
    fn read(group: &Group) -> Result<Self> {
        let read = |name| -> Result<String> {
            let raw = group.read(name)?;
            ensure!(raw.len() < 4096, "memory counter sample was truncated");
            Ok(raw)
        };
        Ok(Self {
            current_bytes: read("memory.current")?.trim().parse()?,
            swap_bytes: read("memory.swap.current")?.trim().parse()?,
            stat: counters(&read("memory.stat")?)?,
            events: counters(&read("memory.events")?)?,
        })
    }
}

fn counters(raw: &str) -> Result<BTreeMap<String, u64>> {
    let mut result = BTreeMap::new();
    for line in raw.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        ensure!(fields.len() == 2, "invalid memory counter");
        ensure!(
            result
                .insert(fields[0].into(), fields[1].parse()?)
                .is_none(),
            "duplicate memory counter"
        );
    }
    Ok(result)
}

fn usable(before: &Sample, after: &Sample) -> bool {
    before.swap_bytes == 0
        && after.swap_bytes == 0
        && ["anon", "file", "kernel"]
            .iter()
            .all(|key| before.stat.contains_key(*key) && after.stat.contains_key(*key))
        && ["max", "oom", "oom_kill"].iter().all(|key| {
            matches!((before.events.get(*key), after.events.get(*key)),
                (Some(first), Some(last)) if first == last)
        })
}

pub(super) struct Observation {
    started: Instant,
    group: Option<Group>,
    before: Option<Sample>,
    report: serde_json::Value,
}

impl Observation {
    pub(super) fn begin(identity: &Identity, directory: &Path, pause: bool) -> Result<Self> {
        // Opening and enforcing the identity binding happens before sampling.
        // Missing diagnostic data is explicit, and does not fabricate savings.
        let group = Group::open(identity).ok().flatten();
        let before = group.as_ref().and_then(|group| {
            group.verify_limits(&identity.spec).ok()?;
            Sample::read(group).ok()
        });
        let run: Option<RunRecord> = read_json(&directory.join("run.json")).ok();
        let run = run.filter(|run| run.generation == identity.generation);
        let report = serde_json::json!({
            "schema": "pvisor-memory-transition/v1",
            "transition_id": uuid::Uuid::new_v4().to_string(),
            "recorded_at": chrono::Utc::now(),
            "sandbox_id": identity.spec.id,
            "generation": identity.generation,
            "run_id": run.as_ref().map(|run| &run.run_id),
            "attempt_id": run.as_ref().map(|run| &run.attempt_id),
            "cgroup_device": identity.cgroup_device,
            "cgroup_inode": identity.cgroup_inode,
            "policy": identity.pause_memory_policy,
            "operation": if pause { "pause" } else { "resume" },
            "before": before,
            "after": null,
            "completed": false,
            "measurement_valid": false,
            "observed_reduction_bytes": null,
            "admission_credit_bytes": 0,
        });
        replace(directory, &report)?;
        Ok(Self {
            started: Instant::now(),
            group,
            before,
            report,
        })
    }

    pub(super) fn finish(mut self, directory: &Path, state: RuntimeState) -> Result<()> {
        let expected = if self.report["operation"] == "pause" {
            RuntimeState::Paused
        } else {
            RuntimeState::Running
        };
        ensure!(
            state == expected,
            "native memory transition was not confirmed"
        );
        let after = self
            .group
            .as_ref()
            .and_then(|group| Sample::read(group).ok());
        let valid = self.report["attempt_id"].is_string()
            && matches!((&self.before, &after), (Some(before), Some(after)) if usable(before, after));
        if valid {
            let before = self.before.as_ref().unwrap();
            let after = after.as_ref().unwrap();
            self.report["observed_reduction_bytes"] =
                serde_json::json!(before.current_bytes.saturating_sub(after.current_bytes));
        }
        self.report["after"] = serde_json::to_value(after)?;
        self.report["completed"] = serde_json::json!(true);
        self.report["measurement_valid"] = serde_json::json!(valid);
        self.report["confirmed_state"] = serde_json::to_value(state)?;
        self.report["elapsed_seconds"] = serde_json::json!(self.started.elapsed().as_secs_f64());
        replace(directory, &self.report)
    }
}

// Only the latest transition is retained. Pending replaces prior successful
// observations before dispatch, so a failed/lost ACK leaves no apparent success.
fn replace(directory: &Path, report: &serde_json::Value) -> Result<()> {
    let temporary = directory.join(format!(".memory-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        publish_new(&temporary, report)?;
        fs::rename(&temporary, directory.join("memory-transition.json"))?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(current: u64) -> Sample {
        Sample {
            current_bytes: current,
            swap_bytes: 0,
            stat: counters("anon 10\nfile 20\nkernel 30\n").unwrap(),
            events: counters("max 0\noom 0\noom_kill 0\n").unwrap(),
        }
    }

    #[test]
    fn missing_counters_swap_and_limit_events_are_not_valid_reclamation_evidence() {
        let before = sample(100);
        let mut after = sample(50);
        assert!(usable(&before, &after));
        after.swap_bytes = 1;
        assert!(!usable(&before, &after));
        after.swap_bytes = 0;
        after.events.insert("max".into(), 1);
        assert!(!usable(&before, &after));
        after.events.clear();
        assert!(!usable(&before, &after));
        assert!(counters("anon 1\nanon 2").is_err());
        assert!(counters("oom nope").is_err());
    }

    fn fixture() -> (tempfile::TempDir, Identity, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let (identity, directory) = super::super::tests::identity_fixture(temp.path());
        publish_new(
            &directory.join("run.json"),
            &RunRecord {
                generation: identity.generation.clone(),
                run_id: "run-1".into(),
                attempt_id: "attempt-1".into(),
            },
        )
        .unwrap();
        (temp, identity, directory)
    }

    #[test]
    fn unconfirmed_transition_replaces_previous_success_with_pending_evidence() {
        let (_temp, identity, directory) = fixture();
        replace(&directory, &serde_json::json!({"completed": true})).unwrap();
        let _pending = Observation::begin(&identity, &directory, true).unwrap();
        let report: serde_json::Value =
            read_json(&directory.join("memory-transition.json")).unwrap();
        assert_eq!(report["completed"], false);
        assert_eq!(report["measurement_valid"], false);
        assert_eq!(report["observed_reduction_bytes"], serde_json::Value::Null);
        assert_eq!(report["admission_credit_bytes"], 0);
        assert_eq!(
            directory
                .join("memory-transition.json")
                .metadata()
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn paused_without_measurement_or_wrong_confirmation_never_claims_savings() {
        let (_temp, identity, directory) = fixture();
        Observation::begin(&identity, &directory, true)
            .unwrap()
            .finish(&directory, RuntimeState::Paused)
            .unwrap();
        let report: serde_json::Value =
            read_json(&directory.join("memory-transition.json")).unwrap();
        assert_eq!(report["completed"], true);
        assert_eq!(report["measurement_valid"], false);
        assert_eq!(report["observed_reduction_bytes"], serde_json::Value::Null);
        assert!(
            Observation::begin(&identity, &directory, true)
                .unwrap()
                .finish(&directory, RuntimeState::Running)
                .is_err()
        );
        let report: serde_json::Value =
            read_json(&directory.join("memory-transition.json")).unwrap();
        assert_eq!(report["completed"], false);
    }

    #[test]
    fn charge_reduction_is_observed_separately_from_admission_credit_and_resume_growth() {
        let (_temp, identity, directory) = fixture();
        // Synthetic controller files exercise report handling, not cgroup
        // enforcement. Production opens only the inode-bound cgroup filesystem.
        let group = directory.join("synthetic-group");
        fs::create_dir(&group).unwrap();
        fs::write(group.join("memory.current"), "40").unwrap();
        fs::write(group.join("memory.swap.current"), "0").unwrap();
        fs::write(group.join("memory.stat"), "anon 10\nfile 20\nkernel 10\n").unwrap();
        fs::write(group.join("memory.events"), "max 0\noom 0\noom_kill 0\n").unwrap();
        for (pause, before, state, expected) in [
            (true, 100, RuntimeState::Paused, 60),
            (false, 20, RuntimeState::Running, 0),
        ] {
            let mut observation = Observation::begin(&identity, &directory, pause).unwrap();
            observation.before = Some(sample(before));
            observation.report["before"] = serde_json::to_value(&observation.before).unwrap();
            observation.group = Some(Group(File::open(&group).unwrap()));
            observation.finish(&directory, state).unwrap();
            let report: serde_json::Value =
                read_json(&directory.join("memory-transition.json")).unwrap();
            assert_eq!(report["measurement_valid"], true);
            assert_eq!(report["observed_reduction_bytes"], expected);
            assert_eq!(report["admission_credit_bytes"], 0);
            assert_eq!(report["after"]["current_bytes"], 40);
        }
    }
}
