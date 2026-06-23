# Command Lifecycle

`sakala-api` adalah pemilik record command dan authorization. Agent hanya mengerjakan command yang diberikan melalui agent API.

## Status

```txt
Pending -> Claimed -> Running -> Succeeded
                            \-> Failed
Pending -> Cancelled
Pending -> Expired
```

Foundation mendefinisikan mapping status tersebut melalui `CommandStatus` dan helper transisi pada core. Connected agent hanya memproses command berstatus `Pending`; status lain yang tidak sengaja dikembalikan API dilewati dan dicatat sebagai warning. Endpoint agent belum diimplementasikan di `sakala-api` pada tahap ini.

## Flow Connected Mode

```txt
1. Agent polling GET /api/agent/v1/commands.
2. Agent memilih command yang diterima.
3. Agent claim command.
4. RuntimeExecutor menjalankan command.
5. Agent mengirim events/logs yang sudah direduksi dari secret.
6. Agent menandai complete atau fail.
```

Polling response memakai envelope `{ "data": [...] }`. Request lifecycle selalu membawa bearer token dan `X-Agent-Id`. Event/log memakai command UUID dari URL, sedangkan sequence dan relasi persistence dimiliki `sakala-api`.

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
