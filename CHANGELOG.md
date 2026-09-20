# Changelog

Semua perubahan penting pada project ini akan dicatat di sini. Format mengikuti Keep a Changelog dan release mendatang akan menggunakan Semantic Versioning.

## [Unreleased]

## [0.2.1] - 2026-09-20

### Fixed

- `metadata.startup_reconciliation.captured_at` pada heartbeat kini diserialisasi sebagai string RFC 3339. Sebelumnya nilai `OffsetDateTime` di dalam `json!` metadata terkirim sebagai tuple `[tahun, hari-dalam-tahun, ...]`, sehingga `sakala-api` menolak setiap heartbeat node connected (driver `noop` maupun `docker`) dengan `422` dan node tidak pernah menjadi `ready`. Test heartbeat kini memverifikasi seluruh field `*_at` pada payload yang diserialisasi sebagai string RFC 3339.

## [0.2.0] - 2026-09-20

### Added

- `metadata.detail_counts` pada heartbeat dan batas 50 item untuk collection detail (`unhealthy_details`, `recovered_workloads`, `orphans`, `stale_routes`, `stale_images`, `compatibility_issues`) agar payload tetap di bawah batas 256 KiB API (#48).
- Report event/log dikirim sebagai batch `{ "events": [...] }` / `{ "logs": [...] }` dengan header `Idempotency-Key` per request; log di-buffer dan di-flush per 100 baris/512 KiB/200 ms serta sebelum `complete`/`fail` (#49).
- Retry backoff terbatas untuk report, `complete`, dan `fail` pada kegagalan transport dan `408`/`429`/`5xx`; `RetryPolicy` pada `ApiClient` (#49).
- `stale_routes[].deployment_id` pada heartbeat `startup_reconciliation` (#49).

### Changed

- `ReconcileWorkload` aksi `restart_log_follower` memasang follower di bawah identitas `DeployProject` asli milik workload (label command-id dan log bounds container) melalui `RuntimeReporterFactory`, bukan command reconcile yang memerintahkannya; workload tanpa label command-id ditolak (#49).
- Respons `200` report wajib membawa acknowledgement yang valid dengan `accepted_count` sama dengan jumlah item batch; body yang tidak dapat dibaca di-retry dengan `Idempotency-Key` yang sama, body yang tidak sesuai kontrak atau parsial dianggap tidak terkirim (#49).
- Update transitive `rustls` ke `0.23.45` untuk `RUSTSEC-2026-0285`.
- Respons `409`/`422`/`413` pada report menghentikan delivery log command tersebut tanpa retry sehingga follower berhenti setelah lease expired atau budget log habis; `terminal_at` pada `409` disertakan dalam pesan konflik terminal (#49).
- Body `fail` disanitasi sesuai batas control plane (`error_code` `[A-Za-z0-9._-]` ≤ 64, `error_message` tanpa control/bidi/zero-width ≤ 1000) (#49).
- Dokumentasi `AGENT_API.md`: bagian polling/claim diisi, shape completion `ReconcileWorkload` disamakan dengan kode, semantik lease/offline/pinning/`Claimed -> Running`, log setelah `complete`, dan catatan `NodeStatus::busy`; keputusan adopsi dicatat di `COMPATIBILITY.md` (#49).

## [0.1.0] - 2026-08-24

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

- Propagasikan kegagalan cleanup container deployment sebelumnya setelah ready/follower agar completion meminta deferred control-plane repair.
- Tandai completion deployment yang melewati post-commit finalization grace sebagai `finalization_deferred` agar control plane dapat menghentikan superseded workload secara eksplisit.
- Batasi finalisasi deployment setelah cutover dengan grace 30 detik dan pisahkan cached dependency versions dari live readiness Docker, Caddy, network, serta workspace.
- Jadikan route cutover sebagai deployment commit point, lindungi modern route dari stale legacy cleanup, pertahankan partial telemetry, dan laporkan node aktif yang tidak operasional sebagai degraded.
- Tutup race concurrent container admission dengan authoritative pre-run check, lindungi route deployment baru dari lifecycle command lama, dan terima output decimal Docker image prune tanpa mengubah cleanup sukses menjadi gagal.
- Checkout Git mengambil commit sebelum checkout, redeploy menghentikan container lama yang masih running, serta semantik missing workload untuk Stop/Sleep dibedakan.

[Unreleased]: https://github.com/gmedia/sakala-agent/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/gmedia/sakala-agent/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/gmedia/sakala-agent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/gmedia/sakala-agent/releases/tag/v0.1.0
