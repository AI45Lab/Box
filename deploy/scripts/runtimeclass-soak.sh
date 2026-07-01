#!/usr/bin/env bash
#
# RuntimeClass cluster soak runner for a3s-box.
#
# Runs from the a3s-box repository root. It targets explicitly enrolled nodes
# only: nodes must carry both a3s-box.io/runtime=true and
# a3s-box.io/test-tier=production-soak, and may be tainted
# a3s-box.io/soak=true:NoSchedule.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

NAMESPACE="a3s-box-validation"
RUNTIME_CLASS="a3s-box"
RUNTIME_CLASS_HANDLER="a3s-box"

dns_safe_name_part() {
    local raw="$1"
    local safe
    safe="$(
        printf '%s' "$raw" \
            | tr '[:upper:]_' '[:lower:]-' \
            | sed -E 's/[^a-z0-9]+/-/g; s/^[^a-z0-9]+//; s/[^a-z0-9]+$//; s/-+/-/g'
    )"
    safe="${safe:0:48}"
    safe="$(
        printf '%s' "$safe" \
            | sed -E 's/^[^a-z0-9]+//; s/[^a-z0-9]+$//'
    )"
    if [ -z "$safe" ]; then
        safe="$(date -u +%Y%m%d%H%M%S)"
    fi
    printf '%s' "$safe"
}

RUN_ID="${A3S_BOX_CLUSTER_SOAK_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}"
STARTED_EPOCH="$(date +%s)"
OUTPUT_DIR="${A3S_BOX_CLUSTER_SOAK_OUTPUT_DIR:-$REPO_ROOT/src/target/a3s-box-runtimeclass-soak/$RUN_ID}"
IMAGE="${A3S_BOX_CLUSTER_SOAK_IMAGE:-docker.m.daocloud.io/library/alpine:latest}"
JOB_COMPLETIONS="${A3S_BOX_CLUSTER_SOAK_JOBS:-500}"
JOB_PARALLELISM="${A3S_BOX_CLUSTER_SOAK_PARALLELISM:-25}"
JOB_TIMEOUT_SECS="${A3S_BOX_CLUSTER_SOAK_JOB_TIMEOUT_SECS:-3600}"
JOB_SLEEP_SECS="${A3S_BOX_CLUSTER_SOAK_JOB_SLEEP_SECS:-1}"
SOAK_DURATION_SECS="${A3S_BOX_CLUSTER_SOAK_DURATION_SECS:-7200}"
SAMPLE_INTERVAL_SECS="${A3S_BOX_CLUSTER_SOAK_SAMPLE_INTERVAL_SECS:-300}"
VERIFY_MIN_DURATION_SECS="${A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_DURATION_SECS:-0}"
VERIFY_MIN_SAMPLES="${A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_SAMPLES:-0}"
VERIFY_MIN_SAMPLE_SPAN_SECS="${A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS:-0}"
VERIFY_MAX_SAMPLE_GAP_SECS="${A3S_BOX_CLUSTER_SOAK_VERIFY_MAX_SAMPLE_GAP_SECS:-0}"
CLEANUP_TIMEOUT_SECS="${A3S_BOX_CLUSTER_SOAK_CLEANUP_TIMEOUT_SECS:-300}"
SKIP_SMOKE=0
SKIP_COMPLEX=0
SKIP_JOBS=0
CLEANUP=0
CLEANUP_ONLY=0
DRY_RUN=0
PREFLIGHT_ONLY=0

SMOKE_MANIFEST="$REPO_ROOT/deploy/shim/runtimeclass-smoke.yaml"
COMPLEX_MANIFEST="$REPO_ROOT/deploy/shim/soak-complex.yaml"
VERIFY_SCRIPT="$REPO_ROOT/deploy/scripts/verify-soak-evidence.sh"
JOB_NAME="a3s-box-churn-$(dns_safe_name_part "$RUN_ID")"
STOP_SAMPLER=""
SAMPLER_PID=""
FAILED_AT=""
FAILED_COMMAND=""

