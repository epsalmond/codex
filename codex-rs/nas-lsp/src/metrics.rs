use crate::config::Readiness;
use crate::config::WorkspaceConfig;
use codex_lsp_protocol::Operation;
use codex_lsp_protocol::Status;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::Permissions;
use std::io;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const LATENCY_BUCKETS: [f64; 8] = [0.01, 0.025, 0.05, 0.1, 0.25, 1.0, 5.0, 15.0];
const PREFIX: &str = "codex_semantic_navigation";

type LookupKey = (&'static str, &'static str, &'static str);

#[derive(Default)]
struct Histogram {
    buckets: [u64; LATENCY_BUCKETS.len()],
    count: u64,
    sum: f64,
}

#[derive(Default)]
struct Counts {
    lookups: BTreeMap<(LookupKey, &'static str, &'static str), u64>,
    latency: BTreeMap<LookupKey, Histogram>,
}

pub(crate) struct Metrics {
    file: PathBuf,
    synthetic_workspace_ids: HashSet<String>,
    counts: Mutex<Counts>,
}

impl Metrics {
    pub(crate) fn new(file: PathBuf, synthetic_workspace_ids: HashSet<String>) -> Self {
        Self {
            file,
            synthetic_workspace_ids,
            counts: Mutex::default(),
        }
    }

    pub(crate) fn lookup_key(
        &self,
        workspace: Option<&WorkspaceConfig>,
        operation: Operation,
    ) -> LookupKey {
        let (traffic, language) = match workspace {
            Some(workspace) => {
                let traffic = if self.synthetic_workspace_ids.contains(&workspace.id) {
                    "synthetic"
                } else {
                    "operator"
                };
                let language = match workspace.readiness {
                    Readiness::PyreflyLazyBlocking => "python",
                    Readiness::TypeScriptProject => "typescript",
                    Readiness::RustAnalyzerProject => "rust",
                };
                (traffic, language)
            }
            None => ("unknown", "unknown"),
        };
        let operation = match operation {
            Operation::Definition => "definition",
            Operation::References => "references",
        };
        (traffic, language, operation)
    }

    pub(crate) fn record(
        &self,
        key: LookupKey,
        status: Status,
        reason: Option<&str>,
        elapsed: Duration,
    ) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let labels = (key, status_label(status), reason_label(reason));
        *counts.lookups.entry(labels).or_default() += 1;
        if status == Status::Ok {
            let histogram = counts.latency.entry(key).or_default();
            let seconds = elapsed.as_secs_f64();
            histogram.count += 1;
            histogram.sum += seconds;
            for (boundary, bucket) in LATENCY_BUCKETS.into_iter().zip(&mut histogram.buckets) {
                if seconds <= boundary {
                    *bucket += 1;
                }
            }
        }
    }

    pub(crate) fn publish(&self, workers: usize, valid_limits: bool) -> io::Result<()> {
        let directory = self
            .file
            .parent()
            .ok_or_else(|| io::Error::other("invalid metrics path"))?;
        let snapshot = self.snapshot(workers, valid_limits)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&snapshot)?;
        temporary
            .as_file()
            .set_permissions(Permissions::from_mode(0o644))?;
        temporary.persist(&self.file)?;
        Ok(())
    }

    fn snapshot(&self, workers: usize, valid_limits: bool) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs();
        writeln!(
            output,
            "# TYPE {PREFIX}_broker_heartbeat_timestamp_seconds gauge\n{PREFIX}_broker_heartbeat_timestamp_seconds {time}"
        )?;
        writeln!(
            output,
            "# TYPE {PREFIX}_broker_active_workers gauge\n{PREFIX}_broker_active_workers {workers}"
        )?;
        writeln!(
            output,
            "# TYPE {PREFIX}_broker_resource_limits_valid gauge\n{PREFIX}_broker_resource_limits_valid {}",
            u8::from(valid_limits)
        )?;
        let counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writeln!(output, "# TYPE {PREFIX}_lookups_total counter")?;
        for (&((traffic, language, operation), status, reason), count) in &counts.lookups {
            writeln!(
                output,
                "{PREFIX}_lookups_total{{traffic=\"{traffic}\",language=\"{language}\",operation=\"{operation}\",status=\"{status}\",reason=\"{reason}\"}} {count}"
            )?;
        }
        writeln!(output, "# TYPE {PREFIX}_lookup_duration_seconds histogram")?;
        for (&(traffic, language, operation), histogram) in &counts.latency {
            let labels =
                format!("traffic=\"{traffic}\",language=\"{language}\",operation=\"{operation}\"");
            for (boundary, count) in LATENCY_BUCKETS.into_iter().zip(histogram.buckets) {
                writeln!(
                    output,
                    "{PREFIX}_lookup_duration_seconds_bucket{{{labels},le=\"{boundary}\"}} {count}"
                )?;
            }
            writeln!(
                output,
                "{PREFIX}_lookup_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\n{PREFIX}_lookup_duration_seconds_sum{{{labels}}} {}\n{PREFIX}_lookup_duration_seconds_count{{{labels}}} {}",
                histogram.count, histogram.sum, histogram.count
            )?;
        }
        Ok(output)
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Ok => "ok",
        Status::Unavailable => "unavailable",
        Status::Indexing => "indexing",
        Status::Stale => "stale",
        Status::Timeout => "timeout",
        Status::Cancelled => "cancelled",
        Status::Busy => "busy",
        Status::InvalidRequest => "invalid_request",
    }
}

