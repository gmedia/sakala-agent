# AGENTS.md - Sakala Agent

Dokumen ini berlaku bagi AI agents, Codex CLI, automation tools, dan contributor yang bekerja pada `sakala-agent`.

## Identity

- Project: **Sakala Agent**
- Product: **Sakala**
- Project stewardship: **Sakala Maintainers**
- Founding sponsor: **PT Media Sarana Data / GMEDIA**
- License: **Apache License 2.0**
- Documentation language: **Bahasa Indonesia**
- Rust identifiers and API field names: **English**

Do not frame Sakala as a closed GMEDIA-owned product. Use this framing instead: Sakala is an open-source project maintained by Sakala maintainers and supported by GMEDIA as founding sponsor.

## Architecture Rules

- `sakala-api` adalah control plane; `sakala-console` adalah frontend; agent adalah runtime executor.
- Agent melakukan polling command dan report outbound ke `sakala-api`.
- Jangan membuat direct API-to-agent command server.
- Jangan memberi `sakala-api` atau `sakala-console` akses Docker socket.
- Docker/Railpack/Caddy hanya boleh diubah dalam `sakala-agent-runtime`; pertahankan validasi payload, process arguments terstruktur, resource limits, dan default executor `noop`.
- `DockerRuntimeExecutor` hanya mengorkestrasi `WorkspaceManager`, `ImageBuilder`, `ContainerEngine`, `HealthChecker`, dan `RouteManager`; jangan masukkan command teknologi langsung kembali ke executor.
- Implementasi route dan transport reload harus terpisah. Caddy file ownership tidak boleh mengasumsikan Caddy selalu berjalan sebagai container.
- Jangan memperkenalkan Kubernetes, containerd, Firecracker, atau microVM pada fase foundation.
- Pertahankan tiga workspace crate yang sudah ada kecuali keputusan arsitektur menyatakan lain.

## Dependency Rules

- Tambahkan Rust dependency menggunakan `cargo add`, bukan menulis package baru secara manual di manifest.
- Setelah dependency ditambahkan, shared version boleh dipusatkan di `[workspace.dependencies]`.
- Jangan menambah dependency bila standard library atau dependency yang sudah ada mencukupi.
- Jangan menambahkan Docker client crate tanpa kebutuhan yang terukur; Phase 8 menggunakan Docker CLI agar surface dependency tetap kecil.

## Code Rules

- Jagalah crate boundary: protocol tidak bergantung pada core/runtime; core bergantung pada protocol dan memiliki application ports; runtime bergantung pada core/protocol untuk mengimplementasikan ports; app adalah composition root. Jangan membuat dependency core ke runtime.
- Tambahkan adapter baru di belakang port runtime yang relevan. Jangan membuat trait tanpa boundary privilege, kebutuhan substitution test, atau variasi implementasi yang terukur.
- Jangan mencatat token atau secret.
- Semua log runtime yang dilaporkan harus melalui redaction boundary.
- Gunakan async worker yang dapat dihentikan dengan graceful shutdown.
- Default mode harus tetap `local` dan bebas network call ke control plane.
- Deploy runtime wajib memakai immutable full commit SHA dan repository host allowlist.
- Secret runtime harus masuk melalui env file berizin terbatas, bukan command argument atau log.
- Railpack CLI dan BuildKit frontend harus dipin ke versi yang kompatibel.
- Gunakan `railpack info` untuk preview repository dan `railpack prepare` hanya untuk deployment. Preview tidak boleh membangun image atau mengubah route.
- Product-level resource policy harus berasal dari `sakala-api`. Agent hanya menegakkan command request, memakai local default saat kosong, dan menolak nilai di atas hard safety maximum node.

## Verification

Jalankan pemeriksaan relevan sebelum finalisasi:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features
```

## Commit Convention

Gunakan Conventional Commits 1.0.0:

```txt
<type>[optional scope]: <description>
```

Scope umum:

```txt
agent protocol core runtime api logs docs ci security
```

Contoh:

```txt
chore(agent): initialize Rust workspace foundation
feat(protocol): add deployment command payload
fix(logs): redact database url output
docs(runtime): define Docker executor boundary
```