usage() {
    cat <<'EOF'
Usage: deploy/scripts/runtimeclass-soak.sh [options]

Options:
  --jobs N              Short RuntimeClass Job completions (default: 500).
  --parallelism N       Short Job parallelism (default: 25).
  --duration SECS       Keep sampling long-lived workloads for this long (default: 7200).
  --sample-interval N   Evidence sampling interval in seconds (default: 300).
  --job-timeout SECS    Timeout while waiting for the churn Job (default: 3600).
  --job-sleep SECS      Per-churn-pod sleep after basic commands (default: 1).
  --image REF           Image for short churn jobs (default: mirror Alpine).
  --output DIR          Evidence output directory.
  --verify-min-duration-secs N
                        Require the final evidence summary duration to be at least N.
  --verify-min-samples N
                        Require at least N resource samples in the final evidence.
  --verify-min-sample-span-secs N
                        Require first-to-last resource sample span to be at least N.
  --verify-max-sample-gap-secs N
                        Require consecutive resource samples to be no more than N seconds apart.
  --cleanup-timeout SECS
                        Wait this long for generated workloads to disappear after cleanup (default: 300).
  --skip-smoke          Do not apply deploy/shim/runtimeclass-smoke.yaml.
  --skip-complex        Do not apply deploy/shim/soak-complex.yaml.
  --skip-jobs           Do not submit the short churn Job.
  --cleanup             Delete generated and checked-in validation workloads before exit.
  --cleanup-only        Delete checked-in validation workloads and exit.
  --dry-run             Generate metadata and Job YAML only; do not call kubectl.
  --preflight-only      Check RuntimeClass and selected nodes; do not mutate the cluster.
  -h, --help            Show this help.

Environment mirrors:
  A3S_BOX_CLUSTER_SOAK_JOBS
  A3S_BOX_CLUSTER_SOAK_PARALLELISM
  A3S_BOX_CLUSTER_SOAK_DURATION_SECS
  A3S_BOX_CLUSTER_SOAK_OUTPUT_DIR
  A3S_BOX_CLUSTER_SOAK_IMAGE
  A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_DURATION_SECS
  A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_SAMPLES
  A3S_BOX_CLUSTER_SOAK_VERIFY_MIN_SAMPLE_SPAN_SECS
  A3S_BOX_CLUSTER_SOAK_VERIFY_MAX_SAMPLE_GAP_SECS
  A3S_BOX_CLUSTER_SOAK_CLEANUP_TIMEOUT_SECS

Examples:
  deploy/scripts/runtimeclass-soak.sh --dry-run --jobs 10 --duration 0
  deploy/scripts/runtimeclass-soak.sh --preflight-only
  deploy/scripts/runtimeclass-soak.sh --jobs 500 --parallelism 25 --duration 7200
  deploy/scripts/runtimeclass-soak.sh --cleanup-only
EOF
}

require_option_value() {
    local option="$1"
    local remaining="$2"
    if [ "$remaining" -eq 0 ]; then
        echo "$option requires a value" >&2
        exit 2
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --jobs)
            shift; require_option_value "--jobs" "$#"; JOB_COMPLETIONS="$1" ;;
        --parallelism)
            shift; require_option_value "--parallelism" "$#"; JOB_PARALLELISM="$1" ;;
        --duration)
            shift; require_option_value "--duration" "$#"; SOAK_DURATION_SECS="$1" ;;
        --sample-interval)
            shift; require_option_value "--sample-interval" "$#"; SAMPLE_INTERVAL_SECS="$1" ;;
        --job-timeout)
            shift; require_option_value "--job-timeout" "$#"; JOB_TIMEOUT_SECS="$1" ;;
        --job-sleep)
            shift; require_option_value "--job-sleep" "$#"; JOB_SLEEP_SECS="$1" ;;
        --image)
            shift; require_option_value "--image" "$#"; IMAGE="$1" ;;
        --output)
            shift; require_option_value "--output" "$#"; OUTPUT_DIR="$1" ;;
        --verify-min-duration-secs)
            shift; require_option_value "--verify-min-duration-secs" "$#"; VERIFY_MIN_DURATION_SECS="$1" ;;
        --verify-min-samples)
            shift; require_option_value "--verify-min-samples" "$#"; VERIFY_MIN_SAMPLES="$1" ;;
        --verify-min-sample-span-secs)
            shift; require_option_value "--verify-min-sample-span-secs" "$#"; VERIFY_MIN_SAMPLE_SPAN_SECS="$1" ;;
        --verify-max-sample-gap-secs)
            shift; require_option_value "--verify-max-sample-gap-secs" "$#"; VERIFY_MAX_SAMPLE_GAP_SECS="$1" ;;
        --cleanup-timeout)
            shift; require_option_value "--cleanup-timeout" "$#"; CLEANUP_TIMEOUT_SECS="$1" ;;
        --skip-smoke)
            SKIP_SMOKE=1 ;;
        --skip-complex)
            SKIP_COMPLEX=1 ;;
        --skip-jobs)
            SKIP_JOBS=1 ;;
        --cleanup)
            CLEANUP=1 ;;
        --cleanup-only)
            CLEANUP=1; CLEANUP_ONLY=1 ;;
        --dry-run)
            DRY_RUN=1 ;;
        --preflight-only)
            PREFLIGHT_ONLY=1 ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2 ;;
    esac
    shift
done

log() {
    printf '\n==> %s\n' "$*"
}

one_line() {
    printf '%s' "$1" | tr '\012\011' '  '
}

run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    "$@" || {
        local exit_code="$?"
        FAILED_AT="${BASH_SOURCE[1]:-${BASH_SOURCE[0]}}:${BASH_LINENO[0]:-$LINENO}"
        FAILED_COMMAND="$(one_line "$*")"
        return "$exit_code"
    }
}

