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
4. `CommandDispatcher` meneruskan command ke port `RuntimeExecutor`.
5. Agent mengirim events/logs yang sudah direduksi dari secret.
6. Agent menandai complete atau fail.
```

Polling response memakai envelope `{ "data": [...] }`. Request lifecycle selalu membawa bearer token dan `X-Agent-Id`. Event/log memakai command UUID dari URL, sedangkan sequence dan relasi persistence dimiliki `sakala-api`.

## Runtime Executor

`NoopRuntimeExecutor` tetap default dan hanya mengirim event/log informasional. `DockerRuntimeExecutor` dapat dipilih operator untuk `InspectProject` dan `DeployProject`. Ia mengirim event dan log melalui `RuntimeReporter` selama command berjalan, sehingga core tetap menjadi satu-satunya boundary report ke API dan redaksi secret.

`InspectProject` melakukan checkout immutable, menjalankan `railpack info`, memindai metadata repository, lalu mengembalikan `ProjectInspection` melalui completion result. Ia tidak membangun image atau mengubah runtime node.

Payload `DeployProject` wajib memuat repository GitHub publik, full `commit_sha`, generated domain, internal port, builder, environment map, dan optional resource request. Resource policy berasal dari API; agent memvalidasi request terhadap node safety ceiling sebelum menjalankan proses apa pun. Runtime tidak menerima command shell mentah.

## Command Types

```txt
InspectProject
DeployProject
RestartProject
StopProject
SleepProject
WakeProject
HealthCheck
RefreshRoute
```
