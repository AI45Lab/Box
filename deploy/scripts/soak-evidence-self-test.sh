#!/usr/bin/env bash
#
# Self-test the a3s-box soak evidence verifier and RuntimeClass runner guardrails.
#
# This script does not require a live Kubernetes cluster. It uses synthetic
# evidence bundles plus a temporary kubectl stub for runner dry/failure paths.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
VERIFY_SCRIPT="$REPO_ROOT/deploy/scripts/verify-soak-evidence.sh"
RUNTIMECLASS_SOAK="$REPO_ROOT/deploy/scripts/runtimeclass-soak.sh"
HOST_INTEGRATION="$REPO_ROOT/scripts/host-integration-smoke.sh"

KEEP_TMP=0

usage() {
    cat <<'EOF'
Usage: deploy/scripts/soak-evidence-self-test.sh [options]

Options:
  --keep-tmp   Keep temporary evidence bundles for inspection.
  -h, --help   Show this help.

Checks:
  - host verifier positive and leak-detection paths;
  - host runner pass and failure summaries without booting a VM;
  - host and RuntimeClass runner verifier gate forwarding;
  - minimum soak duration, sample-count, sample-span, and max-gap enforcement;
  - cluster verifier positive, missing-artifact, malformed-TSV, and failure
    summary diagnostics;
  - cluster RuntimeClass proof, unresolved pod/job, and pod restart diagnostics;
  - RuntimeClass dry-run Job manifest guardrails;
  - RuntimeClass preflight-only summary without mutating kubectl calls;
  - RuntimeClass cleanup-only and mid-run failure summaries with kubectl stubs.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --keep-tmp)
            KEEP_TMP=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

tmp_parent="${TMPDIR:-/tmp}"
tmp_parent="${tmp_parent%/}"
TMP_ROOT="$(mktemp -d "$tmp_parent/a3s-soak-evidence-self-test.XXXXXX")"

cleanup() {
    if [ "$KEEP_TMP" -eq 0 ]; then
        rm -rf "$TMP_ROOT"
    else
        printf 'kept temporary self-test files: %s\n' "$TMP_ROOT"
    fi
}
trap cleanup EXIT

log() {
    printf '==> %s\n' "$*"
}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

require_grep() {
    local pattern="$1"
    local file="$2"
    grep -qE -- "$pattern" "$file" || fail "pattern '$pattern' not found in $file"
}

expect_failure() {
    local name="$1"
    local pattern="$2"
    shift 2
    local dir="$TMP_ROOT/$name"
    mkdir -p "$dir"

    set +e
    "$@" >"$dir/stdout.txt" 2>"$dir/stderr.txt"
    local status="$?"
    set -e

    if [ "$status" -eq 0 ]; then
        fail "$name unexpectedly passed"
    fi
    require_grep "$pattern" "$dir/stderr.txt"
}

write_nonempty_file() {
    local path="$1"
    printf 'synthetic evidence: %s\n' "$(basename "$path")" >"$path"
}

make_host_bundle() {
    local dir="$1"
    local final_shims="${2:-1}"
    mkdir -p "$dir"
    cat >"$dir/metadata.txt" <<'EOF'
run_id=synthetic-host
started_at=2026-06-29T00:00:00Z
selected_suites=core=0 host=0 linux_run=0 cri=0 bench=1
soak_duration_secs=7200
soak_iterations=2
soak_interval_secs=0
soak_verify_min_duration_secs=0
soak_verify_min_samples=0
soak_verify_min_sample_span_secs=0
soak_verify_max_sample_gap_secs=0
dry_run=0
EOF
    cat >"$dir/summary.txt" <<'EOF'
finished_at=2026-06-29T00:00:00Z
result=pass
duration_secs=7200
iterations=2
failed_iterations=0
EOF
    cat >"$dir/resource-samples.tsv" <<EOF
timestamp	phase	shims	mounts	box_dirs	socket_dirs
2026-06-29T00:00:00Z	start	1	1	1	1
2026-06-29T00:01:00Z	final	${final_shims}	1	1	1
EOF
    write_nonempty_file "$dir/start-cli-snapshot.txt"
    write_nonempty_file "$dir/final-cli-snapshot.txt"
    write_nonempty_file "$dir/iteration-1-cli-snapshot.txt"
    write_nonempty_file "$dir/iteration-2-cli-snapshot.txt"
    write_nonempty_file "$dir/iteration-1-bench-leak.log"
    write_nonempty_file "$dir/iteration-1-bench-race.log"
    write_nonempty_file "$dir/iteration-2-bench-leak.log"
    write_nonempty_file "$dir/iteration-2-bench-race.log"
}

make_host_failure_bundle() {
    local dir="$1"
    make_host_bundle "$dir"
    cat >"$dir/summary.txt" <<'EOF'
finished_at=2026-06-29T00:00:00Z
result=fail
duration_secs=7200
iterations=2
failed_iterations=1
exit_code=1
failed_at=scripts/host-integration-smoke.sh:650
failed_command=deploy/scripts/verify-soak-evidence.sh --kind host /tmp/a3s-box-soak
evidence_dir=synthetic
EOF
}

make_cluster_bundle() {
    local dir="$1"
    mkdir -p "$dir"
    cat >"$dir/metadata.txt" <<'EOF'
run_id=synthetic-cluster
started_at=2026-06-29T00:00:00Z
runtime_class=a3s-box
runtime_class_handler=a3s-box
job_name=a3s-box-churn-synthetic
job_completions=2
verify_min_duration_secs=0
verify_min_samples=0
verify_min_sample_span_secs=0
verify_max_sample_gap_secs=0
cleanup=0
skip_smoke=0
skip_jobs=0
skip_complex=0
dry_run=0
EOF
    cat >"$dir/summary.txt" <<'EOF'
finished_at=2026-06-29T00:00:00Z
result=pass
duration_secs=7200
evidence_dir=synthetic
EOF
    cat >"$dir/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	7	0	5	2	0	0	0	0	2	0
EOF
    local name
    for name in \
        selected-nodes.txt \
        selected-node-names.txt \
        selected-node-labels.tsv \
        runtimeclass.yaml \
        final-get-all.txt \
        final-pods.yaml \
        final-pod-runtimeclasses.tsv \
        final-pod-nodes.tsv \
        final-pod-statuses.tsv \
        events.txt \
        events.tsv \
        describe-pods.txt \
        smoke-exec.txt \
        complex-exec.txt \
        job.yaml \
        job-runtimeclass.txt \
        job-pods.txt \
        job-pod-statuses.tsv \
        complex-logs.txt
    do
        write_nonempty_file "$dir/$name"
    done
    cat >"$dir/final-pods.yaml" <<'EOF'
apiVersion: v1
items:
  - metadata:
      name: synthetic-runtimeclass-pod
    spec:
      runtimeClassName: a3s-box
EOF
    cat >"$dir/runtimeclass.yaml" <<'EOF'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: a3s-box
scheduling:
  nodeSelector:
    a3s-box.io/runtime: "true"
EOF
    cat >"$dir/job.yaml" <<'EOF'
apiVersion: batch/v1
kind: Job
metadata:
  name: a3s-box-churn-synthetic
spec:
  template:
    spec:
      runtimeClassName: a3s-box
EOF
    cat >"$dir/final-pod-runtimeclasses.tsv" <<'EOF'
synthetic-runtimeclass-pod	a3s-box
cplx-redis	a3s-box
cplx-postgres	a3s-box
cplx-nginx	a3s-box
cplx-python	a3s-box
synthetic-runtimeclass-job-1	a3s-box
synthetic-runtimeclass-job-2	a3s-box
EOF
    cat >"$dir/selected-node-names.txt" <<'EOF'
node-a
EOF
    cat >"$dir/selected-node-labels.tsv" <<'EOF'
node-a	true	production-soak
EOF
    cat >"$dir/final-pod-nodes.tsv" <<'EOF'
synthetic-runtimeclass-pod	node-a
cplx-redis	node-a
cplx-postgres	node-a
cplx-nginx	node-a
cplx-python	node-a
synthetic-runtimeclass-job-1	node-a
synthetic-runtimeclass-job-2	node-a
EOF
    cat >"$dir/final-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-pod	Running	0
cplx-redis	Running	0
cplx-postgres	Running	0
cplx-nginx	Running	0
cplx-python	Running	0
synthetic-runtimeclass-job-1	Succeeded	0
synthetic-runtimeclass-job-2	Succeeded	0
EOF
    cat >"$dir/job-runtimeclass.txt" <<'EOF'
a3s-box
EOF
    cat >"$dir/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
synthetic-runtimeclass-job-2	Succeeded	0	node-a
EOF
    cat >"$dir/job-logs.txt" <<'EOF'
a3s-box-churn-synthetic A3S_BOX_JOB_START 2026-06-29T00:00:01Z synthetic-job
a3s-box-churn-synthetic Linux synthetic 6.0
a3s-box-churn-synthetic A3S_BOX_JOB_RUNTIME_CLASS=a3s-box
a3s-box-churn-synthetic A3S_BOX_JOB_DONE 2026-06-29T00:00:02Z
a3s-box-churn-synthetic A3S_BOX_JOB_START 2026-06-29T00:00:03Z synthetic-job
a3s-box-churn-synthetic Linux synthetic 6.0
a3s-box-churn-synthetic A3S_BOX_JOB_RUNTIME_CLASS=a3s-box
a3s-box-churn-synthetic A3S_BOX_JOB_DONE 2026-06-29T00:00:04Z
EOF
    cat >"$dir/complex-logs.txt" <<'EOF'
cplx-redis REDIS_SOAK start=2026-06-29T00:00:00Z
cplx-postgres PG_SOAK start=2026-06-29T00:00:00Z
cplx-nginx NGINX_SOAK start=2026-06-29T00:00:00Z
cplx-python PY_SOAK start=2026-06-29T00:00:00Z
EOF
    cat >"$dir/events.tsv" <<'EOF'
Normal	Scheduled	synthetic-runtimeclass-pod	Successfully assigned validation pod
Normal	Pulled	synthetic-runtimeclass-pod	Container image already present
EOF
    cat >"$dir/smoke-exec.txt" <<'EOF'
selector=app=a3s-box-runtimeclass-smoke
pod_list_result=pass
pod=synthetic-runtimeclass-pod node=node-a workload=<none>
exec_pod=synthetic-runtimeclass-pod node=node-a workload=<none>
A3S_BOX_EXEC_OK synthetic-runtimeclass-pod
x86_64
exec_result=pass pod=synthetic-runtimeclass-pod node=node-a workload=<none>
EOF
    cat >"$dir/complex-exec.txt" <<'EOF'
selector=soak=cplx
pod_list_result=pass
pod=cplx-redis node=node-a workload=redis
pod=cplx-postgres node=node-a workload=postgres
pod=cplx-nginx node=node-a workload=nginx
pod=cplx-python node=node-a workload=python
exec_pod=cplx-redis node=node-a workload=redis
A3S_BOX_EXEC_OK cplx-redis
x86_64
exec_result=pass pod=cplx-redis node=node-a workload=redis
exec_pod=cplx-postgres node=node-a workload=postgres
A3S_BOX_EXEC_OK cplx-postgres
x86_64
exec_result=pass pod=cplx-postgres node=node-a workload=postgres
exec_pod=cplx-nginx node=node-a workload=nginx
A3S_BOX_EXEC_OK cplx-nginx
x86_64
exec_result=pass pod=cplx-nginx node=node-a workload=nginx
exec_pod=cplx-python node=node-a workload=python
A3S_BOX_EXEC_OK cplx-python
x86_64
exec_result=pass pod=cplx-python node=node-a workload=python
EOF
}

