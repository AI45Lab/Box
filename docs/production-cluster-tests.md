# Production Cluster Test Design

This document extends the single-host KVM validation ladder to production Linux
server clusters. It defines how to run real a3s-box integration tests and long
soak tests on production-grade nodes without turning the business cluster into
an uncontrolled experiment.

The goal is not to prove every CRI edge case. The goal is to answer the
production question: can a3s-box boot, run, observe, restart, and clean up real
MicroVM workloads repeatedly on the host class that will run them?

## Scope

In scope:

- Production Linux nodes with `/dev/kvm`, containerd, and the production kernel.
- The local CLI runtime installed on each selected node.
- The Kubernetes RuntimeClass path through `containerd-shim-a3s-box-v2`.
- Real microVM lifecycle, exec, logs, networking, image cache, cleanup, and
  crash-recovery behavior.
- Long-running churn and service workloads that expose resource leaks.

Out of scope for this gate:

- Treating a3s-box as the default Kubernetes runtime for all pods.
- Full `critest` conformance as a production pass/fail gate. Keep the published
  conformance scoreboard in `cri-conformance.md`.
- Hardware TEE production claims unless the selected node class can produce and
  verify real attestation evidence. Simulation mode is not evidence.
- Destructive node operations outside a declared canary pool.

## Cluster Safety Model

Run production-cluster validation only on explicitly enrolled nodes.

Required node labels:

```bash
kubectl label node <node> a3s-box.io/runtime=true
kubectl label node <node> a3s-box.io/test-tier=production-soak
```

Recommended taint:

```bash
kubectl taint node <node> a3s-box.io/soak=true:NoSchedule
```

All validation pods must use:

- namespace `a3s-box-validation`;
- `runtimeClassName: a3s-box` for runtime-path tests;
- `nodeSelector` matching both labels above;
- a toleration for `a3s-box.io/soak=true:NoSchedule`;
- explicit CPU and memory requests/limits;
- disposable images, volumes, and secrets only.

Never schedule soak workloads by raw `nodeName` in production except during a
single-node emergency reproduction. Use labels and taints so cluster state stays
auditable and reversible.

## Node Admission Checklist

Before a node joins the test cohort, capture:

```bash
uname -a
cat /etc/os-release
ls -l /dev/kvm
containerd --version
crictl version || true
a3s-box --version || true
a3s-box info || true
```

Admission requirements:

- `/dev/kvm` exists and is usable by the runtime user.
- The production containerd config can register `io.containerd.a3s-box.v2`.
- The node has enough headroom for the planned cohort size. Reserve at least
  30 percent CPU and memory for existing production safety margin.
- The image path is deterministic: either a local mirror is configured or all
  soak images are preloaded.
- Node exporter, containerd logs, kubelet logs, and disk metrics are visible to
  the on-call dashboard.

Reject the node from this gate if any admission check is ambiguous. A smaller
known-good cohort is more useful than broad noisy coverage.

## Phase 0: Build And Install Artifacts

Install the same release or commit on every selected node. Prefer release
artifacts. For a candidate build, use `deploy/scripts/install-runtimeclass.sh`
with `--from-dir` so the exact local artifacts are installed everywhere.

```bash
sudo deploy/scripts/install-runtimeclass.sh \
  --version v2.6.0 \
  --from-dir /opt/a3s-box-artifacts \
  --warmup-image docker.m.daocloud.io/library/busybox:latest
```

Record the installed binary digests:

```bash
sha256sum \
  /usr/local/bin/a3s-box \
  /usr/local/bin/a3s-box-cri \
  /usr/local/bin/a3s-box-shim \
  /usr/local/bin/a3s-box-guest-init \
  /usr/local/bin/containerd-shim-a3s-box-v2
```

Rollback must be prepared before the first soak starts:

```bash
kubectl label node <node> a3s-box.io/runtime-
kubectl label node <node> a3s-box.io/test-tier-
kubectl taint node <node> a3s-box.io/soak:NoSchedule-
kubectl delete ns a3s-box-validation --wait=false
```