// Response reasons are strings on the wire. Never export raw values.
fn reason_label(reason: Option<&str>) -> &'static str {
    const REASONS: &[&str] = &[
        "broker_shutdown",
        "cancelled",
        "client_disconnected",
        "deadline",
        "deadline_exceeded",
        "external_result",
        "files_changed_during_query",
        "indexing",
        "invalid_cwd",
        "invalid_path",
        "invalid_position",
        "invalid_request",
        "invalid_source_path",
        "pool_full",
        "query_limit",
        "resource_limit_unavailable",
        "result_file_changed",
        "server_content_modified",
        "server_not_ready",
        "server_unavailable",
        "source_unavailable",
        "unregistered_workspace",
        "unsupported_source",
        "worker_lost",
        "worker_queue_full",
        "workspace_identity_changed",
        "workspace_too_large",
    ];
    match reason {
        None => "none",
        Some(reason) => REASONS
            .iter()
            .copied()
            .find(|allowed| *allowed == reason)
            .unwrap_or("other"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_a_private_label_set_and_cumulative_latency_buckets() {
        let directory = tempfile::tempdir().expect("temporary collector");
        let path = directory.path().join("navigation.prom");
        let metrics = Metrics::new(path.clone(), HashSet::new());
        let key = ("operator", "rust", "definition");
        metrics.record(key, Status::Ok, None, Duration::from_millis(25));
        metrics.record(key, Status::Ok, None, Duration::from_millis(700));
        metrics.record(
            key,
            Status::Unavailable,
            Some("/private/unknown-project/source.rs"),
            Duration::from_millis(2),
        );
        metrics.publish(3, true).expect("first snapshot");
        let text = std::fs::read_to_string(&path).expect("published snapshot");
        let labels = r#"traffic="operator",language="rust",operation="definition""#;
        assert!(text.contains(&format!(
            r#"{PREFIX}_lookups_total{{{labels},status="ok",reason="none"}} 2"#
        )));
        assert!(text.contains(&format!(
            r#"{PREFIX}_lookups_total{{{labels},status="unavailable",reason="other"}} 1"#
        )));
        for (bucket, count) in [("0.01", 0), ("0.25", 1), ("1", 2), ("+Inf", 2)] {
            assert!(text.contains(&format!(
                r#"{PREFIX}_lookup_duration_seconds_bucket{{{labels},le="{bucket}"}} {count}"#
            )));
        }
        assert!(text.contains(&format!(
            "{PREFIX}_lookup_duration_seconds_count{{{labels}}} 2"
        )));
        assert!(text.contains(&format!("{PREFIX}_broker_active_workers 3\n")));
        assert!(text.contains(&format!("{PREFIX}_broker_resource_limits_valid 1\n")));
        assert!(!text.contains("/private"));
        let held_open = std::fs::File::open(&path).expect("hold first snapshot");
        metrics.publish(0, false).expect("replace snapshot");
        let replacement = std::fs::read_to_string(&path).expect("replacement snapshot");
        assert!(replacement.contains(&format!("{PREFIX}_broker_active_workers 0\n")));
        assert!(replacement.contains(&format!("{PREFIX}_broker_resource_limits_valid 0\n")));
        assert!(replacement.contains(&format!(
            r#"{PREFIX}_lookups_total{{{labels},status="ok",reason="none"}} 2"#
        )));
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            held_open.metadata().expect("held metadata").ino(),
            std::fs::metadata(&path).expect("current metadata").ino()
        );
        assert_eq!(
            std::fs::metadata(&path).expect("current metadata").mode() & 0o777,
            0o644
        );
    }
}