make_cluster_failure_bundle() {
    local dir="$1"
    make_cluster_bundle "$dir"
    cat >"$dir/summary.txt" <<'EOF'
finished_at=2026-06-29T00:00:00Z
result=fail
duration_secs=7200
exit_code=17
failed_at=deploy/scripts/runtimeclass-soak.sh:300
failed_command=kubectl get runtimeclass a3s-box
evidence_dir=synthetic
EOF
}

write_kubectl_stub() {
    local path="$1"
    cat >"$path" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

mode="${KUBECTL_STUB_MODE:-ok}"

has_arg() {
    local expected="$1"
    shift
    for arg in "$@"; do
        [ "$arg" = "$expected" ] && return 0
    done
    return 1
}

has_arg_prefix() {
    local prefix="$1"
    shift
    for arg in "$@"; do
        case "$arg" in
            "$prefix"*)
                return 0
                ;;
        esac
    done
    return 1
}

if [ "$mode" = "forbid-mutate" ]; then
    for arg in "$@"; do
        case "$arg" in
            apply|create|delete|label|wait|rollout)
                echo "mutating kubectl command is forbidden in this stub: $*" >&2
                exit 23
                ;;
        esac
    done
fi

if [ "${1:-}" = "config" ]; then
    echo "stub-context"
    exit 0
fi

if [ "${1:-}" = "version" ]; then
    echo "stub-version"
    exit 0
fi

if [ "$mode" = "runtimeclass-fail" ] &&
    [ "${1:-}" = "get" ] &&
    [ "${2:-}" = "runtimeclass" ]; then
    echo "runtimeclass unavailable" >&2
    exit 17
fi

if [ "${1:-}" = "get" ] && [ "${2:-}" = "runtimeclass" ]; then
    if [ "${4:-}" = "-o" ] && [ "${5:-}" = "yaml" ]; then
        cat <<'YAML'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: a3s-box
scheduling:
  nodeSelector:
    a3s-box.io/runtime: "true"
YAML
        exit 0
    fi
    echo "a3s-box a3s-box"
    exit 0
fi

if [ "${1:-}" = "get" ] && [ "${2:-}" = "nodes" ]; then
    for arg in "$@"; do
        case "$arg" in
            jsonpath=*)
                echo "node-a	true	production-soak"
                exit 0
                ;;
            custom-columns=NAME:.metadata.name)
                echo "node-a"
                exit 0
                ;;
        esac
    done
    echo "node-a Ready"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    [ "${5:-}" = "--no-headers" ]; then
    echo "runtime-pod 1/1 Running 0 1m"
    echo "cplx-redis 1/1 Running 0 1m"
    echo "cplx-postgres 1/1 Running 0 1m"
    echo "cplx-nginx 1/1 Running 0 1m"
    echo "cplx-python 1/1 Running 0 1m"
    exit 0
fi

if [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "daemonset" ]; then
    exit 0
fi

if [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "jobs" ]; then
    exit 0
fi

if [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    has_arg "-l" "$@" &&
    ! has_arg "-o" "$@"; then
    exit 0
fi

if [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    [ "${5:-}" = "--no-headers" ]; then
    exit 0
fi

if [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "events" ]; then
    if has_arg "custom-columns=TYPE:.type,REASON:.reason,OBJECT:.involvedObject.name,MESSAGE:.message" "$@"; then
        echo "Normal Scheduled runtime-pod Successfully assigned"
        exit 0
    fi
    echo "LAST SEEN   TYPE     REASON      OBJECT        MESSAGE"
    echo "1s          Normal   Scheduled   pod/runtime   Successfully assigned"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    has_arg "-l" "$@" &&
    has_arg "soak=cplx" "$@" &&
    has_arg "custom-columns=NAME:.metadata.name,NODE:.spec.nodeName,WORKLOAD:.metadata.labels.workload" "$@"; then
    echo "cplx-redis node-a redis"
    echo "cplx-postgres node-a postgres"
    echo "cplx-nginx node-a nginx"
    echo "cplx-python node-a python"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    has_arg "custom-columns=NAME:.metadata.name,NODE:.spec.nodeName,WORKLOAD:.metadata.labels.workload" "$@"; then
    echo "runtime-pod node-a <none>"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    has_arg "custom-columns=NAME:.metadata.name,NODE:.spec.nodeName" "$@"; then
    echo "runtime-pod node-a"
    echo "cplx-redis node-a"
    echo "cplx-postgres node-a"
    echo "cplx-nginx node-a"
    echo "cplx-python node-a"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    has_arg "custom-columns=NAME:.metadata.name" "$@"; then
    echo "runtime-pod"
    echo "cplx-redis"
    echo "cplx-postgres"
    echo "cplx-nginx"
    echo "cplx-python"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    [ "${5:-}" = "-o" ]; then
    case "${6:-}" in
        custom-columns=NAME:.metadata.name,RUNTIMECLASS:.spec.runtimeClassName)
            echo "runtime-pod a3s-box"
            echo "cplx-redis a3s-box"
            echo "cplx-postgres a3s-box"
            echo "cplx-nginx a3s-box"
            echo "cplx-python a3s-box"
            exit 0
            ;;
        custom-columns=NAME:.metadata.name,NODE:.spec.nodeName)
            echo "runtime-pod node-a"
            echo "cplx-redis node-a"
            echo "cplx-postgres node-a"
            echo "cplx-nginx node-a"
            echo "cplx-python node-a"
            exit 0
            ;;
        custom-columns=NAME:.metadata.name,PHASE:.status.phase,RESTARTS:.status.containerStatuses\[\*\].restartCount)
            echo "runtime-pod Running 0"
            echo "cplx-redis Running 0"
            echo "cplx-postgres Running 0"
            echo "cplx-nginx Running 0"
            echo "cplx-python Running 0"
            exit 0
            ;;
    esac
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "pods" ] &&
    [ "${5:-}" = "-o" ] &&
    [ "${6:-}" = "yaml" ]; then
    cat <<'YAML'
apiVersion: v1
items:
  - metadata:
      name: runtime-pod
    spec:
      runtimeClassName: a3s-box
  - metadata:
      name: cplx-redis
    spec:
      runtimeClassName: a3s-box
  - metadata:
      name: cplx-postgres
    spec:
      runtimeClassName: a3s-box
  - metadata:
      name: cplx-nginx
    spec:
      runtimeClassName: a3s-box
  - metadata:
      name: cplx-python
    spec:
      runtimeClassName: a3s-box
YAML
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "exec" ]; then
    echo "A3S_BOX_EXEC_OK ${4:-runtime-pod}"
    echo "x86_64"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "logs" ] &&
    has_arg "-l" "$@" &&
    has_arg "soak=cplx" "$@"; then
    echo "cplx-redis REDIS_SOAK start=2026-06-29T00:00:00Z"
    echo "cplx-postgres PG_SOAK start=2026-06-29T00:00:00Z"
    echo "cplx-nginx NGINX_SOAK start=2026-06-29T00:00:00Z"
    echo "cplx-python PY_SOAK start=2026-06-29T00:00:00Z"
    exit 0
fi

if [ "$mode" = "with-runtime-pods" ] &&
    [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "logs" ] &&
    has_arg "-l" "$@" &&
    has_arg_prefix "job-name=" "$@"; then
    echo "a3s-box-churn-synthetic A3S_BOX_JOB_START 2026-06-29T00:00:01Z synthetic-job"
    echo "a3s-box-churn-synthetic Linux synthetic 6.0"
    echo "a3s-box-churn-synthetic A3S_BOX_JOB_RUNTIME_CLASS=a3s-box"
    echo "a3s-box-churn-synthetic A3S_BOX_JOB_DONE 2026-06-29T00:00:02Z"
    exit 0
fi

if [ "${1:-}" = "-n" ] &&
    [ "${3:-}" = "get" ] &&
    [ "${4:-}" = "job" ]; then
    if [ "$mode" = "with-runtime-pods" ] && [ "${5:-}" = "-o" ]; then
        case "${6:-}" in
            jsonpath=*)
                echo "a3s-box"
                exit 0
                ;;
        esac
    fi
    exit 0
fi

printf 'stub kubectl %s\n' "$*"
EOF
    chmod +x "$path"
}

log "Verifying synthetic host evidence"
host_pass="$TMP_ROOT/host-pass"
make_host_bundle "$host_pass"
"$VERIFY_SCRIPT" --kind host "$host_pass" >"$host_pass/verify.out"
require_grep 'PASS: host soak evidence verified' "$host_pass/verify.out"
"$VERIFY_SCRIPT" --kind host --min-duration-secs 7200 "$host_pass" \
    >"$host_pass/verify-min-duration.out"
require_grep 'PASS: host soak evidence verified' "$host_pass/verify-min-duration.out"
"$VERIFY_SCRIPT" --kind host --min-samples 2 "$host_pass" \
    >"$host_pass/verify-min-samples.out"
require_grep 'PASS: host soak evidence verified' "$host_pass/verify-min-samples.out"
"$VERIFY_SCRIPT" --kind host --min-sample-span-secs 60 "$host_pass" \
    >"$host_pass/verify-min-sample-span.out"
require_grep 'PASS: host soak evidence verified' "$host_pass/verify-min-sample-span.out"
"$VERIFY_SCRIPT" --kind host --max-sample-gap-secs 60 "$host_pass" \
    >"$host_pass/verify-max-sample-gap.out"
require_grep 'PASS: host soak evidence verified' "$host_pass/verify-max-sample-gap.out"
expect_failure host-duration-too-short 'host soak duration too short' \
    "$VERIFY_SCRIPT" --kind host --min-duration-secs 7201 "$host_pass"
expect_failure host-too-few-samples 'host soak has too few resource samples' \
    "$VERIFY_SCRIPT" --kind host --min-samples 3 "$host_pass"
expect_failure host-sample-span-too-short 'host soak sample span too short' \
    "$VERIFY_SCRIPT" --kind host --min-sample-span-secs 61 "$host_pass"
