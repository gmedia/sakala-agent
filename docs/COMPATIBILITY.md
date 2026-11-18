# Compatibility and Release Policy

## Version model

Sakala Agent reports two independent values in every heartbeat:

- `metadata.version`: semantic version of the Agent binary;
- `metadata.protocol_version`: revision of the Agent/API wire contract.

The current protocol revision is `4`. Revision 4 adds authoritative node
lifecycle bootstrap. Revision 3 adds recovery metadata,
explicit safe actions on `ReconcileWorkload`, and approval-gated
`CleanupRuntime`. Revision 2 added workload lifecycle commands
(`RestartProject`, `StopProject`, `SleepProject`, `WakeProject`, `HealthCheck`,
`RefreshRoute`) and node maintenance commands (`DrainNode`, `ResumeNode`). A control plane must only assign work to
an Agent revision it supports. Until the control plane enforces that admission
check, operators must deploy compatible `sakala-api` and Agent releases as a
pair and verify the heartbeat metadata before enabling a node.

## Command compatibility

Unknown protocol command values fail during deserialization. Known command
types without an Agent handler fail explicitly with
`unsupported_runtime_command`; they are never acknowledged as successful.

`DeployProject`, `InspectProject`, workload lifecycle, reconciliation, approved
runtime cleanup, and node maintenance commands are supported by the current
runtime. A control plane that only supports revision 1–3 must not admit a
revision-4 connected node until it implements `GET /api/agent/v1/node-state`.

## Revision 4 wire additions in v0.2.0

