## Ringkasan

Jelaskan perubahan utama pada pull request ini.

## Jenis Perubahan

- [ ] `feat` - kemampuan baru
- [ ] `fix` - perbaikan bug
- [ ] `docs` - dokumentasi
- [ ] `refactor` - perubahan struktur tanpa mengubah behavior
- [ ] `test` - pengujian
- [ ] `build` - dependency/build system
- [ ] `ci` - automation
- [ ] `chore` - maintenance

## Area yang Terdampak

- [ ] Protocol / API contract
- [ ] Core workers / API client
- [ ] Runtime executor
- [ ] Configuration / telemetry
- [ ] Logs / redaction
- [ ] Security boundary
- [ ] Documentation
- [ ] CI / build

## Cara Menguji

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features
```

## Checklist

- [ ] Commit mengikuti Conventional Commits (`type(scope): message`).
- [ ] Dependency baru ditambahkan melalui `cargo add`.
- [ ] Tidak ada token, credential, atau secret di source/log/fixture.
- [ ] Perubahan tidak mengekspos Docker socket atau command server inbound.
- [ ] Crate boundary tetap sesuai arsitektur.
- [ ] Tests ditambahkan/diperbarui bila behavior berubah.
- [ ] Dokumentasi diperbarui bila protocol, config, runtime, atau safety boundary berubah.

## Related Issue

Closes #
