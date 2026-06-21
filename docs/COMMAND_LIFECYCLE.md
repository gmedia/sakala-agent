# Command Lifecycle

`sakala-api` adalah pemilik record command dan authorization. Agent hanya mengerjakan command yang diberikan melalui agent API.

## Status

```txt
Pending -> Claimed -> Running -> Succeeded
                            \-> Failed
Pending -> Cancelled
Pending -> Expired
```

Foundation mendefinisikan mapping status tersebut melalui `CommandStatus` dan helper transisi pada core. Endpoint agent belum diimplementasikan di `sakala-api` pada tahap ini.

## Flow Connected Mode

```txt
1. Agent polling GET /api/agent/v1/commands.
2. Agent memilih command yang diterima.
3. Agent claim command.
4. RuntimeExecutor menjalankan command.
5. Agent mengirim events/logs yang sudah direduksi dari secret.
6. Agent menandai complete atau fail.
```

## Foundation Executor

`NoopRuntimeExecutor` menyelesaikan command dengan event dan log informasional. Ia tidak melakukan build, container lifecycle, route mutation, atau health check nyata. Hal ini memungkinkan lifecycle/API contract diuji sebelum operasi privileged ditambahkan.

## Command Types

```txt
DeployProject
RestartProject
StopProject
SleepProject
WakeProject
HealthCheck
RefreshRoute
```