On the node, remove the containerd runtime drop-in only during a planned
rollback window, then restart containerd.

## Phase 1: Node-Local CLI Integration

Run from each selected node, not from the monorepo root:

```bash
cd /opt/a3s-box
export A3S_BOX_TEST_ALPINE_TAR=/opt/a3s-images/alpine-oci.tar
export A3S_BOX_SMOKE_SKIP_PULL=1
export A3S_BOX_HOST_SMOKE_TIMEOUT_SECS=300

scripts/host-integration-smoke.sh --core --host --no-pure
bench/bench.sh leak
bench/bench.sh race
```

Pass criteria:

- `core_smoke` and `host_smoke` pass on every admitted node.
- `bench/bench.sh leak` returns to the baseline count for shims, mounts, and
  box directories.
- `bench/bench.sh race` reports no lost update and leaves no race boxes behind.
- `a3s-box ps -a`, `a3s-box images`, `a3s-box volume ls`, and
  `a3s-box snapshot ls` are readable after the run.

This phase catches node-local issues before Kubernetes adds scheduler, kubelet,
and CRI noise.

## Phase 2: RuntimeClass Integration

Apply the RuntimeClass smoke DaemonSet from the repository:

```bash
kubectl apply -f deploy/shim/runtimeclass-smoke.yaml
```

Then verify:

```bash
kubectl -n a3s-box-validation rollout status ds/a3s-box-runtimeclass-smoke --timeout=300s
kubectl -n a3s-box-validation logs -l app=a3s-box-runtimeclass-smoke --prefix --tail=20
kubectl -n a3s-box-validation get pods -l app=a3s-box-runtimeclass-smoke -o wide
for pod in $(kubectl -n a3s-box-validation get pod -l app=a3s-box-runtimeclass-smoke -o name); do
  kubectl -n a3s-box-validation exec "$pod" -- sh -c 'echo EXEC_OK && uname -m'
done
```

Pass criteria:

- one smoke pod runs on each enrolled node and nowhere else;
- every pod uses `runtimeClassName: a3s-box`;
- logs are delivered to Kubernetes;
- `kubectl exec` works for at least one smoke pod on each selected node;
- deleting the DaemonSet cleans up the node-side shim, box directory, socket
  directory, and mounts.

## Phase 3: Real Integration Matrix

Run this matrix on a canary cohort first, then on the full selected cohort.

| Area | Test | Pass signal |
| --- | --- | --- |
| Lifecycle | create/start/stop/remove 100 CLI boxes per node | no orphan shims, mounts, sockets, or box dirs |
| Kubernetes lifecycle | create/delete 100 `runtimeClassName: a3s-box` Jobs | all Jobs complete; no kubelet or shim crash loop |
| Exec | `kubectl exec` and `a3s-box exec` against long-lived boxes | stdout, stderr, TTY, and exit code are correct |
| Logs | high-volume stdout and log rotation | logs are ordered enough for diagnosis and no writer deadlock occurs |
| Images | pull from mirror, load offline archive, cache reuse | no unbounded image-store growth after prune |
| Networking | in-pod localhost where bridge is expected; service reachability where Kubernetes provides it | documented networking mode behavior matches reality |
| Volumes | emptyDir/hostPath-style CRI mounts plus CLI named volumes | data is visible for the intended lifetime and removed after cleanup |
| Restart | box restart policy and kubelet pod restart | state transitions are visible and cleanup remains bounded |
| Crash recovery | kill `a3s-box-cri` or the shim on a canary node only | restart reaps or marks stale workloads without leaking host resources |

Keep failure evidence small and actionable: pod YAML, command, node name,
`journalctl -u containerd -u kubelet`, `a3s-box inspect`, and the resource
baseline/after counts.

## Phase 4: Soak Profiles

Use three profiles. Do not skip the shorter profiles; they are the guardrails
that keep a 72-hour run from wasting a production window.

### Guardrail Soak: 2 Hours

