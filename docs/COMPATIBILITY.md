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

Control-plane semantics the Agent relies on (lease expiry, offline detection,
pinned command types, `Claimed -> Running` on first report, report
sanitisation limits) are documented in
[Sakala Agent API](AGENT_API.md#semantik-control-plane-yang-diandalkan-agent).

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
