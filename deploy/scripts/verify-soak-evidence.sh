#!/usr/bin/env bash
#
# Verify a3s-box host or RuntimeClass soak evidence bundles.

set -euo pipefail

KIND="auto"
ALLOW_DRY_RUN=0
MIN_DURATION_SECS=0
MIN_SAMPLES=0
MIN_SAMPLE_SPAN_SECS=0
MAX_SAMPLE_GAP_SECS=0
EVIDENCE_DIR=""
SUMMARY_DURATION_SAMPLE_SPAN_SKEW_SECS=60

usage() {
    cat <<'EOF'
Usage: deploy/scripts/verify-soak-evidence.sh [options] EVIDENCE_DIR

Options:
  --kind host|cluster|auto  Evidence kind (default: auto).
  --allow-dry-run           Accept dry-run metadata without a completed summary.
  --min-duration-secs N     Require summary duration_secs to be at least N.
  --min-samples N           Require resource-samples.tsv to contain at least N samples.
  --min-sample-span-secs N  Require first-to-last resource sample span to be at least N.
  --max-sample-gap-secs N   Require consecutive resource samples to be no more than N seconds apart.
  -h, --help                Show this help.

Checks:
  host     summary has result=pass and failed_iterations=0, final
           shim/mount/box/socket counts do not exceed the start sample, and
           start/final/iteration CLI snapshots plus per-iteration logs exist.
  cluster  summary result=pass, selected nodes are nonzero, evidence proves
           the RuntimeClass object, runtimeClassName, kubectl exec, and
           selected-node labels and placement via explicit artifacts, failed
           pods/jobs and pod restarts are zero, unresolved final pods/jobs are
           zero, and job completions match metadata unless the run skipped jobs;
           final pod/event/describe/log artifacts must exist.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --kind)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--kind requires a value" >&2
                exit 2
            fi
            KIND="$1"
            ;;
        --allow-dry-run)
            ALLOW_DRY_RUN=1
            ;;
        --min-duration-secs)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--min-duration-secs requires a value" >&2
                exit 2
            fi
            MIN_DURATION_SECS="$1"
            ;;
        --min-samples)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--min-samples requires a value" >&2
                exit 2
            fi
            MIN_SAMPLES="$1"
            ;;
        --min-sample-span-secs)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--min-sample-span-secs requires a value" >&2
                exit 2
            fi
            MIN_SAMPLE_SPAN_SECS="$1"
            ;;
        --max-sample-gap-secs)
            shift
            if [ "$#" -eq 0 ]; then
                echo "--max-sample-gap-secs requires a value" >&2
                exit 2
            fi
            MAX_SAMPLE_GAP_SECS="$1"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
        *)
            if [ -n "$EVIDENCE_DIR" ]; then
                echo "only one evidence directory may be provided" >&2
                exit 2
            fi
            EVIDENCE_DIR="$1"
            ;;
    esac
    shift
done

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

pass() {
    echo "PASS: $*"
}

is_non_negative_int() {
    case "$1" in
        ''|*[!0-9]*)
            return 1
            ;;
        *)
            return 0
            ;;
    esac
}

require_file() {
    local path="$1"
    [ -f "$path" ] || fail "missing required file: $path"
}

require_nonempty_file() {
    local path="$1"
    require_file "$path"
    [ -s "$path" ] || fail "required file is empty: $path"
}

require_kubectl_artifact() {
    local path="$1"
    local label="$2"

    require_nonempty_file "$path"
    if grep -Eq '^(Error from server|The connection to the server|Unable to connect to the server|Unable to connect|error: )' "$path"; then
        fail "$label contains kubectl collection error: $path"
    fi
}

require_runtimeclass_rows() {
    local path="$1"
    local expected="$2"
    local label="$3"

    awk -v expected="$expected" '
        NF > 0 {
            rows++
            name = $1
            runtime_class = $2
            if (runtime_class != expected) {
                printf "%s has runtimeClassName=%s expected=%s\n", name, runtime_class, expected > "/dev/stderr"
                bad = 1
            }
        }
        END {
            if (rows == 0 || bad) {
                exit 1
            }
        }
    ' "$path" || fail "$label must list only runtimeClassName: $expected"
}

require_runtimeclass_value() {
    local path="$1"
    local expected="$2"
    local label="$3"
    local value

    value="$(awk 'NF > 0 { print $1; exit }' "$path")"
    [ "$value" = "$expected" ] || fail "$label runtimeClassName is ${value:-missing}; expected $expected"
}