The following changes ship in Agent v0.2.0 and stay within protocol revision 4
because the control plane accepted them before the Agent adopted them, and an
older API answers them compatibly. `sakala-api` gates admitted revisions through
`SAKALA_AGENT_SUPPORTED_PROTOCOL_VERSIONS`; none of these require raising it.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `metadata.detail_counts` and 50-item bound on heartbeat detail collections | Adopted (#48). | Optional for the API; validated fully when present. |
| `stale_routes[].deployment_id` on heartbeat | Adopted. | Additive field, `null` for legacy route generations. |
| Batch report bodies `{ "events": [...] }` / `{ "logs": [...] }` | Adopted (#49). | Single-object bodies are no longer sent. The API accepts both. |
| `Idempotency-Key` header and bounded retry on reports, `complete`, `fail` | Adopted (#49). | Retries reuse the key; the API deduplicates per item and answers `409` when a key is reused for a different payload. A `200` must carry a valid acknowledgement; `204` without a body is still accepted for older control planes. |
| `409` body `terminal_at` | Adopted (#49). | Parsed when present and included in the conflict message; `null` is tolerated. |
| `409`/`422`/`413` on log reports stop delivery | Adopted (#49). | Matches the API guidance for followers after `complete`. |
| Claim response carrying the materialised command resource | **Not adopted**, recorded here. | The Agent keeps the polled record as the payload source. Using the claim body for secret materialisation would move payload trust to a second code path without a current need; revisit when the API stops materialising `environment` on poll. |
| `NodeStatus::busy` | Not emitted. | Reserved in the enum; capacity pressure is reported through `execution`/`workloads` instead. |

## Revision 4 additions in the next release

These changes stay within protocol revision 4 and do not require raising
`SAKALA_AGENT_SUPPORTED_PROTOCOL_VERSIONS`. Deploy `sakala-api` first, so that it
can read the new values before an Agent starts sending them. An older API stores
the new `error_code` values as ordinary strings and ignores the new heartbeat
fields, so this order is about what users see, not about rejected requests.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `error_code` values `runtime_app_not_listening`, `runtime_app_unhealthy`, `runtime_app_crashed`, `runtime_app_out_of_memory`, `runtime_port_conflict` | Sent on `fail`. | Additive. `runtime_port_conflict` replaces `invalid_runtime_command` for a `PORT` that disagrees with `container_port`. |
| `metadata.workloads.crashed_details` and `metadata.detail_counts.crashed_details` | Sent on heartbeat, bound 50. | Optional for the API. Both keys are omitted when the crash state is unknown, and a heartbeat without them must not change any status. Shape in [Sakala Agent API](AGENT_API.md#heartbeat-payload). |
| `HealthCheck` completion field `error_code` | Sent, `null` when ready. | Additive. `ready` now also requires the app to answer HTTP. |
| Application output before `fail` | Sent on the existing logs endpoint. | No new endpoint or field. |
| `cancel_requested` on `GET /commands` | Honoured. | Optional, treated as `[]` when absent. Cancelled work reports `fail` with `runtime_cancelled`; work past the route cutover reports `complete`. |
| `DeployProject` fields `root_directory`, `build_command`, `start_command` | Honoured. | Optional. A payload without them behaves as before. An older Agent ignores them and auto-detects. |
| `InspectProject` field `root_directory` | Honoured when sent. | Optional. The API does not send it yet, so inspection stays at the repository root until it does. |
| `error_code` value `runtime_root_directory_invalid` | Sent on `fail`. | Additive. The API needs to classify it as user-fixable, like `runtime_port_conflict`. |
| Events `deployment.analysis.started`, `deployment.health.started`, `deployment.route.started` | Sent. | Additive. An API that does not map them stores them on the timeline without changing status. On Railpack builds, `deployment.build.started` now follows analysis instead of preceding it. |

To show the new stages, the API needs this mapping in
`DeploymentStatus::fromAgentEventType`: `deployment.analysis.started` to `Analyzing`,
`deployment.health.started` to `HealthChecking`, and `deployment.route.started` to
`Routing`. `deployment.runtime.ready` must stop mapping to `Routing`. `order()` must
also rank `HealthChecking` before `Routing`, because the Agent checks health first
and transitions are forward-only: with the current order, `route.started` would be
ignored.

Host requirement: `SAKALA_RAILPACK_IMAGE` must name the sandbox image by digest, or
the docker driver refuses to start. The Railpack CLI no longer has to be on the host.
`metadata.runtime_dependencies.railpack` now reports that image reference.

Additive: `metadata.runtime_dependencies.container_runtime` names the runtime the node
was configured with, or is `null` for Docker's default.
`metadata.runtime_dependencies.container_runtime_limits_enforced` is `false` when that
runtime runs with `--ignore-cgroups`, which means `applied_resources` is what was asked
for and not what is enforced. It is `null` without a configured runtime, or when Docker
could not be asked.

Host requirement: the runtime Caddy container must provide `wget`. The readiness
probe runs inside it, and `caddy:*-alpine` images include BusyBox `wget`. Without
it every deployment fails with `runtime_dependency_failed`, and preflight logs a
non-fatal `caddy-http-probe` warning at startup.

Behaviour change for operators: a deployment that used to report success while
its app was broken now fails. That includes an app that ignores `$PORT` or listens
only on `127.0.0.1`, which is the purpose of the change.

Control-plane semantics the Agent relies on (lease expiry, offline detection,
pinned command types, `Claimed -> Running` on first report, report
sanitisation limits) are documented in
[Sakala Agent API](AGENT_API.md#semantik-control-plane-yang-diandalkan-agent).

## Revision 4 additions after v0.3.0

Still revision 4. The Agent accepts the field below before the API sends it, so
either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `StopProject` payload field `remove_images` | Honoured. | Optional boolean. The API should send `true` only on the stop it creates when a project is deleted (`DeleteProjectAction`, which today sends `{}`). The stop result then adds `removed_images` and `reclaimed_image_bytes`. An older Agent ignores the field; without it, the periodic image GC removes a deleted project's images once they are older than `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS`. |

## Revision 4 additions after v0.4.0

Still revision 4, and either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `InspectProject` result field `exposed_ports` | Sent, always. | List of TCP ports from `EXPOSE` in the final Dockerfile stage, `[]` when there is none. When that stage has no `EXPOSE` at all, the ports come from its base image's config on an allowlisted public registry, so `FROM nginx:alpine` alone reports `[80]`; the field and its meaning for the API stay the same. Rules in [Sakala Agent API](AGENT_API.md#project-inspection-command). An older API validates `result` only as an object and copies known keys, so it ignores the field. A result without the field comes from an older Agent, and the API must then leave its stored port alone. The API stores the port as `detected_port` only when the list has exactly one entry, and `container_port` stays `configured_port`, then `detected_port`, then 3000. |
| `InspectProject` result `railpack: null` | Sent when `railpack info` fails on a project with a Dockerfile. | Such an inspection used to end in `fail` with `runtime_build_failed`. `ProjectInspectionResultData` in every released API already reads a non-object `railpack` as `null`. |

## Revision 4 additions for idle sleep

Still revision 4: there is no new command type, only additive heartbeat fields and
different runtime behaviour behind the existing `SleepProject` and `WakeProject`.
Either side can be deployed first. An older API ignores the new heartbeat keys and
the new capability, and an older Agent never sends them.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `idle-sleep` | Advertised by the `docker` driver only. | The API should only put a project to sleep for idleness on a node that advertises it. |
| `metadata.workloads.traffic` and `metadata.detail_counts.traffic` | Sent on heartbeat, bound `SAKALA_MAX_ACTIVE_CONTAINERS`. | Items `{ project_id, deployment_id, last_request_at }`. Both keys are omitted while traffic is unknown, and a heartbeat without them must not make any project look idle. Shape and semantics in [Sakala Agent API](AGENT_API.md#traffic-workload). |
| `SleepProject` installs a sleeping route | Behaviour change. | The project's domain answers a 503 page that asks `POST /api/v1/public/projects/<slug>/wake` (under `SAKALA_PUBLIC_API_URL`, which defaults to `SAKALA_API_URL`) and reloads itself, instead of the router's 404. Suspension uses the same command, so the API must refuse wake requests for a suspended project. |
| `WakeProject` checks node capacity | Behaviour change. | A full node fails the wake with `runtime_capacity_exceeded`, an existing code, and keeps the sleeping route. |

Host requirement: the runtime Caddyfile needs `metrics { per_host }` in its global
block for `traffic` to be sent. Without it the Agent omits the list and everything
else works as before. The admin endpoint stays on the container's loopback; the
Agent reads it through `docker exec` at `127.0.0.1:2019`. See
[Caddy Integration](CADDY_INTEGRATION.md#route-tidur-dan-counter-request).

## Revision 4 additions after v0.7.0

Still revision 4, and either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `SleepProject` payload field `page` | Honoured. | Optional string, `waking` (default) or `unavailable`. `unavailable` installs a static 503 page, "Proyek tidak tersedia", with no script, no wake request, no reload, and no `Retry-After`; the API should send it when a visit must not wake the project, such as on suspension. The Agent never refuses a sleep over this field: an unknown value, `null`, or a non-object payload reads as `waking` and is logged as a warning. Example in `examples/commands/sleep-project-unavailable.json`; semantics in [Sakala Agent API](AGENT_API.md#workload-lifecycle-commands). The API should only send `page` to nodes that advertise `idle-sleep`. Agent v0.7.0 advertises that capability but ignores `page` and shows the waking page, so a suspended project on a v0.7.0 node still shows "Proyek sedang dinyalakan" until the node is upgraded. |
| Waking page stops reloading | Behaviour change. | The page now asks to wake the project once per visit (counted in the browser's `sessionStorage`), reloads every 3 seconds for at most 2 minutes, then shows "Proyek belum bisa dinyalakan" with a "Coba lagi" button that sends one more wake request and starts another round. Without JavaScript, or when `sessionStorage` is blocked, it reloads every 15 seconds without a cap. Each open tab now calls the public wake endpoint once per visit and once per click on "Coba lagi", instead of every 3 seconds. |

## Revision 4 additions for replicas

Still revision 4: no new command type, only an optional payload field, additive
heartbeat fields, and a new capability. Either side can be deployed first. An older
Agent ignores `replicas` and runs one container, so the API must only send a value
above 1 to a node that advertises `replicas`.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `replicas` | Advertised by the `docker` driver only. | The node's limit is `metadata.workloads.maximum_replicas_per_project` on every heartbeat, from `SAKALA_MAX_REPLICAS_PER_PROJECT` (default 4). `null` on a driver without replicas. |
| `DeployProject` payload field `replicas` | Honoured. | Optional positive integer, default 1 when absent or `null`. The Agent never refuses a deployment over an unreadable value: `0`, a negative number, a string such as `"3"`, a fraction, or any other type runs one replica and is logged as a warning. A value above the node's limit fails the deployment with `runtime_replicas_exceeded` before checkout; the Agent does not quietly run fewer. Example in `examples/commands/deploy-project-replicas.json`. |
| `error_code` value `runtime_replicas_exceeded` | Sent on `fail`. | Additive. The API should validate `replicas` against the node limit first, so this is a safety net. |
| `metadata.workloads.replicas` and `metadata.detail_counts.replicas` | Sent on heartbeat, bound `SAKALA_MAX_ACTIVE_CONTAINERS`. | Items `{ project_id, deployment_id, desired, ready }` for running deployments with more than one replica. Both keys are omitted when there is none. Shape in [Sakala Agent API](AGENT_API.md#replika-workload). |
| `crashed_details` per deployment | Behaviour change for replicas only. | A deployment is listed only when every running replica is crashed, once, with the replica that ended last. A partly crashed deployment shows as `ready < desired` instead. One-replica deployments are reported as before. |
| `HealthCheck` result fields `desired_replicas`, `ready_replicas`, `replicas` | Sent for deployments with more than one replica. | Additive. `ready` is `true` while at least one replica answers, and the top-level fields describe the first ready replica, or the first replica when none is. |
| Events `deployment.container.started` and `deployment.runtime.ready` | Add `replicas` and `containers` for more than one replica. | `container` keeps the first replica's name. |

Capacity: every replica counts against `SAKALA_MAX_ACTIVE_CONTAINERS`, and so does
`metadata.workloads.active`. A deploy or wake is admitted when the running containers
of other projects plus the new replicas fit; the project's own running containers are
not counted, as before, because the new set replaces them. While a rolling deploy
switches over, the node briefly runs both sets.

A one-replica deployment keeps the container name and the route every earlier Agent
wrote, so existing and sleeping workloads keep working across the upgrade, and a
node rolled back to an older Agent still finds them.

## Revision 4 additions for placement

Still revision 4: only additive heartbeat fields, read by `sakala-api` since PR #163
(region and capacity placement, Edge route table). Deploy `sakala-api` first. An older
API ignores the new keys, and an older Agent never sends them, which the API treats as
"no region, no edge address, unknown capacity".

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `metadata.runtime.region` | Sent when `SAKALA_AGENT_REGION` is set. | Matches `^[a-z0-9]+(?:-[a-z0-9]+)*$`, at most 32 characters, the API's `RegionSlug` rule. |
| `metadata.runtime.edge_address` | Sent when `SAKALA_AGENT_EDGE_ADDRESS` is set. | `host:port` where Sakala Edge reaches the node's runtime router: IPv4, bracketed IPv6, or a DNS name, port 1 to 65535 without leading zeros, at most 255 characters, the API's `EdgeAddress` rule. |
| `metadata.runtime` | Sent only when at least one of the two keys above is. | A new object; earlier heartbeats had no `runtime` key. Never sent empty. |
| `metadata.workloads.maximum_active_workloads` | Sent whenever the runtime reports its capacity. | Integer ≥ 0 from `SAKALA_MAX_ACTIVE_CONTAINERS`. Omitted, not `null`, on the `noop` driver and when the capacity probe fails. |

The API rejects the whole heartbeat with `422` over one malformed `region` or
`edge_address`. The Agent validates both variables at startup with the same rules and
refuses to start on a value the API would reject; an empty variable means unset. The
only difference is stricter on the Agent side: a value ending in a newline is refused,
while the API's PHP regex accepts it. An admin override stored in the API wins over
both heartbeat values. Shape in [Sakala Agent API](AGENT_API.md#lokasi-dan-kapasitas-node).

## Revision 4 behaviour change: replica restore

Still revision 4, with no wire change: no new command, payload field, heartbeat key,
or capability. Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Stopped replicas of a serving deployment | Started again by the Agent. | A replica that is `exited` while another replica of the same deployment is `Up`, and that deployment is the only one of its project with a replica `Up`, is started with `docker start` on the next health worker interval. Up to 5 starts per replica, the first at once, then after 30, 60, 120, and 240 s; staying up for 5 minutes resets the count. Sleeping projects, one-replica deployments, projects with a command in flight, and a full node are left alone. Details in [Runtime Hardening](RUNTIME_HARDENING.md#pemulihan-replika). |
| `metadata.workloads.replicas` | Same shape. | `ready < desired` now usually clears within one or two health intervals. A value that stays low means the replica failed 5 starts, its container is gone (it returns with the next deployment), or Docker is restarting it in a crash loop. |
| `crashed_details` | Unchanged. | A one-replica deployment whose container stopped is still not restored by the Agent and is reported and reconciled as before. |

## Revision 4 additions for release commands

Still revision 4: no new command type, only an optional payload field, two failure
codes, two events, and a new capability. Either side can be deployed first. An older
Agent ignores `release_command` and starts the new version without running it, so the
API must only send the field to a node that advertises `release-command`.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `release-command` | Advertised by the `docker` driver only. | The node's deadline is `SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS` (default 600). It is not sent on the heartbeat. |
| `DeployProject` payload field `release_command` | Honoured. | Optional string, 1 to 1000 characters, no ASCII control character (`\x00` to `\x1F`, `\x7F`), the rules the API applies to `start_command`. Absent, `null`, empty, or only spaces means no release command. A longer value or one with a control character fails with `invalid_runtime_command` before checkout. Runs once per deployment, after the build and before any replica, in a container of the new image with the deployment's environment, project network, resources, runtime, and security options. Example in `examples/commands/deploy-project-release-command.json`; semantics in [Sakala Agent API](AGENT_API.md#release-command). |
| `error_code` value `release_command_failed` | Sent on `fail`. | Additive and user-fixable. The message names the exit code, and the memory limit when Docker reports an out-of-memory kill. Nothing of the new deployment started; the previous one keeps serving. |
| `error_code` value `release_command_timeout` | Sent on `fail`. | Additive and user-fixable. The container was killed and removed; nothing of the new deployment started. |
| Events `deployment.release.started` and `deployment.release.completed` | Sent only for deployments with a release command. | Additive. An API that does not map them stores them on the timeline without changing status. To show a stage, map `deployment.release.started` to a status ranked after `Building` and before `HealthChecking`. |
| Log lines prefixed `[release]` | Sent on the existing logs endpoint. | The release container's output, redacted like runtime output. |

Capacity: the release container is admitted under the same serialized check as the
replicas that follow it, asking for the deployment's full replica count, so a node that
could not start the new version refuses before the command changes anything. While it
runs, other projects count it as one container. It does not appear in
`metadata.workloads` or any other heartbeat field.

The release command never runs on `WakeProject`, `RestartProject`, `RefreshRoute`,
`ReconcileWorkload`, or replica restore. Migrations must stay backward compatible with
the previous version, which keeps serving while the command runs and until the route
switches.

## Upgrade procedure

1. Check the release notes for protocol and configuration changes.
2. Upgrade `sakala-api` to a release that supports the target protocol
   revision.
3. Drain the node through the control plane once drain semantics are enabled;
   otherwise wait for active commands to finish.
4. Stop the Agent. It cancels active work and waits up to
   `SAKALA_SHUTDOWN_GRACE_SECONDS` for cleanup.
5. Install the new binary and validate its configuration.
6. Start the Agent and confirm Docker preflight and heartbeat metadata.

## Migration notes

### Protocol revision 1 → 2

Revision 2 introduces workload lifecycle and node maintenance commands. Before
an API begins assigning those commands, deploy an Agent that reports
`metadata.protocol_version: 2` and confirm it through its heartbeat. Operators
can also check the installed binary with `sakala-agent --version`. The Agent
continues to reject unknown command values; an older revision-1 API must not
assign revision-2 commands.

### Protocol revision 2 → 3

Revision 3 menyimpan command identity dan bounded-log policy sebagai label
container agar Agent dapat membangun ulang log follower setelah restart.
`ReconcileWorkload.actions` bersifat opt-in dan `CleanupRuntime` mewajibkan
`approved: true`. API revision 2 tetap dapat memakai command lama, tetapi tidak
boleh mengirim command revision 3 sebelum mendukung payload serta completion
result yang didokumentasikan.

### Protocol revision 3 → 4

Revision 4 mewajibkan API menyediakan `GET /api/agent/v1/node-state` dengan
desired state `active`, `draining`, `drained`, atau `maintenance`. Agent membaca
state ini sebelum polling sehingga restart process tidak mengaktifkan kembali
node yang masih di-drain. Connected Agent fail-closed bila endpoint belum ada
atau tidak dapat dijangkau.

The Agent repository owns binary behavior, protocol fixtures, and release
notes. Host installation, service management, and rollout orchestration remain
the responsibility of the deployment/infrastructure repository.