record_failure() {
    local exit_code="$?"
    if [ -z "$FAILED_AT" ]; then
        FAILED_AT="${BASH_SOURCE[1]:-$0}:${BASH_LINENO[0]:-$LINENO}"
    fi
    if [ -z "$FAILED_COMMAND" ]; then
        FAILED_COMMAND="$(one_line "$BASH_COMMAND")"
    fi
    return "$exit_code"
}

duration_secs() {
    local now
    now="$(date +%s)"
    echo $((now - STARTED_EPOCH))
}

is_non_negative_int() {
    case "$1" in
        ''|*[!0-9]*)
            return 1 ;;
        *)
            return 0 ;;
    esac
}

require_positive_int() {
    local name="$1"
    local value="$2"
    if ! is_non_negative_int "$value" || [ "$value" -eq 0 ]; then
        echo "$name must be a positive integer" >&2
        exit 2
    fi
}

require_non_negative_int() {
    local name="$1"
    local value="$2"
    if ! is_non_negative_int "$value"; then
        echo "$name must be a non-negative integer" >&2
        exit 2
    fi
}

has_verifier_gate() {
    [ "$VERIFY_MIN_DURATION_SECS" -gt 0 ] ||
        [ "$VERIFY_MIN_SAMPLES" -gt 0 ] ||
        [ "$VERIFY_MIN_SAMPLE_SPAN_SECS" -gt 0 ] ||
        [ "$VERIFY_MAX_SAMPLE_GAP_SECS" -gt 0 ]
}

validate_args() {
    require_non_negative_int "--duration" "$SOAK_DURATION_SECS"
    require_positive_int "--sample-interval" "$SAMPLE_INTERVAL_SECS"
    require_positive_int "--job-timeout" "$JOB_TIMEOUT_SECS"
    require_non_negative_int "--job-sleep" "$JOB_SLEEP_SECS"
    require_non_negative_int "--verify-min-duration-secs" "$VERIFY_MIN_DURATION_SECS"
    require_non_negative_int "--verify-min-samples" "$VERIFY_MIN_SAMPLES"
    require_non_negative_int "--verify-min-sample-span-secs" "$VERIFY_MIN_SAMPLE_SPAN_SECS"
    require_non_negative_int "--verify-max-sample-gap-secs" "$VERIFY_MAX_SAMPLE_GAP_SECS"
    require_non_negative_int "--cleanup-timeout" "$CLEANUP_TIMEOUT_SECS"
    if [ "$DRY_RUN" -eq 1 ] && has_verifier_gate; then
        echo "--dry-run cannot satisfy verifier gate options" >&2
        exit 2
    fi
    if [ "$PREFLIGHT_ONLY" -eq 1 ] && has_verifier_gate; then
        echo "--preflight-only cannot satisfy verifier gate options" >&2
        exit 2
    fi
    if [ "$PREFLIGHT_ONLY" -eq 1 ] && [ "$DRY_RUN" -eq 1 ]; then
        echo "--preflight-only and --dry-run cannot be combined" >&2
        exit 2
    fi
    if [ "$PREFLIGHT_ONLY" -eq 1 ] && [ "$CLEANUP_ONLY" -eq 1 ]; then
        echo "--preflight-only and --cleanup-only cannot be combined" >&2
        exit 2
    fi
    if [ "$CLEANUP_ONLY" -eq 1 ] && has_verifier_gate; then
        echo "--cleanup-only cannot satisfy verifier gate options" >&2
        exit 2
    fi
    if [ "$SKIP_JOBS" -eq 0 ] && [ "$CLEANUP_ONLY" -eq 0 ]; then
        require_positive_int "--jobs" "$JOB_COMPLETIONS"
        require_positive_int "--parallelism" "$JOB_PARALLELISM"
        if [ "$JOB_PARALLELISM" -gt "$JOB_COMPLETIONS" ]; then
            echo "--parallelism cannot exceed --jobs" >&2
            exit 2
        fi
    fi
}

need_kubectl() {
    command -v kubectl >/dev/null 2>&1 || {
        echo "kubectl not found on PATH" >&2
        exit 2
    }
}