yaml_top_level_value() {
    local path="$1"
    local key="$2"

    awk -v key="$key" '
        $0 ~ "^" key ":[[:space:]]*" {
            value = $0
            sub("^[^:]+:[[:space:]]*", "", value)
            sub(/[[:space:]]+#.*$/, "", value)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
            gsub(/^["\047]|["\047]$/, "", value)
            print value
            exit
        }
    ' "$path"
}

yaml_metadata_name() {
    local path="$1"

    awk '
        /^metadata:[[:space:]]*$/ {
            in_metadata = 1
            next
        }
        in_metadata && /^[^[:space:]]/ {
            in_metadata = 0
        }
        in_metadata && /^[[:space:]]+name:[[:space:]]*/ {
            value = $0
            sub(/^[[:space:]]+name:[[:space:]]*/, "", value)
            sub(/[[:space:]]+#.*$/, "", value)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
            gsub(/^["\047]|["\047]$/, "", value)
            print value
            exit
        }
    ' "$path"
}

yaml_scheduling_node_selector_value() {
    local path="$1"
    local key="$2"

    awk -v key="$key" '
        /^scheduling:[[:space:]]*$/ {
            in_scheduling = 1
            in_node_selector = 0
            scheduling_indent = match($0, /[^[:space:]]/) - 1
            next
        }
        in_scheduling {
            current_indent = match($0, /[^[:space:]]/) - 1
            if (current_indent <= scheduling_indent && $0 !~ /^[[:space:]]*$/) {
                in_scheduling = 0
                in_node_selector = 0
            }
        }
        in_scheduling && /^[[:space:]]+nodeSelector:[[:space:]]*$/ {
            in_node_selector = 1
            node_selector_indent = match($0, /[^[:space:]]/) - 1
            next
        }
        in_node_selector {
            current_indent = match($0, /[^[:space:]]/) - 1
            if (current_indent <= node_selector_indent && $0 !~ /^[[:space:]]*$/) {
                in_node_selector = 0
            }
        }
        in_node_selector && $0 ~ "^[[:space:]]+" key ":[[:space:]]*" {
            value = $0
            sub("^[[:space:]]*" key ":[[:space:]]*", "", value)
            sub(/[[:space:]]+#.*$/, "", value)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
            gsub(/^["\047]|["\047]$/, "", value)
            print value
            exit
        }
    ' "$path"
}

require_runtimeclass_object() {
    local path="$1"
    local expected_name="$2"
    local expected_handler="$3"
    local kind name handler runtime_selector

    require_nonempty_file "$path"
    kind="$(yaml_top_level_value "$path" kind)"
    [ "$kind" = "RuntimeClass" ] ||
        fail "RuntimeClass object kind is ${kind:-missing}; expected RuntimeClass"
    name="$(yaml_metadata_name "$path")"
    [ "$name" = "$expected_name" ] ||
        fail "RuntimeClass object name is ${name:-missing}; expected $expected_name"
    handler="$(yaml_top_level_value "$path" handler)"
    [ "$handler" = "$expected_handler" ] ||
        fail "RuntimeClass object handler is ${handler:-missing}; expected $expected_handler"
    runtime_selector="$(yaml_scheduling_node_selector_value "$path" "a3s-box.io/runtime")"
    [ "$runtime_selector" = "true" ] ||
        fail "RuntimeClass object scheduling.nodeSelector a3s-box.io/runtime is ${runtime_selector:-missing}; expected true"
}

require_exec_proof() {
    local path="$1"
    local label="$2"

    require_nonempty_file "$path"
    grep -q '^pod=' "$path" ||
        fail "$label exec evidence lists no pods"
    grep -q '^A3S_BOX_EXEC_OK ' "$path" ||
        fail "$label exec evidence missing A3S_BOX_EXEC_OK marker"
    if grep -q '^pod_list_result=fail' "$path"; then
        fail "$label exec evidence failed to list pods"
    fi
    if grep -q '^exec_result=fail' "$path"; then
        fail "$label exec evidence contains failed exec"
    fi
    grep -q '^exec_result=pass pod=' "$path" ||
        fail "$label exec evidence has no successful exec"
}

require_exec_on_selected_nodes() {
    local path="$1"
    local selected_nodes="$2"
    local label="$3"

    awk '
        NR == FNR {
            if (NF > 0) {
                selected[$1] = 1
                selected_count++
            }
            next
        }
        /^exec_result=pass[[:space:]]/ {
            node = ""
            for (i = 1; i <= NF; i++) {
                if ($i ~ /^node=/) {
                    node = substr($i, 6)
                }
            }
            if (node == "" || node == "<none>" || node == "<unknown>") {
                printf "successful exec has no selected node: %s\n", $0 > "/dev/stderr"
                bad = 1
            } else if (!selected[node]) {
                printf "successful exec ran on unselected node %s\n", node > "/dev/stderr"
                bad = 1
            } else {
                exec_node[node] = 1
            }
        }
        END {
            for (node in selected) {
                if (!exec_node[node]) {
                    printf "missing successful exec on selected node %s\n", node > "/dev/stderr"
                    bad = 1
                }
            }
            if (selected_count == 0 || bad) {
                exit 1
            }
        }
    ' "$selected_nodes" "$path" ||
        fail "$label exec evidence must include successful exec on every selected node"
}

require_exec_for_workloads() {
    local path="$1"
    local label="$2"
    shift 2
    local expected_workloads="$*"

    awk -v expected_workloads="$expected_workloads" -v label="$label" '
        BEGIN {
            split(expected_workloads, expected, " ")
            for (i in expected) {
                wanted[expected[i]] = 1
            }
        }
        /^exec_result=pass[[:space:]]/ {
            workload = ""
            for (i = 1; i <= NF; i++) {
                if ($i ~ /^workload=/) {
                    workload = substr($i, 10)
                }
            }
            if (workload != "" && workload != "<none>" && workload != "<unknown>") {
                seen[workload] = 1
            }
        }
        END {
            for (workload in wanted) {
                if (!seen[workload]) {
                    printf "missing successful exec for %s workload %s\n", label, workload > "/dev/stderr"
                    bad = 1
                }
            }
            if (bad) {
                exit 1
            }
        }
    ' "$path" || fail "$label exec evidence must include every expected workload"
}

require_exec_pods_in_final_evidence() {
    local path="$1"
    local final_statuses="$2"
    local label="$3"

    awk -v label="$label" '
        NR == FNR {
            if (NF > 0) {
                final[$1] = 1
                final_count++
            }
            next
        }
        /^exec_result=pass[[:space:]]/ {
            pod = ""
            for (i = 1; i <= NF; i++) {
                if ($i ~ /^pod=/) {
                    pod = substr($i, 5)
                }
            }
            if (pod == "" || pod == "<none>" || pod == "<unknown>") {
                printf "%s successful exec has no pod: %s\n", label, $0 > "/dev/stderr"
                bad = 1
            } else if (seen[pod]) {
                printf "%s successful exec pod appears more than once: %s\n", label, pod > "/dev/stderr"
                bad = 1
            } else if (!final[pod]) {
                printf "%s successful exec pod is missing from final pod evidence: %s\n", label, pod > "/dev/stderr"
                bad = 1
            }
            seen[pod] = 1
            rows++
        }
        END {
            if (final_count == 0 || rows == 0 || bad) {
                exit 1
            }
        }
    ' "$final_statuses" "$path" ||
        fail "$label exec evidence must reference pods covered by final pod evidence"
}

artifact_row_count() {
    local path="$1"
    awk 'NF > 0 { count++ } END { print count + 0 }' "$path"
}

require_unique_first_column() {
    local path="$1"
    local label="$2"

    awk -v label="$label" '
        NF > 0 {
            name = $1
            if (seen[name]) {
                printf "%s appears more than once in %s\n", name, label > "/dev/stderr"
                bad = 1
            }
            seen[name] = 1
            rows++
        }
        END {
            if (rows == 0 || bad) {
                exit 1
            }
        }
    ' "$path" || fail "$label must contain unique non-empty names"
}

require_matching_first_column_sets() {
    local left="$1"
    local right="$2"
    local label="$3"

    awk -v label="$label" '
        NR == FNR {
            if (NF > 0) {
                name = $1
                if (left_seen[name]) {
                    printf "%s appears more than once in first %s artifact\n", name, label > "/dev/stderr"
                    bad = 1
                }
                left_seen[name] = 1
                left_count++
            }
            next
        }
        NF > 0 {
            name = $1
            if (right_seen[name]) {
                printf "%s appears more than once in second %s artifact\n", name, label > "/dev/stderr"
                bad = 1
            }
            right_seen[name] = 1
            right_count++
            if (!left_seen[name]) {
                printf "%s is missing from first %s artifact\n", name, label > "/dev/stderr"
                bad = 1
            }
        }
        END {
            for (name in left_seen) {
                if (!right_seen[name]) {
                    printf "%s is missing from second %s artifact\n", name, label > "/dev/stderr"
                    bad = 1
                }
            }
            if (left_count == 0 || right_count == 0 || bad) {
                exit 1
            }
        }
    ' "$left" "$right" ||
        fail "$label artifacts must contain matching unique node names"
}

require_first_column_subset() {
    local subset="$1"
    local superset="$2"
    local label="$3"

    awk -v label="$label" '
        NR == FNR {
            if (NF > 0) {
                name = $1
                superset_seen[name] = 1
                superset_count++
            }
            next
        }
        NF > 0 {
            name = $1
            if (subset_seen[name]) {
                printf "%s appears more than once in %s subset artifact\n", name, label > "/dev/stderr"
                bad = 1
            }
            subset_seen[name] = 1
            subset_count++
            if (!superset_seen[name]) {
                printf "%s is missing from %s superset artifact\n", name, label > "/dev/stderr"
                bad = 1
            }
        }
        END {
            if (superset_count == 0 || subset_count == 0 || bad) {
                exit 1
            }
        }
    ' "$superset" "$subset" ||
        fail "$label artifact must be covered by final pod evidence"
}

require_selected_node_label_rows() {
    local path="$1"

    awk '
        NF > 0 {
            name = $1
            runtime = $2
            tier = $3
            if (runtime != "true") {
                printf "%s has a3s-box.io/runtime=%s expected=true\n", name, runtime > "/dev/stderr"
                bad = 1
            }
            if (tier != "production-soak") {
                printf "%s has a3s-box.io/test-tier=%s expected=production-soak\n", name, tier > "/dev/stderr"
                bad = 1
            }
            rows++
        }
        END {
            if (rows == 0 || bad) {
                exit 1
            }
        }
    ' "$path" || fail "selected node label evidence must list only enrolled production-soak nodes"
}

require_post_cleanup_counts() {
    local path="$1"
    local samples="$2"
    local metric value final_ts cleanup_ts final_epoch cleanup_epoch

    require_nonempty_file "$path"
    tsv_require_columns "$path" timestamp phase smoke_daemonsets smoke_pods complex_pods churn_jobs churn_pods
    tsv_require_non_negative_int_columns "$path" "post-cleanup" smoke_daemonsets smoke_pods complex_pods churn_jobs churn_pods
    tsv_require_monotonic_timestamps "$path" "post-cleanup"
    tsv_require_phase_count "$path" post-cleanup 1 "post-cleanup"
    final_ts="$(tsv_value "$samples" final timestamp)"
    cleanup_ts="$(tsv_value "$path" post-cleanup timestamp)"
    [ -n "$final_ts" ] || fail "final resource sample timestamp is missing"
    [ -n "$cleanup_ts" ] || fail "post-cleanup timestamp is missing"
    final_epoch="$(iso_to_epoch "$final_ts")" ||
        fail "final resource sample timestamp is not parseable: $final_ts"
    cleanup_epoch="$(iso_to_epoch "$cleanup_ts")" ||
        fail "post-cleanup resource sample timestamp is not parseable: $cleanup_ts"
    if [ "$cleanup_epoch" -lt "$final_epoch" ]; then
        fail "post-cleanup timestamp is earlier than final sample: post_cleanup=$cleanup_ts final_sample=$final_ts"
    fi
    for metric in smoke_daemonsets smoke_pods complex_pods churn_jobs churn_pods; do
        value="$(tsv_value "$path" post-cleanup "$metric")"
        [ -n "$value" ] || fail "post-cleanup evidence missing $metric"
        is_non_negative_int "$value" || fail "post-cleanup $metric is not a non-negative integer: $value"
        [ "$value" -eq 0 ] || fail "post-cleanup $metric still present: $value"
    done
}

marker_count() {
    local path="$1"
    local marker="$2"

    grep -cF "$marker" "$path" 2>/dev/null || true
}

require_marker_count_at_least() {
    local path="$1"
    local marker="$2"
    local minimum="$3"
    local label="$4"
    local display="${5:-$marker}"
    local count

    count="$(marker_count "$path" "$marker")"
    if [ "$count" -eq 0 ]; then
        fail "$label missing $display marker"
    fi
    [ "$count" -ge "$minimum" ] ||
        fail "$label has too few $display markers: found=$count expected_at_least=$minimum"
}

require_marker_count_exact() {
    local path="$1"
    local marker="$2"
    local expected="$3"
    local label="$4"
    local display="${5:-$marker}"
    local count

    count="$(marker_count "$path" "$marker")"
    if [ "$count" -eq 0 ]; then
        fail "$label missing $display marker"
    fi
    [ "$count" -eq "$expected" ] ||
        fail "$label has wrong $display marker count: found=$count expected=$expected"
}

require_job_log_markers() {
    local path="$1"
    local runtime_class="$2"
    local completions="$3"

    require_kubectl_artifact "$path" "job logs"
    is_non_negative_int "$completions" && [ "$completions" -gt 0 ] ||
        fail "metadata job_completions is not a positive integer: ${completions:-missing}"
    require_marker_count_exact "$path" 'A3S_BOX_JOB_START ' "$completions" "job logs" "A3S_BOX_JOB_START"
    require_marker_count_exact "$path" "A3S_BOX_JOB_RUNTIME_CLASS=$runtime_class" "$completions" "job logs" "A3S_BOX_JOB_RUNTIME_CLASS=$runtime_class"
    require_marker_count_exact "$path" 'A3S_BOX_JOB_DONE ' "$completions" "job logs" "A3S_BOX_JOB_DONE"
}

require_complex_log_markers() {
    local path="$1"

    require_kubectl_artifact "$path" "complex logs"
    require_workload_log_marker "$path" redis REDIS_SOAK
    require_workload_log_marker "$path" postgres PG_SOAK
    require_workload_log_marker "$path" nginx NGINX_SOAK
    require_workload_log_marker "$path" python PY_SOAK
}

require_workload_log_marker() {
    local path="$1"
    local workload="$2"
    local marker="$3"

    awk -v workload="$workload" -v marker="$marker" '
        index($0, workload) && index($0, marker) {
            found = 1
            exit
        }
        END {
            if (!found) {
                exit 1
            }
        }
    ' "$path" || fail "complex logs missing $marker marker for $workload workload"
}

require_normal_event_rows() {
    local path="$1"

    require_nonempty_file "$path"
    awk '
        NF > 0 {
            type = $1
            if (type != "Normal") {
                printf "Kubernetes event type %s is not Normal: %s\n", type, $0 > "/dev/stderr"
                bad = 1
            }
            rows++
        }
        END {
            if (rows == 0 || bad) {
                exit 1
            }
        }
    ' "$path" || fail "Kubernetes event evidence must contain only Normal events"
}

require_pods_on_selected_nodes() {
    local pod_nodes="$1"
    local selected_nodes="$2"

    awk '
        NR == FNR {
            if (NF > 0) {
                selected[$1] = 1
                selected_count++
            }
            next
        }
        NF > 0 {
            pod_count++
            pod = $1
            node = $2
            if (node == "" || node == "<none>" || node == "<unknown>") {
                printf "%s has no assigned selected node\n", pod > "/dev/stderr"
                bad = 1
            } else if (!selected[node]) {
                printf "%s is assigned to unselected node %s\n", pod, node > "/dev/stderr"
                bad = 1
            }
        }
        END {
            if (selected_count == 0 || pod_count == 0 || bad) {
                exit 1
            }
        }
    ' "$selected_nodes" "$pod_nodes" ||
        fail "final pod node evidence must contain only selected nodes"
}

require_healthy_pod_status_rows() {
    local path="$1"

    awk '
        NF > 0 {
            pod = $1
            phase = $2
            restarts = $3
            if (phase != "Running" && phase != "Succeeded") {
                printf "%s has final phase %s\n", pod, phase > "/dev/stderr"
                bad = 1
            }
            if (restarts == "" || restarts == "<none>") {
                printf "%s has missing restart count\n", pod > "/dev/stderr"
                bad = 1
            } else {
                split(restarts, counts, ",")
                for (i in counts) {
                    if (counts[i] !~ /^[0-9]+$/ || counts[i] + 0 != 0) {
                        printf "%s has restart count %s\n", pod, counts[i] > "/dev/stderr"
                        bad = 1
                    }
                }
            }
            rows++
        }
        END {
            if (rows == 0 || bad) {
                exit 1
            }
        }
    ' "$path" || fail "final pod status evidence must list only Running/Succeeded pods with zero restarts"
}

require_job_pod_status_rows() {
    local path="$1"
    local selected_nodes="$2"
    local expected="$3"

    awk -v expected="$expected" '
        NR == FNR {
            if (NF > 0) {
                selected[$1] = 1
                selected_count++
            }
            next
        }
        NF > 0 {
            pod = $1
            phase = $2
            restarts = $3
            node = $4
            if (seen[pod]) {
                printf "%s appears more than once in churn Job pod status evidence\n", pod > "/dev/stderr"
                bad = 1
            }
            seen[pod] = 1
            rows++
            if (phase != "Succeeded") {
                printf "%s has churn Job phase %s\n", pod, phase > "/dev/stderr"
                bad = 1
            }
            if (restarts == "" || restarts == "<none>") {
                printf "%s has missing churn Job restart count\n", pod > "/dev/stderr"
                bad = 1
            } else {
                split(restarts, counts, ",")
                for (i in counts) {
                    if (counts[i] !~ /^[0-9]+$/ || counts[i] + 0 != 0) {
                        printf "%s has churn Job restart count %s\n", pod, counts[i] > "/dev/stderr"
                        bad = 1
                    }
                }
            }
            if (node == "" || node == "<none>" || node == "<unknown>") {
                printf "%s has no churn Job node\n", pod > "/dev/stderr"
                bad = 1
            } else if (!selected[node]) {
                printf "%s churn Job pod is assigned to unselected node %s\n", pod, node > "/dev/stderr"
                bad = 1
            }
        }
        END {
            if (selected_count == 0 || rows != expected || bad) {
                if (rows != expected) {
                    printf "churn Job pod status row count mismatch: rows=%d expected=%d\n", rows, expected > "/dev/stderr"
                }
                exit 1
            }
        }
    ' "$selected_nodes" "$path" ||
        fail "churn Job pod status evidence must list exactly $expected Succeeded pods with zero restarts on selected nodes"
}

require_matching_pod_artifact_names() {
    local left="$1"
    local right="$2"

    awk '
        NR == FNR {
            if (NF > 0) {
                name = $1
                if (left_seen[name]) {
                    printf "%s appears more than once in first pod artifact\n", name > "/dev/stderr"
                    bad = 1
                }
                left_seen[name] = 1
                left_count++
            }
            next
        }
        NF > 0 {
            name = $1
            if (right_seen[name]) {
                printf "%s appears more than once in second pod artifact\n", name > "/dev/stderr"
                bad = 1
            }
            right_seen[name] = 1
            right_count++
            if (!left_seen[name]) {
                printf "%s is missing from first pod artifact\n", name > "/dev/stderr"
                bad = 1
            }
        }
        END {
            for (name in left_seen) {
                if (!right_seen[name]) {
                    printf "%s is missing from second pod artifact\n", name > "/dev/stderr"
                    bad = 1
                }
            }
            if (left_count == 0 || right_count == 0 || bad) {
                exit 1
            }
        }
    ' "$left" "$right" ||
        fail "final pod artifacts must contain matching unique pod names"
}

kv_get() {
    local file="$1"
    local key="$2"
    awk -F= -v key="$key" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$file"
}

require_kv_nonempty() {
    local file="$1"
    local key="$2"
    local label="$3"
    local value

    value="$(kv_get "$file" "$key")"
    [ -n "$value" ] || fail "$label metadata missing $key"
    printf '%s\n' "$value"
}

require_kv_timestamp() {
    local file="$1"
    local key="$2"
    local label="$3"
    local value

    value="$(require_kv_nonempty "$file" "$key" "$label")"
    iso_to_epoch "$value" >/dev/null ||
        fail "$label metadata $key is not parseable: $value"
}

require_kv_non_negative_int() {
    local file="$1"
    local key="$2"
    local label="$3"
    local value

    value="$(require_kv_nonempty "$file" "$key" "$label")"
    is_non_negative_int "$value" ||
        fail "$label metadata $key is not a non-negative integer: $value"
    printf '%s\n' "$value"
}

require_kv_positive_int() {
    local file="$1"
    local key="$2"
    local label="$3"
    local value

    value="$(require_kv_non_negative_int "$file" "$key" "$label")"
    [ "$value" -gt 0 ] ||
        fail "$label metadata $key must be positive: $value"
    printf '%s\n' "$value"
}

require_kv_bool() {
    local file="$1"
    local key="$2"
    local label="$3"
    local value

    value="$(require_kv_nonempty "$file" "$key" "$label")"
    case "$value" in
        0|1)
            printf '%s\n' "$value"
            ;;
        *)
            fail "$label metadata $key must be 0 or 1: $value"
            ;;
    esac
}

host_selected_suite_flag() {
    local suites="$1"
    local key="$2"

    awk -v key="$key" '
        {
            for (i = 1; i <= NF; i++) {
                split($i, kv, "=")
                if (kv[1] == key) {
                    print kv[2]
                    found = 1
                    exit
                }
            }
        }
        END {
            if (!found) {
                exit 1
            }
        }
    ' <<EOF
$suites
EOF
}

require_host_selected_suites() {
    local metadata="$1"
    local suites key value selected=0

    suites="$(require_kv_nonempty "$metadata" selected_suites "host")"
    for key in core host linux_run cri bench; do
        value="$(host_selected_suite_flag "$suites" "$key")" ||
            fail "host metadata selected_suites missing $key flag"
        case "$value" in
            0|1)
                ;;
            *)
                fail "host metadata selected_suites $key must be 0 or 1: $value"
                ;;
        esac
        if [ "$value" = "1" ]; then
            selected=$((selected + 1))
        fi
    done
    [ "$selected" -gt 0 ] || fail "host metadata selected_suites has no selected work"
    printf '%s\n' "$suites"
}

