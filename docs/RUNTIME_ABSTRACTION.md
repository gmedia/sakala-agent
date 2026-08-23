# Runtime Abstraction

Crate `sakala-agent-runtime` memisahkan orchestrator agent dari operasi host.

## Trait

`RuntimeExecutor` dan `RuntimeReporter` didefinisikan di `sakala-agent-core`. Runtime menerima `AgentCommand`, menjalankan implementasi host, lalu memakai reporter untuk mengirim event/log. Core tetap menangani API client, command lifecycle, dan redaksi.

Boundary ini memungkinkan:

- lifecycle worker diuji tanpa Docker;
- runtime implementation diganti tanpa mengubah protocol;
- validasi keamanan dilakukan sebelum executor privileged aktif.

## Noop Runtime

`NoopRuntimeExecutor` adalah implementasi aktif pada foundation:

- menerima semua command protocol;
- menulis tracing metadata command;
- menghasilkan satu event sukses dan satu system log;
- tidak melakukan perubahan host atau network.

## Docker Implementation

`DockerRuntimeExecutor` adalah application service untuk inspection dan deploy runtime. Ia tidak membentuk command Git, Docker, Railpack, atau Caddy secara langsung. Dependency-nya diinjeksi melalui port internal runtime:

| Port | Implementasi Phase 8 | Tanggung jawab |
| --- | --- | --- |
| `WorkspaceManager` | `GitWorkspaceManager` | Checkout immutable commit dan cleanup workspace. |
| `ProjectInspector` | `RailpackProjectInspector` | Jalankan `railpack info` dan scanner metadata ringan. |
| `ImageBuilder` | `ImageBuildService` | Pilih Dockerfile/Railpack dan bangun image. |
| `ContainerEngine` | `DockerContainerEngine` | Start, logs, replacement, dan failure cleanup. |
| `HealthChecker` | `DockerHealthChecker` | Menunggu status running/healthy. |
| `RouteManager` | `CaddyFileRouteManager` | Tulis route atomik, activate, dan rollback. |

Semua adapter command memakai injectable `ProcessRunner`, sehingga test dapat memverifikasi behavior tanpa daemon. `CaddyFileRouteManager` menerima `CaddyReloader`; Phase 8 memakai `DockerExecCaddyReloader`, tetapi route manager tidak bergantung pada lokasi proses Caddy.

Port application-level `RuntimeExecutor` dan `RuntimeReporter` berada di core. Port teknologi seperti `ImageBuilder`, `ContainerEngine`, dan `RouteManager` tetap internal di runtime. Dependency berjalan satu arah: runtime mengimplementasikan port core, sedangkan core tidak bergantung pada runtime. Binary menjadi composition root yang menginjeksi implementasi terpilih.

Host telemetry mengikuti boundary yang sama. Heartbeat core hanya memanggil
`RuntimeExecutor::node_telemetry`; pembacaan `/proc` serta eksekusi Git, Docker,
Buildx, Railpack, `df`, dan `du` berada di adapter runtime dan menggunakan
process runner dengan deadline serta child-process cleanup. Metadata versi
dependency di-cache, sedangkan readiness Docker daemon, Caddy, network runtime,
dan workspace diperiksa ulang pada setiap snapshot.

`InspectProject` dan `DeployProject` aktif pada Docker executor. Sleep/wake dan command runtime lain belum diaktifkan.

## Abstraction Policy

- Tambahkan trait ketika ada boundary privilege, kebutuhan test substitution, atau variasi implementasi yang jelas.
- Hindari generic type graph dan crate baru sebelum ada kebutuhan ownership yang nyata.
- Executor mengatur urutan use case; adapter mengatur detail teknologi.
- Protocol tetap hanya DTO/serde contract dan tidak mengenal port runtime.
