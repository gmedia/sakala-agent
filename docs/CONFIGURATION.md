# Configuration

Agent memuat konfigurasi dari environment variable atau CLI flag yang sepadan. CLI flag mengoverride environment value.

| Variable | Default | Keterangan |
| --- | --- | --- |
| `SAKALA_AGENT_MODE` | `local` | `local` atau `connected`. |
| `SAKALA_AGENT_ID` | `local-agent-01` | Identitas node yang dikirim sebagai `X-Agent-Id`. |
| `SAKALA_AGENT_TOKEN` | tidak ada | Bearer token; wajib dan bukan `change-me` pada connected mode. |
| `SAKALA_API_URL` | `http://localhost:8000` | Base URL `sakala-api` control plane. |
| `SAKALA_POLL_INTERVAL_SECONDS` | `3` | Interval polling command, harus lebih besar dari nol. |
| `SAKALA_HEARTBEAT_INTERVAL_SECONDS` | `10` | Interval heartbeat, harus lebih besar dari nol. |
| `SAKALA_RUNTIME_NETWORK` | `sakala-runtime` | Nama network referensi dari `sakala-infra`. |
| `SAKALA_LOG_LEVEL` | `info` | Filter `tracing-subscriber`, misalnya `debug` atau `sakala_agent=debug`. |

## Local Mode

```bash
cp examples/env/local.env.example .env
cargo run -p sakala-agent
```

File `.env` tidak dimuat otomatis oleh binary. Export variable menggunakan shell/environment runner yang dipilih, atau jalankan dengan default local mode tanpa file. Tidak ada control-plane request pada mode ini.

## Connected Mode

```bash
SAKALA_AGENT_MODE=connected \
SAKALA_AGENT_ID=node-01 \
SAKALA_AGENT_TOKEN='<issued-agent-token>' \
SAKALA_API_URL=http://localhost:8000 \
cargo run -p sakala-agent
```

Jangan menyimpan token issued pada repository atau command history yang dibagikan.