write_metadata() {
    {
        echo "run_id=$RUN_ID"
        echo "started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "repo_root=$REPO_ROOT"
        echo "namespace=$NAMESPACE"
        echo "runtime_class=$RUNTIME_CLASS"
        echo "runtime_class_handler=$RUNTIME_CLASS_HANDLER"
        echo "image=$IMAGE"
        echo "job_name=$JOB_NAME"
        echo "job_completions=$JOB_COMPLETIONS"
        echo "job_parallelism=$JOB_PARALLELISM"
        echo "job_timeout_secs=$JOB_TIMEOUT_SECS"
        echo "job_sleep_secs=$JOB_SLEEP_SECS"
        echo "soak_duration_secs=$SOAK_DURATION_SECS"
        echo "sample_interval_secs=$SAMPLE_INTERVAL_SECS"
        echo "verify_min_duration_secs=$VERIFY_MIN_DURATION_SECS"
        echo "verify_min_samples=$VERIFY_MIN_SAMPLES"
        echo "verify_min_sample_span_secs=$VERIFY_MIN_SAMPLE_SPAN_SECS"
        echo "verify_max_sample_gap_secs=$VERIFY_MAX_SAMPLE_GAP_SECS"
        echo "cleanup_timeout_secs=$CLEANUP_TIMEOUT_SECS"
        echo "skip_smoke=$SKIP_SMOKE"
        echo "skip_complex=$SKIP_COMPLEX"
        echo "skip_jobs=$SKIP_JOBS"
        echo "cleanup=$CLEANUP"
        echo "cleanup_only=$CLEANUP_ONLY"
        echo "dry_run=$DRY_RUN"
        echo "preflight_only=$PREFLIGHT_ONLY"
        echo "git_sha=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || true)"
        echo "git_status_short:"
        git -C "$REPO_ROOT" status --short 2>/dev/null || true
        if [ "$DRY_RUN" -eq 0 ]; then
            echo "kubectl_context=$(kubectl config current-context 2>/dev/null || true)"
            echo "kubectl_version:"
            kubectl version --client=true 2>/dev/null || true
        fi
    } >"$OUTPUT_DIR/metadata.txt"
}

write_job_manifest() {
    local path="$1"
    cat >"$path" <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: ${JOB_NAME}
  namespace: ${NAMESPACE}
  labels:
    app: a3s-box-runtimeclass-churn
    a3s-box.io/run-id: "${RUN_ID}"
spec:
  completions: ${JOB_COMPLETIONS}
  parallelism: ${JOB_PARALLELISM}
  backoffLimit: 0
  ttlSecondsAfterFinished: 3600
  template:
    metadata:
      labels:
        app: a3s-box-runtimeclass-churn
        a3s-box.io/run-id: "${RUN_ID}"
    spec:
      runtimeClassName: ${RUNTIME_CLASS}
      nodeSelector:
        a3s-box.io/runtime: "true"
        a3s-box.io/test-tier: production-soak
      tolerations:
        - key: a3s-box.io/soak
          operator: Equal
          value: "true"
          effect: NoSchedule
      restartPolicy: Never
      terminationGracePeriodSeconds: 30
      containers:
        - name: churn
          image: ${IMAGE}
          command:
            - sh
            - -c
            - |
              echo A3S_BOX_JOB_START \$(date -u +%Y-%m-%dT%H:%M:%SZ) \$(hostname)
              uname -a
              echo A3S_BOX_JOB_RUNTIME_CLASS=${RUNTIME_CLASS}
              sleep ${JOB_SLEEP_SECS}
              echo A3S_BOX_JOB_DONE \$(date -u +%Y-%m-%dT%H:%M:%SZ)
          resources:
            requests:
              cpu: "10m"
              memory: "64Mi"
            limits:
              cpu: "250m"
              memory: "256Mi"
EOF
}

ensure_namespace() {
    kubectl create namespace "$NAMESPACE" --dry-run=client -o yaml | kubectl apply -f -
    kubectl label namespace "$NAMESPACE" a3s-box.io/validation=true --overwrite
}

collect_selected_node_evidence() {
    kubectl get nodes -l a3s-box.io/runtime=true,a3s-box.io/test-tier=production-soak -o wide \
        >"$OUTPUT_DIR/selected-nodes.txt"
    kubectl get nodes -l a3s-box.io/runtime=true,a3s-box.io/test-tier=production-soak \
        -o custom-columns=NAME:.metadata.name --no-headers \
        >"$OUTPUT_DIR/selected-node-names.txt"
    kubectl get nodes -l a3s-box.io/runtime=true,a3s-box.io/test-tier=production-soak \
        -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.metadata.labels.a3s-box\.io/runtime}{"\t"}{.metadata.labels.a3s-box\.io/test-tier}{"\n"}{end}' \
        >"$OUTPUT_DIR/selected-node-labels.tsv"
}

preflight_cluster() {
    log "Preflight cluster checks"
    run kubectl get runtimeclass "$RUNTIME_CLASS"
    kubectl get runtimeclass "$RUNTIME_CLASS" -o yaml >"$OUTPUT_DIR/runtimeclass.yaml"
    local nodes
    nodes="$(kubectl get nodes \
        -l a3s-box.io/runtime=true,a3s-box.io/test-tier=production-soak \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')"
    echo "selected_nodes=$nodes" | tee "$OUTPUT_DIR/preflight.txt"
    if [ "$nodes" -eq 0 ]; then
        echo "no nodes match a3s-box runtime + production-soak labels" >&2
        exit 1
    fi
    collect_selected_node_evidence
}