expect_failure host-sample-gap-too-large 'host soak sample gap too large' \
    "$VERIFY_SCRIPT" --kind host --max-sample-gap-secs 59 "$host_pass"

host_metadata_gate_fail="$TMP_ROOT/host-metadata-gate-fail"
make_host_bundle "$host_metadata_gate_fail"
awk -F= '
    $1 == "soak_verify_min_samples" { print "soak_verify_min_samples=3"; next }
    { print }
' "$host_metadata_gate_fail/metadata.txt" >"$host_metadata_gate_fail/metadata.gated.txt"
mv "$host_metadata_gate_fail/metadata.gated.txt" "$host_metadata_gate_fail/metadata.txt"
expect_failure host-metadata-gate-fail 'host soak has too few resource samples' \
    "$VERIFY_SCRIPT" --kind host "$host_metadata_gate_fail"

host_duration_shorter_than_samples="$TMP_ROOT/host-duration-shorter-than-samples"
make_host_bundle "$host_duration_shorter_than_samples"
cat >"$host_duration_shorter_than_samples/summary.txt" <<'EOF'
finished_at=2026-06-29T00:00:00Z
result=pass
duration_secs=0
iterations=2
failed_iterations=0
EOF
cat >"$host_duration_shorter_than_samples/resource-samples.tsv" <<'EOF'
timestamp	phase	shims	mounts	box_dirs	socket_dirs
2026-06-29T00:00:00Z	start	1	1	1	1
2026-06-29T00:02:01Z	final	1	1	1	1
EOF
expect_failure host-duration-shorter-than-samples 'host summary duration is shorter than resource sample span' \
    "$VERIFY_SCRIPT" --kind host "$host_duration_shorter_than_samples"

host_missing_timestamp="$TMP_ROOT/host-missing-timestamp"
make_host_bundle "$host_missing_timestamp"
cat >"$host_missing_timestamp/resource-samples.tsv" <<'EOF'
phase	shims	mounts	box_dirs	socket_dirs
start	1	1	1	1
final	1	1	1	1
EOF
expect_failure host-missing-timestamp 'missing required column' \
    "$VERIFY_SCRIPT" --kind host --max-sample-gap-secs 60 "$host_missing_timestamp"

host_non_numeric_sample="$TMP_ROOT/host-non-numeric-sample"
make_host_bundle "$host_non_numeric_sample"
cat >"$host_non_numeric_sample/resource-samples.tsv" <<'EOF'
timestamp	phase	shims	mounts	box_dirs	socket_dirs
2026-06-29T00:00:00Z	start	1	1	1	1
2026-06-29T00:01:00Z	final	many	1	1	1
EOF
expect_failure host-non-numeric-sample 'host resource sample counters must be non-negative integers' \
    "$VERIFY_SCRIPT" --kind host "$host_non_numeric_sample"

host_bad_timestamp="$TMP_ROOT/host-bad-timestamp"
make_host_bundle "$host_bad_timestamp"
cat >"$host_bad_timestamp/resource-samples.tsv" <<'EOF'
timestamp	phase	shims	mounts	box_dirs	socket_dirs
not-a-timestamp	start	1	1	1	1
2026-06-29T00:01:00Z	final	1	1	1	1
EOF
expect_failure host-bad-timestamp 'host resource sample timestamp is not parseable' \
    "$VERIFY_SCRIPT" --kind host "$host_bad_timestamp"

host_duplicate_final="$TMP_ROOT/host-duplicate-final"
make_host_bundle "$host_duplicate_final"
cat >"$host_duplicate_final/resource-samples.tsv" <<'EOF'
timestamp	phase	shims	mounts	box_dirs	socket_dirs
2026-06-29T00:00:00Z	start	1	1	1	1
2026-06-29T00:01:00Z	final	1	1	1	1
2026-06-29T00:02:00Z	final	1	1	1	1
EOF
expect_failure host-duplicate-final 'host resource samples must contain exactly 1 final row' \
    "$VERIFY_SCRIPT" --kind host "$host_duplicate_final"

host_leak="$TMP_ROOT/host-leak"
make_host_bundle "$host_leak" 2
expect_failure host-leak-fails 'shims grew' "$VERIFY_SCRIPT" --kind host "$host_leak"

host_failure="$TMP_ROOT/host-failure"
make_host_failure_bundle "$host_failure"
expect_failure host-failure-summary 'host soak failed: failed_iterations=1' \
    "$VERIFY_SCRIPT" --kind host "$host_failure"

host_missing_result="$TMP_ROOT/host-missing-result"
make_host_bundle "$host_missing_result"
awk -F= '$1 != "result"' "$host_missing_result/summary.txt" \
    >"$host_missing_result/summary.without-result.txt"
mv "$host_missing_result/summary.without-result.txt" "$host_missing_result/summary.txt"
expect_failure host-missing-result 'host summary result is missing' \
    "$VERIFY_SCRIPT" --kind host "$host_missing_result"

host_bad_metadata_counter="$TMP_ROOT/host-bad-metadata-counter"
make_host_bundle "$host_bad_metadata_counter"
awk -F= '
    $1 == "soak_iterations" { print "soak_iterations=two"; next }
    { print }
' "$host_bad_metadata_counter/metadata.txt" >"$host_bad_metadata_counter/metadata.bad.txt"
mv "$host_bad_metadata_counter/metadata.bad.txt" "$host_bad_metadata_counter/metadata.txt"
expect_failure host-bad-metadata-counter 'host metadata soak_iterations is not a non-negative integer' \
    "$VERIFY_SCRIPT" --kind host "$host_bad_metadata_counter"

host_bad_selected_suites="$TMP_ROOT/host-bad-selected-suites"
make_host_bundle "$host_bad_selected_suites"
awk -F= '
    $1 == "selected_suites" { print "selected_suites=synthetic"; next }
    { print }
' "$host_bad_selected_suites/metadata.txt" >"$host_bad_selected_suites/metadata.bad.txt"
mv "$host_bad_selected_suites/metadata.bad.txt" "$host_bad_selected_suites/metadata.txt"
expect_failure host-bad-selected-suites 'host metadata selected_suites missing core flag' \
    "$VERIFY_SCRIPT" --kind host "$host_bad_selected_suites"

host_missing_declared_log="$TMP_ROOT/host-missing-declared-log"
make_host_bundle "$host_missing_declared_log"
rm "$host_missing_declared_log/iteration-2-bench-race.log"
expect_failure host-missing-declared-log 'missing required file: .*iteration-2-bench-race.log' \
    "$VERIFY_SCRIPT" --kind host "$host_missing_declared_log"

log "Verifying host soak runner rehearsal summary"
host_runner_dir="$TMP_ROOT/host-runner"
"$HOST_INTEGRATION" --no-pure --linux-run --soak --soak-no-bench \
    --soak-duration 0 --soak-iterations 1 --soak-output "$host_runner_dir" \
    >"$host_runner_dir.out"
require_grep '^result=pass$' "$host_runner_dir/summary.txt"
require_grep '^duration_secs=[0-9]+$' "$host_runner_dir/summary.txt"
require_grep 'PASS: host soak evidence verified' "$host_runner_dir/verify.out"
"$VERIFY_SCRIPT" --kind host "$host_runner_dir" >"$host_runner_dir.verify.out"
require_grep 'PASS: host soak evidence verified' "$host_runner_dir.verify.out"

log "Verifying host soak runner verifier gate forwarding"
host_runner_gate_dir="$TMP_ROOT/host-runner-gate"
"$HOST_INTEGRATION" --no-pure --linux-run --soak --soak-no-bench \
    --soak-duration 0 --soak-iterations 1 \
    --soak-verify-min-samples 3 \
    --soak-output "$host_runner_gate_dir" \
    >"$host_runner_gate_dir.out"
require_grep '^result=pass$' "$host_runner_gate_dir/summary.txt"
require_grep '^soak_verify_min_samples=3$' "$host_runner_gate_dir/metadata.txt"
require_grep 'PASS: host soak evidence verified' "$host_runner_gate_dir/verify.out"

host_runner_gate_fail_dir="$TMP_ROOT/host-runner-gate-fail"
set +e
"$HOST_INTEGRATION" --no-pure --linux-run --soak --soak-no-bench \
    --soak-duration 0 --soak-iterations 1 \
    --soak-verify-min-duration-secs 3600 \
    --soak-output "$host_runner_gate_fail_dir" \
    >"$host_runner_gate_fail_dir.out" 2>"$host_runner_gate_fail_dir.err"
host_runner_gate_fail_status="$?"
set -e
if [ "$host_runner_gate_fail_status" -eq 0 ]; then
    fail "host runner verifier gate failure rehearsal unexpectedly passed"
fi
require_grep '^result=fail$' "$host_runner_gate_fail_dir/summary.txt"
require_grep '^failed_iterations=0$' "$host_runner_gate_fail_dir/summary.txt"
require_grep 'failed_command=.*/deploy/scripts/verify-soak-evidence.sh --kind host --min-duration-secs 3600 ' \
    "$host_runner_gate_fail_dir/summary.txt"
require_grep 'host soak duration too short' "$host_runner_gate_fail_dir/verify.out"
expect_failure host-runner-gate-failure-diagnostic 'host soak failed: failed_iterations=0' \
    "$VERIFY_SCRIPT" --kind host "$host_runner_gate_fail_dir"

log "Verifying host soak runner failure summary"
host_runner_fail_dir="$TMP_ROOT/host-runner-fail"
set +e
A3S_BOX="$TMP_ROOT/missing-a3s-box" "$HOST_INTEGRATION" \
    --no-pure --soak --soak-duration 0 --soak-iterations 1 \
    --soak-output "$host_runner_fail_dir" \
    >"$host_runner_fail_dir.out" 2>"$host_runner_fail_dir.err"
host_runner_fail_status="$?"
set -e
if [ "$host_runner_fail_status" -eq 0 ]; then
    fail "host runner failure rehearsal unexpectedly passed"
fi
require_grep '^result=fail$' "$host_runner_fail_dir/summary.txt"
require_grep '^duration_secs=[0-9]+$' "$host_runner_fail_dir/summary.txt"
require_grep '^iterations=1$' "$host_runner_fail_dir/summary.txt"
require_grep '^failed_iterations=1$' "$host_runner_fail_dir/summary.txt"
require_grep '^exit_code=1$' "$host_runner_fail_dir/summary.txt"
require_grep '^failed_at=iteration-1-bench-leak$' "$host_runner_fail_dir/summary.txt"
require_grep '^failed_command=run_bench_leak$' "$host_runner_fail_dir/summary.txt"
expect_failure host-runner-failure-diagnostic 'host soak failed: failed_iterations=1' \
    "$VERIFY_SCRIPT" --kind host "$host_runner_fail_dir"

