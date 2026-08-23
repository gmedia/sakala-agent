# Compatibility and Release Policy

## Version model

Sakala Agent reports two independent values in every heartbeat:

- `metadata.version`: semantic version of the Agent binary;
- `metadata.protocol_version`: revision of the Agent/API wire contract.

The current protocol revision is `3`. Revision 3 adds recovery metadata,
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
runtime. A control plane that only supports revision 1 or 2 must not assign
revision-3 commands to the node.

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

The Agent repository owns binary behavior, protocol fixtures, and release
notes. Host installation, service management, and rollout orchestration remain
the responsibility of the deployment/infrastructure repository.