sample_once() {
    local phase="$1"
    local file="$OUTPUT_DIR/resource-samples.tsv"
    local selected_nodes
    selected_nodes="$(kubectl get nodes \
        -l a3s-box.io/runtime=true,a3s-box.io/test-tier=production-soak \
        --no-headers 2>/dev/null | wc -l | tr -d ' ')"

    local pod_counts
    pod_counts="$(kubectl -n "$NAMESPACE" get pods --no-headers 2>/dev/null |
        awk '
            BEGIN { pending=0; running=0; succeeded=0; failed=0; unknown=0; restarts=0; total=0 }
            {
                total++;
                restarts += $4 + 0;
                if ($3 == "Pending") pending++;
                else if ($3 == "Running") running++;
                else if ($3 == "Succeeded" || $3 == "Completed") succeeded++;
                else if ($3 == "Failed") failed++;
                else unknown++;
            }
            END { printf "%d\t%d\t%d\t%d\t%d\t%d\t%d", total, pending, running, succeeded, failed, unknown, restarts }
        ')"

    local job_status active succeeded failed
    job_status="$(kubectl -n "$NAMESPACE" get job "$JOB_NAME" \
        -o jsonpath='{.status.active}{"\t"}{.status.succeeded}{"\t"}{.status.failed}' 2>/dev/null || true)"
    active="$(printf '%s' "$job_status" | awk -F '\t' 'BEGIN { value = 0 } NF > 0 && $1 != "" { value = $1 } END { print value }')"
    succeeded="$(printf '%s' "$job_status" | awk -F '\t' 'BEGIN { value = 0 } NF > 0 && $2 != "" { value = $2 } END { print value }')"
    failed="$(printf '%s' "$job_status" | awk -F '\t' 'BEGIN { value = 0 } NF > 0 && $3 != "" { value = $3 } END { print value }')"

    if [ ! -f "$file" ]; then
        printf 'timestamp\tphase\tselected_nodes\tpods_total\tpods_pending\tpods_running\tpods_succeeded\tpods_failed\tpods_unknown\tpod_restarts\tjob_active\tjob_succeeded\tjob_failed\n' >"$file"
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$phase" \
        "$selected_nodes" \
        "$pod_counts" \
        "$active" \
        "$succeeded" \
        "$failed" >>"$file"
}

sample_loop() {
    while [ ! -f "$STOP_SAMPLER" ]; do
        sample_once "interval" || true
        local slept=0
        while [ "$slept" -lt "$SAMPLE_INTERVAL_SECS" ] && [ ! -f "$STOP_SAMPLER" ]; do
            sleep 1
            slept=$((slept + 1))
        done
    done
}

start_sampler() {
    STOP_SAMPLER="$(mktemp "$OUTPUT_DIR/stop-sampler.XXXXXX")"
    rm -f "$STOP_SAMPLER"
    sample_loop &
    SAMPLER_PID="$!"
}

stop_sampler() {
    if [ -n "${SAMPLER_PID:-}" ]; then
        : >"$STOP_SAMPLER"
        wait "$SAMPLER_PID" 2>/dev/null || true
        SAMPLER_PID=""
    fi
}

collect_exec_proof() {
    local selector="$1"
    local output="$2"
    local pods pod node workload

    {
        echo "selector=$selector"
        if ! pods="$(kubectl -n "$NAMESPACE" get pods -l "$selector" \
            -o custom-columns=NAME:.metadata.name,NODE:.spec.nodeName,WORKLOAD:.metadata.labels.workload --no-headers 2>&1)"; then
            echo "pod_list_result=fail"
            printf '%s\n' "$pods"
            return
        fi
        echo "pod_list_result=pass"
        printf '%s\n' "$pods" | awk 'NF > 0 { printf "pod=%s node=%s workload=%s\n", $1, $2, $3 }'
        while read -r pod node workload _; do
            [ -n "$pod" ] || continue
            [ -n "$node" ] || node="<unknown>"
            [ -n "$workload" ] || workload="<none>"
            echo "exec_pod=$pod node=$node workload=$workload"
            if kubectl -n "$NAMESPACE" exec "$pod" -- sh -c \
                'echo A3S_BOX_EXEC_OK "$(hostname)"; uname -m' 2>&1; then
                echo "exec_result=pass pod=$pod node=$node workload=$workload"
            else
                echo "exec_result=fail pod=$pod node=$node workload=$workload"
            fi
        done < <(printf '%s\n' "$pods" | awk 'NF > 0 { print $1, $2, $3 }')
    } >"$output"
}