require_host_iteration_log() {
    local iteration="$1"
    local name="$2"
    require_nonempty_file "$EVIDENCE_DIR/iteration-${iteration}-${name}.log"
}

require_host_declared_suite_logs() {
    local suites="$1"
    local iterations="$2"
    local iteration

    for iteration in $(seq 1 "$iterations"); do
        if [ "$(host_selected_suite_flag "$suites" core)" = "1" ]; then
            require_host_iteration_log "$iteration" core
        fi
        if [ "$(host_selected_suite_flag "$suites" host)" = "1" ]; then
            require_host_iteration_log "$iteration" host
        fi
        if [ "$(host_selected_suite_flag "$suites" linux_run)" = "1" ]; then
            require_host_iteration_log "$iteration" linux-run
        fi
        if [ "$(host_selected_suite_flag "$suites" cri)" = "1" ]; then
            require_host_iteration_log "$iteration" cri
        fi
        if [ "$(host_selected_suite_flag "$suites" bench)" = "1" ]; then
            require_host_iteration_log "$iteration" bench-leak
            require_host_iteration_log "$iteration" bench-race
        fi
    done
}

tsv_value() {
    local file="$1"
    local phase="$2"
    local column="$3"
    awk -F '\t' -v phase="$phase" -v column="$column" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            next
        }
        $index_by_name["phase"] == phase {
            value = $index_by_name[column]
        }
        END {
            if (value != "") {
                print value
            }
        }
    ' "$file"
}

