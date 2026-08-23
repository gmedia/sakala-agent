# Changelog

Semua perubahan penting pada project ini akan dicatat di sini. Format mengikuti Keep a Changelog dan release mendatang akan menggunakan Semantic Versioning.

## [Unreleased]

### Added

- Protocol revision 4 dengan bootstrap desired lifecycle node dari control plane sebelum scheduler mengklaim command.
- Dokumentasi matriks operating mode, ownership command melalui atomic claim, dan batas topologi Caddy container/host.
- Docker runtime executor untuk checkout immutable GitHub commit, Buildx image build, candidate container, health check, Caddy route activation, dan cleanup deployment lama.
- Dockerfile-first builder selection dengan Railpack fallback melalui version-pinned BuildKit frontend.
- Command `InspectProject` untuk preview repository melalui `railpack info`, scanner metadata ringan, dan typed completion result.
- Per-line subprocess log streaming, bounded output capture, runtime resource limits, dan temporary mode-0600 environment files.
- Typed deployment resource request, node safety defaults/ceilings, dan requested/applied resource reporting.
- Per-repository task tracking untuk connected agent, real runtime, dan hardening berikutnya.
- Wiremock coverage for connected heartbeat and successful/failed command reporting lifecycles.
- Private repository checkout dengan temporary credential in-memory, credential-free remote URL, dan `GIT_ASKPASS` owner-only.
- Bounded scheduler untuk command lintas project serta batas build image Docker/Railpack.
- Cancellation end-to-end sampai process group, cleanup candidate/workspace, dan graceful shutdown deadline.
- Docker preflight, label workload canonical, heartbeat protocol revision, serta GC workspace UUID yang konservatif.
- Cargo workspace dengan binary agent serta crate protocol, core, dan runtime.
- Safe local mode dengan heartbeat/polling log dan graceful shutdown.
- Control-plane API client skeleton untuk connected mode.
- `NoopRuntimeExecutor` sebagai runtime driver default tanpa host mutation.
- Protocol types, log redaction, integration tests, CI, dan dokumentasi awal.
- Protocol revision 3 untuk recovery log follower, explicit workload reconciliation actions, dan approval-gated Sakala-only runtime cleanup.
- Restart-in-flight serta repeated redeploy soak coverage untuk memory, process cleanup, workspace/container/image/route retention, follower deduplication, dan API retry pacing.

### Changed

- Route Caddy membawa deployment identity, host telemetry berpindah dari core ke runtime adapter, dan snapshot reconciliation heartbeat diberi nama serta timestamp startup yang eksplisit.
- Recovery menoleransi metadata container legacy per workload, stale route hanya mempertahankan workload running, dan semaphore build dilepas sebelum fase start/readiness.
- Heartbeat meng-cache versi dependency dan membatasi durasi probe subprocess/runtime.
- Refactor runtime crate dari flat modules menjadi executor, workspace, builders, containers, routing, health, logs, dan process boundaries dengan dependency injection.
- Pisahkan protocol sebagai DTO-only, core sebagai command lifecycle/application ports, runtime sebagai adapter implementation, dan binary sebagai composition root.
- Pisahkan Caddy file route transaction dari transport reload `docker exec`, sehingga lokasi proses Caddy dapat diganti tanpa mengubah deploy orchestration.
- Align heartbeat, command polling, event, log, and failure payloads with the `sakala-api` resource and persistence conventions.
- Integrasi connected mode diarahkan ke `sakala-api` melalui `SAKALA_API_URL` dan modul `api`.
- Update transitive `quinn-proto` dependency to `0.11.15` to address `RUSTSEC-2026-0185`.
- Update transitive `h2` dependency to `0.4.16` to address `RUSTSEC-2026-0258`.
- Container runtime menyimpan command identity dan bounded-log policy sebagai label agar execution bookkeeping dan follower dapat dipulihkan setelah Agent restart.

### Fixed

- Jadikan route cutover sebagai deployment commit point, lindungi modern route dari stale legacy cleanup, pertahankan partial telemetry, dan laporkan node aktif yang tidak operasional sebagai degraded.
- Tutup race concurrent container admission dengan authoritative pre-run check, lindungi route deployment baru dari lifecycle command lama, dan terima output decimal Docker image prune tanpa mengubah cleanup sukses menjadi gagal.
- Checkout Git mengambil commit sebelum checkout, redeploy menghentikan container lama yang masih running, serta semantik missing workload untuk Stop/Sleep dibedakan.