collect_evidence() {
    log "Collecting evidence"
    sample_once "final" || true
    kubectl get runtimeclass "$RUNTIME_CLASS" -o yaml >"$OUTPUT_DIR/runtimeclass.yaml" 2>&1 || true
    collect_selected_node_evidence || true
    kubectl -n "$NAMESPACE" get all -o wide >"$OUTPUT_DIR/final-get-all.txt" 2>&1 || true
    kubectl -n "$NAMESPACE" get pods -o yaml >"$OUTPUT_DIR/final-pods.yaml" 2>&1 || true
    kubectl -n "$NAMESPACE" get pods \
        -o custom-columns=NAME:.metadata.name,RUNTIMECLASS:.spec.runtimeClassName \
        --no-headers >"$OUTPUT_DIR/final-pod-runtimeclasses.tsv" 2>&1 || true
    kubectl -n "$NAMESPACE" get pods \
        -o custom-columns=NAME:.metadata.name,NODE:.spec.nodeName \
        --no-headers >"$OUTPUT_DIR/final-pod-nodes.tsv" 2>&1 || true
    kubectl -n "$NAMESPACE" get pods \
        -o custom-columns=NAME:.metadata.name,PHASE:.status.phase,RESTARTS:.status.containerStatuses[*].restartCount \
        --no-headers >"$OUTPUT_DIR/final-pod-statuses.tsv" 2>&1 || true
    kubectl -n "$NAMESPACE" get events --sort-by=.lastTimestamp >"$OUTPUT_DIR/events.txt" 2>&1 || true
    kubectl -n "$NAMESPACE" get events \
        -o custom-columns=TYPE:.type,REASON:.reason,OBJECT:.involvedObject.name,MESSAGE:.message \
        --no-headers >"$OUTPUT_DIR/events.tsv" 2>&1 || true
    kubectl -n "$NAMESPACE" describe pods >"$OUTPUT_DIR/describe-pods.txt" 2>&1 || true
    kubectl -n "$NAMESPACE" logs -l soak=cplx --prefix --tail=-1 --all-containers=true \
        >"$OUTPUT_DIR/complex-logs.txt" 2>&1 || true
    if [ "$SKIP_SMOKE" -eq 0 ]; then
        collect_exec_proof "app=a3s-box-runtimeclass-smoke" "$OUTPUT_DIR/smoke-exec.txt" || true
    fi
    if [ "$SKIP_COMPLEX" -eq 0 ]; then
        collect_exec_proof "soak=cplx" "$OUTPUT_DIR/complex-exec.txt" || true
    fi
    if [ "$SKIP_JOBS" -eq 0 ]; then
        kubectl -n "$NAMESPACE" get job "$JOB_NAME" -o yaml >"$OUTPUT_DIR/job.yaml" 2>&1 || true
        kubectl -n "$NAMESPACE" get job "$JOB_NAME" \
            -o jsonpath='{.spec.template.spec.runtimeClassName}{"\n"}' \
            >"$OUTPUT_DIR/job-runtimeclass.txt" 2>&1 || true
        kubectl -n "$NAMESPACE" get pods -l job-name="$JOB_NAME" -o wide \
            >"$OUTPUT_DIR/job-pods.txt" 2>&1 || true
        kubectl -n "$NAMESPACE" get pods -l job-name="$JOB_NAME" \
            -o custom-columns=NAME:.metadata.name,PHASE:.status.phase,RESTARTS:.status.containerStatuses[*].restartCount,NODE:.spec.nodeName \
            --no-headers >"$OUTPUT_DIR/job-pod-statuses.tsv" 2>&1 || true
        kubectl -n "$NAMESPACE" logs -l job-name="$JOB_NAME" --prefix --tail=-1 --all-containers=true \
            >"$OUTPUT_DIR/job-logs.txt" 2>&1 || true
    fi
}

resource_count() {
    local output

    output="$("$@" --no-headers 2>/dev/null || true)"
    printf '%s\n' "$output" | awk 'NF > 0 { count++ } END { print count + 0 }'
}

cleanup_counts_row() {
    printf '%s\tpost-cleanup\t%s\t%s\t%s\t%s\t%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$(resource_count kubectl -n "$NAMESPACE" get daemonset a3s-box-runtimeclass-smoke)" \
        "$(resource_count kubectl -n "$NAMESPACE" get pods -l app=a3s-box-runtimeclass-smoke)" \
        "$(resource_count kubectl -n "$NAMESPACE" get pods -l soak=cplx)" \
        "$(resource_count kubectl -n "$NAMESPACE" get jobs -l app=a3s-box-runtimeclass-churn)" \
        "$(resource_count kubectl -n "$NAMESPACE" get pods -l job-name="$JOB_NAME")"
}

cleanup_pending_total() {
    cleanup_counts_row | awk -F '\t' '{ print $3 + $4 + $5 + $6 + $7 }'
}

wait_for_cleanup_completion() {
    if [ "$CLEANUP" -eq 0 ]; then
        return
    fi

    local deadline now pending
    deadline=$(( $(date +%s) + CLEANUP_TIMEOUT_SECS ))
    log "Waiting up to ${CLEANUP_TIMEOUT_SECS}s for generated workloads to disappear"
    while :; do
        pending="$(cleanup_pending_total)"
        [ "$pending" -eq 0 ] && return
        now="$(date +%s)"
        [ "$now" -ge "$deadline" ] && {
            echo "cleanup timed out with $pending generated workloads still present" >&2
            return 1
        }
        sleep 2
    done
}

