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
| `InspectProject` result field `exposed_ports` | Sent, always. | List of TCP ports from `EXPOSE` in the final Dockerfile stage, `[]` when there is none. Rules in [Sakala Agent API](AGENT_API.md#project-inspection-command). An older API validates `result` only as an object and copies known keys, so it ignores the field. A result without the field comes from an older Agent, and the API must then leave its stored port alone. The API stores the port as `detected_port` only when the list has exactly one entry, and `container_port` stays `configured_port`, then `detected_port`, then 3000. |
| `InspectProject` result `railpack: null` | Sent when `railpack info` fails on a project with a Dockerfile. | Such an inspection used to end in `fail` with `runtime_build_failed`. `ProjectInspectionResultData` in every released API already reads a non-object `railpack` as `null`. |

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
