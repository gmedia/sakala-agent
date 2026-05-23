# Contributing to Sakala Agent

Sakala Agent adalah komponen runtime privileged. Kontribusi harus kecil, dapat diuji, dan mempertahankan boundary keamanan antara dashboard, agent, dan host runtime.

## Local Development

```bash
cargo build --workspace
cargo run -p sakala-agent
```

Agent berjalan dalam `local` mode secara default. Gunakan `Ctrl+C` untuk menghentikan worker secara graceful.

## Crate Boundaries

- `sakala-agent-protocol`: hanya tipe kontrak yang dapat diserialisasi.
- `sakala-agent-runtime`: trait dan implementasi operasi runtime.
- `sakala-agent-core`: HTTP client, workers, lifecycle, dan log safety.
- `apps/sakala-agent`: startup/config/telemetry/shutdown.

Jangan meletakkan operasi host di protocol atau binary wiring. Jangan menambahkan crate baru untuk logic yang masih sesuai boundary di atas.

## Dependencies

Dependency baru harus ditambahkan melalui Cargo:

```bash
cargo add <crate>@<version> -p <package>
```

Setelah ditambahkan, dependency bersama dapat diubah menjadi inheritance dari `[workspace.dependencies]`.

## Quality Checks

```bash
make fmt
make lint
make test
make build
```

Perubahan terhadap auth, lifecycle, payload protocol, atau redaction harus memiliki test.

## Conventional Commits

```txt
<type>[optional scope]: <description>
```

Contoh:

```txt
chore(agent): initialize Rust workspace foundation
feat(core): poll dashboard commands
test(logs): cover secret redaction formats
docs(api): document command completion endpoint
```

Tipe yang digunakan:

```txt
feat fix docs style refactor perf test build ci chore revert
```

## Pull Request Checklist

- [ ] Crate boundary tetap jelas.
- [ ] Dependency baru ditambahkan melalui `cargo add`.
- [ ] Tidak ada token, secret, atau credential yang tercetak/ter-commit.
- [ ] Tidak ada Docker socket atau eksekusi runtime berisiko tanpa review.
- [ ] Tests dan docs diperbarui untuk perubahan contract.
- [ ] Quality checks relevan sudah berjalan.
