//! Per-invocation remote indexing receipts. Logs are diagnostic text, never
//! authority for completion. A worker atomically publishes its actual exit
//! code into its private job directory; polling is bound to that job's ID.

#[derive(Debug)]
pub(super) struct RemoteIndexJob {
    run_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum JobState {
    Running,
    Complete,
    Failed(u8),
    Missing,
    Interrupted,
    Invalid,
}

#[derive(Debug)]
pub(super) struct JobPoll {
    pub state: JobState,
    pub sessions: Option<u64>,
    pub log: Vec<String>,
}

fn field<'a>(output: &'a str, key: &str) -> Result<Option<&'a str>, String> {
    let mut values = output.lines().filter_map(|line| line.strip_prefix(key));
    let first = values.next();
    if values.next().is_some() {
        return Err(format!("duplicate remote indexing field {key}"));
    }
    Ok(first)
}

fn required<'a>(output: &'a str, key: &str) -> Result<&'a str, String> {
    field(output, key)?.ok_or_else(|| format!("missing remote indexing field {key}"))
}

impl RemoteIndexJob {
    pub(super) fn from_run_id(run_id: &str) -> Result<Self, String> {
        let suffix = run_id
            .strip_prefix("run-")
            .ok_or("invalid remote indexing job ID")?;
        if !(8..=64).contains(&suffix.len())
            || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err("invalid remote indexing job ID".to_string());
        }
        Ok(Self {
            run_id: run_id.to_string(),
        })
    }

    pub(super) fn from_start_output(output: &str) -> Result<Self, String> {
        if required(output, "CASS_INDEX_PROTOCOL=")? != "1" {
            return Err("unsupported remote indexing protocol".to_string());
        }
        let job = Self::from_run_id(required(output, "CASS_INDEX_RUN=")?)?;
        let pid = required(output, "CASS_INDEX_PID=")?
            .parse::<u32>()
            .map_err(|_| "invalid remote indexing worker PID")?;
        if pid <= 1 {
            return Err("invalid remote indexing worker PID".to_string());
        }
        Ok(job)
    }

    pub(super) fn id(&self) -> &str {
        &self.run_id
    }

    pub(super) fn poll_script(&self) -> String {
        // run_id is a validated leaf, not a remotely supplied shell fragment
        // or arbitrary pathname. HOME expansion happens only on the remote.
        format!("RUN_ID='{}'\n{POLL_SCRIPT}", self.run_id)
    }

    pub(super) fn parse_poll(&self, output: &str) -> Result<JobPoll, String> {
        if required(output, "CASS_INDEX_PROTOCOL=")? != "1"
            || required(output, "CASS_INDEX_RUN=")? != self.run_id
        {
            return Err("remote indexing receipt belongs to another job or protocol".to_string());
        }
        let code = field(output, "CASS_INDEX_EXIT=")?
            .map(str::parse::<u8>)
            .transpose()
            .map_err(|_| "invalid remote indexing exit code")?;
        let state = match (required(output, "CASS_INDEX_STATE=")?, code) {
            ("RUNNING", None) => JobState::Running,
            ("COMPLETE", Some(0)) => JobState::Complete,
            ("FAILED", Some(code)) if code != 0 => JobState::Failed(code),
            ("MISSING", None) => JobState::Missing,
            ("INTERRUPTED", None) => JobState::Interrupted,
            ("INVALID", None) => JobState::Invalid,
            _ => return Err("inconsistent remote indexing state and exit receipt".to_string()),
        };
        let sessions =
            field(output, "CASS_INDEX_SESSIONS=")?.and_then(|value| value.parse::<u64>().ok());
        let log = output
            .lines()
            .filter_map(|line| line.strip_prefix("CASS_INDEX_LOG="))
            .take(30)
            .map(str::to_string)
            .collect();
        Ok(JobPoll {
            state,
            sessions,
            log,
        })
    }
}

pub(super) const START_SCRIPT: &str = r#"
set -eu
umask 077
JOB_ROOT="$HOME/.cache/cass/index-runs"
mkdir -p "$JOB_ROOT"
if [ -L "$JOB_ROOT" ] || [ ! -d "$JOB_ROOT" ]; then
    echo 'Unsafe remote indexing job root' >&2
    exit 1