log "Verifying synthetic cluster evidence"
cluster_pass="$TMP_ROOT/cluster-pass"
make_cluster_bundle "$cluster_pass"
"$VERIFY_SCRIPT" --kind cluster "$cluster_pass" >"$cluster_pass/verify.out"
require_grep 'PASS: cluster soak evidence verified' "$cluster_pass/verify.out"
"$VERIFY_SCRIPT" --kind cluster --min-duration-secs 7200 "$cluster_pass" \
    >"$cluster_pass/verify-min-duration.out"
require_grep 'PASS: cluster soak evidence verified' "$cluster_pass/verify-min-duration.out"
"$VERIFY_SCRIPT" --kind cluster --min-samples 2 "$cluster_pass" \
    >"$cluster_pass/verify-min-samples.out"
require_grep 'PASS: cluster soak evidence verified' "$cluster_pass/verify-min-samples.out"
"$VERIFY_SCRIPT" --kind cluster --min-sample-span-secs 60 "$cluster_pass" \
    >"$cluster_pass/verify-min-sample-span.out"
require_grep 'PASS: cluster soak evidence verified' "$cluster_pass/verify-min-sample-span.out"
"$VERIFY_SCRIPT" --kind cluster --max-sample-gap-secs 60 "$cluster_pass" \
    >"$cluster_pass/verify-max-sample-gap.out"
require_grep 'PASS: cluster soak evidence verified' "$cluster_pass/verify-max-sample-gap.out"
expect_failure cluster-duration-too-short 'cluster soak duration too short' \
    "$VERIFY_SCRIPT" --kind cluster --min-duration-secs 7201 "$cluster_pass"
expect_failure cluster-too-few-samples 'cluster soak has too few resource samples' \
    "$VERIFY_SCRIPT" --kind cluster --min-samples 3 "$cluster_pass"
expect_failure cluster-sample-span-too-short 'cluster soak sample span too short' \
    "$VERIFY_SCRIPT" --kind cluster --min-sample-span-secs 61 "$cluster_pass"
expect_failure cluster-sample-gap-too-large 'cluster soak sample gap too large' \
    "$VERIFY_SCRIPT" --kind cluster --max-sample-gap-secs 59 "$cluster_pass"

cluster_metadata_gate_fail="$TMP_ROOT/cluster-metadata-gate-fail"
make_cluster_bundle "$cluster_metadata_gate_fail"
awk -F= '
    $1 == "verify_min_samples" { print "verify_min_samples=3"; next }
    { print }
' "$cluster_metadata_gate_fail/metadata.txt" >"$cluster_metadata_gate_fail/metadata.gated.txt"
mv "$cluster_metadata_gate_fail/metadata.gated.txt" "$cluster_metadata_gate_fail/metadata.txt"
expect_failure cluster-metadata-gate-fail 'cluster soak has too few resource samples' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_metadata_gate_fail"

cluster_bad_skip_flag="$TMP_ROOT/cluster-bad-skip-flag"
make_cluster_bundle "$cluster_bad_skip_flag"
awk -F= '
    $1 == "skip_smoke" { print "skip_smoke=maybe"; next }
    { print }
' "$cluster_bad_skip_flag/metadata.txt" >"$cluster_bad_skip_flag/metadata.bad.txt"
mv "$cluster_bad_skip_flag/metadata.bad.txt" "$cluster_bad_skip_flag/metadata.txt"
expect_failure cluster-bad-skip-flag 'cluster metadata skip_smoke must be 0 or 1' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_bad_skip_flag"

cluster_duration_shorter_than_samples="$TMP_ROOT/cluster-duration-shorter-than-samples"
make_cluster_bundle "$cluster_duration_shorter_than_samples"
cat >"$cluster_duration_shorter_than_samples/summary.txt" <<'EOF'
finished_at=2026-06-29T00:00:00Z
result=pass
duration_secs=0
evidence_dir=synthetic
EOF
cat >"$cluster_duration_shorter_than_samples/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:02:01Z	final	1	2	0	0	2	0	0	0	0	2	0
EOF
expect_failure cluster-duration-shorter-than-samples 'cluster summary duration is shorter than resource sample span' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duration_shorter_than_samples"

cluster_cleanup_pass="$TMP_ROOT/cluster-cleanup-pass"
make_cluster_bundle "$cluster_cleanup_pass"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_pass/metadata.txt" \
    >"$cluster_cleanup_pass/metadata.without-cleanup.txt"
mv "$cluster_cleanup_pass/metadata.without-cleanup.txt" "$cluster_cleanup_pass/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_pass/metadata.txt"
write_nonempty_file "$cluster_cleanup_pass/post-cleanup-namespace.txt"
write_nonempty_file "$cluster_cleanup_pass/post-cleanup-get-all.txt"
cat >"$cluster_cleanup_pass/post-cleanup-counts.tsv" <<'EOF'
timestamp	phase	smoke_daemonsets	smoke_pods	complex_pods	churn_jobs	churn_pods
2026-06-29T00:01:30Z	post-cleanup	0	0	0	0	0
EOF
"$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_pass" >"$cluster_cleanup_pass/verify.out"
require_grep 'PASS: cluster soak evidence verified' "$cluster_cleanup_pass/verify.out"

cluster_cleanup_missing_counts="$TMP_ROOT/cluster-cleanup-missing-counts"
make_cluster_bundle "$cluster_cleanup_missing_counts"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_missing_counts/metadata.txt" \
    >"$cluster_cleanup_missing_counts/metadata.without-cleanup.txt"
mv "$cluster_cleanup_missing_counts/metadata.without-cleanup.txt" "$cluster_cleanup_missing_counts/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_missing_counts/metadata.txt"
write_nonempty_file "$cluster_cleanup_missing_counts/post-cleanup-namespace.txt"
write_nonempty_file "$cluster_cleanup_missing_counts/post-cleanup-get-all.txt"
expect_failure cluster-cleanup-missing-counts 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_missing_counts"

cluster_cleanup_nonzero="$TMP_ROOT/cluster-cleanup-nonzero"
make_cluster_bundle "$cluster_cleanup_nonzero"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_nonzero/metadata.txt" \
    >"$cluster_cleanup_nonzero/metadata.without-cleanup.txt"
mv "$cluster_cleanup_nonzero/metadata.without-cleanup.txt" "$cluster_cleanup_nonzero/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_nonzero/metadata.txt"
write_nonempty_file "$cluster_cleanup_nonzero/post-cleanup-namespace.txt"
write_nonempty_file "$cluster_cleanup_nonzero/post-cleanup-get-all.txt"
cat >"$cluster_cleanup_nonzero/post-cleanup-counts.tsv" <<'EOF'
timestamp	phase	smoke_daemonsets	smoke_pods	complex_pods	churn_jobs	churn_pods
2026-06-29T00:01:30Z	post-cleanup	0	1	0	0	0
EOF
expect_failure cluster-cleanup-nonzero 'post-cleanup smoke_pods still present' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_nonzero"

cluster_cleanup_duplicate_counts="$TMP_ROOT/cluster-cleanup-duplicate-counts"
make_cluster_bundle "$cluster_cleanup_duplicate_counts"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_duplicate_counts/metadata.txt" \
    >"$cluster_cleanup_duplicate_counts/metadata.without-cleanup.txt"
mv "$cluster_cleanup_duplicate_counts/metadata.without-cleanup.txt" "$cluster_cleanup_duplicate_counts/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_duplicate_counts/metadata.txt"
write_nonempty_file "$cluster_cleanup_duplicate_counts/post-cleanup-namespace.txt"
write_nonempty_file "$cluster_cleanup_duplicate_counts/post-cleanup-get-all.txt"
cat >"$cluster_cleanup_duplicate_counts/post-cleanup-counts.tsv" <<'EOF'
timestamp	phase	smoke_daemonsets	smoke_pods	complex_pods	churn_jobs	churn_pods
2026-06-29T00:01:30Z	post-cleanup	0	1	0	0	0
2026-06-29T00:01:31Z	post-cleanup	0	0	0	0	0
EOF
expect_failure cluster-cleanup-duplicate-counts 'post-cleanup resource samples must contain exactly 1 post-cleanup row' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_duplicate_counts"

cluster_cleanup_kubectl_error="$TMP_ROOT/cluster-cleanup-kubectl-error"
make_cluster_bundle "$cluster_cleanup_kubectl_error"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_kubectl_error/metadata.txt" \
    >"$cluster_cleanup_kubectl_error/metadata.without-cleanup.txt"
mv "$cluster_cleanup_kubectl_error/metadata.without-cleanup.txt" "$cluster_cleanup_kubectl_error/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_kubectl_error/metadata.txt"
write_nonempty_file "$cluster_cleanup_kubectl_error/post-cleanup-namespace.txt"
cat >"$cluster_cleanup_kubectl_error/post-cleanup-get-all.txt" <<'EOF'
Error from server (ServiceUnavailable): apiserver unavailable while listing post-cleanup objects
EOF
cat >"$cluster_cleanup_kubectl_error/post-cleanup-counts.tsv" <<'EOF'
timestamp	phase	smoke_daemonsets	smoke_pods	complex_pods	churn_jobs	churn_pods
2026-06-29T00:01:30Z	post-cleanup	0	0	0	0	0
EOF
expect_failure cluster-cleanup-kubectl-error 'post-cleanup Kubernetes object listing contains kubectl collection error' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_kubectl_error"

cluster_cleanup_bad_timestamp="$TMP_ROOT/cluster-cleanup-bad-timestamp"
make_cluster_bundle "$cluster_cleanup_bad_timestamp"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_bad_timestamp/metadata.txt" \
    >"$cluster_cleanup_bad_timestamp/metadata.without-cleanup.txt"
mv "$cluster_cleanup_bad_timestamp/metadata.without-cleanup.txt" "$cluster_cleanup_bad_timestamp/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_bad_timestamp/metadata.txt"
write_nonempty_file "$cluster_cleanup_bad_timestamp/post-cleanup-namespace.txt"
write_nonempty_file "$cluster_cleanup_bad_timestamp/post-cleanup-get-all.txt"
cat >"$cluster_cleanup_bad_timestamp/post-cleanup-counts.tsv" <<'EOF'
timestamp	phase	smoke_daemonsets	smoke_pods	complex_pods	churn_jobs	churn_pods
not-a-timestamp	post-cleanup	0	0	0	0	0
EOF
expect_failure cluster-cleanup-bad-timestamp 'post-cleanup resource sample timestamp is not parseable' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_bad_timestamp"

cluster_cleanup_early_timestamp="$TMP_ROOT/cluster-cleanup-early-timestamp"
make_cluster_bundle "$cluster_cleanup_early_timestamp"
awk -F= '$1 != "cleanup"' "$cluster_cleanup_early_timestamp/metadata.txt" \
    >"$cluster_cleanup_early_timestamp/metadata.without-cleanup.txt"