Purpose: reproduce the existing endurance gate on production Linux nodes.

Per selected node:

```bash
export IMAGE=docker.m.daocloud.io/library/alpine:latest
export CHURN=2500
export RACE=32
scripts/host-integration-smoke.sh \
  --no-pure \
  --core \
  --host \
  --soak \
  --soak-duration 7200 \
  --soak-verify-min-duration-secs 7200 \
  --soak-verify-min-sample-span-secs 7200 \
  --soak-verify-min-samples 4 \
  --soak-output "/var/log/a3s-box-soak/$(hostname)-guardrail-$(date -u +%Y%m%dT%H%M%SZ)"
```

Cluster:

- 500 short RuntimeClass job completions across the selected cohort.
- long-lived Redis/Postgres/nginx/Python pods with periodic `kubectl exec`,
  `logs`, and delete/recreate.

Run the reusable cluster soak runner from a control-plane machine:

```bash
deploy/scripts/runtimeclass-soak.sh \
  --preflight-only \
  --output "./target/a3s-box-runtimeclass-soak/preflight-$(date -u +%Y%m%dT%H%M%SZ)"
```

The preflight command checks that `runtimeClassName: a3s-box` exists and that at
least one node is explicitly selected with the production-soak labels. It writes
metadata, selected-node details, a resource sample, and a `summary.txt` with
`result=preflight`; it does not create a namespace, apply manifests, submit
Jobs, or delete workloads.

```bash
deploy/scripts/runtimeclass-soak.sh \
  --jobs 500 \
  --parallelism 25 \
  --duration 7200 \
  --verify-min-duration-secs 7200 \
  --verify-min-sample-span-secs 7200 \
  --verify-min-samples 24 \
  --verify-max-sample-gap-secs 600 \
  --output "./target/a3s-box-runtimeclass-soak/guardrail-$(date -u +%Y%m%dT%H%M%SZ)"
```

The runner verifies its evidence bundle before returning success, including
Kubernetes final state, `metadata.txt` with parseable `started_at`, boolean
skip/cleanup flags, and positive Job completions when churn jobs run,
`runtimeclass.yaml` with `metadata.name: a3s-box`, `handler: a3s-box`, and
`scheduling.nodeSelector.a3s-box.io/runtime: "true"`, `resource-samples.tsv`
with parseable monotonic timestamps, non-negative integer counters, exactly one
`final` row, and a `summary.txt` duration that is not shorter than the sampled
time span, `smoke-exec.txt` proving `kubectl exec` on every selected node,
`complex-exec.txt` proving exec against every long-lived workload
(`redis`, `postgres`, `nginx`, and `python`),
`complex-logs.txt` with workload-prefixed `REDIS_SOAK`, `PG_SOAK`,
`NGINX_SOAK`, and `PY_SOAK` markers,
`final-pod-runtimeclasses.tsv`, `job-runtimeclass.txt` when churn jobs are
enabled, `job-pod-statuses.tsv` proving exactly the declared number of Succeeded
churn pods with zero restarts on selected nodes and covered by the final pod
evidence, `job-logs.txt` with `A3S_BOX_JOB_START`,
`A3S_BOX_JOB_RUNTIME_CLASS=a3s-box`, and `A3S_BOX_JOB_DONE` markers exactly
matching the declared Job completion count, `selected-node-names.txt`,
`final-pod-nodes.tsv` covering the same unique final pod set,
`selected-node-labels.tsv` proving the required production-soak labels,
`final-pod-statuses.tsv`, `events.tsv` with only `Normal` events, describe
output, logs, a unique selected node list matching the sampled selected node
count, pod restart count, unresolved Pending/Unknown pods, active jobs, and Job
completion counts. Structural Kubernetes artifacts are rejected if they contain
captured `kubectl` connection or API errors. When `--cleanup` is enabled,
`post-cleanup-counts.tsv` must be a single timestamped row with non-negative
integer zero counts proving the generated smoke, complex, and churn workloads
are gone. Its timestamp must be parseable and must not be earlier than the final
resource sample, and the post-cleanup namespace/object listings must not contain
captured `kubectl` errors; the runner waits up to `--cleanup-timeout` seconds
before collecting those counts. The `--verify-*` options make the
2-hour gate part of the runner success path instead of a manual afterthought,
and `verify.out` records the verifier pass or concrete failure. The saved
`metadata.txt` also records those gates, so later verifier runs enforce them even
when the re-check command omits the `--min-*` options. If the runner fails before
completion, it still writes a failure `summary.txt` with the exit code, failed
script location, and failed command before collecting the partial Kubernetes
evidence. To re-check a saved bundle:

```bash
deploy/scripts/verify-soak-evidence.sh \
  --kind cluster \
  --min-duration-secs 7200 \
  --min-sample-span-secs 7200 \
  --min-samples 24 \
  --max-sample-gap-secs 600 \
  <evidence-dir>
```

Before running an expensive cluster soak after changing these scripts, validate
the local evidence logic first:

```bash
deploy/scripts/soak-evidence-self-test.sh
```

Pass criteria: zero leaks, zero lost updates, all cleanup counters return to
baseline, and no production SLO alert fires.

### Release Soak: 24 Hours

Purpose: validate normal release readiness.

Workload mix:

- 60 percent short jobs: boot, run a shell command, emit metrics, exit.
- 20 percent service pods: nginx/python/redis-style long-lived workloads.
- 10 percent exec/log churn: repeated `exec`, `logs`, and `stats`.
- 10 percent failure injection on canary nodes: kill CRI process, remove a test
  pod mid-boot, interrupt a CLI run, and restart containerd during a window.

Cadence:

- every minute: submit bounded RuntimeClass job completions;
- every five minutes: keep `runtimeclass-soak.sh` sampling Kubernetes pod/job
  status, and sample node-local `a3s-box ps -a`, shim count, mount count,
  box dir count, image-store size, and containerd/kubelet errors;
- every hour: run `bench/bench.sh race` on one rotating canary node.

Pass criteria:

- Job success rate at least 99.5 percent after excluding deliberate fault
  injection.
- No validation pod restarts outside deliberate fault-injection windows.
- No monotonic growth in orphan shims, mounts, socket dirs, or box dirs.
- CRI RSS slope stays under 50 MiB per day after warm-up.
- Test image/cache growth is bounded by configured pruning.
- p95 RuntimeClass pod start latency does not regress more than 30 percent from
  the first stable hour.
- Start the final RuntimeClass runner with
  `--verify-min-duration-secs 86400 --verify-min-sample-span-secs 86400 --verify-min-samples 288 --verify-max-sample-gap-secs 600`.
- Re-check the final RuntimeClass evidence bundle with
  `deploy/scripts/verify-soak-evidence.sh --kind cluster --min-duration-secs 86400 --min-sample-span-secs 86400 --min-samples 288 --max-sample-gap-secs 600 <evidence-dir>`.

### Endurance Soak: 72 Hours

Purpose: prove operational stability over a weekend-length window.

Run the 24-hour profile for 72 hours with reduced concurrency if the cluster is
also carrying production traffic. Add one controlled canary node reboot only if
the production change window allows it.

Pass criteria:

- all 24-hour criteria hold for the full window;
- recovery after any allowed node reboot leaves no stale a3s-box host resources;
- validation pods and RuntimeClass churn jobs finish without unexpected
  restarts, Pending pods, Unknown pods, active jobs, or failed pods/jobs;
- no alert requires manual cleanup to keep the test running;
- final cleanup returns all selected nodes to their pre-soak resource baseline.
- Start the final RuntimeClass runner with
  `--verify-min-duration-secs 259200 --verify-min-sample-span-secs 259200 --verify-min-samples 864 --verify-max-sample-gap-secs 600`.
- Re-check the final RuntimeClass evidence bundle with
  `deploy/scripts/verify-soak-evidence.sh --kind cluster --min-duration-secs 259200 --min-sample-span-secs 259200 --min-samples 864 --max-sample-gap-secs 600 <evidence-dir>`.