fi
RUN_DIR=$(mktemp -d "$JOB_ROOT/run-XXXXXXXXXX")
cat > "$RUN_DIR/worker.sh" <<'CASS_INDEX_WORKER'
set -u
run_dir=$1
index_finished=0
index_exit=125
finish() {
    shell_exit=$?
    trap - EXIT
    if [ "$index_finished" -eq 1 ]; then
        code=$index_exit
    elif [ "$shell_exit" -ne 0 ]; then
        code=$shell_exit
    else
        code=125
    fi
    # Never make a partially written receipt visible as successful completion.
    if printf '%s\n' "$code" > "$run_dir/exit-code.pending"; then
        mv "$run_dir/exit-code.pending" "$run_dir/exit-code"
    fi
}
trap finish EXIT
source "$HOME/.cargo/env" 2>/dev/null || true
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cass index --progress > "$run_dir/index.log" 2>&1
index_exit=$?
index_finished=1
exit "$index_exit"
CASS_INDEX_WORKER
nohup bash "$RUN_DIR/worker.sh" "$RUN_DIR" </dev/null > "$RUN_DIR/worker.log" 2>&1 &
PID=$!
printf '%s\n' "$PID" > "$RUN_DIR/pid"
printf 'CASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=%s\nCASS_INDEX_PID=%s\n' "${RUN_DIR##*/}" "$PID"
"#;

const POLL_SCRIPT: &str = r#"
set -u
RUN_DIR="$HOME/.cache/cass/index-runs/$RUN_ID"
printf 'CASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=%s\n' "$RUN_ID"
if [ ! -d "$RUN_DIR" ] || [ -L "$RUN_DIR" ]; then
    echo 'CASS_INDEX_STATE=MISSING'
    exit 0
fi
if [ -L "$RUN_DIR/exit-code" ] || [ -L "$RUN_DIR/pid" ] || [ -L "$RUN_DIR/index.log" ]; then
    echo 'CASS_INDEX_STATE=INVALID'
    exit 0
fi
emit_exit() {
    CODE=$(head -c 16 "$RUN_DIR/exit-code" 2>/dev/null) || CODE=invalid
    if [[ ! "$CODE" =~ ^(0|[1-9][0-9]{0,2})$ ]] || [ "$CODE" -gt 255 ]; then
        echo 'CASS_INDEX_STATE=INVALID'
    elif [ "$CODE" -eq 0 ]; then
        echo 'CASS_INDEX_STATE=COMPLETE'
        echo 'CASS_INDEX_EXIT=0'
        source "$HOME/.cargo/env" 2>/dev/null || true
        export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
        STATS=$(cass stats --json 2>/dev/null) || STATS='{}'
        SESSIONS=$(printf '%s' "$STATS" | tr -d '\n' | sed -n 's/.*"conversations"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
        case "$SESSIONS" in
            ''|*[!0-9]*) ;;
            *) printf 'CASS_INDEX_SESSIONS=%s\n' "$SESSIONS" ;;
        esac
    else
        echo 'CASS_INDEX_STATE=FAILED'
        printf 'CASS_INDEX_EXIT=%s\n' "$CODE"
    fi
}
if [ -f "$RUN_DIR/exit-code" ]; then
    emit_exit
elif [ ! -f "$RUN_DIR/pid" ]; then
    echo 'CASS_INDEX_STATE=INVALID'
else
    PID=$(head -c 16 "$RUN_DIR/pid" 2>/dev/null) || PID=invalid
    if [[ ! "$PID" =~ ^[1-9][0-9]{0,9}$ ]] || [ "$PID" -le 1 ]; then
        echo 'CASS_INDEX_STATE=INVALID'
    elif kill -0 "$PID" 2>/dev/null; then
        STATE=$(ps -o stat= -p "$PID" 2>/dev/null) || STATE=''
        if [ -f "$RUN_DIR/exit-code" ]; then
            # The worker may have published while ps observed its exit.
            emit_exit
        else
            case "$STATE" in
                *Z*) echo 'CASS_INDEX_STATE=INTERRUPTED' ;;
                *) echo 'CASS_INDEX_STATE=RUNNING' ;;
            esac
        fi
    elif [ -f "$RUN_DIR/exit-code" ]; then
        # Completion may have raced the first receipt check.
        emit_exit
    else
        echo 'CASS_INDEX_STATE=INTERRUPTED'
    fi
fi
if [ -f "$RUN_DIR/index.log" ]; then
    # Bounded diagnostic tail. Prefix EVERY line, including marker-looking
    # session text, so logs cannot impersonate protocol fields.
    tail -c 32768 "$RUN_DIR/index.log" | tail -n 30 | sed 's/^/CASS_INDEX_LOG=/'