mv "$cluster_cleanup_early_timestamp/metadata.without-cleanup.txt" "$cluster_cleanup_early_timestamp/metadata.txt"
printf 'cleanup=1\n' >>"$cluster_cleanup_early_timestamp/metadata.txt"
write_nonempty_file "$cluster_cleanup_early_timestamp/post-cleanup-namespace.txt"
write_nonempty_file "$cluster_cleanup_early_timestamp/post-cleanup-get-all.txt"
cat >"$cluster_cleanup_early_timestamp/post-cleanup-counts.tsv" <<'EOF'
timestamp	phase	smoke_daemonsets	smoke_pods	complex_pods	churn_jobs	churn_pods
2026-06-28T23:59:59Z	post-cleanup	0	0	0	0	0
EOF
expect_failure cluster-cleanup-early-timestamp 'post-cleanup timestamp is earlier than final sample' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_cleanup_early_timestamp"

cluster_missing_timestamp="$TMP_ROOT/cluster-missing-timestamp"
make_cluster_bundle "$cluster_missing_timestamp"
cat >"$cluster_missing_timestamp/resource-samples.tsv" <<'EOF'
phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
interval	1	1	0	1	0	0	0	0	1	1	0
final	1	2	0	0	2	0	0	0	0	2	0
EOF
expect_failure cluster-missing-timestamp 'missing required column' \
    "$VERIFY_SCRIPT" --kind cluster --max-sample-gap-secs 60 "$cluster_missing_timestamp"

cluster_failure="$TMP_ROOT/cluster-failure"
make_cluster_failure_bundle "$cluster_failure"
expect_failure cluster-failure-summary 'cluster soak failed: exit_code=17' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_failure"

cluster_missing="$TMP_ROOT/cluster-missing-job-logs"
make_cluster_bundle "$cluster_missing"
rm "$cluster_missing/job-logs.txt"
expect_failure cluster-missing-job-logs 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing"

cluster_missing_job_log_marker="$TMP_ROOT/cluster-missing-job-log-marker"
make_cluster_bundle "$cluster_missing_job_log_marker"
cat >"$cluster_missing_job_log_marker/job-logs.txt" <<'EOF'
a3s-box-churn-synthetic A3S_BOX_JOB_START 2026-06-29T00:00:01Z synthetic-job
a3s-box-churn-synthetic Linux synthetic 6.0
a3s-box-churn-synthetic A3S_BOX_JOB_RUNTIME_CLASS=a3s-box
a3s-box-churn-synthetic A3S_BOX_JOB_START 2026-06-29T00:00:03Z synthetic-job
a3s-box-churn-synthetic Linux synthetic 6.0
a3s-box-churn-synthetic A3S_BOX_JOB_RUNTIME_CLASS=a3s-box
EOF
expect_failure cluster-missing-job-log-marker 'job logs missing A3S_BOX_JOB_DONE marker' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_job_log_marker"

cluster_incomplete_job_log_count="$TMP_ROOT/cluster-incomplete-job-log-count"
make_cluster_bundle "$cluster_incomplete_job_log_count"
cat >"$cluster_incomplete_job_log_count/job-logs.txt" <<'EOF'
a3s-box-churn-synthetic A3S_BOX_JOB_START 2026-06-29T00:00:01Z synthetic-job
a3s-box-churn-synthetic Linux synthetic 6.0
a3s-box-churn-synthetic A3S_BOX_JOB_RUNTIME_CLASS=a3s-box
a3s-box-churn-synthetic A3S_BOX_JOB_DONE 2026-06-29T00:00:02Z
EOF
expect_failure cluster-incomplete-job-log-count 'job logs has wrong A3S_BOX_JOB_START marker count' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_incomplete_job_log_count"

cluster_extra_job_log_count="$TMP_ROOT/cluster-extra-job-log-count"
make_cluster_bundle "$cluster_extra_job_log_count"
cat >>"$cluster_extra_job_log_count/job-logs.txt" <<'EOF'
a3s-box-churn-synthetic A3S_BOX_JOB_DONE 2026-06-29T00:00:05Z
EOF
expect_failure cluster-extra-job-log-count 'job logs has wrong A3S_BOX_JOB_DONE marker count' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_extra_job_log_count"

cluster_job_logs_kubectl_error="$TMP_ROOT/cluster-job-logs-kubectl-error"
make_cluster_bundle "$cluster_job_logs_kubectl_error"
cat >"$cluster_job_logs_kubectl_error/job-logs.txt" <<'EOF'
error: unable to collect logs for churn job
EOF
expect_failure cluster-job-logs-kubectl-error 'job logs contains kubectl collection error' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_job_logs_kubectl_error"

cluster_missing_events_tsv="$TMP_ROOT/cluster-missing-events-tsv"
make_cluster_bundle "$cluster_missing_events_tsv"
rm "$cluster_missing_events_tsv/events.tsv"
expect_failure cluster-missing-events-tsv 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_events_tsv"

cluster_kubectl_error_artifact="$TMP_ROOT/cluster-kubectl-error-artifact"
make_cluster_bundle "$cluster_kubectl_error_artifact"
cat >"$cluster_kubectl_error_artifact/final-get-all.txt" <<'EOF'
The connection to the server 127.0.0.1:26443 was refused - did you specify the right host or port?
EOF
expect_failure cluster-kubectl-error-artifact 'final Kubernetes object listing contains kubectl collection error' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_kubectl_error_artifact"

cluster_job_kubectl_error_artifact="$TMP_ROOT/cluster-job-kubectl-error-artifact"
make_cluster_bundle "$cluster_job_kubectl_error_artifact"
cat >"$cluster_job_kubectl_error_artifact/job-pods.txt" <<'EOF'
Error from server (InternalError): unable to list pods for churn job
EOF
expect_failure cluster-job-kubectl-error-artifact 'churn Job pod listing contains kubectl collection error' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_job_kubectl_error_artifact"

cluster_missing_job_pod_statuses="$TMP_ROOT/cluster-missing-job-pod-statuses"
make_cluster_bundle "$cluster_missing_job_pod_statuses"
rm "$cluster_missing_job_pod_statuses/job-pod-statuses.tsv"
expect_failure cluster-missing-job-pod-statuses 'missing required file: .*job-pod-statuses.tsv' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_job_pod_statuses"

cluster_incomplete_job_pod_statuses="$TMP_ROOT/cluster-incomplete-job-pod-statuses"
make_cluster_bundle "$cluster_incomplete_job_pod_statuses"
cat >"$cluster_incomplete_job_pod_statuses/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
EOF
expect_failure cluster-incomplete-job-pod-statuses 'churn Job pod status evidence must list exactly 2 Succeeded pods' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_incomplete_job_pod_statuses"

cluster_bad_job_pod_phase="$TMP_ROOT/cluster-bad-job-pod-phase"
make_cluster_bundle "$cluster_bad_job_pod_phase"
cat >"$cluster_bad_job_pod_phase/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
synthetic-runtimeclass-job-2	Running	0	node-a
EOF
expect_failure cluster-bad-job-pod-phase 'churn Job pod status evidence must list exactly 2 Succeeded pods' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_bad_job_pod_phase"

cluster_restarted_job_pod="$TMP_ROOT/cluster-restarted-job-pod"
make_cluster_bundle "$cluster_restarted_job_pod"
cat >"$cluster_restarted_job_pod/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
synthetic-runtimeclass-job-2	Succeeded	1	node-a
EOF
expect_failure cluster-restarted-job-pod 'churn Job pod status evidence must list exactly 2 Succeeded pods' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_restarted_job_pod"

cluster_unselected_job_pod="$TMP_ROOT/cluster-unselected-job-pod"
make_cluster_bundle "$cluster_unselected_job_pod"
cat >"$cluster_unselected_job_pod/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
synthetic-runtimeclass-job-2	Succeeded	0	node-b
EOF
expect_failure cluster-unselected-job-pod 'churn Job pod status evidence must list exactly 2 Succeeded pods' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_unselected_job_pod"

cluster_duplicate_job_pod="$TMP_ROOT/cluster-duplicate-job-pod"
make_cluster_bundle "$cluster_duplicate_job_pod"
cat >"$cluster_duplicate_job_pod/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
synthetic-runtimeclass-job-1	Succeeded	0	node-a
EOF
expect_failure cluster-duplicate-job-pod 'churn Job pod status evidence must list exactly 2 Succeeded pods' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duplicate_job_pod"

cluster_job_pod_not_final="$TMP_ROOT/cluster-job-pod-not-final"
make_cluster_bundle "$cluster_job_pod_not_final"
cat >"$cluster_job_pod_not_final/job-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-job-1	Succeeded	0	node-a
missing-runtimeclass-job-2	Succeeded	0	node-a
EOF
expect_failure cluster-job-pod-not-final 'churn Job pod status artifact must be covered by final pod evidence' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_job_pod_not_final"

cluster_warning_event="$TMP_ROOT/cluster-warning-event"
make_cluster_bundle "$cluster_warning_event"
cat >"$cluster_warning_event/events.tsv" <<'EOF'
Normal	Scheduled	synthetic-runtimeclass-pod	Successfully assigned validation pod
Warning	BackOff	synthetic-runtimeclass-pod	Back-off restarting failed container
EOF
expect_failure cluster-warning-event 'Kubernetes event evidence must contain only Normal events' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_warning_event"

cluster_missing_selected_nodes="$TMP_ROOT/cluster-missing-selected-nodes"
make_cluster_bundle "$cluster_missing_selected_nodes"
rm "$cluster_missing_selected_nodes/selected-node-names.txt"
expect_failure cluster-missing-selected-nodes 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_selected_nodes"

cluster_missing_selected_nodes_wide="$TMP_ROOT/cluster-missing-selected-nodes-wide"
make_cluster_bundle "$cluster_missing_selected_nodes_wide"
rm "$cluster_missing_selected_nodes_wide/selected-nodes.txt"
expect_failure cluster-missing-selected-nodes-wide 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_selected_nodes_wide"

cluster_missing_selected_node_labels="$TMP_ROOT/cluster-missing-selected-node-labels"
make_cluster_bundle "$cluster_missing_selected_node_labels"
rm "$cluster_missing_selected_node_labels/selected-node-labels.tsv"
expect_failure cluster-missing-selected-node-labels 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_selected_node_labels"

cluster_bad_selected_node_label="$TMP_ROOT/cluster-bad-selected-node-label"
make_cluster_bundle "$cluster_bad_selected_node_label"
cat >"$cluster_bad_selected_node_label/selected-node-labels.tsv" <<'EOF'
node-a	true	staging
EOF
expect_failure cluster-bad-selected-node-label 'selected node label evidence must list only enrolled production-soak nodes' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_bad_selected_node_label"