collect_post_cleanup_evidence() {
    if [ "$CLEANUP" -eq 0 ]; then
        return
    fi

    log "Collecting post-cleanup evidence"
    local namespace_ref namespace_err
    namespace_err="$OUTPUT_DIR/post-cleanup-namespace.err"
    if namespace_ref="$(kubectl get namespace "$NAMESPACE" --ignore-not-found -o name 2>"$namespace_err")"; then
        if [ -n "$namespace_ref" ]; then
            kubectl get namespace "$NAMESPACE" >"$OUTPUT_DIR/post-cleanup-namespace.txt" 2>&1 || true
            kubectl -n "$NAMESPACE" get all -o wide >"$OUTPUT_DIR/post-cleanup-get-all.txt" 2>&1 || true
        else
            echo "namespace=$NAMESPACE status=not-found" >"$OUTPUT_DIR/post-cleanup-namespace.txt"
            echo "namespace=$NAMESPACE status=not-found; no Kubernetes resources remain" \
                >"$OUTPUT_DIR/post-cleanup-get-all.txt"
        fi
    else
        cat "$namespace_err" >"$OUTPUT_DIR/post-cleanup-namespace.txt"
        echo "namespace=$NAMESPACE status=unknown; namespace query failed" \
            >"$OUTPUT_DIR/post-cleanup-get-all.txt"
    fi
    rm -f "$namespace_err"

    {
        printf 'timestamp\tphase\tsmoke_daemonsets\tsmoke_pods\tcomplex_pods\tchurn_jobs\tchurn_pods\n'
        cleanup_counts_row
    } >"$OUTPUT_DIR/post-cleanup-counts.tsv"
}

write_failure_summary() {
    local exit_code="$1"
    {
        echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "result=fail"
        echo "duration_secs=$(duration_secs)"
        echo "exit_code=$exit_code"
        echo "failed_at=${FAILED_AT:-unknown}"
        echo "failed_command=${FAILED_COMMAND:-unknown}"
        echo "evidence_dir=$OUTPUT_DIR"
    } >"$OUTPUT_DIR/summary.txt"
}

write_preflight_summary() {
    local selected_nodes="$1"
    {
        echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "result=preflight"
        echo "duration_secs=$(duration_secs)"
        echo "selected_nodes=$selected_nodes"
        echo "evidence_dir=$OUTPUT_DIR"
    } >"$OUTPUT_DIR/summary.txt"
}

run_soak_verifier() {
    local args=("$VERIFY_SCRIPT" --kind cluster)
    local output="$OUTPUT_DIR/verify.out"
    local exit_code

    if [ "$VERIFY_MIN_DURATION_SECS" -gt 0 ]; then
        args+=(--min-duration-secs "$VERIFY_MIN_DURATION_SECS")
    fi
    if [ "$VERIFY_MIN_SAMPLES" -gt 0 ]; then
        args+=(--min-samples "$VERIFY_MIN_SAMPLES")
    fi
    if [ "$VERIFY_MIN_SAMPLE_SPAN_SECS" -gt 0 ]; then
        args+=(--min-sample-span-secs "$VERIFY_MIN_SAMPLE_SPAN_SECS")
    fi
    if [ "$VERIFY_MAX_SAMPLE_GAP_SECS" -gt 0 ]; then
        args+=(--max-sample-gap-secs "$VERIFY_MAX_SAMPLE_GAP_SECS")
    fi

    set +e
    run "${args[@]}" "$OUTPUT_DIR" >"$output" 2>&1
    exit_code="$?"
    set -e
    cat "$output"
    return "$exit_code"
}

handle_exit() {
    local exit_code="$1"
    if [ "$exit_code" -eq 0 ]; then
        return
    fi

    set +e
    trap - ERR
    trap - EXIT
    stop_sampler
    if [ -f "$OUTPUT_DIR/summary.txt" ] && grep -q '^result=pass$' "$OUTPUT_DIR/summary.txt"; then
        {
            echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
            echo "result=fail"
            echo "duration_secs=$(duration_secs)"
            echo "exit_code=$exit_code"
            echo "failed_at=${FAILED_AT:-unknown}"
            echo "failed_command=${FAILED_COMMAND:-unknown}"
            echo "evidence_dir=$OUTPUT_DIR"
        } >"$OUTPUT_DIR/failure-summary.txt"
        collect_post_cleanup_evidence
        log "RuntimeClass soak failed after cleanup: exit_code=$exit_code evidence=$OUTPUT_DIR"
        exit "$exit_code"
    fi

    write_failure_summary "$exit_code"
    collect_evidence
    cleanup_workloads
    wait_for_cleanup_completion
    collect_post_cleanup_evidence
    log "RuntimeClass soak failed: exit_code=$exit_code evidence=$OUTPUT_DIR"
    exit "$exit_code"
}