## Stop Conditions

Stop the soak immediately if any of these occur:

- production workload SLO or paging alert attributed to the validation cohort;
- node disk usage for `/var/lib/a3s-box`, `/var/lib/containerd`, or the root
  filesystem exceeds 80 percent;
- orphan shim, mount, or box directory count grows for three consecutive samples;
- containerd or kubelet enters a crash loop on any selected node;
- `a3s-box ps -a` cannot read state after one retry;
- a validation pod escapes the selected namespace, labels, or resource limits.

The kill switch is:

```bash
kubectl delete ns a3s-box-validation --wait=false
kubectl label nodes -l a3s-box.io/test-tier=production-soak a3s-box.io/runtime-
kubectl taint nodes -l a3s-box.io/test-tier=production-soak a3s-box.io/soak:NoSchedule- || true
```

Then run node-local cleanup and record before/after counts:

```bash
A3S_HOME=/var/lib/a3s-box a3s-box ps -a || true
A3S_HOME=/var/lib/a3s-box a3s-box system-prune --force || true
pgrep -af 'a3s-box|containerd-shim-a3s-box' || true
findmnt | grep -E 'a3s|krun|overlay' || true
```

## Evidence Bundle

Each run must produce a small evidence bundle:

- run ID, Git SHA, a3s-box version, artifact SHA256 values;
- exact selected node list and labels, plus `selected-node-names.txt`,
  `selected-node-labels.tsv`, and `final-pod-nodes.tsv` proving every final pod
  stayed on the unique, count-matched selected node set;
- OS release, kernel, containerd version, and `/dev/kvm` permissions;
- command transcript for each phase;
- image references and digests;
- `resource-samples.tsv` with parseable monotonic timestamps, non-negative
  integer counters for selected nodes, pod phases, restarts, and Job status,
  plus exactly one final sample row;
- Kubernetes events and pod summaries, including `runtimeclass.yaml` proof that
  the cluster object is named `a3s-box`, uses handler `a3s-box`, and keeps
  `scheduling.nodeSelector.a3s-box.io/runtime: "true"`,
  `events.tsv` proof that no validation namespace event is `Warning`,
  `smoke-exec.txt` proof that `kubectl exec` works on every selected node,
  `complex-exec.txt` proof that exec works against every long-lived workload,
  `complex-logs.txt` proof that Redis, Postgres, nginx, and Python each emitted
  their own workload log marker,
  `final-pod-runtimeclasses.tsv`, and `job-runtimeclass.txt` proof that every
  unique final validation pod and churn job uses `runtimeClassName: a3s-box`,
  `job-pod-statuses.tsv` proof that churn pods completed exactly once with zero
  restarts on selected nodes and are covered by the final pod evidence, plus
  `job-logs.txt` proof that churn jobs emitted start, runtime-class, and done
  markers exactly matching the declared completion count;
- `final-pod-statuses.tsv` proving every final pod is `Running` or `Succeeded`
  with zero restarts;
- per-node baseline and final counts for shims, mounts, box dirs, socket dirs,
  image-store bytes, and disk usage;
- p50/p95/p99 pod start latency and operation success rate;
- all stop-condition checks and whether any fired;
- `post-cleanup-counts.tsv` when `--cleanup` is enabled;
- `verify.out` / `verify-soak-evidence.sh` result for the host and cluster
  bundles;
- known deviations from this design.

Store the bundle with the release candidate. The pass/fail decision should be
made from this bundle, not from chat notes or terminal scrollback.

## Promotion Policy

Promote a build through the cluster ladder in this order:

1. single-node CLI integration;
2. single-node RuntimeClass smoke;
3. three-node guardrail soak;
4. full selected-cohort 24-hour release soak;
5. 72-hour endurance soak before widening production use.

Do not widen the RuntimeClass label to more production nodes until the previous
step has a recorded evidence bundle and cleanup has returned all nodes to
baseline.