cluster_mismatched_selected_node_label="$TMP_ROOT/cluster-mismatched-selected-node-label"
make_cluster_bundle "$cluster_mismatched_selected_node_label"
cat >"$cluster_mismatched_selected_node_label/selected-node-labels.tsv" <<'EOF'
node-b	true	production-soak
EOF
expect_failure cluster-mismatched-selected-node-label 'selected node artifacts must contain matching unique node names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_mismatched_selected_node_label"

cluster_missing_smoke_exec="$TMP_ROOT/cluster-missing-smoke-exec"
make_cluster_bundle "$cluster_missing_smoke_exec"
rm "$cluster_missing_smoke_exec/smoke-exec.txt"
expect_failure cluster-missing-smoke-exec 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_smoke_exec"

cluster_missing_smoke_exec_node="$TMP_ROOT/cluster-missing-smoke-exec-node"
make_cluster_bundle "$cluster_missing_smoke_exec_node"
cat >"$cluster_missing_smoke_exec_node/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	2	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	2	7	0	5	2	0	0	0	0	2	0
EOF
cat >"$cluster_missing_smoke_exec_node/selected-node-names.txt" <<'EOF'
node-a
node-b
EOF
cat >"$cluster_missing_smoke_exec_node/selected-node-labels.tsv" <<'EOF'
node-a	true	production-soak
node-b	true	production-soak
EOF
expect_failure cluster-missing-smoke-exec-node 'smoke exec evidence must include successful exec on every selected node' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_smoke_exec_node"

cluster_smoke_exec_not_final="$TMP_ROOT/cluster-smoke-exec-not-final"
make_cluster_bundle "$cluster_smoke_exec_not_final"
cat >"$cluster_smoke_exec_not_final/smoke-exec.txt" <<'EOF'
selector=app=a3s-box-runtimeclass-smoke
pod_list_result=pass
pod=not-final-smoke-pod node=node-a workload=<none>
exec_pod=not-final-smoke-pod node=node-a workload=<none>
A3S_BOX_EXEC_OK not-final-smoke-pod
x86_64
exec_result=pass pod=not-final-smoke-pod node=node-a workload=<none>
EOF
expect_failure cluster-smoke-exec-not-final 'smoke exec evidence must reference pods covered by final pod evidence' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_smoke_exec_not_final"

cluster_failed_complex_exec="$TMP_ROOT/cluster-failed-complex-exec"
make_cluster_bundle "$cluster_failed_complex_exec"
cat >"$cluster_failed_complex_exec/complex-exec.txt" <<'EOF'
selector=soak=cplx
pod_list_result=pass
pod=cplx-redis node=node-a workload=redis
exec_pod=cplx-redis node=node-a workload=redis
exec_result=fail pod=cplx-redis node=node-a workload=redis
EOF
expect_failure cluster-failed-complex-exec 'complex exec evidence missing A3S_BOX_EXEC_OK marker' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_failed_complex_exec"

cluster_missing_complex_workload="$TMP_ROOT/cluster-missing-complex-workload"
make_cluster_bundle "$cluster_missing_complex_workload"
cat >"$cluster_missing_complex_workload/complex-exec.txt" <<'EOF'
selector=soak=cplx
pod_list_result=pass
pod=cplx-redis node=node-a workload=redis
pod=cplx-postgres node=node-a workload=postgres
pod=cplx-nginx node=node-a workload=nginx
exec_pod=cplx-redis node=node-a workload=redis
A3S_BOX_EXEC_OK cplx-redis
x86_64
exec_result=pass pod=cplx-redis node=node-a workload=redis
exec_pod=cplx-postgres node=node-a workload=postgres
A3S_BOX_EXEC_OK cplx-postgres
x86_64
exec_result=pass pod=cplx-postgres node=node-a workload=postgres
exec_pod=cplx-nginx node=node-a workload=nginx
A3S_BOX_EXEC_OK cplx-nginx
x86_64
exec_result=pass pod=cplx-nginx node=node-a workload=nginx
EOF
expect_failure cluster-missing-complex-workload 'complex exec evidence must include every expected workload' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_complex_workload"

cluster_complex_exec_not_final="$TMP_ROOT/cluster-complex-exec-not-final"
make_cluster_bundle "$cluster_complex_exec_not_final"
cat >"$cluster_complex_exec_not_final/complex-exec.txt" <<'EOF'
selector=soak=cplx
pod_list_result=pass
pod=missing-cplx-redis node=node-a workload=redis
pod=cplx-postgres node=node-a workload=postgres
pod=cplx-nginx node=node-a workload=nginx
pod=cplx-python node=node-a workload=python
exec_pod=missing-cplx-redis node=node-a workload=redis
A3S_BOX_EXEC_OK missing-cplx-redis
x86_64
exec_result=pass pod=missing-cplx-redis node=node-a workload=redis
exec_pod=cplx-postgres node=node-a workload=postgres
A3S_BOX_EXEC_OK cplx-postgres
x86_64
exec_result=pass pod=cplx-postgres node=node-a workload=postgres
exec_pod=cplx-nginx node=node-a workload=nginx
A3S_BOX_EXEC_OK cplx-nginx
x86_64
exec_result=pass pod=cplx-nginx node=node-a workload=nginx
exec_pod=cplx-python node=node-a workload=python
A3S_BOX_EXEC_OK cplx-python
x86_64
exec_result=pass pod=cplx-python node=node-a workload=python
EOF
expect_failure cluster-complex-exec-not-final 'complex exec evidence must reference pods covered by final pod evidence' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_complex_exec_not_final"

cluster_missing_complex_log_marker="$TMP_ROOT/cluster-missing-complex-log-marker"
make_cluster_bundle "$cluster_missing_complex_log_marker"
cat >"$cluster_missing_complex_log_marker/complex-logs.txt" <<'EOF'
cplx-redis REDIS_SOAK start=2026-06-29T00:00:00Z
cplx-postgres PG_SOAK start=2026-06-29T00:00:00Z
cplx-python PY_SOAK start=2026-06-29T00:00:00Z
EOF
expect_failure cluster-missing-complex-log-marker 'complex logs missing NGINX_SOAK marker for nginx workload' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_complex_log_marker"

cluster_misattributed_complex_log="$TMP_ROOT/cluster-misattributed-complex-log"
make_cluster_bundle "$cluster_misattributed_complex_log"
cat >"$cluster_misattributed_complex_log/complex-logs.txt" <<'EOF'
cplx-redis REDIS_SOAK start=2026-06-29T00:00:00Z
cplx-postgres PG_SOAK start=2026-06-29T00:00:00Z
cplx-redis NGINX_SOAK start=2026-06-29T00:00:00Z
cplx-python PY_SOAK start=2026-06-29T00:00:00Z
EOF
expect_failure cluster-misattributed-complex-log 'complex logs missing NGINX_SOAK marker for nginx workload' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_misattributed_complex_log"

cluster_complex_logs_kubectl_error="$TMP_ROOT/cluster-complex-logs-kubectl-error"
make_cluster_bundle "$cluster_complex_logs_kubectl_error"
cat >"$cluster_complex_logs_kubectl_error/complex-logs.txt" <<'EOF'
error: unable to collect complex workload logs
EOF
expect_failure cluster-complex-logs-kubectl-error 'complex logs contains kubectl collection error' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_complex_logs_kubectl_error"

cluster_missing_runtimeclass_object="$TMP_ROOT/cluster-missing-runtimeclass-object"
make_cluster_bundle "$cluster_missing_runtimeclass_object"
rm "$cluster_missing_runtimeclass_object/runtimeclass.yaml"
expect_failure cluster-missing-runtimeclass-object 'missing required file' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_runtimeclass_object"

cluster_wrong_runtimeclass_name="$TMP_ROOT/cluster-wrong-runtimeclass-name"
make_cluster_bundle "$cluster_wrong_runtimeclass_name"
cat >"$cluster_wrong_runtimeclass_name/runtimeclass.yaml" <<'EOF'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: not-a3s-box
handler: a3s-box
scheduling:
  nodeSelector:
    a3s-box.io/runtime: "true"
EOF
expect_failure cluster-wrong-runtimeclass-name 'RuntimeClass object name is not-a3s-box; expected a3s-box' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_wrong_runtimeclass_name"

cluster_wrong_runtimeclass_handler="$TMP_ROOT/cluster-wrong-runtimeclass-handler"
make_cluster_bundle "$cluster_wrong_runtimeclass_handler"
cat >"$cluster_wrong_runtimeclass_handler/runtimeclass.yaml" <<'EOF'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: runc
scheduling:
  nodeSelector:
    a3s-box.io/runtime: "true"
EOF
expect_failure cluster-wrong-runtimeclass-handler 'RuntimeClass object handler is runc; expected a3s-box' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_wrong_runtimeclass_handler"

cluster_missing_runtimeclass_selector="$TMP_ROOT/cluster-missing-runtimeclass-selector"
make_cluster_bundle "$cluster_missing_runtimeclass_selector"
cat >"$cluster_missing_runtimeclass_selector/runtimeclass.yaml" <<'EOF'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: a3s-box
EOF
expect_failure cluster-missing-runtimeclass-selector 'RuntimeClass object scheduling.nodeSelector a3s-box.io/runtime is missing; expected true' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_runtimeclass_selector"

cluster_wrong_runtimeclass_selector="$TMP_ROOT/cluster-wrong-runtimeclass-selector"
make_cluster_bundle "$cluster_wrong_runtimeclass_selector"
cat >"$cluster_wrong_runtimeclass_selector/runtimeclass.yaml" <<'EOF'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: a3s-box
handler: a3s-box
scheduling:
  nodeSelector:
    a3s-box.io/runtime: "false"
EOF
expect_failure cluster-wrong-runtimeclass-selector 'RuntimeClass object scheduling.nodeSelector a3s-box.io/runtime is false; expected true' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_wrong_runtimeclass_selector"

cluster_selected_node_count_mismatch="$TMP_ROOT/cluster-selected-node-count-mismatch"
make_cluster_bundle "$cluster_selected_node_count_mismatch"
cat >"$cluster_selected_node_count_mismatch/selected-node-names.txt" <<'EOF'
node-a
node-b
EOF
expect_failure cluster-selected-node-count-mismatch 'selected node evidence row count mismatch' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_selected_node_count_mismatch"

cluster_duplicate_selected_node="$TMP_ROOT/cluster-duplicate-selected-node"
make_cluster_bundle "$cluster_duplicate_selected_node"
cat >"$cluster_duplicate_selected_node/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	2	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	2	7	0	5	2	0	0	0	0	2	0
EOF
cat >"$cluster_duplicate_selected_node/selected-node-names.txt" <<'EOF'
node-a
node-a
EOF
expect_failure cluster-duplicate-selected-node 'selected node evidence must contain unique non-empty names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duplicate_selected_node"