tsv_require_columns() {
    local file="$1"
    shift
    awk -F '\t' -v columns="$*" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                seen[$i] = 1
            }
            split(columns, required, " ")
            for (i in required) {
                if (!seen[required[i]]) {
                    exit 1
                }
            }
            exit 0
        }
    ' "$file" || fail "missing required column in $file"
}

tsv_require_non_negative_int_columns() {
    local file="$1"
    local label="$2"
    shift 2

    awk -F '\t' -v columns="$*" -v label="$label" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            split(columns, required, " ")
            for (i in required) {
                column = required[i]
                required_columns[i] = column
                if (!index_by_name[column]) {
                    printf "%s sample column %s is missing\n", label, column > "/dev/stderr"
                    bad = 1
                }
            }
            next
        }
        NF > 0 {
            rows++
            for (i in required_columns) {
                column = required_columns[i]
                value = $index_by_name[column]
                if (value !~ /^[0-9]+$/) {
                    printf "%s sample column %s is not a non-negative integer: %s\n", label, column, value > "/dev/stderr"
                    bad = 1
                }
            }
        }
        END {
            if (rows == 0 || bad) {
                exit 1
            }
        }
    ' "$file" || fail "$label resource sample counters must be non-negative integers"
}

tsv_require_phase_count() {
    local file="$1"
    local phase="$2"
    local expected="$3"
    local label="$4"
    local count

    count="$(awk -F '\t' -v phase="$phase" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            next
        }
        NF > 0 && $index_by_name["phase"] == phase {
            count++
        }
        END {
            print count + 0
        }
    ' "$file")"
    [ "$count" -eq "$expected" ] ||
        fail "$label resource samples must contain exactly $expected $phase row(s); found=$count"
}

