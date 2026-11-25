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
| `SleepProject` installs a sleeping route | Behaviour change. | The project's domain answers a 503 page that asks `POST /api/v1/public/projects/<slug>/wake` (under `SAKALA_PUBLIC_API_URL`, which defaults to `SAKALA_API_URL`) and reloads itself, instead of the router's 404. From the release after v0.15.0 the page asks again every 15 seconds while it waits, so the API must keep treating a wake that is already queued as the same wake, and its per-address limit must allow at least 5 requests a minute per project. Suspension uses the same command, so the API must refuse wake requests for a suspended project. |
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
| `SleepProject` payload field `page` | Honoured. | Optional string, `waking` (default) or `unavailable`. `unavailable` installs a static 503 page, "Aplikasi ini sedang tidak tersedia" ("Proyek tidak tersedia" before the release after v0.15.0), with no script, no wake request, no reload, and no `Retry-After`; the API should send it when a visit must not wake the project, such as on suspension. The Agent never refuses a sleep over this field: an unknown value, `null`, or a non-object payload reads as `waking` and is logged as a warning. Example in `examples/commands/sleep-project-unavailable.json`; semantics in [Sakala Agent API](AGENT_API.md#workload-lifecycle-commands). The API should only send `page` to nodes that advertise `idle-sleep`. Agent v0.7.0 advertises that capability but ignores `page` and shows the waking page, so a suspended project on a v0.7.0 node still shows "Proyek sedang dinyalakan" until the node is upgraded. |
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

## Revision 4 additions for workload metrics

Still revision 4: only additive heartbeat fields and a new capability. Either side can
be deployed first. An older API ignores the new keys and the capability, and an older
Agent never sends them.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `workload-metrics` | Advertised by the `docker` driver only. | The API should only expect `workloads.metrics` from a node that advertises it. |
| `metadata.workloads.metrics` and `metadata.detail_counts.metrics` | Sent in the heartbeat after a fresh sample, bound `SAKALA_MAX_ACTIVE_CONTAINERS` × `SAKALA_MAX_REPLICAS_PER_PROJECT`. | Items `{ project_id, deployment_id, replica, sampled_at, cpu_percent, memory_bytes, memory_limit_bytes, restart_count }`, one per running workload container. Sampled at most once per `SAKALA_METRICS_INTERVAL_SECONDS` (default 60) and sent once, so heartbeats in between carry neither key. Both keys are also omitted when a sample fails; a heartbeat without them is "no new reading", never zero usage. `cpu_percent` is percent of one core and may exceed 100. Shape in [Sakala Agent API](AGENT_API.md#metrik-workload). |

New configuration: `SAKALA_METRICS_INTERVAL_SECONDS`, see
[Configuration](CONFIGURATION.md).

## Revision 4 additions for managed PostgreSQL

Still revision 4, but this release adds six command types. An older Agent fails to
deserialize a poll response that holds one of them, so the API must only assign them
to a node that advertises `managed-postgres`, and must not raise
`SAKALA_AGENT_SUPPORTED_PROTOCOL_VERSIONS` for it. Deploy `sakala-api` first: an older
API ignores the new capability and heartbeat keys.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `managed-postgres` | Advertised when `SAKALA_DATABASE_DRIVER=postgres`, next to the runtime driver's capabilities. | A dedicated database host runs `SAKALA_RUNTIME_DRIVER=noop`; a single-node installation may run `docker` with the database role. See [Database Host](DATABASE_HOST.md). |
| Command types `CreateDatabase`, `DropDatabase`, `RotateDatabasePassword`, `LockDatabase`, `UnlockDatabase`, `InstallDatabaseExtension` | Handled by the database role. | `project_id` and `deployment_id` are `null`. Names match `^p_[a-z0-9]{16,40}$`; passwords are 24 to 128 printable ASCII characters; extensions come from an allow-list, with `pgvector` accepted for `vector`. Every command is idempotent. Payloads, results, and semantics in [Sakala Agent API](AGENT_API.md#database-host); example in `examples/commands/database-commands.json`. |
| `error_code` values `database_name_invalid`, `database_password_invalid`, `database_extension_not_allowed`, `database_extension_unavailable`, `database_not_found`, `database_conflict`, `database_admin_unavailable`, `database_operation_failed` | Sent on `fail` of database commands. | Additive. Meaning and suggested API handling in [Sakala Agent API](AGENT_API.md#kode-kegagalan-database). |
| `metadata.database` | Sent on every heartbeat of a database host. | `{ endpoint: { host, port }, available }`. `available: false` also makes the node `degraded`. |
| `metadata.databases` and `metadata.detail_counts.databases` | Sent after a fresh size read, at most once per 60 s, bound 1000. | Items `{ name, size_bytes }` from `pg_database_size()`, managed databases only. A heartbeat without them carries no new reading. Example in `examples/heartbeat/database-host.json`. |
| Lock semantics | `NOLOGIN` on the owner role, not `ALLOW_CONNECTIONS false`. | The wire contract is unchanged. A locked database stays reachable for the admin user, so backups and size reads continue, and the tenant cannot undo the lock as database owner. |

New configuration: `SAKALA_DATABASE_DRIVER`, `SAKALA_DATABASE_ADMIN_DSN`,
`SAKALA_DATABASE_PUBLIC_HOST`, `SAKALA_DATABASE_PUBLIC_PORT`, see
[Configuration](CONFIGURATION.md) and [Database Host](DATABASE_HOST.md).

## Revision 4 additions for JavaScript monorepos

Still revision 4: no new command type or payload field, only an additive inspection
result field, a new event, and a behaviour change for Railpack builds of a workspace
package (gmedia/sakala-agent#74). Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Railpack build context for a workspace package | Behaviour change. | When `root_directory` names a package of a pnpm, npm, Yarn, or Bun workspace whose root is above it, `railpack prepare` and the build context move to the workspace root, and the build and start commands target the package. A project's own `build_command`, `start_command`, and `release_command` run as `cd '<package>' && <command>`. A Vite single-page app without a `start` script is served as static files. Dockerfile builds and repositories without a workspace are unchanged. Rules in [Docker Runtime](DOCKER_RUNTIME.md#monorepo-javascript). |
| `InspectProject` result field `workspace` | Sent, always. | `null` without a workspace. Otherwise `{ manager, root_directory, package_directory, package_name, build_command, start_command, static_output_directory }`; `manager` is one of `npm`, `pnpm`, `yarn`, `bun`, and `package_manager` then holds the same value. A result without the field comes from an older Agent. An older API copies known keys only, so it ignores the field. Shape in [Sakala Agent API](AGENT_API.md#project-inspection-command). |
| Event `deployment.workspace.detected` | Sent only for such a build, after `deployment.analysis.started`. | Additive. Metadata has the shape of `workspace` above with the commands actually used. An API that does not map it stores it on the timeline without changing status; it maps to no status. |

## Revision 4 additions for host checks

Still revision 4: one additive failure code and no wire change otherwise. Either side
can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `error_code` value `runtime_app_host_blocked` | Sent on `fail` of `DeployProject`. | Additive and user-fixable. The app answered, but Vite's preview or dev server (403) or Django's debug page (400) refuses the project's domain, so every visitor would get that refusal. Such a deployment used to succeed, because any status below 500 passes readiness. The message names the domain, the framework, and the fix. The previous deployment keeps serving. Suggested mapping: category start, hint to add the domain to `allowedHosts`/`ALLOWED_HOSTS` or to clear a `vite preview` start command. Rules in [Docker Runtime](DOCKER_RUNTIME.md#host-yang-ditolak-framework). |

Host requirement: the runtime router image needs BusyBox `nc`, which `caddy:*-alpine`
has. Without it the check is skipped and readiness behaves as before.

## Revision 4 additions for image reuse

Still revision 4: no new command type, error code, or completion field, only an
optional payload field, two events, image labels, and a new capability. Either side can
be deployed first. An older Agent ignores `reuse_image_from_deployment_id` and builds,
which is correct but slow, so the API must only send the field to a node that
advertises `image-reuse`.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `image-reuse` | Advertised by the `docker` driver only. | Not sent on the heartbeat otherwise. |
| `DeployProject` payload field `reuse_image_from_deployment_id` | Honoured. | Optional string, the UUID of the deployment that built the image to start from. Absent or `null` builds as before. The Agent never refuses a deployment over this field: any value that is not a UUID string builds and reports `invalid_reference`. The Agent looks the image up as `sakala/project-<project_id>:<commit12>-<source8>`, computed from the command's `project_id`, the payload's `commit_sha`, and the field, never from an image reference in the payload, and reuses it only when its labels `dev.sakala.project-id` and `dev.sakala.deployment-id` name that project and deployment and `dev.sakala.builder` is `dockerfile` or `railpack`. A reuse skips checkout, the workspace disk check, and the build queue; the release command, capacity checks, readiness, replicas, and the zero-downtime switch run as for a build. Example in `examples/commands/deploy-project-image-reuse.json`; semantics in [Sakala Agent API](AGENT_API.md#memakai-ulang-image-tanpa-build). |
| Event `deployment.image.reused` | Sent on reuse, after `deployment.timeouts.resolved`. | Level `info`, metadata `{ image, source_deployment_id, builder }`. Replaces `deployment.checkout.started`, `deployment.analysis.started`, `deployment.workspace.detected`, and `deployment.build.started`. `deployment.runtime.ready` reports the reused `image` and the labelled `builder`. Maps to no status. |
| Event `deployment.image.reuse_skipped` | Sent when reuse was asked for but the Agent builds, before `deployment.checkout.started`. | Level `warning`, metadata `{ source_deployment_id, reason }`; `source_deployment_id` is `null` for `invalid_reference`. `reason` is one of `image_missing`, `image_not_owned`, `image_metadata_missing`, `invalid_reference`. Not a failure; the build path follows unchanged. Maps to no status. |
| Image labels `dev.sakala.builder` and `dev.sakala.package-directory` | Written on every image built from this release on. | `dev.sakala.builder` is `dockerfile` or `railpack`. `dev.sakala.package-directory` is written only for an image built from a JavaScript workspace root. Images built by an older Agent lack them, so the first configuration redeploy after the upgrade builds once with `image_metadata_missing`. |
| Failure cleanup | Behaviour change for reuse only. | A reused deployment that fails before its route switches removes its candidate containers but never the image, which the serving deployment still uses. Image GC is unchanged. |

## Revision 4 additions for image rollback

Still revision 4: no new command type, payload field, error code, or completion field.
A rollback is an ordinary `DeployProject` that reuses the target's image through
`reuse_image_from_deployment_id`. Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Rollback request | No change on the wire. | The API sends the target's `commit_sha` and, as `reuse_image_from_deployment_id`, the deployment that built the target's image. It may send that field only to a node that advertises `image-reuse`; any other node gets a plain build of that commit. A node with `image-reuse` but without `image-rollback` (Agent v0.14.0 to v0.15.0) still reuses the image when its old GC rule kept it, and builds with `image_missing` otherwise. |
| Node capability `image-rollback` | Advertised by the `docker` driver only. | Promises that the live image and the previous image of every project stay on the node, so the API may promise a rollback "tanpa build" when the target built one of those two images on that node. Not sent on the heartbeat otherwise. |
| Previous image | Kept and marked. | After a successful `DeployProject`, the image of the workload it replaced, when different from the new live image, gets the extra tag `sakala/project-<project_id>:previous`; a redeploy on the live image does not move it, a failed deployment never does. The original tag stays, so reuse lookup is unchanged. Only images whose every name is in the project's repository and whose `dev.sakala.project-id` label names the project are tagged. |
| Post-deployment prune | New. | Still under the project lock, every other image of the project that no container uses is removed with `docker image rm` by name, never `--force`. A failure there does not fail the deployment: the Agent sends `deployment.images.retained` (level `warning`, metadata `{ project_id }`) after `deployment.runtime.ready` and leaves the images to periodic GC. Maps to no status. |
| Image GC of a project with containers | **Behaviour change.** | Keeps every image a container uses, in any state, plus the image tagged `previous`; without that tag (images of an older Agent) the newest image older than the newest one in use, as before. Age no longer protects anything: `SAKALA_IMAGE_GC_MAX_AGE_SECONDS` is ignored, still parsed so existing configuration loads, and logged as ignored at startup. GC removes a project's images only while it holds that project's command lock, choosing them again under the lock. Projects without containers keep the `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` rule unchanged. |
| `RestartProject` on a project whose replicas all exited | **Behaviour change.** | Restarted after the same capacity check as a wake, instead of `runtime_workload_not_running`. A crash-looping deployment (Docker status `Restarting`) is restarted too. A deployment whose project route is the sleeping page for it is still refused with `runtime_workload_not_running`. The API should send `RestartProject` only for `runtime_status` `running` or `crashed`: a project whose sleeping page could not be installed has no sleeping route, and the Agent cannot tell it from a crash. |

## Revision 4 behaviour change: log delivery retries

Still revision 4, with no wire change: the same endpoint, body, `Idempotency-Key`
semantics, and acknowledgement. Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Transient failures on `POST /commands/{id}/logs` | Retried until acknowledged. | Connection and timeout failures, an unreadable body, `408`, `429`, and `5xx` are retried with the same key and the same batch, 1 to 15 s exponential backoff plus jitter, never sooner than `Retry-After` (capped at 5 minutes), for as long as the command runs, and up to 120 s before `complete`/`fail` (never past `command_timeout_seconds`, at least 10 s). Agent v0.13.0 and older gave up after 3 attempts and dropped every later line of the command. The API must keep answering a replayed key with the original sequence range, which it does for as long as the log rows exist. |
| Order of log lines | Unchanged. | One batch per command is in flight at a time, so sequence order is the order the lines were produced. |
| Lines dropped by the Agent | Marked in the timeline. | The Agent queues at most 10,000 lines and 2 MiB per command besides the batch in flight; past that the oldest queued lines are dropped and a `system` line `<n> log lines are missing here: ...` is sent where they were. It is an ordinary log line and counts toward `max_total_bytes`. |
| Other `4xx` on log reports | Stop delivery, as `409`/`413`/`422` do. | No change in practice: these were never retried. |

## Revision 4 behaviour change: event delivery retries

Still revision 4, with no wire change: the same endpoint, body, `Idempotency-Key`
semantics, and acknowledgement. Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Transient failures on `POST /commands/{id}/events` | Retried for up to 90 s per event, then given up. | Same key and body on every attempt, 1 to 15 s exponential backoff plus jitter, never sooner than `Retry-After` (capped at 5 minutes). After one event is given up, later events of the command get one attempt each until one is delivered. Agent v0.16.2 and older gave up after 3 attempts and failed the deployment step that sent the event. |
| Undeliverable events | **Behaviour change.** Dropped with a warning; the command continues. | Applies to the budget running out, `413`, `422`, other `4xx`, and a `200` without a valid acknowledgement. Dropped events are not resent later. The API may therefore see a deployment complete while its status still shows an earlier stage, or without `deployment.runtime.ready` and so without `image_reference`. `complete`/`fail` are unchanged. |
| `409` on events | Unchanged. | Still an error that stops the step, because the API no longer accepts reports for the command. |

## Revision 4 additions for env and database suggestions

Still revision 4: no new command type or payload field, only two additive inspection
result fields. Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `InspectProject` result field `env_example` | Sent, always. | `null` when the inspected directory has no `.env.example`, `.env.sample`, or `.env.dist`, or the first one that exists cannot be read safely (symlink leaving the directory, not a regular file, over 64 KiB). Otherwise `{ file, entries: [{ key, example_value }], truncated }`, at most 200 entries; `example_value` is `null` when empty, multi-line, or credential-like. `.env` is never opened and nothing is interpolated. A result without the field comes from an older Agent. An older API copies known keys only, so it ignores the field. Rules in [Sakala Agent API](AGENT_API.md#variabel-dari-envexample). |
| `InspectProject` result field `database_hint` | Sent, always, as an object. | `{ likely, engine, signals: [{ kind, file, name }], suggested_release_command }`. `engine` is one of `postgresql`, `mysql`, `sqlite`, `unknown`; `kind` is one of `env_variable`, `laravel_migrations`, `prisma_schema`, `django_settings`, `rails_database_config`, `package_dependency`, and new kinds may be added, so consumers must ignore unknown ones. `suggested_release_command` is `null` or exactly one of `php artisan migrate --force`, `python manage.py migrate`, `npx prisma migrate deploy`, `bin/rails db:migrate`. At most 16 signals. A result without the field comes from an older Agent. Rules in [Sakala Agent API](AGENT_API.md#petunjuk-database). |

## Revision 4 additions for framework hints

Still revision 4: no new command type or payload field, only one additive inspection
result field. Either side can be deployed first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `InspectProject` result field `frameworks` | Sent, always, as an array. | Sorted, unique strings from the closed set `astro`, `docusaurus`, `eleventy`, `gatsby`, `next`, `nuxt`, `react-router`, `remix`, `sveltekit`, `tanstack-start`, `vitepress`: build-time-rendering frameworks named by exact package names in `dependencies` or `devDependencies` of the inspected directory's `package.json` (the package directory in a JavaScript workspace). `[]` when none match or there is no readable `package.json`; an invalid or oversized file never fails the inspection. A result without the field comes from an older Agent, and the API then falls back to Railpack signals. An older API copies known keys only, so it ignores the field. Rules and the package table in [Sakala Agent API](AGENT_API.md#framework-yang-merender-saat-build). |

## Revision 4 additions for sleeping workloads

Still revision 4: no new command type or payload field, only additive heartbeat keys.
Either side can be deployed first. The API persists heartbeat metadata as reported,
so an older API keeps the new keys without reading them.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| `metadata.startup_reconciliation.sleeping_workloads` and `metadata.detail_counts.sleeping_workloads` | Sent, always. | Items `{ container_id, project_id, deployment_id, status }`, at most 50, for stopped containers of the deployment a project's sleeping route names. These were reported under `orphans` with reason `stale stopped deployment container` by Agent v0.15.0 and older; a stopped container no sleeping route names still is. `metadata.workloads.stopped` counts both, as before. See [Sakala Agent API](AGENT_API.md#heartbeat-payload). |

## Revision 4 additions for managed object storage

Still revision 4, but this release adds six command types (ADR-021). An older Agent
fails to deserialize a poll response that holds one of them, so the API must only
assign them to a node that advertises `managed-s3`, and must not raise
`SAKALA_AGENT_SUPPORTED_PROTOCOL_VERSIONS` for it. A new Agent with
`SAKALA_OBJECT_STORAGE_DRIVER=none` (the default) changes nothing. A new Agent with
`rustfs` against an older API sends heartbeat keys the API must ignore, and never
receives bucket commands; check the API with the heartbeat example below before
switching a node on.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `managed-s3` | Advertised when `SAKALA_OBJECT_STORAGE_DRIVER=rustfs`, after the runtime driver's capabilities and `managed-postgres`. | A dedicated storage VM runs `SAKALA_RUNTIME_DRIVER=noop`; a co-located node runs `docker` with the role. See [Object Storage Host](OBJECT_STORAGE_HOST.md). |
| Command types `CreateBucket`, `DeleteBucket`, `RotateBucketKey`, `SetBucketQuota`, `LockBucket`, `UnlockBucket` | Handled by the object storage role, appended to `CommandType` in the API's order. | `project_id` and `deployment_id` are `null`. Buckets match `^b-[a-z0-9]{16,40}$`, access key IDs `^SK[A-Z0-9]{16,38}$`, secrets `^[A-Za-z0-9]{32,128}$`, and `quota_mb` is 1 to 1048576. Every command is idempotent, and a repeated `CreateBucket` never unlocks a locked key. Payloads, results, and semantics in [Sakala Agent API](AGENT_API.md#object-storage-host); example in `examples/commands/bucket-commands.json`. |
| `error_code` values `bucket_name_invalid`, `bucket_key_invalid`, `bucket_quota_invalid`, `bucket_capacity_exceeded`, `bucket_admin_unavailable`, `bucket_not_found`, `bucket_conflict`, `bucket_operation_failed`, `bucket_outcome_unknown` | Sent on `fail` of bucket commands. | Additive. `bucket_outcome_unknown` (only `RotateBucketKey`) means the old secret may already be dead: resend the same command. Example bodies in `examples/failures/bucket-failures.json`. Meaning and suggested API handling in [Sakala Agent API](AGENT_API.md#kode-kegagalan-bucket). |
| `metadata.object_storage` | Sent on every heartbeat of an object storage host. | `{ endpoint, region, capacity_mb, available }`. `available: false` also makes the node `degraded`. |
| `metadata.buckets` and `metadata.detail_counts.buckets` | Sent after a fresh usage read, at most once per 60 s, bound 1000. | Items `{ name, size_bytes }`, managed buckets only, sorted by name. A heartbeat without them carries no new reading. Example in `examples/heartbeat/object-storage-host.json`. |
| Lock semantics | `rc admin user disable`, not a policy detach. | rc 0.1.37 supports disable and enable, so the fallback in the contract (detach the policy and a `locked` hint on `CreateBucket`) is not needed. The key and the presigned URLs it signed are refused while locked. |

New configuration: `SAKALA_OBJECT_STORAGE_DRIVER`, `SAKALA_OBJECT_STORAGE_CONTAINER`,
`SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT`, `SAKALA_OBJECT_STORAGE_REGION`,
`SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB`, `SAKALA_OBJECT_STORAGE_MIN_FREE_MB`,
`SAKALA_OBJECT_STORAGE_DATA_DIR`, see [Configuration](CONFIGURATION.md) and
[Object Storage Host](OBJECT_STORAGE_HOST.md). None of them holds a credential: the
RustFS root key stays in the RustFS container.

## Revision 4 additions for worker processes and scheduled jobs

Still revision 4 (ADR-022), but this release adds one command type, `RunJob`. An older
Agent fails to deserialize a poll response that holds it, so the API must only assign
`RunJob` to a node that advertises `scheduled-jobs`, and must not raise
`SAKALA_AGENT_SUPPORTED_PROTOCOL_VERSIONS` for it. `processes` is an optional payload
field: an older Agent ignores it and runs the web process only, so the API must only send
it, and place a deployment with workers, on a node that advertises `worker-processes`.
A new Agent against an older API changes nothing: that API never sends `processes` or
`RunJob`, so no worker or job container exists and none of the new keys below is sent.
Deploy either side first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capabilities `worker-processes` and `scheduled-jobs` | Advertised by the `docker` driver only, after `image-rollback`. | Neither is advertised by `noop`. |
| `DeployProject` payload field `processes` | Honoured. | Optional list of at most 5 `{ name, command, replicas, resources }`. Absent, `null`, or `[]` deploys the web process only, exactly as before. A value that is not a list of definitions never fails the payload: the web process deploys and event `deployment.processes.ignored` (level `warning`) says why. A readable list that breaks a rule fails with `invalid_runtime_command`, and `replicas` above the node limit with `runtime_replicas_exceeded`, both before checkout. Example in `examples/commands/deploy-project-processes.json`; semantics in [Sakala Agent API](AGENT_API.md#proses-worker). |
| `error_code` value `worker_start_failed` | Sent on `fail` of `DeployProject`. | Additive and user-fixable: a worker exited or was restarted, with any exit code, before the route switched. Nothing of the new deployment stays; the previous one keeps serving with its own workers. |
| Command type `RunJob` | Handled by the `docker` driver, appended last to `CommandType`. | `project_id` and `deployment_id` (the live deployment) are required. Payload `{ command, timeout_seconds, commit_sha, reuse_image_from_deployment_id, container_port, environment, resources, log_bounds }`; `timeout_seconds` defaults to 300 and must be 1 to 600. `complete` result `{ exit_code, duration_ms, oom_killed }`. Example in `examples/commands/run-job.json`; semantics in [Sakala Agent API](AGENT_API.md#job-runjob). |
| `error_code` values `job_failed`, `job_timeout`, `job_image_missing` | Sent on `fail` of `RunJob`. | Additive. `runtime_capacity_exceeded`, `runtime_cancelled`, `invalid_runtime_command`, and `runtime_port_conflict` keep their meaning. Example bodies in `examples/failures/job-failures.json`. |
| Events `job.started` and `job.finished` | Sent for every `RunJob` that started a container. | `job.finished` carries `exit_code` (`null` after a timeout), `duration_ms`, `oom_killed`, and `timed_out`, also when the command then fails. |
| Events `deployment.container.started`, `deployment.runtime.ready`, `deployment.health.started` | Add `processes` (and `worker_startup_seconds`) only for a deployment with workers. | A deployment without workers sends the events every earlier Agent sent. |
| Log item field `process` | Sent on lines of worker containers (the worker name) and of `RunJob` (`job`). | Absent on build, release, and web lines, as before. Each worker's lines go through a reporter of their own with their own `max_total_bytes` budget and stopped state. The current API ignores unknown log keys; an API that enforces the budget per command should count it per `process`. |
| `process` on `metadata.workloads.crashed_details[]`, `metrics[]`, and `unhealthy_details[]` | Sent only for worker containers. | Absent means the web process, so earlier shapes are unchanged. Crashes are grouped per process: a crashing worker never marks the web process crashed, and a worker that ends with code 0 is never a crash. `workloads.replicas` counts web replicas only. Example in `examples/heartbeat/worker-processes.json`. |

Capacity: every worker container counts against `SAKALA_MAX_ACTIVE_CONTAINERS` like a
replica, on deploy, wake, and restart of a crashed deployment, and in
`metadata.workloads.active`. A running job counts as one container for every admission,
its own project's deploy included, because a deploy never replaces a job. Jobs run in
their own pool, `SAKALA_MAX_CONCURRENT_JOBS` (default 2), outside the per-project queue.

Lifecycle: `RestartProject`, `SleepProject`, `WakeProject`, and `StopProject` handle web
and worker containers together; `HealthCheck`, `RefreshRoute`, and route restore look at
web replicas only. `StopProject` also cancels the project's running jobs and removes the
project's job and release containers in any state; `SleepProject` does not, and a job
never wakes a project. A job or release container never makes a lifecycle command of its
project fail. A rollback runs the `processes` recorded in the
target's payload.

New configuration: `SAKALA_MAX_CONCURRENT_JOBS`, `SAKALA_WORKER_STARTUP_SECONDS`,
`SAKALA_WORKER_STOP_GRACE_SECONDS`, see [Configuration](CONFIGURATION.md).

Rolling the Agent back: an Agent without `worker-processes` reads a worker container as
a web replica and routes to it. Before downgrading, redeploy the projects that have
workers without `processes`, or stop them, and check that
`docker ps -a --filter label=dev.sakala.workload-kind=worker` is empty.

## Revision 4 additions for managed Valkey

Still revision 4 (ADR-023), but this release adds six command types. An older Agent
fails to deserialize a poll response that holds one of them, so the API must only
assign them to a node that advertises `managed-valkey`, and must not raise
`SAKALA_AGENT_SUPPORTED_PROTOCOL_VERSIONS` for it. `valkey` is an optional payload field
of `DeployProject` and `RunJob`: an older Agent ignores it and the application cannot
reach its store, so the API must only send it to, and place an attached project on, the
node that runs the store. A new Agent with `SAKALA_VALKEY_DRIVER=none` (the default)
changes nothing. A new Agent with the role against an older API sends heartbeat keys
the API must ignore, and never receives Valkey commands. Deploy the Agent first.

| Capability | Agent decision | Notes |
| --- | --- | --- |
| Node capability `managed-valkey` | Advertised when `SAKALA_VALKEY_DRIVER=docker`, last, after `managed-postgres` and `managed-s3`. | Requires `SAKALA_RUNTIME_DRIVER=docker`. See [Valkey Host](VALKEY_HOST.md). |
| Command types `CreateValkey`, `DeleteValkey`, `RotateValkeyPassword`, `LockValkey`, `UnlockValkey`, `ResizeValkey` | Handled by the Valkey role, appended to `CommandType` after `RunJob`. | `project_id` and `deployment_id` are `null`. `valkey_id` is a UUID, `mode` is `cache` or `persistent`, `maxmemory_mb` is 16 to 512, `password` is `^[A-Za-z0-9]{32,128}$`. `CreateValkey` returns `network` and `endpoint: { host, port }`. Every command is idempotent; a repeated `CreateValkey` never unlocks and refuses another mode or size. Payloads, results, and semantics in [Sakala Agent API](AGENT_API.md#valkey-host); example in `examples/commands/valkey-commands.json`. |
| `DeployProject` and `RunJob` payload field `valkey` | Honoured. | `{ network, host }` from `CreateValkey`'s result. Absent or `null` changes nothing. `network` must be `sakala-valkey-<uuid>`, otherwise `invalid_runtime_command`. Checked before checkout and before any container; every container of the deployment and the job is created on the project network and the store network at once, never connected later. Examples in `examples/commands/deploy-project-valkey.json` and `examples/commands/run-job-valkey.json`. |
| `error_code` values `valkey_mode_invalid`, `valkey_memory_invalid`, `valkey_password_invalid`, `valkey_capacity_exceeded`, `valkey_not_found`, `valkey_conflict`, `valkey_size_below_usage`, `valkey_in_use`, `valkey_unavailable`, `valkey_operation_failed` | Sent on `fail` of Valkey commands. | Additive. Example bodies in `examples/failures/valkey-failures.json`; meaning and suggested handling in [Sakala Agent API](AGENT_API.md#kode-kegagalan-valkey). |
| `error_code` value `valkey_network_unavailable` | Sent on `fail` of `DeployProject` and `RunJob`. | The store network is missing, not this agent's, or its store is not at `host`. Nothing of the deployment or job started; the previous deployment keeps running. |
| `metadata.valkey` | Sent on every heartbeat of a Valkey host. | `{ capacity_mb, available }`. `available: false` also makes the node `degraded`. |
| `metadata.valkeys` and `metadata.detail_counts.valkeys` | Sent after a fresh read, at most once per 60 s, bound 1000. | Items `{ valkey_id, mode, maxmemory_mb, locked, status, restart_count, used_memory, maxmemory, evicted_keys, errorstat_oom, connected_clients, aof_last_write_status }`, sorted by `valkey_id`; the `INFO` fields are `null` for a store that is not running or did not answer. Example in `examples/heartbeat/valkey-host.json`. |

Store containers carry `dev.sakala.valkey-id`, not `dev.sakala.managed` or
`dev.sakala.role`, so they are never workloads, never one-off containers, and never
count against `SAKALA_MAX_ACTIVE_CONTAINERS`.

New configuration: `SAKALA_VALKEY_DRIVER`, `SAKALA_VALKEY_IMAGE`,
`SAKALA_VALKEY_NETWORK_POOL`, `SAKALA_VALKEY_TOTAL_MEMORY_MB`,
`SAKALA_VALKEY_DATA_DIR`, see [Configuration](CONFIGURATION.md) and
[Valkey Host](VALKEY_HOST.md). None of them holds a credential: each store's admin
password is generated by the Agent and stays in the store's directory.

Rolling the Agent back: an Agent without `managed-valkey` ignores `valkey`, so the next
deployment of an attached project loses its store network without an error. Before
downgrading, delete the stores or stop the projects that use them, and check that
`docker ps -a --filter label=dev.sakala.valkey-id` is empty.

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