cluster_truncated_runtime_rows="$TMP_ROOT/cluster-truncated-runtime-rows"
make_cluster_bundle "$cluster_truncated_runtime_rows"
cat >"$cluster_truncated_runtime_rows/final-pod-runtimeclasses.tsv" <<'EOF'
synthetic-runtimeclass-pod	a3s-box
cplx-redis	a3s-box
cplx-postgres	a3s-box
cplx-nginx	a3s-box
cplx-python	a3s-box
synthetic-runtimeclass-job-1	a3s-box
EOF
expect_failure cluster-truncated-runtime-rows 'final pod RuntimeClass evidence row count mismatch' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_truncated_runtime_rows"

cluster_truncated_node_rows="$TMP_ROOT/cluster-truncated-node-rows"
make_cluster_bundle "$cluster_truncated_node_rows"
cat >"$cluster_truncated_node_rows/final-pod-nodes.tsv" <<'EOF'
synthetic-runtimeclass-pod	node-a
cplx-redis	node-a
cplx-postgres	node-a
cplx-nginx	node-a
cplx-python	node-a
synthetic-runtimeclass-job-1	node-a
EOF
expect_failure cluster-truncated-node-rows 'final pod node evidence row count mismatch' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_truncated_node_rows"

cluster_duplicate_runtime_name="$TMP_ROOT/cluster-duplicate-runtime-name"
make_cluster_bundle "$cluster_duplicate_runtime_name"
cat >"$cluster_duplicate_runtime_name/final-pod-runtimeclasses.tsv" <<'EOF'
synthetic-runtimeclass-pod	a3s-box
synthetic-runtimeclass-pod	a3s-box
cplx-redis	a3s-box
cplx-postgres	a3s-box
cplx-nginx	a3s-box
cplx-python	a3s-box
synthetic-runtimeclass-job-2	a3s-box
EOF
expect_failure cluster-duplicate-runtime-name 'final pod artifacts must contain matching unique pod names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duplicate_runtime_name"

cluster_duplicate_node_name="$TMP_ROOT/cluster-duplicate-node-name"
make_cluster_bundle "$cluster_duplicate_node_name"
cat >"$cluster_duplicate_node_name/final-pod-nodes.tsv" <<'EOF'
synthetic-runtimeclass-pod	node-a
synthetic-runtimeclass-pod	node-a
cplx-redis	node-a
cplx-postgres	node-a
cplx-nginx	node-a
cplx-python	node-a
synthetic-runtimeclass-job-2	node-a
EOF
expect_failure cluster-duplicate-node-name 'final pod artifacts must contain matching unique pod names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duplicate_node_name"

cluster_mismatched_pod_names="$TMP_ROOT/cluster-mismatched-pod-names"
make_cluster_bundle "$cluster_mismatched_pod_names"
cat >"$cluster_mismatched_pod_names/final-pod-nodes.tsv" <<'EOF'
synthetic-runtimeclass-pod	node-a
cplx-redis	node-a
cplx-postgres	node-a
cplx-nginx	node-a
cplx-python	node-a
synthetic-runtimeclass-job-1	node-a
different-runtimeclass-job-2	node-a
EOF
expect_failure cluster-mismatched-pod-names 'final pod artifacts must contain matching unique pod names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_mismatched_pod_names"

cluster_truncated_status_rows="$TMP_ROOT/cluster-truncated-status-rows"
make_cluster_bundle "$cluster_truncated_status_rows"
cat >"$cluster_truncated_status_rows/final-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-pod	Running	0
cplx-redis	Running	0
cplx-postgres	Running	0
cplx-nginx	Running	0
cplx-python	Running	0
synthetic-runtimeclass-job-1	Succeeded	0
EOF
expect_failure cluster-truncated-status-rows 'final pod status evidence row count mismatch' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_truncated_status_rows"

cluster_duplicate_status_name="$TMP_ROOT/cluster-duplicate-status-name"
make_cluster_bundle "$cluster_duplicate_status_name"
cat >"$cluster_duplicate_status_name/final-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-pod	Running	0
synthetic-runtimeclass-pod	Succeeded	0
cplx-redis	Running	0
cplx-postgres	Running	0
cplx-nginx	Running	0
cplx-python	Running	0
synthetic-runtimeclass-job-2	Succeeded	0
EOF
expect_failure cluster-duplicate-status-name 'final pod artifacts must contain matching unique pod names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duplicate_status_name"

cluster_mismatched_status_names="$TMP_ROOT/cluster-mismatched-status-names"
make_cluster_bundle "$cluster_mismatched_status_names"
cat >"$cluster_mismatched_status_names/final-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-pod	Running	0
cplx-redis	Running	0
cplx-postgres	Running	0
cplx-nginx	Running	0
cplx-python	Running	0
synthetic-runtimeclass-job-1	Succeeded	0
different-runtimeclass-job-2	Succeeded	0
EOF
expect_failure cluster-mismatched-status-names 'final pod artifacts must contain matching unique pod names' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_mismatched_status_names"

cluster_bad_pod_phase="$TMP_ROOT/cluster-bad-pod-phase"
make_cluster_bundle "$cluster_bad_pod_phase"
cat >"$cluster_bad_pod_phase/final-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-pod	Pending	0
cplx-redis	Running	0
cplx-postgres	Running	0
cplx-nginx	Running	0
cplx-python	Running	0
synthetic-runtimeclass-job-1	Succeeded	0
synthetic-runtimeclass-job-2	Succeeded	0
EOF
expect_failure cluster-bad-pod-phase 'final pod status evidence must list only Running/Succeeded pods with zero restarts' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_bad_pod_phase"

cluster_bad_pod_restart="$TMP_ROOT/cluster-bad-pod-restart"
make_cluster_bundle "$cluster_bad_pod_restart"
cat >"$cluster_bad_pod_restart/final-pod-statuses.tsv" <<'EOF'
synthetic-runtimeclass-pod	Running	1
cplx-redis	Running	0
cplx-postgres	Running	0
cplx-nginx	Running	0
cplx-python	Running	0
synthetic-runtimeclass-job-1	Succeeded	0
synthetic-runtimeclass-job-2	Succeeded	0
EOF
expect_failure cluster-bad-pod-restart 'final pod status evidence must list only Running/Succeeded pods with zero restarts' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_bad_pod_restart"

cluster_no_final_pods="$TMP_ROOT/cluster-no-final-pods"
make_cluster_bundle "$cluster_no_final_pods"
cat >"$cluster_no_final_pods/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	0	0	0	0	0	0	0	0	2	0
EOF
expect_failure cluster-no-final-pods 'no pods were sampled in final state' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_no_final_pods"

cluster_missing_pod_runtime="$TMP_ROOT/cluster-missing-pod-runtime"
make_cluster_bundle "$cluster_missing_pod_runtime"
cat >"$cluster_missing_pod_runtime/final-pod-runtimeclasses.tsv" <<'EOF'
synthetic-runtimeclass-pod	<none>
cplx-redis	a3s-box
cplx-postgres	a3s-box
cplx-nginx	a3s-box
cplx-python	a3s-box
synthetic-runtimeclass-job-1	a3s-box
synthetic-runtimeclass-job-2	a3s-box
EOF
expect_failure cluster-missing-pod-runtime 'final pod RuntimeClass evidence must list only runtimeClassName: a3s-box' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_pod_runtime"

cluster_missing_job_runtime="$TMP_ROOT/cluster-missing-job-runtime"
make_cluster_bundle "$cluster_missing_job_runtime"
cat >"$cluster_missing_job_runtime/job-runtimeclass.txt" <<'EOF'
<none>
EOF
expect_failure cluster-missing-job-runtime 'job evidence runtimeClassName is <none>; expected a3s-box' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_job_runtime"

cluster_unselected_node="$TMP_ROOT/cluster-unselected-node"
make_cluster_bundle "$cluster_unselected_node"
cat >"$cluster_unselected_node/final-pod-nodes.tsv" <<'EOF'
synthetic-runtimeclass-pod	node-b
cplx-redis	node-a
cplx-postgres	node-a
cplx-nginx	node-a
cplx-python	node-a
synthetic-runtimeclass-job-1	node-a
synthetic-runtimeclass-job-2	node-a
EOF
expect_failure cluster-unselected-node 'final pod node evidence must contain only selected nodes' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_unselected_node"

cluster_missing_pod_node="$TMP_ROOT/cluster-missing-pod-node"
make_cluster_bundle "$cluster_missing_pod_node"
cat >"$cluster_missing_pod_node/final-pod-nodes.tsv" <<'EOF'
synthetic-runtimeclass-pod	<none>
cplx-redis	node-a
cplx-postgres	node-a
cplx-nginx	node-a
cplx-python	node-a
synthetic-runtimeclass-job-1	node-a
synthetic-runtimeclass-job-2	node-a
EOF
expect_failure cluster-missing-pod-node 'final pod node evidence must contain only selected nodes' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_missing_pod_node"

cluster_pending="$TMP_ROOT/cluster-final-pending"
make_cluster_bundle "$cluster_pending"
cat >"$cluster_pending/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	2	1	0	1	0	0	0	0	2	0
EOF
expect_failure cluster-final-pending 'pods still pending in final sample' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_pending"

cluster_unknown="$TMP_ROOT/cluster-final-unknown"
make_cluster_bundle "$cluster_unknown"
cat >"$cluster_unknown/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	2	0	0	1	0	1	0	0	2	0
EOF
expect_failure cluster-final-unknown 'pods still unknown in final sample' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_unknown"

cluster_active_job="$TMP_ROOT/cluster-final-active-job"
make_cluster_bundle "$cluster_active_job"
cat >"$cluster_active_job/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	2	0	0	2	0	0	0	1	2	0
EOF
expect_failure cluster-final-active-job 'job still active in final sample' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_active_job"

cluster_restarts="$TMP_ROOT/cluster-pod-restarts"
make_cluster_bundle "$cluster_restarts"
cat >"$cluster_restarts/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	1	1	1	0
2026-06-29T00:01:00Z	final	1	2	0	0	2	0	0	1	0	2	0
EOF
expect_failure cluster-pod-restarts 'pod restarts observed' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_restarts"

cluster_non_numeric_sample="$TMP_ROOT/cluster-non-numeric-sample"
make_cluster_bundle "$cluster_non_numeric_sample"
cat >"$cluster_non_numeric_sample/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	2	many	0	2	0	0	0	0	2	0
EOF
expect_failure cluster-non-numeric-sample 'cluster resource sample counters must be non-negative integers' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_non_numeric_sample"

cluster_non_monotonic_timestamp="$TMP_ROOT/cluster-non-monotonic-timestamp"
make_cluster_bundle "$cluster_non_monotonic_timestamp"
cat >"$cluster_non_monotonic_timestamp/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:01:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:00:00Z	final	1	2	0	0	2	0	0	0	0	2	0
EOF
expect_failure cluster-non-monotonic-timestamp 'cluster resource sample timestamps are not monotonic' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_non_monotonic_timestamp"