tsv_max() {
    local file="$1"
    local column="$2"
    awk -F '\t' -v column="$column" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            next
        }
        {
            value = $index_by_name[column] + 0
            if (value > max) {
                max = value
            }
        }
        END {
            print max + 0
        }
    ' "$file"
}

tsv_sample_count() {
    local file="$1"
    awk -F '\t' 'NR > 1 && NF > 0 { count++ } END { print count + 0 }' "$file"
}

tsv_first_value_by_column() {
    local file="$1"
    local column="$2"
    awk -F '\t' -v column="$column" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            idx = index_by_name[column]
            next
        }
        NR > 1 && NF > 0 && idx {
            print $idx
            found = 1
            exit
        }
        END {
            if (!idx || !found) {
                exit 1
            }
        }
    ' "$file"
}

tsv_last_value_by_column() {
    local file="$1"
    local column="$2"
    awk -F '\t' -v column="$column" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            idx = index_by_name[column]
            next
        }
        NR > 1 && NF > 0 && idx {
            value = $idx
            found = 1
        }
        END {
            if (!idx || !found) {
                exit 1
            }
            print value
        }
    ' "$file"
}

tsv_values_by_column() {
    local file="$1"
    local column="$2"
    awk -F '\t' -v column="$column" '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                index_by_name[$i] = i
            }
            idx = index_by_name[column]
            next
        }
        NR > 1 && NF > 0 && idx {
            print $idx
            found = 1
        }
        END {
            if (!idx || !found) {
                exit 1
            }
        }
    ' "$file"
}

iso_to_epoch() {
    local timestamp="$1"
    local parsed
    parsed="$(date -u -d "$timestamp" +%s 2>/dev/null)" && {
        echo "$parsed"
        return 0
    }
    parsed="$(date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "$timestamp" +%s 2>/dev/null)" && {
        echo "$parsed"
        return 0
    }
    return 1
}

tsv_require_monotonic_timestamps() {
    local file="$1"
    local label="$2"
    local timestamp epoch previous_ts="" previous_epoch="" rows=0

    while IFS= read -r timestamp; do
        rows=$((rows + 1))
        epoch="$(iso_to_epoch "$timestamp")" ||
            fail "$label resource sample timestamp is not parseable: $timestamp"
        if [ -n "$previous_epoch" ] && [ "$epoch" -lt "$previous_epoch" ]; then
            fail "$label resource sample timestamps are not monotonic: previous=$previous_ts current=$timestamp"
        fi
        previous_ts="$timestamp"
        previous_epoch="$epoch"
    done < <(tsv_values_by_column "$file" timestamp)

    [ "$rows" -gt 0 ] || fail "$label resource samples missing timestamps"
}

assert_int_le() {
    local name="$1"
    local actual="$2"
    local expected="$3"
    [ -n "$actual" ] || fail "$name is missing"
    [ -n "$expected" ] || fail "$name baseline is missing"
    [ "$actual" -le "$expected" ] || fail "$name grew: actual=$actual expected_max=$expected"
}

max_int() {
    if [ "$1" -ge "$2" ]; then
        echo "$1"
    else
        echo "$2"
    fi
}

strictest_nonzero_max_gap() {
    local left="$1"
    local right="$2"

    if [ "$left" -eq 0 ]; then
        echo "$right"
        return
    fi
    if [ "$right" -eq 0 ]; then
        echo "$left"
        return
    fi
    if [ "$left" -le "$right" ]; then
        echo "$left"
    else
        echo "$right"
    fi
}

apply_metadata_verifier_gates() {
    local metadata="$1"
    local label="$2"
    local duration_key="$3"
    local samples_key="$4"
    local span_key="$5"
    local gap_key="$6"
    local metadata_duration metadata_samples metadata_span metadata_gap

    metadata_duration="$(require_kv_non_negative_int "$metadata" "$duration_key" "$label")"
    metadata_samples="$(require_kv_non_negative_int "$metadata" "$samples_key" "$label")"
    metadata_span="$(require_kv_non_negative_int "$metadata" "$span_key" "$label")"
    metadata_gap="$(require_kv_non_negative_int "$metadata" "$gap_key" "$label")"

    MIN_DURATION_SECS="$(max_int "$MIN_DURATION_SECS" "$metadata_duration")"
    MIN_SAMPLES="$(max_int "$MIN_SAMPLES" "$metadata_samples")"
    MIN_SAMPLE_SPAN_SECS="$(max_int "$MIN_SAMPLE_SPAN_SECS" "$metadata_span")"
    MAX_SAMPLE_GAP_SECS="$(strictest_nonzero_max_gap "$MAX_SAMPLE_GAP_SECS" "$metadata_gap")"
}

summary_duration_secs() {
    local label="$1"
    local summary="$2"
    local duration

    duration="$(kv_get "$summary" duration_secs)"
    [ -n "$duration" ] || fail "$label summary missing duration_secs"
    is_non_negative_int "$duration" ||
        fail "$label summary duration_secs is not a non-negative integer: $duration"
    echo "$duration"
}

verify_summary_duration_covers_samples() {
    local label="$1"
    local summary="$2"
    local samples="$3"
    local duration first_ts last_ts first_epoch last_epoch span

    duration="$(summary_duration_secs "$label" "$summary")"
    first_ts="$(tsv_first_value_by_column "$samples" timestamp)" ||
        fail "$label resource samples missing first timestamp"
    last_ts="$(tsv_last_value_by_column "$samples" timestamp)" ||
        fail "$label resource samples missing last timestamp"
    first_epoch="$(iso_to_epoch "$first_ts")" ||
        fail "$label first resource sample timestamp is not parseable: $first_ts"
    last_epoch="$(iso_to_epoch "$last_ts")" ||
        fail "$label last resource sample timestamp is not parseable: $last_ts"
    if [ "$last_epoch" -lt "$first_epoch" ]; then
        fail "$label resource sample timestamps are not monotonic: first=$first_ts last=$last_ts"
    fi
    span=$((last_epoch - first_epoch))
    if [ "$span" -gt $((duration + SUMMARY_DURATION_SAMPLE_SPAN_SKEW_SECS)) ]; then
        fail "$label summary duration is shorter than resource sample span: duration_secs=$duration sample_span_secs=$span first_sample=$first_ts last_sample=$last_ts"
    fi
}

