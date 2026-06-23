# Changelog

Semua perubahan penting pada project ini akan dicatat di sini. Format mengikuti Keep a Changelog dan release mendatang akan menggunakan Semantic Versioning.

## [Unreleased]

### Added

- Dokumentasi matriks operating mode, ownership command melalui atomic claim, dan batas topologi Caddy container/host.
- Docker runtime executor untuk checkout immutable GitHub commit, Buildx image build, candidate container, health check, Caddy route activation, dan cleanup deployment lama.
- Dockerfile-first builder selection dengan Railpack fallback melalui version-pinned BuildKit frontend.
- Command `InspectProject` untuk preview repository melalui `railpack info`, scanner metadata ringan, dan typed completion result.
- Per-line subprocess log streaming, bounded output capture, runtime resource limits, dan temporary mode-0600 environment files.
- Typed deployment resource request, node safety defaults/ceilings, dan requested/applied resource reporting.
- Per-repository task tracking untuk connected agent, real runtime, dan hardening berikutnya.
- Wiremock coverage for connected heartbeat and successful/failed command reporting lifecycles.
- Cargo workspace dengan binary agent serta crate protocol, core, dan runtime.
- Safe local mode dengan heartbeat/polling log dan graceful shutdown.
- Control-plane API client skeleton untuk connected mode.
- `NoopRuntimeExecutor` sebagai runtime driver default tanpa host mutation.
- Protocol types, log redaction, integration tests, CI, dan dokumentasi awal.

### Changed

- Refactor runtime crate dari flat modules menjadi executor, workspace, builders, containers, routing, health, logs, dan process boundaries dengan dependency injection.
- Pisahkan protocol sebagai DTO-only, core sebagai command lifecycle/application ports, runtime sebagai adapter implementation, dan binary sebagai composition root.
- Pisahkan Caddy file route transaction dari transport reload `docker exec`, sehingga lokasi proses Caddy dapat diganti tanpa mengubah deploy orchestration.
- Align heartbeat, command polling, event, log, and failure payloads with the `sakala-api` resource and persistence conventions.
- Integrasi connected mode diarahkan ke `sakala-api` melalui `SAKALA_API_URL` dan modul `api`.
- Update transitive `quinn-proto` dependency to `0.11.15` to address `RUSTSEC-2026-0185`.