cluster_duplicate_final="$TMP_ROOT/cluster-duplicate-final"
make_cluster_bundle "$cluster_duplicate_final"
cat >"$cluster_duplicate_final/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_pending	pods_running	pods_succeeded	pods_failed	pods_unknown	pod_restarts	job_active	job_succeeded	job_failed
2026-06-29T00:00:00Z	interval	1	1	0	1	0	0	0	0	1	1	0
2026-06-29T00:01:00Z	final	1	2	0	0	2	0	0	0	0	2	0
2026-06-29T00:02:00Z	final	1	2	0	0	2	0	0	0	0	2	0
EOF
expect_failure cluster-duplicate-final 'cluster resource samples must contain exactly 1 final row' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_duplicate_final"

cluster_bad_tsv="$TMP_ROOT/cluster-bad-tsv"
make_cluster_bundle "$cluster_bad_tsv"
cat >"$cluster_bad_tsv/resource-samples.tsv" <<'EOF'
timestamp	phase	selected_nodes	pods_total	pods_failed	job_succeeded
2026-06-29T00:00:00Z	final	1	2	0	2
EOF
expect_failure cluster-bad-tsv 'missing required column' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_bad_tsv"

log "Verifying RuntimeClass dry-run guardrails"
dry_run_dir="$TMP_ROOT/runtimeclass-dry-run"
"$RUNTIMECLASS_SOAK" --dry-run --jobs 3 --parallelism 2 --duration 0 \
    --output "$dry_run_dir" >"$dry_run_dir.out"
"$VERIFY_SCRIPT" --allow-dry-run --kind cluster "$dry_run_dir" >"$dry_run_dir.verify.out"
expect_failure dry-run-min-duration 'dry-run evidence cannot satisfy --min-duration-secs=1' \
    "$VERIFY_SCRIPT" --allow-dry-run --kind cluster --min-duration-secs 1 "$dry_run_dir"
expect_failure dry-run-min-samples 'dry-run evidence cannot satisfy --min-samples=1' \
    "$VERIFY_SCRIPT" --allow-dry-run --kind cluster --min-samples 1 "$dry_run_dir"
expect_failure dry-run-min-sample-span 'dry-run evidence cannot satisfy --min-sample-span-secs=1' \
    "$VERIFY_SCRIPT" --allow-dry-run --kind cluster --min-sample-span-secs 1 "$dry_run_dir"
expect_failure dry-run-max-sample-gap 'dry-run evidence cannot satisfy --max-sample-gap-secs=1' \
    "$VERIFY_SCRIPT" --allow-dry-run --kind cluster --max-sample-gap-secs 1 "$dry_run_dir"
expect_failure runtimeclass-dry-run-verifier-gate '--dry-run cannot satisfy verifier gate options' \
    "$RUNTIMECLASS_SOAK" --dry-run --jobs 3 --parallelism 2 --duration 0 \
        --verify-min-duration-secs 1 --output "$TMP_ROOT/runtimeclass-dry-run-gate"
require_grep '^dry_run=1$' "$dry_run_dir/metadata.txt"
require_grep 'runtimeClassName: a3s-box' "$dry_run_dir/churn-job.yaml"
require_grep 'a3s-box.io/test-tier: production-soak' "$dry_run_dir/churn-job.yaml"
require_grep 'a3s-box.io/soak' "$dry_run_dir/churn-job.yaml"
require_grep 'limits:' "$dry_run_dir/churn-job.yaml"

log "Verifying RuntimeClass cleanup-only summary with kubectl stub"
stub_dir="$TMP_ROOT/bin"
mkdir -p "$stub_dir"
write_kubectl_stub "$stub_dir/kubectl"

log "Verifying RuntimeClass preflight-only summary with kubectl stub"
preflight_dir="$TMP_ROOT/runtimeclass-preflight"
KUBECTL_STUB_MODE=forbid-mutate PATH="$stub_dir:$PATH" \
    "$RUNTIMECLASS_SOAK" --preflight-only --output "$preflight_dir" >"$preflight_dir.out"
require_grep '^result=preflight$' "$preflight_dir/summary.txt"
require_grep '^selected_nodes=1$' "$preflight_dir/summary.txt"
require_grep '^preflight_only=1$' "$preflight_dir/metadata.txt"
require_grep '^selected_nodes=1$' "$preflight_dir/preflight.txt"
require_grep '^node-a[[:space:]]+true[[:space:]]+production-soak$' \
    "$preflight_dir/selected-node-labels.tsv"
require_grep '^timestamp[[:space:]]+phase[[:space:]]+selected_nodes' \
    "$preflight_dir/resource-samples.tsv"
require_grep '^[0-9TZ:-]+[[:space:]]+preflight[[:space:]]+1([[:space:]]+0){10}$' \
    "$preflight_dir/resource-samples.tsv"
expect_failure runtimeclass-preflight-not-pass 'cluster summary result is preflight' \
    "$VERIFY_SCRIPT" --kind cluster "$preflight_dir"
expect_failure runtimeclass-preflight-verifier-gate '--preflight-only cannot satisfy verifier gate options' \
    "$RUNTIMECLASS_SOAK" --preflight-only --verify-min-duration-secs 1 \
        --output "$TMP_ROOT/runtimeclass-preflight-gate"
expect_failure runtimeclass-preflight-dry-run-combo '--preflight-only and --dry-run cannot be combined' \
    "$RUNTIMECLASS_SOAK" --preflight-only --dry-run \
        --output "$TMP_ROOT/runtimeclass-preflight-dry-run"

cleanup_dir="$TMP_ROOT/runtimeclass-cleanup"
PATH="$stub_dir:$PATH" "$RUNTIMECLASS_SOAK" --cleanup-only \
    --output "$cleanup_dir" >"$cleanup_dir.out"
require_grep '^result=cleanup$' "$cleanup_dir/summary.txt"
require_grep '^duration_secs=[0-9]+$' "$cleanup_dir/summary.txt"
require_grep '^cleanup_timeout_secs=300$' "$cleanup_dir/metadata.txt"
require_grep '^timestamp[[:space:]]+phase[[:space:]]+smoke_daemonsets' \
    "$cleanup_dir/post-cleanup-counts.tsv"
require_grep '^[0-9TZ:-]+[[:space:]]+post-cleanup([[:space:]]+0){5}$' \
    "$cleanup_dir/post-cleanup-counts.tsv"
expect_failure runtimeclass-cleanup-not-pass 'cluster summary result is cleanup' \
    "$VERIFY_SCRIPT" --kind cluster "$cleanup_dir"
expect_failure runtimeclass-cleanup-verifier-gate '--cleanup-only cannot satisfy verifier gate options' \
    env PATH="$stub_dir:$PATH" "$RUNTIMECLASS_SOAK" --cleanup-only \
        --verify-min-duration-secs 1 --output "$TMP_ROOT/runtimeclass-cleanup-gate"

log "Verifying RuntimeClass skip-jobs pass summary with kubectl stub"
skip_jobs_dir="$TMP_ROOT/runtimeclass-skip-jobs"
KUBECTL_STUB_MODE=with-runtime-pods PATH="$stub_dir:$PATH" \
    "$RUNTIMECLASS_SOAK" --skip-jobs \
    --duration 0 --output "$skip_jobs_dir" >"$skip_jobs_dir.out"
require_grep '^result=pass$' "$skip_jobs_dir/summary.txt"
require_grep '^skip_jobs=1$' "$skip_jobs_dir/metadata.txt"
require_grep '^[0-9TZ:-]+[[:space:]]+final[[:space:]]+1[[:space:]]+5[[:space:]]+0[[:space:]]+5([[:space:]]+0){7}$' \
    "$skip_jobs_dir/resource-samples.tsv"
"$VERIFY_SCRIPT" --kind cluster "$skip_jobs_dir" >"$skip_jobs_dir.verify.out"
require_grep 'PASS: cluster soak evidence verified' "$skip_jobs_dir.verify.out"
require_grep 'PASS: cluster soak evidence verified' "$skip_jobs_dir/verify.out"

log "Verifying RuntimeClass runner verifier gate failure summary with kubectl stub"
cluster_runner_gate_fail_dir="$TMP_ROOT/runtimeclass-gate-fail"
set +e
PATH="$stub_dir:$PATH" "$RUNTIMECLASS_SOAK" --skip-smoke --skip-complex --skip-jobs \
    --duration 0 --verify-min-duration-secs 3600 \
    --output "$cluster_runner_gate_fail_dir" \
    >"$cluster_runner_gate_fail_dir.out" 2>"$cluster_runner_gate_fail_dir.err"
cluster_runner_gate_fail_status="$?"
set -e
if [ "$cluster_runner_gate_fail_status" -eq 0 ]; then
    fail "RuntimeClass verifier gate failure stub unexpectedly passed"
fi
require_grep '^result=fail$' "$cluster_runner_gate_fail_dir/summary.txt"
require_grep '^exit_code=1$' "$cluster_runner_gate_fail_dir/summary.txt"
require_grep 'failed_command=.*/deploy/scripts/verify-soak-evidence.sh --kind cluster --min-duration-secs 3600 ' \
    "$cluster_runner_gate_fail_dir/summary.txt"
require_grep 'cluster soak duration too short' "$cluster_runner_gate_fail_dir/verify.out"
expect_failure runtimeclass-gate-failure-diagnostic 'cluster soak failed: exit_code=1' \
    "$VERIFY_SCRIPT" --kind cluster "$cluster_runner_gate_fail_dir"

log "Verifying RuntimeClass failure summary with kubectl stub"
failure_dir="$TMP_ROOT/runtimeclass-failure"
set +e
KUBECTL_STUB_MODE=runtimeclass-fail PATH="$stub_dir:$PATH" \
    "$RUNTIMECLASS_SOAK" --jobs 1 --parallelism 1 --duration 0 \
    --output "$failure_dir" >"$failure_dir.out" 2>"$failure_dir.err"
failure_status="$?"
set -e
if [ "$failure_status" -eq 0 ]; then
    fail "RuntimeClass failure stub unexpectedly passed"
fi
require_grep '^result=fail$' "$failure_dir/summary.txt"
require_grep '^duration_secs=[0-9]+$' "$failure_dir/summary.txt"
require_grep '^exit_code=17$' "$failure_dir/summary.txt"
require_grep '^failed_command=kubectl get runtimeclass a3s-box$' "$failure_dir/summary.txt"
expect_failure runtimeclass-failure-diagnostic 'cluster soak failed: exit_code=17' \
    "$VERIFY_SCRIPT" --kind cluster "$failure_dir"

log "Soak evidence self-test passed"