verify_min_duration() {
    local label="$1"
    local summary="$2"
    if [ "$MIN_DURATION_SECS" -eq 0 ]; then
        return
    fi

    local duration
    duration="$(summary_duration_secs "$label" "$summary")"
    [ "$duration" -ge "$MIN_DURATION_SECS" ] ||
        fail "$label soak duration too short: duration_secs=$duration required_min=$MIN_DURATION_SECS"
}

verify_min_samples() {
    local label="$1"
    local samples="$2"
    if [ "$MIN_SAMPLES" -eq 0 ]; then
        return
    fi

    local sample_count
    sample_count="$(tsv_sample_count "$samples")"
    [ "$sample_count" -ge "$MIN_SAMPLES" ] ||
        fail "$label soak has too few resource samples: samples=$sample_count required_min=$MIN_SAMPLES"
}

verify_min_sample_span() {
    local label="$1"
    local samples="$2"
    if [ "$MIN_SAMPLE_SPAN_SECS" -eq 0 ]; then
        return
    fi

    local first_ts last_ts first_epoch last_epoch span
    first_ts="$(tsv_first_value_by_column "$samples" timestamp)" ||
        fail "$label resource samples missing first timestamp"
    last_ts="$(tsv_last_value_by_column "$samples" timestamp)" ||
        fail "$label resource samples missing last timestamp"
    first_epoch="$(iso_to_epoch "$first_ts")" ||
        fail "$label first resource sample timestamp is not parseable: $first_ts"
    last_epoch="$(iso_to_epoch "$last_ts")" ||
        fail "$label last resource sample timestamp is not parseable: $last_ts"
    if [ "$last_epoch" -lt "$first_epoch" ]; then
        fail "$label resource sample timestamps are not monotonic: first=$first_ts last=$last_ts"
    fi
    span=$((last_epoch - first_epoch))
    [ "$span" -ge "$MIN_SAMPLE_SPAN_SECS" ] ||
        fail "$label soak sample span too short: sample_span_secs=$span required_min=$MIN_SAMPLE_SPAN_SECS first_sample=$first_ts last_sample=$last_ts"
}

verify_max_sample_gap() {
    local label="$1"
    local samples="$2"
    if [ "$MAX_SAMPLE_GAP_SECS" -eq 0 ]; then
        return
    fi

    local sample_count
    sample_count="$(tsv_sample_count "$samples")"
    [ "$sample_count" -ge 2 ] ||
        fail "$label soak needs at least two resource samples for --max-sample-gap-secs"

    local previous_ts="" previous_epoch="" current_ts current_epoch gap
    while IFS= read -r current_ts; do
        current_epoch="$(iso_to_epoch "$current_ts")" ||
            fail "$label resource sample timestamp is not parseable: $current_ts"
        if [ -n "$previous_epoch" ]; then
            if [ "$current_epoch" -lt "$previous_epoch" ]; then
                fail "$label resource sample timestamps are not monotonic: previous=$previous_ts current=$current_ts"
            fi
            gap=$((current_epoch - previous_epoch))
            [ "$gap" -le "$MAX_SAMPLE_GAP_SECS" ] ||
                fail "$label soak sample gap too large: sample_gap_secs=$gap allowed_max=$MAX_SAMPLE_GAP_SECS previous_sample=$previous_ts current_sample=$current_ts"
        fi
        previous_ts="$current_ts"
        previous_epoch="$current_epoch"
    done < <(tsv_values_by_column "$samples" timestamp)
}

detect_kind() {
    local metadata="$1"
    if [ "$KIND" != "auto" ]; then
        case "$KIND" in
            host|cluster)
                echo "$KIND"
                return
                ;;
            *)
                echo "--kind must be host, cluster, or auto" >&2
                exit 2
                ;;
        esac
    fi

    if [ -n "$(kv_get "$metadata" runtime_class)" ]; then
        echo "cluster"
        return
    fi
    if grep -qE '^selected_suites[:=]' "$metadata"; then
        echo "host"
        return
    fi

    fail "could not auto-detect evidence kind"
}

verify_host() {
    local metadata="$EVIDENCE_DIR/metadata.txt"
    local summary="$EVIDENCE_DIR/summary.txt"
    local samples="$EVIDENCE_DIR/resource-samples.tsv"
    require_nonempty_file "$summary"
    local result
    result="$(kv_get "$summary" result)"
    if [ "$result" = "fail" ]; then
        local failures exit_code failed_at failed_command
        failures="$(kv_get "$summary" failed_iterations)"
        exit_code="$(kv_get "$summary" exit_code)"
        failed_at="$(kv_get "$summary" failed_at)"
        failed_command="$(kv_get "$summary" failed_command)"
        fail "host soak failed: failed_iterations=${failures:-missing} exit_code=${exit_code:-not-recorded} failed_at=${failed_at:-not-recorded} failed_command=${failed_command:-not-recorded}"
    fi
    [ "$result" = "pass" ] || fail "host summary result is ${result:-missing}"
    local selected_suites
    require_kv_nonempty "$metadata" run_id "host" >/dev/null
    require_kv_timestamp "$metadata" started_at "host"
    selected_suites="$(require_host_selected_suites "$metadata")"
    require_kv_non_negative_int "$metadata" soak_duration_secs "host" >/dev/null
    require_kv_non_negative_int "$metadata" soak_iterations "host" >/dev/null
    require_kv_non_negative_int "$metadata" soak_interval_secs "host" >/dev/null
    apply_metadata_verifier_gates \
        "$metadata" \
        "host" \
        soak_verify_min_duration_secs \
        soak_verify_min_samples \
        soak_verify_min_sample_span_secs \
        soak_verify_max_sample_gap_secs
    require_nonempty_file "$samples"
    tsv_require_columns "$samples" timestamp phase shims mounts box_dirs socket_dirs
    tsv_require_non_negative_int_columns "$samples" "host" shims mounts box_dirs socket_dirs
    tsv_require_monotonic_timestamps "$samples" "host"
    tsv_require_phase_count "$samples" start 1 "host"
    tsv_require_phase_count "$samples" final 1 "host"
    verify_summary_duration_covers_samples "host" "$summary" "$samples"
    verify_min_duration "host" "$summary"
    verify_min_samples "host" "$samples"
    verify_min_sample_span "host" "$samples"
    verify_max_sample_gap "host" "$samples"

    local iterations failures
    iterations="$(kv_get "$summary" iterations)"
    failures="$(kv_get "$summary" failed_iterations)"
    [ -n "$iterations" ] || fail "host summary missing iterations"
    is_non_negative_int "$iterations" ||
        fail "host summary iterations is not a non-negative integer: $iterations"
    [ "$iterations" -gt 0 ] || fail "host soak ran zero iterations"
    is_non_negative_int "${failures:-}" ||
        fail "host summary failed_iterations is not a non-negative integer: ${failures:-missing}"
    [ "${failures:-}" = "0" ] || fail "host soak reported failed_iterations=${failures:-missing}"

    require_nonempty_file "$EVIDENCE_DIR/start-cli-snapshot.txt"
    require_nonempty_file "$EVIDENCE_DIR/final-cli-snapshot.txt"
    local iteration
    for iteration in $(seq 1 "$iterations"); do
        require_nonempty_file "$EVIDENCE_DIR/iteration-${iteration}-cli-snapshot.txt"
    done
    require_host_declared_suite_logs "$selected_suites" "$iterations"

    local metric start final
    for metric in shims mounts box_dirs socket_dirs; do
        start="$(tsv_value "$samples" start "$metric")"
        final="$(tsv_value "$samples" final "$metric")"
        assert_int_le "$metric" "$final" "$start"
    done

    pass "host soak evidence verified: iterations=$iterations"
}

