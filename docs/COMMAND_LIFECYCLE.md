# Command Lifecycle

`sakala-api` adalah pemilik record command dan authorization. Agent hanya mengerjakan command yang diberikan melalui agent API.

## Status

```txt
Pending -> Claimed -> Running -> Succeeded
                            \-> Failed
Pending -> Cancelled
Pending -> Expired
```

Foundation mendefinisikan mapping status tersebut melalui `CommandStatus` dan helper transisi pada core. Connected agent hanya memproses command berstatus `Pending`; status lain yang tidak sengaja dikembalikan API dilewati dan dicatat sebagai warning. Protocol revision 4 juga mewajibkan bootstrap `GET /api/agent/v1/node-state` sebelum polling dimulai.

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

`NoopRuntimeExecutor` tetap default dan hanya mengirim event/log informasional. `DockerRuntimeExecutor` dapat dipilih operator untuk `InspectProject`, `DeployProject`, dan lifecycle workload. Ia mengirim event dan log melalui `RuntimeReporter` selama command berjalan, sehingga core tetap menjadi satu-satunya boundary report ke API dan redaksi secret.

`InspectProject` melakukan checkout immutable, menjalankan `railpack info`, memindai metadata repository, lalu mengembalikan `ProjectInspection` melalui completion result. Ia tidak membangun image atau mengubah runtime node.

Payload `DeployProject` wajib memuat repository GitHub public/private, full `commit_sha`, generated domain, internal port, builder, environment map, dan optional resource request. Private repository memakai credential lease sementara; runtime tidak menerima token atau command shell mentah dari payload.

## Contract Lifecycle v1

Command lifecycle memakai `project_id` dan `deployment_id` pada record command;
payload lifecycle v1 adalah object kosong. Docker executor menemukan workload melalui
label ownership yang dipasang saat deploy. Domain dan port untuk revalidasi route
tersimpan pada label managed container, sehingga API tidak mengirim ulang data yang
dapat drift dari deployment aktif.

| Command | Perilaku | Idempotensi |
| --- | --- | --- |
| `RestartProject` | Restart container aktif dengan graceful Docker stop timeout, tunggu readiness, lalu tulis dan validasi ulang route. | Workload yang sudah stopped menghasilkan `runtime_workload_not_running`; tidak membuat deployment baru. |
| `StopProject` | Hapus route, stop lalu remove container. Image deployment tetap dipertahankan. | Bila container sudah tidak ada, route owned Agent tetap dinonaktifkan dan command sukses idempoten. |
| `SleepProject` | Hapus route dan stop container, tetapi tidak menghapus container maupun image. | Container yang sudah stopped tetap sukses; container yang hilang menghasilkan drift/error karena tidak ada artifact untuk di-wake. |
| `WakeProject` | Start kembali container hasil sleep, tunggu readiness, lalu restore route dari label domain/port. | Container yang sudah running hanya merevalidasi readiness dan route. |
| `HealthCheck` | Menginspeksi container managed dan mengembalikan state running, ready, status Docker, serta reason aman. | Read-only. |
| `RefreshRoute` | Menolak container yang tidak ready; kemudian membangun ulang route deterministik dari label domain/port dan menjalankan validate/reload Caddy. | Menulis konten route yang sama secara atomik; aman diulang. |
| `ReconcileWorkload` | Melaporkan desired/actual state. Mutasi hanya dijalankan bila payload memuat aksi `restart_log_follower`, `cleanup_failed_candidate`, atau `restore_route` secara eksplisit. | Tanpa `actions` bersifat read-only; follower didedup per container, route deterministik, dan candidate hanya boleh berstatus Created/Exited/Dead. |
| `CleanupRuntime` | Menjalankan target GC yang disetujui: `stale_workspaces`, `stale_images`, dan/atau `stale_routes`. | Wajib `approved: true`; seluruh discovery/deletion dibatasi ownership Sakala dan aman diulang. |
| `DrainNode` | Mengubah node ke `draining`: workload aktif tetap berjalan, tetapi scheduler tidak mengklaim command workload baru. Setelah command aktif habis, node menjadi `drained`. | Aman diulang. |
| `ResumeNode` | Menjalankan runtime preflight; hanya bila tidak ada kegagalan fatal scheduler kembali menerima command workload. | Aman diulang saat node sudah active. |

`StopProject` dan `SleepProject` sengaja berbeda pada retensi: stop menghapus
container (namun bukan image), sedangkan sleep mempertahankan container supaya wake
tidak membutuhkan checkout atau build. Agent tidak membuat kebijakan retry/restart
otomatis; ia hanya menjalankan intent command yang sudah disetujui control plane.
`CleanupRuntime` menolak payload tanpa approval eksplisit, sementara penghapusan
route memverifikasi marker ownership Sakala beserta deployment ID. Command lama
tidak boleh menghapus route deployment baru pada project yang sama.

Aktivasi route candidate adalah deployment commit point. Cancellation, deadline,
atau kegagalan reporting sebelum cutover membatalkan deployment dan membersihkan
candidate. Setelah cutover berhasil, Agent memberi finalisasi committed deployment
grace maksimum 30 detik. Jika grace habis atau finalisasi gagal, Agent memakai
hasil yang disimpan saat cutover untuk mengirim `Complete`; sisa cleanup menjadi
pekerjaan reconciliation/GC. Kegagalan event `deployment.runtime.ready` menjadi
warning dan tidak boleh mengubah workload yang sudah live menjadi `Failed`.

State drain tidak hanya disimpan di memory process. Saat connected Agent
bootstrap, desired lifecycle dipulihkan dari control plane sebelum polling
command dimulai. Karena itu restart binary tidak boleh mengaktifkan node yang
masih `draining`, `drained`, atau `maintenance`.

`DrainNode` dan `ResumeNode` tidak memerlukan `project_id` atau `deployment_id`.

## Kelas scheduler

`DeployProject` dan lifecycle yang melakukan mutasi workload memakai pool
heavy yang dibatasi `SAKALA_MAX_CONCURRENT_COMMANDS`. `InspectProject`,
`HealthCheck`, `RefreshRoute`, `DrainNode`, dan `ResumeNode` memakai satu slot
lightweight terpisah. Dengan begitu build panjang tidak memblokir health check
atau maintenance read-only pada project lain. Semua command untuk project yang
sama tetap serial; command yang tidak memperoleh slot tetap `Pending` di
control plane untuk poll berikutnya.
Keduanya tetap boleh diproses ketika node sedang draining/drained agar operator
selalu memiliki jalur untuk melanjutkan service tanpa mematikan workload.

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
ReconcileWorkload
CleanupRuntime
DrainNode
ResumeNode
```