cleanup_workloads() {
    if [ "$CLEANUP" -eq 0 ]; then
        return
    fi
    log "Cleaning up validation workloads"
    if [ "$SKIP_JOBS" -eq 0 ]; then
        kubectl -n "$NAMESPACE" delete job "$JOB_NAME" --ignore-not-found=true
    fi
    if [ "$SKIP_COMPLEX" -eq 0 ]; then
        kubectl delete -f "$COMPLEX_MANIFEST" --ignore-not-found=true
    fi
    if [ "$SKIP_SMOKE" -eq 0 ]; then
        kubectl delete -f "$SMOKE_MANIFEST" --ignore-not-found=true
    fi
}

main() {
    validate_args
    mkdir -p "$OUTPUT_DIR"
    trap 'record_failure' ERR
    trap 'handle_exit "$?"' EXIT
    local job_manifest="$OUTPUT_DIR/churn-job.yaml"
    if [ "$SKIP_JOBS" -eq 0 ] && [ "$CLEANUP_ONLY" -eq 0 ] && [ "$PREFLIGHT_ONLY" -eq 0 ]; then
        write_job_manifest "$job_manifest"
    fi
    write_metadata

    if [ "$DRY_RUN" -eq 1 ]; then
        log "Dry run complete"
        echo "evidence_dir=$OUTPUT_DIR"
        if [ "$SKIP_JOBS" -eq 0 ] && [ "$CLEANUP_ONLY" -eq 0 ] && [ "$PREFLIGHT_ONLY" -eq 0 ]; then
            echo "generated_job_manifest=$job_manifest"
        fi
        exit 0
    fi

    need_kubectl
    if [ "$CLEANUP_ONLY" -eq 1 ]; then
        cleanup_workloads
        wait_for_cleanup_completion
        collect_evidence
        collect_post_cleanup_evidence
        {
            echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
            echo "result=cleanup"
            echo "duration_secs=$(duration_secs)"
            echo "evidence_dir=$OUTPUT_DIR"
        } >"$OUTPUT_DIR/summary.txt"
        log "RuntimeClass validation cleanup completed; evidence: $OUTPUT_DIR"
        exit 0
    fi

    preflight_cluster
    if [ "$PREFLIGHT_ONLY" -eq 1 ]; then
        local selected_nodes
        selected_nodes="$(awk -F= '$1 == "selected_nodes" { print $2; exit }' "$OUTPUT_DIR/preflight.txt")"
        sample_once "preflight" || true
        write_preflight_summary "${selected_nodes:-0}"
        log "RuntimeClass preflight completed; evidence: $OUTPUT_DIR"
        exit 0
    fi

    ensure_namespace
    start_sampler

    if [ "$SKIP_SMOKE" -eq 0 ]; then
        log "Applying RuntimeClass smoke DaemonSet"
        run kubectl apply -f "$SMOKE_MANIFEST"
        run kubectl -n "$NAMESPACE" rollout status ds/a3s-box-runtimeclass-smoke --timeout=300s
    fi

    if [ "$SKIP_COMPLEX" -eq 0 ]; then
        log "Applying complex long-lived soak pods"
        run kubectl apply -f "$COMPLEX_MANIFEST"
        run kubectl -n "$NAMESPACE" wait pod -l soak=cplx --for=condition=Ready --timeout=600s
    fi

    if [ "$SKIP_JOBS" -eq 0 ]; then
        log "Submitting short RuntimeClass churn Job"
        run kubectl apply -f "$job_manifest"
        run kubectl -n "$NAMESPACE" wait "job/$JOB_NAME" --for=condition=complete --timeout="${JOB_TIMEOUT_SECS}s"
    fi

    if [ "$SOAK_DURATION_SECS" -gt 0 ]; then
        log "Keeping workload under observation for ${SOAK_DURATION_SECS}s"
        local end now remaining sleep_for
        end=$(( $(date +%s) + SOAK_DURATION_SECS ))
        while :; do
            now="$(date +%s)"
            [ "$now" -ge "$end" ] && break
            remaining=$((end - now))
            sleep_for="$SAMPLE_INTERVAL_SECS"
            [ "$remaining" -lt "$sleep_for" ] && sleep_for="$remaining"
            sleep "$sleep_for"
        done
    fi

    stop_sampler
    collect_evidence
    cleanup_workloads
    wait_for_cleanup_completion
    collect_post_cleanup_evidence
    {
        echo "finished_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "result=pass"
        echo "duration_secs=$(duration_secs)"
        echo "evidence_dir=$OUTPUT_DIR"
    } >"$OUTPUT_DIR/summary.txt"
    run_soak_verifier
    log "RuntimeClass soak completed; evidence: $OUTPUT_DIR"
}

main