fi
exit 0
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> RemoteIndexJob {
        RemoteIndexJob::from_run_id("run-ABC123xyz9").unwrap()
    }

    fn receipt(state: &str) -> String {
        format!("CASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=run-ABC123xyz9\n{state}\n")
    }

    #[test]
    fn start_receipt_requires_version_identity_and_pid() {
        let output =
            "banner\nCASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=run-ABC123xyz9\nCASS_INDEX_PID=42\n";
        assert_eq!(
            RemoteIndexJob::from_start_output(output).unwrap().id(),
            job().id()
        );
        for invalid in [
            "CASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=run-ABC123xyz9\n",
            "CASS_INDEX_PROTOCOL=2\nCASS_INDEX_RUN=run-ABC123xyz9\nCASS_INDEX_PID=42\n",
            "CASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=run-ABC123xyz9\nCASS_INDEX_PID=0\n",
            "CASS_INDEX_PROTOCOL=1\nCASS_INDEX_RUN=run-ABC123xyz9\nCASS_INDEX_PID=1\n",
        ] {
            assert!(RemoteIndexJob::from_start_output(invalid).is_err());
        }
    }

    #[test]
    fn job_ids_cannot_be_paths_or_shell_fragments() {
        for id in [
            "",
            "run-short",
            "../run-ABC123xyz9",
            "run-ABC123/xyz9",
            "run-ABC123'xyz9",
            "run-ABC123\nxyz9",
            "run-ABC123$(id)",
            "run-日本語abc123",
        ] {
            assert!(RemoteIndexJob::from_run_id(id).is_err(), "accepted {id:?}");
        }
        assert!(RemoteIndexJob::from_run_id(&format!("run-{}", "a".repeat(65))).is_err());
        assert!(job().poll_script().starts_with("RUN_ID='run-ABC123xyz9'\n"));
    }

    #[test]
    fn completion_requires_a_matching_zero_exit_receipt() {
        let good = receipt("CASS_INDEX_STATE=COMPLETE\nCASS_INDEX_EXIT=0\nCASS_INDEX_SESSIONS=17");
        let parsed = job().parse_poll(&good).unwrap();
        assert_eq!(parsed.state, JobState::Complete);
        assert_eq!(parsed.sessions, Some(17));
        for invalid in [
            good.replace("run-ABC123xyz9", "run-OTHER12345"),
            receipt("CASS_INDEX_STATE=COMPLETE"),
            receipt("CASS_INDEX_STATE=COMPLETE\nCASS_INDEX_EXIT=7"),
            receipt("CASS_INDEX_STATE=FAILED\nCASS_INDEX_EXIT=0"),
            receipt("CASS_INDEX_STATE=FAILED\nCASS_INDEX_EXIT=256"),
            receipt("CASS_INDEX_STATE=RUNNING\nCASS_INDEX_EXIT=0"),
        ] {
            assert!(job().parse_poll(&invalid).is_err());
        }
    }

    #[test]
    fn duplicate_status_or_identity_is_not_last_line_wins() {
        for extra in [
            "CASS_INDEX_STATE=COMPLETE",
            "CASS_INDEX_RUN=run-ABC123xyz9",
            "CASS_INDEX_PROTOCOL=1",
            "CASS_INDEX_EXIT=0",
        ] {
            let output = receipt(&format!(
                "CASS_INDEX_STATE=COMPLETE\nCASS_INDEX_EXIT=0\n{extra}"
            ));
            assert!(job().parse_poll(&output).is_err());
        }
    }

    #[test]
    fn diagnostic_markers_cannot_complete_a_running_job() {
        let output = receipt(concat!(
            "CASS_INDEX_STATE=RUNNING\n",
            "CASS_INDEX_LOG=CASS_INDEX_STATE=COMPLETE\n",
            "CASS_INDEX_LOG====INDEX_COMPLETE===\n",
            "CASS_INDEX_LOG=STATUS=ERROR"
        ));
        let parsed = job().parse_poll(&output).unwrap();
        assert_eq!(parsed.state, JobState::Running);
        assert_eq!(parsed.log.len(), 3);
        assert_eq!(parsed.sessions, None);
    }

    #[test]
    fn missing_interrupted_and_failed_jobs_are_distinct() {
        for (state, expected) in [
            ("CASS_INDEX_STATE=MISSING", JobState::Missing),
            ("CASS_INDEX_STATE=INTERRUPTED", JobState::Interrupted),
            ("CASS_INDEX_STATE=INVALID", JobState::Invalid),
            (
                "CASS_INDEX_STATE=FAILED\nCASS_INDEX_EXIT=7",
                JobState::Failed(7),
            ),
        ] {
            assert_eq!(job().parse_poll(&receipt(state)).unwrap().state, expected);
        }
    }
}