verify_cluster() {
    local metadata="$EVIDENCE_DIR/metadata.txt"
    local summary="$EVIDENCE_DIR/summary.txt"
    local samples="$EVIDENCE_DIR/resource-samples.tsv"
    require_nonempty_file "$summary"
    local result runtime_class runtime_class_handler skip_smoke skip_jobs skip_complex cleanup dry_run job_completions
    result="$(kv_get "$summary" result)"
    if [ "$result" = "fail" ]; then
        local exit_code failed_at failed_command
        exit_code="$(kv_get "$summary" exit_code)"
        failed_at="$(kv_get "$summary" failed_at)"
        failed_command="$(kv_get "$summary" failed_command)"
        fail "cluster soak failed: exit_code=${exit_code:-missing} failed_at=${failed_at:-missing} failed_command=${failed_command:-missing}"
    fi
    [ "$result" = "pass" ] || fail "cluster summary result is ${result:-missing}"
    require_kv_nonempty "$metadata" run_id "cluster" >/dev/null
    require_kv_timestamp "$metadata" started_at "cluster"
    runtime_class="$(require_kv_nonempty "$metadata" runtime_class "cluster")"
    runtime_class_handler="$(require_kv_nonempty "$metadata" runtime_class_handler "cluster")"
    skip_smoke="$(require_kv_bool "$metadata" skip_smoke "cluster")"
    skip_jobs="$(require_kv_bool "$metadata" skip_jobs "cluster")"
    skip_complex="$(require_kv_bool "$metadata" skip_complex "cluster")"
    cleanup="$(require_kv_bool "$metadata" cleanup "cluster")"
    dry_run="$(require_kv_bool "$metadata" dry_run "cluster")"
    apply_metadata_verifier_gates \
        "$metadata" \
        "cluster" \
        verify_min_duration_secs \
        verify_min_samples \
        verify_min_sample_span_secs \
        verify_max_sample_gap_secs
    [ "$dry_run" = "0" ] || fail "cluster metadata dry_run must be 0 for completed soak evidence"
    if [ "$skip_jobs" != "1" ]; then
        require_kv_nonempty "$metadata" job_name "cluster" >/dev/null
        job_completions="$(require_kv_positive_int "$metadata" job_completions "cluster")"
    fi
    require_nonempty_file "$samples"
    tsv_require_columns "$samples" timestamp phase selected_nodes pods_total pods_pending pods_running pods_succeeded pods_failed pods_unknown pod_restarts job_active job_succeeded job_failed
    tsv_require_non_negative_int_columns "$samples" "cluster" selected_nodes pods_total pods_pending pods_running pods_succeeded pods_failed pods_unknown pod_restarts job_active job_succeeded job_failed
    tsv_require_monotonic_timestamps "$samples" "cluster"
    tsv_require_phase_count "$samples" final 1 "cluster"
    verify_summary_duration_covers_samples "cluster" "$summary" "$samples"
    verify_min_duration "cluster" "$summary"
    verify_min_samples "cluster" "$samples"
    verify_min_sample_span "cluster" "$samples"
    verify_max_sample_gap "cluster" "$samples"

    require_kubectl_artifact "$EVIDENCE_DIR/selected-nodes.txt" "selected node listing"
    require_kubectl_artifact "$EVIDENCE_DIR/selected-node-names.txt" "selected node names"
    require_kubectl_artifact "$EVIDENCE_DIR/selected-node-labels.tsv" "selected node labels"
    require_kubectl_artifact "$EVIDENCE_DIR/runtimeclass.yaml" "RuntimeClass YAML"
    require_kubectl_artifact "$EVIDENCE_DIR/final-get-all.txt" "final Kubernetes object listing"
    require_kubectl_artifact "$EVIDENCE_DIR/final-pods.yaml" "final pod YAML"
    require_kubectl_artifact "$EVIDENCE_DIR/final-pod-runtimeclasses.tsv" "final pod RuntimeClass table"
    require_kubectl_artifact "$EVIDENCE_DIR/final-pod-nodes.tsv" "final pod node table"
    require_kubectl_artifact "$EVIDENCE_DIR/final-pod-statuses.tsv" "final pod status table"
    require_kubectl_artifact "$EVIDENCE_DIR/events.txt" "Kubernetes events listing"
    require_kubectl_artifact "$EVIDENCE_DIR/events.tsv" "Kubernetes events table"
    require_kubectl_artifact "$EVIDENCE_DIR/describe-pods.txt" "pod describe output"

    local selected_nodes pods_total pods_pending pods_failed pods_unknown pod_restarts job_active job_failed
    selected_nodes="$(tsv_value "$samples" final selected_nodes)"
    pods_total="$(tsv_value "$samples" final pods_total)"
    pods_pending="$(tsv_value "$samples" final pods_pending)"
    pods_failed="$(tsv_max "$samples" pods_failed)"
    pods_unknown="$(tsv_value "$samples" final pods_unknown)"
    pod_restarts="$(tsv_max "$samples" pod_restarts)"
    job_active="$(tsv_value "$samples" final job_active)"
    job_failed="$(tsv_max "$samples" job_failed)"
    require_runtimeclass_object "$EVIDENCE_DIR/runtimeclass.yaml" "$runtime_class" "$runtime_class_handler"
    [ -n "$selected_nodes" ] || fail "final selected_nodes sample is missing"
    [ -n "$pods_total" ] || fail "final pods_total sample is missing"
    [ -n "$pods_pending" ] || fail "final pods_pending sample is missing"
    [ -n "$pods_unknown" ] || fail "final pods_unknown sample is missing"
    [ -n "$job_active" ] || fail "final job_active sample is missing"
    [ "$selected_nodes" -gt 0 ] || fail "no selected nodes were sampled"
    local selected_node_rows
    selected_node_rows="$(artifact_row_count "$EVIDENCE_DIR/selected-node-names.txt")"
    [ "$selected_node_rows" -eq "$selected_nodes" ] ||
        fail "selected node evidence row count mismatch: rows=$selected_node_rows expected_nodes=$selected_nodes"
    require_unique_first_column "$EVIDENCE_DIR/selected-node-names.txt" "selected node evidence"
    local selected_label_rows
    selected_label_rows="$(artifact_row_count "$EVIDENCE_DIR/selected-node-labels.tsv")"
    [ "$selected_label_rows" -eq "$selected_nodes" ] ||
        fail "selected node label evidence row count mismatch: rows=$selected_label_rows expected_nodes=$selected_nodes"
    require_matching_first_column_sets "$EVIDENCE_DIR/selected-node-names.txt" "$EVIDENCE_DIR/selected-node-labels.tsv" "selected node"
    require_selected_node_label_rows "$EVIDENCE_DIR/selected-node-labels.tsv"
    [ "$pods_total" -gt 0 ] || fail "no pods were sampled in final state"
    [ "$pods_pending" -eq 0 ] || fail "pods still pending in final sample: $pods_pending"
    [ "$pods_failed" -eq 0 ] || fail "pod failures observed: $pods_failed"
    [ "$pods_unknown" -eq 0 ] || fail "pods still unknown in final sample: $pods_unknown"
    [ "$pod_restarts" -eq 0 ] || fail "pod restarts observed: $pod_restarts"
    [ "$job_active" -eq 0 ] || fail "job still active in final sample: $job_active"
    [ "$job_failed" -eq 0 ] || fail "job failures observed: $job_failed"
    local runtimeclass_rows pod_node_rows pod_status_rows
    runtimeclass_rows="$(artifact_row_count "$EVIDENCE_DIR/final-pod-runtimeclasses.tsv")"
    pod_node_rows="$(artifact_row_count "$EVIDENCE_DIR/final-pod-nodes.tsv")"
    pod_status_rows="$(artifact_row_count "$EVIDENCE_DIR/final-pod-statuses.tsv")"
    [ "$runtimeclass_rows" -eq "$pods_total" ] ||
        fail "final pod RuntimeClass evidence row count mismatch: rows=$runtimeclass_rows expected_pods=$pods_total"
    [ "$pod_node_rows" -eq "$pods_total" ] ||
        fail "final pod node evidence row count mismatch: rows=$pod_node_rows expected_pods=$pods_total"
    [ "$pod_status_rows" -eq "$pods_total" ] ||
        fail "final pod status evidence row count mismatch: rows=$pod_status_rows expected_pods=$pods_total"
    require_matching_pod_artifact_names "$EVIDENCE_DIR/final-pod-runtimeclasses.tsv" "$EVIDENCE_DIR/final-pod-nodes.tsv"
    require_matching_pod_artifact_names "$EVIDENCE_DIR/final-pod-runtimeclasses.tsv" "$EVIDENCE_DIR/final-pod-statuses.tsv"
    require_runtimeclass_rows "$EVIDENCE_DIR/final-pod-runtimeclasses.tsv" "$runtime_class" "final pod RuntimeClass evidence"
    require_pods_on_selected_nodes "$EVIDENCE_DIR/final-pod-nodes.tsv" "$EVIDENCE_DIR/selected-node-names.txt"
    require_healthy_pod_status_rows "$EVIDENCE_DIR/final-pod-statuses.tsv"
    require_normal_event_rows "$EVIDENCE_DIR/events.tsv"

    if [ "${skip_smoke:-0}" != "1" ]; then
        require_exec_proof "$EVIDENCE_DIR/smoke-exec.txt" "smoke"
        require_exec_on_selected_nodes "$EVIDENCE_DIR/smoke-exec.txt" "$EVIDENCE_DIR/selected-node-names.txt" "smoke"
        require_exec_pods_in_final_evidence "$EVIDENCE_DIR/smoke-exec.txt" "$EVIDENCE_DIR/final-pod-statuses.tsv" "smoke"
    fi

    local completions succeeded
    if [ "${skip_jobs:-0}" != "1" ]; then
        require_kubectl_artifact "$EVIDENCE_DIR/job.yaml" "churn Job YAML"
        require_kubectl_artifact "$EVIDENCE_DIR/job-runtimeclass.txt" "churn Job RuntimeClass value"
        require_kubectl_artifact "$EVIDENCE_DIR/job-pods.txt" "churn Job pod listing"
        require_kubectl_artifact "$EVIDENCE_DIR/job-pod-statuses.tsv" "churn Job pod status table"
        require_runtimeclass_value "$EVIDENCE_DIR/job-runtimeclass.txt" "$runtime_class" "job evidence"
        completions="$job_completions"
        succeeded="$(tsv_value "$samples" final job_succeeded)"
        require_job_pod_status_rows "$EVIDENCE_DIR/job-pod-statuses.tsv" "$EVIDENCE_DIR/selected-node-names.txt" "$completions"
        require_first_column_subset "$EVIDENCE_DIR/job-pod-statuses.tsv" "$EVIDENCE_DIR/final-pod-statuses.tsv" "churn Job pod status"
        require_job_log_markers "$EVIDENCE_DIR/job-logs.txt" "$runtime_class" "$completions"
        [ -n "$succeeded" ] || fail "final job_succeeded sample is missing"
        [ "$succeeded" -eq "$completions" ] ||
            fail "job completions mismatch: succeeded=$succeeded expected=$completions"
    fi

    if [ "${skip_complex:-0}" != "1" ]; then
        require_complex_log_markers "$EVIDENCE_DIR/complex-logs.txt"
        require_exec_proof "$EVIDENCE_DIR/complex-exec.txt" "complex"
        require_exec_for_workloads "$EVIDENCE_DIR/complex-exec.txt" "complex" redis postgres nginx python
        require_exec_pods_in_final_evidence "$EVIDENCE_DIR/complex-exec.txt" "$EVIDENCE_DIR/final-pod-statuses.tsv" "complex"
    fi

    if [ "${cleanup:-0}" = "1" ]; then
        require_kubectl_artifact "$EVIDENCE_DIR/post-cleanup-namespace.txt" "post-cleanup namespace listing"
        require_kubectl_artifact "$EVIDENCE_DIR/post-cleanup-get-all.txt" "post-cleanup Kubernetes object listing"
        require_post_cleanup_counts "$EVIDENCE_DIR/post-cleanup-counts.tsv" "$samples"
    fi

    pass "cluster soak evidence verified: selected_nodes=$selected_nodes"
}

