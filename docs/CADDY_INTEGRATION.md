# Caddy Integration

Agent menulis satu file `<project-id>.Caddyfile` pada `SAKALA_CADDY_SITES_DIR`. Domain hanya diterima bila cocok dengan `*.run.sakala.localhost` atau `*.run.sakala.dev`; upstream selalu candidate container terkelola dan port tervalidasi.

```txt
write temporary file
-> atomic rename
-> docker exec caddy validate
-> docker exec caddy reload
-> restore route lama bila reload gagal
```

Contract `sakala-infra` memasang folder route read-only ke `/etc/caddy/sites`, mengimport `*.Caddyfile`, dan membatasi Admin API ke loopback container. Port admin tidak dipublish. Docker socket tidak pernah diberikan ke Caddy.

TLS production, concurrent per-project lease, dan distributed route storage belum termasuk MVP.