if [ -z "$EVIDENCE_DIR" ]; then
    usage >&2
    exit 2
fi

[ -d "$EVIDENCE_DIR" ] || fail "evidence directory does not exist: $EVIDENCE_DIR"
is_non_negative_int "$MIN_DURATION_SECS" || {
    echo "--min-duration-secs must be a non-negative integer" >&2
    exit 2
}
is_non_negative_int "$MIN_SAMPLES" || {
    echo "--min-samples must be a non-negative integer" >&2
    exit 2
}
is_non_negative_int "$MIN_SAMPLE_SPAN_SECS" || {
    echo "--min-sample-span-secs must be a non-negative integer" >&2
    exit 2
}
is_non_negative_int "$MAX_SAMPLE_GAP_SECS" || {
    echo "--max-sample-gap-secs must be a non-negative integer" >&2
    exit 2
}
metadata="$EVIDENCE_DIR/metadata.txt"
require_nonempty_file "$metadata"

dry_run="$(kv_get "$metadata" dry_run)"
if [ "$ALLOW_DRY_RUN" -eq 1 ] && [ "${dry_run:-0}" = "1" ]; then
    if [ "$MIN_DURATION_SECS" -gt 0 ]; then
        fail "dry-run evidence cannot satisfy --min-duration-secs=$MIN_DURATION_SECS"
    fi
    if [ "$MIN_SAMPLES" -gt 0 ]; then
        fail "dry-run evidence cannot satisfy --min-samples=$MIN_SAMPLES"
    fi
    if [ "$MIN_SAMPLE_SPAN_SECS" -gt 0 ]; then
        fail "dry-run evidence cannot satisfy --min-sample-span-secs=$MIN_SAMPLE_SPAN_SECS"
    fi
    if [ "$MAX_SAMPLE_GAP_SECS" -gt 0 ]; then
        fail "dry-run evidence cannot satisfy --max-sample-gap-secs=$MAX_SAMPLE_GAP_SECS"
    fi
    pass "dry-run evidence metadata verified"
    exit 0
fi

kind="$(detect_kind "$metadata")"
case "$kind" in
    host)
        verify_host
        ;;
    cluster)
        verify_cluster
        ;;
    *)
        fail "unsupported evidence kind: $kind"
        ;;
esac
