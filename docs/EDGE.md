# Sakala Edge: sinkronisasi route

`sakala-edge` adalah service kecil yang menjaga Caddy di host Edge tetap melayani tabel route terbaru dari `sakala-api`, dan tetap melayani tabel terakhir yang baik saat API tidak bisa dihubungi. Sakala Edge terdiri dari Caddy (data plane) dan service ini, sesuai tahap 2 [ADR-019](https://sakala.dev/docs/proyek/adr/): Edge menarik tabel route dengan kredensialnya sendiri, menyimpan salinan terakhir yang baik, dan tabel yang basi tetap melayani, bukan gagal tertutup.

Edge tidak menjalankan kode tenant, tidak membuka port jaringan sendiri, dan tokennya hanya bisa membaca tabel route. Traefik sempat dipertimbangkan, tetapi ditolak karena tidak menyimpan konfigurasi hasil polling ke disk: restart saat API mati berarti semua route hilang.

```txt
start
-> baca routes.json (snapshot), cek ulang, render, validate + reload bila isinya berubah
-> loop:
     GET /api/edge/v1/routes (If-None-Match: "<revision>")
     -> tolak bila tidak valid, simpan error di status.json, backoff
     -> revision sama: tidak menulis apa pun
     -> tulis routes.caddy (tmp + rename) -> caddy validate -> reload
        -> gagal: kembalikan routes.caddy lama, reload lagi
        -> berhasil: tulis routes.json
```

Binary `sakala-edge` dibangun dari workspace yang sama dengan Agent dan memakai ulang aturan domain (`ProjectDomainSuffixes`), process runner, dan backoff milik Agent. Asset rilisnya `sakala-edge-linux-x86_64`, dengan checksum di `SHA256SUMS` yang sama.

## Kontrak dengan API

Edge memanggil `GET {SAKALA_API_URL}/api/edge/v1/routes` dengan header `Authorization: Bearer <SAKALA_EDGE_TOKEN>` dan `Accept: application/json`, ditambah tiga header yang melaporkan keadaannya (lihat [Laporan ke API](#laporan-ke-api)). Sisi API dijelaskan di `docs/EDGE_API.md` milik `sakala-api`; kontrak di bawah adalah yang diharapkan Edge.

```json
{
  "generated_at": "2026-11-18T03:00:00Z",
  "revision": "sha256:9f86d081884c7d65",
  "expected_minimum_routes": 2,
  "routes": [
    {
      "host": "blog.sakala.app",
      "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a",
      "upstreams": ["10.0.47.201:8180", "10.0.47.202:8180"]
    },
    {
      "host": "shop.sakala.app",
      "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b",
      "upstreams": ["10.0.47.201:8180"]
    }
  ]
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `generated_at` | string RFC 3339, wajib | Waktu tabel dibuat. Hanya dicatat di snapshot dan status. |
| `revision` | string, wajib | Identitas isi tabel, sebaiknya hash isi. 1 sampai 128 karakter `A-Z a-z 0-9 . _ : -`. Revision yang sama dianggap tabel yang sama. |
| `expected_minimum_routes` | integer `>= 0`, wajib | Jumlah project yang menurut API sedang dilayani. Tabel dengan route lebih sedikit ditolak. |
| `routes[].host` | string, wajib | Hostname project, tepat satu label di bawah suffix yang diizinkan. |
| `routes[].project_id` | UUID, wajib | Project pemilik route. Ditulis sebagai komentar di file route. |
| `routes[].upstreams` | array string, wajib | Router node yang melayani project, berupa `IP:port` (`10.0.47.201:8180`, `[fd00::1]:8180`), 1 sampai 64 entri. |

Field lain diabaikan, jadi API boleh menambah field tanpa memutus Edge versi lama.
Dua field opsional untuk custom domain, `expected_minimum_custom_routes` dan
`custom_routes`, dijelaskan di [Custom domain](#custom-domain); Edge sebelum fitur itu
mengabaikannya.

### Laporan ke API

Setiap request tabel membawa keadaan Edge, supaya API bisa memberi tahu admin saat sebuah Edge melayani tabel basi atau terus gagal:

| Header | Nilai |
| --- | --- |
| `User-Agent` | `sakala-edge/<versi>`, misalnya `sakala-edge/0.9.1`. |
| `Sakala-Edge-Applied-Revision` | Revision yang sedang dilayani Caddy: tabel terakhir yang berhasil ditulis, divalidasi, dan di-reload, revision baru yang route-nya sama persis dengan yang sudah dilayani (tanpa reload), atau snapshot yang diterima saat start. `none` sebelum ada tabel yang diterapkan, termasuk setelah snapshot ditolak saat start. |
| `Sakala-Edge-Status` | `ok` bila siklus sinkronisasi terakhir berhasil, termasuk `304` dan revision yang sama. Selain itu, kode error siklus terakhir, sama dengan `last_error.code` di `status.json` (lihat [Kapan tabel ditolak](#kapan-tabel-ditolak)): huruf kecil `snake_case`, paling panjang 64 karakter. Bila siklusnya berhasil tetapi bagian custom domain revision yang dilayani ditolak, nilainya `custom_routes_rejected` (lihat [Custom domain](#custom-domain)). |

Tabel yang ditolak atau di-rollback tidak menggeser `Sakala-Edge-Applied-Revision`; yang berubah hanya `Sakala-Edge-Status`. Header dikirim sebelum siklus itu berjalan, jadi hasil sebuah siklus baru terlihat di request berikutnya. Revision yang sama juga dipakai untuk `If-None-Match` di bawah.

### Conditional request

Fitur ini opsional bagi API. Setelah punya tabel, Edge mengirim `If-None-Match: "<revision>"`. API yang mendukungnya menjawab `304 Not Modified` tanpa body bila revision-nya masih sama; API yang tidak mendukung cukup menjawab `200` seperti biasa, dan Edge membandingkan revision sendiri. Jadi ETag yang dipakai API, bila ada, adalah revision dalam tanda kutip.

## Kapan tabel ditolak

Satu pelanggaran membuat seluruh tabel ditolak. Route yang sedang dilayani tidak berubah, `status.json` mencatat kodenya, dan polling berikutnya menunggu lebih lama (backoff).

| Kode di `status.json` | Penyebab |
| --- | --- |
| `api_unreachable` | Koneksi atau timeout (10 detik) gagal. |
| `api_status` | API menjawab selain `200`, atau `304` padahal Edge belum mengirim revision. |
| `body_too_large` | Body lebih dari 8 MiB. |
| `malformed` | Bukan JSON, field wajib hilang, tipe salah, `generated_at` bukan RFC 3339, `project_id` bukan UUID, atau angka negatif. |
| `invalid_revision` | Revision kosong, terlalu panjang, atau memakai karakter di luar daftar. |
| `empty_table` | `routes` kosong, berapa pun `expected_minimum_routes`. |
| `below_expected_minimum` | Jumlah route kurang dari `expected_minimum_routes`. |
| `too_many_routes` | Lebih dari 10.000 route. |
| `host_outside_suffixes` | Host bukan tepat satu label di bawah `SAKALA_EDGE_DOMAIN_SUFFIXES`. Aturannya sama dengan domain deployment Agent: huruf kecil, tanpa wildcard, tanpa port, label `xn--` harus punycode valid. |
| `duplicate_host` | Host yang sama muncul dua kali. |
| `no_upstreams` / `too_many_upstreams` | Route tanpa upstream, atau lebih dari 64. |
| `invalid_upstream` | Upstream bukan `IP:port` dalam bentuk kanonis dengan port bukan nol. Hostname ditolak supaya routing Edge tidak bergantung pada DNS. |
| `upstream_not_allowed` | IP upstream di luar `SAKALA_EDGE_UPSTREAM_CIDRS`. |
| `duplicate_upstream` | Upstream yang sama muncul dua kali dalam satu route. |
| `caddy_refused` | `caddy validate` atau reload gagal. File route lama dikembalikan. |
| `caddy_unavailable` | Command validate atau reload tidak bisa dijalankan sama sekali. |
| `storage` | File di state directory tidak bisa ditulis. |

Tabel kosong selalu ditolak karena tabel kosong dari API yang sedang bermasalah tidak bisa dibedakan dari tabel kosong yang sah. Akibatnya, setelah project terakhir dihapus, Edge tetap meneruskan host itu ke node lama, dan router node menjawab 404.

Karena semua nilai dicek sebelum dirender, tidak ada nilai dari API yang bisa membawa spasi, `{`, `}`, tanda kutip, atau baris baru ke Caddyfile.

## File yang dihasilkan

Edge hanya menulis di `SAKALA_EDGE_STATE_DIR` (default `/var/lib/sakala-edge`). Setiap file ditulis ke `<nama>.tmp`, di-`fsync`, lalu di-rename, sehingga Caddy, restart, atau probe monitoring tidak pernah membaca file setengah jadi.

| File | Isi |
| --- | --- |
| `routes.caddy` | Potongan Caddyfile yang diimport Caddyfile host. |
| `custom-domains.caddy` | Satu site per custom domain, diimport di tingkat atas Caddyfile host. Path-nya bisa diatur dengan `SAKALA_EDGE_CUSTOM_DOMAINS_FILE`. |
| `routes.json` | Tabel terakhir yang diterima Caddy, dalam format API. Dibaca saat start. |
| `status.json` | Status sinkronisasi untuk monitoring. |

Contoh `routes.caddy` untuk dua project, salah satunya dilayani dua node:

```caddyfile
# Managed by sakala-edge. Do not edit: the next sync replaces this file.
# Routes: 2

# Project 0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a
@sakala_edge_blog_sakala_app host blog.sakala.app
handle @sakala_edge_blog_sakala_app {
	reverse_proxy 10.0.47.201:8180 10.0.47.202:8180 {
		lb_policy least_conn
		lb_try_duration 5s
		lb_try_interval 250ms
		fail_duration 10s
	}
}

# Project 0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b
@sakala_edge_shop_sakala_app host shop.sakala.app
handle @sakala_edge_shop_sakala_app {
	reverse_proxy 10.0.47.201:8180
}
```

Setiap project menjadi satu matcher `host` dan satu blok `handle`, diurutkan menurut host. Dengan lebih dari satu node, Caddy memilih node dengan request aktif paling sedikit, mencoba node lain selama 5 detik bila satu node tidak menjawab, dan tidak mengirim request ke node yang gagal selama 10 detik (passive health check). Nilainya sama dengan route replika di router node; alasannya ada di [Caddy Integration](CADDY_INTEGRATION.md). Dengan satu node, opsi itu tidak ditulis, karena menandai satu-satunya node sebagai mati hanya mengubah satu request gagal menjadi 503 selama 10 detik. Caddy meneruskan header `Host` apa adanya, dan itu yang dipakai router node untuk memilih project. Karena Caddy host tidak mempercayai pengunjung sebagai proxy, ia menimpa `X-Forwarded-For`, `X-Forwarded-Proto`, dan `X-Forwarded-Host` dengan yang ia lihat sendiri (alamat pengunjung, `https`, host). Router node mempercayai header itu hanya dari alamat asal Edge yang terdaftar di `SAKALA_RUNTIME_TRUSTED_PROXIES` di node tersebut; setiap host Edge baru harus ditambahkan di sana. Lihat [Alamat klien dan header forwarded](CADDY_INTEGRATION.md#alamat-klien-dan-header-forwarded).

Revision dan `generated_at` sengaja tidak ditulis ke `routes.caddy`. Dengan begitu, revision baru yang isinya sama tidak memicu reload Caddy; Edge hanya memperbarui `routes.json` dan `status.json`.

## Caddyfile host

TLS, listener, dan jawaban untuk host yang tidak dikenal tetap di konfigurasi statis. File yang dihasilkan diimport di dalam site wildcard. Contoh lengkap ada di [`examples/edge/Caddyfile.example`](../examples/edge/Caddyfile.example):

```caddyfile
*.sakala.app {
	tls {
		dns cloudflare {env.CF_TOKEN}
	}
	import /var/lib/sakala-edge/routes.caddy
	handle {
		respond "Not found" 404
	}
}
```

Blok `handle` tanpa matcher selalu diurutkan Caddy paling akhir, jadi host yang tidak ada di tabel mendapat 404. File yang sama boleh diimport di beberapa site wildcard, misalnya `*.sakala.app` dan `*.staging.sakala.app`; matcher host yang tidak cocok dengan site itu tidak pernah terpicu.

Buat `routes.caddy` kosong saat instalasi (`install -m 0644 /dev/null /var/lib/sakala-edge/routes.caddy`), karena Caddy menolak `import` ke file yang belum ada dan Caddy bisa start sebelum Edge. Bila Caddy berjalan di container, mount direktorinya, bukan filenya, supaya rename terlihat di dalam container.

## Validate dan reload

Edge menjalankan dua command tanpa shell, dipisah per spasi. Tanda kutip dan backslash ditolak saat start.

- `SAKALA_EDGE_CADDY_VALIDATE_CMD`, default `caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile`;
- `SAKALA_EDGE_CADDY_RELOAD_CMD`, default `systemctl reload caddy`.

Command berjalan dengan environment terbatas yang sama dengan child process Agent: `PATH`, `HOME`, locale, dan beberapa variabel proxy serta TLS. Variabel `SAKALA_*`, termasuk token Edge, tidak pernah diteruskan.

Default itu cocok untuk Caddy distribusi tanpa modul DNS. Host seperti di `sakala-deployment`, yang menjalankan `/usr/local/bin/caddy-cloudflare` dan membaca `CF_TOKEN` dari EnvironmentFile milik `caddy.service`, perlu dua penyesuaian. Keduanya sudah dicoba dengan Caddy 2.11.3 plus `caddy-dns/cloudflare`:

1. `caddy validate` dari Caddy distribusi gagal dengan `module not registered: dns.providers.cloudflare`. Arahkan validate ke binary yang sama dengan service.
2. Validate dengan `CF_TOKEN` kosong gagal dengan `API token '' appears invalid`, sedangkan token tiruan 40 karakter lolos, karena validate hanya memeriksa format token dan tidak menghubungi Cloudflare. Supaya Edge tidak perlu membaca token asli, beri token tiruan lewat `env`:

```dotenv
SAKALA_EDGE_CADDY_VALIDATE_CMD=env CF_TOKEN=0000000000000000000000000000000000000000 /usr/local/bin/caddy-cloudflare validate --config /etc/caddy/Caddyfile --adapter caddyfile
```

Bentuk persis dengan `env` di atas belum dijalankan di host sungguhan, jadi cek sekali saat instalasi.

Reload lewat `systemctl reload caddy` menjalankan `ExecReload` di environment `caddy.service` sendiri, sehingga token asli tetap hanya dimiliki Caddy. Jangan ganti reload dengan `caddy reload` yang dijalankan user `sakala-edge` kecuali konfigurasi tidak memakai variabel environment. `caddy reload` sendiri juga menolak konfigurasi yang tidak valid, jadi validate terpisah berguna untuk pesan error yang lebih awal dan jelas, bukan satu-satunya pengaman.

Bila validate atau reload gagal, Edge mengembalikan `routes.caddy` sebelumnya (atau menghapusnya bila sebelumnya belum ada), menjalankan reload sekali lagi, lalu mencatat error. Semantiknya sama dengan route manager Agent. Urutannya: file ditulis dan di-rename dulu, baru divalidasi, karena `caddy validate` memeriksa Caddyfile host yang mengimport `routes.caddy`, bukan file sementara. Selama jeda itu Caddy yang berjalan belum di-reload, jadi trafik tetap memakai konfigurasi lama.

## Konfigurasi

Semua variabel juga bisa diberikan sebagai flag CLI (`--api-url`, `--edge-token`, dan seterusnya). Nilai kosong dianggap tidak diisi. Nilai yang tidak valid menghentikan service saat start dengan pesan yang menyebut nama variabelnya.

| Variabel | Default | Arti |
| --- | --- | --- |
| `SAKALA_API_URL` | wajib | URL dasar `sakala-api`, `http` atau `https`, tanpa kredensial, query, atau fragment. |
| `SAKALA_EDGE_TOKEN` | wajib | Token Edge dari API. `change-me` ditolak. |
| `SAKALA_EDGE_DOMAIN_SUFFIXES` | `run.sakala.localhost,staging.sakala.app,sakala.app` | Suffix host yang diizinkan, aturan sama dengan `SAKALA_PROJECT_DOMAIN_SUFFIXES` Agent. |
| `SAKALA_EDGE_UPSTREAM_CIDRS` | `10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,fc00::/7` | Jaringan tempat IP upstream harus berada. Network address dengan host bit terisi (`10.0.0.1/8`) dan `/0` ditolak. Persempit ke subnet privat node, misalnya subnet WireGuard. |
| `SAKALA_EDGE_POLL_INTERVAL_SECONDS` | `5` | Jeda polling saat semuanya berhasil. |
| `SAKALA_EDGE_MAX_BACKOFF_SECONDS` | `60` | Jeda terpanjang setelah gagal berturut-turut. Jeda berlipat dua setiap kegagalan: 10, 20, 40, lalu 60 detik dengan default. Harus minimal sama dengan poll interval. |
| `SAKALA_EDGE_STATE_DIR` | `/var/lib/sakala-edge` | Lokasi `routes.caddy`, `routes.json`, dan `status.json`. |
| `SAKALA_EDGE_CUSTOM_DOMAINS_FILE` | `<SAKALA_EDGE_STATE_DIR>/custom-domains.caddy` | File site custom domain. Ditulis lewat `<path>.tmp` lalu rename, jadi direktorinya harus bisa ditulis user `sakala-edge`. |
| `SAKALA_EDGE_CUSTOM_DOMAIN_FORBIDDEN_ZONES` | `sakala.app,sakala.dev,localhost` | Zona yang tidak boleh menjadi custom domain, dipisah koma; nama di bawahnya juga ditolak. Suffix di `SAKALA_EDGE_DOMAIN_SUFFIXES` selalu ditolak. Aturan sama dengan `SAKALA_CUSTOM_DOMAIN_FORBIDDEN_ZONES` Agent. |
| `SAKALA_EDGE_CADDY_VALIDATE_CMD` | `caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile` | Lihat [Validate dan reload](#validate-dan-reload). |
| `SAKALA_EDGE_CADDY_RELOAD_CMD` | `systemctl reload caddy` | Lihat [Validate dan reload](#validate-dan-reload). |
| `SAKALA_EDGE_LOG_LEVEL` | `info` | Filter `tracing`, log berformat JSON. |

Edge tidak membaca file `.env` sendiri; systemd memberikannya lewat `EnvironmentFile`. Contoh ada di [`examples/edge/sakala-edge.env.example`](../examples/edge/sakala-edge.env.example).

## Snapshot dan restart

Saat start, sebelum polling pertama, Edge membaca `routes.json`, memeriksanya dengan aturan yang sama seperti respons API, lalu merender ulang. Bila hasilnya sama dengan `routes.caddy` di disk, Caddy tidak disentuh. Bila berbeda, misalnya file route hilang atau format render berubah setelah upgrade, Edge memvalidasi dan me-reload seperti biasa. Jadi restart Edge saat API mati tetap melayani semua route.

Snapshot yang tidak lolos aturan, misalnya setelah suffix atau CIDR dipersempit, ditolak dengan peringatan, dan `routes.caddy` dibiarkan seperti adanya sampai API memberi tabel yang valid.

## Monitoring

Edge tidak membuka port. Kondisinya dibaca dari `status.json`, yang ditulis setelah setiap percobaan:

```json
{
  "revision": "sha256:9f86d081884c7d65",
  "route_count": 2,
  "custom_route_count": 1,
  "custom_routes_error": null,
  "generated_at": "2026-11-18T03:00:00Z",
  "source": "api",
  "last_attempt_at": "2026-11-18T03:00:05.12Z",
  "last_success_at": "2026-11-18T03:00:05.12Z",
  "consecutive_failures": 0,
  "last_error": null
}
```

`source` bernilai `snapshot` setelah start dari `routes.json` sampai API berhasil dihubungi, lalu `api`. Edge yang sehat punya `last_success_at` yang baru (beberapa kali poll interval). Tabel basi terlihat sebagai `last_success_at` yang makin lama bersama `last_error` berisi kode dari tabel di atas. Waktu ditulis dalam UTC.

API juga memantau setiap Edge dari [laporan di setiap request](#laporan-ke-api): admin mendapat peringatan bila sebuah Edge basi (revision yang dilayaninya tertinggal) atau terus gagal. Aturan dan ambangnya ada di `docs/EDGE_API.md` milik `sakala-api`. Edge yang sama sekali tidak bisa menghubungi API tidak mengirim laporan apa pun, jadi detail penyebabnya tetap hanya ada di `status.json` di host.

## Custom domain

Custom domain project (ADR-024) datang di array terpisah, karena Edge versi lama
menolak seluruh tabel bila satu host di `routes` berada di luar suffix. API hanya
memasukkan domain yang sudah `verified` dari project yang tidak dihapus. Contoh tabel
lengkap ada di [`examples/edge/route-table-custom-domains.json`](../examples/edge/route-table-custom-domains.json):

```json
{
  "expected_minimum_custom_routes": 2,
  "custom_routes": [
    {
      "host": "www.contoh.com",
      "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a",
      "upstreams": ["10.0.47.201:8180"]
    },
    {
      "host": "contoh.net",
      "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b",
      "upstreams": []
    }
  ]
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `expected_minimum_custom_routes` | integer `>= 0`, opsional | Jumlah custom domain yang menurut API sedang dilayani. Tidak ada atau `null` berarti 0. |
| `custom_routes` | array, opsional | Tidak ada, `null`, atau `[]` berarti tidak ada custom domain. Berbeda dengan `routes`, array kosong diterima, asal `expected_minimum_custom_routes` 0. |
| `custom_routes[].host` | string, wajib | Custom domain dalam bentuk akhirnya: huruf kecil, minimal dua label, paling panjang 253 karakter, label `xn--` harus punycode valid, bukan alamat IP, bukan di bawah `SAKALA_EDGE_DOMAIN_SUFFIXES` atau `SAKALA_EDGE_CUSTOM_DOMAIN_FORBIDDEN_ZONES`. Aturannya sama dengan Agent. |
| `custom_routes[].project_id` | UUID, wajib | Ditulis sebagai komentar. |
| `custom_routes[].upstreams` | array string, wajib | Sama dengan `routes[].upstreams`, tetapi boleh kosong: project tanpa workload. Site-nya menjawab 404 dengan teks "Belum ada aplikasi di alamat ini", jadi sertifikat tetap terbit dan diperpanjang dan pengunjung tidak melihat error TLS. |

Bagian custom diperiksa terpisah dari `routes`. Satu pelanggaran menolak seluruh bagian
custom, tetapi route project dari tabel yang sama tetap diterapkan, dan custom domain
yang sedang dilayani tetap dilayani dari salinan terakhir yang baik. Bentuk yang salah
di bagian custom (misalnya `custom_routes` bukan array) juga hanya menolak bagian
custom, bukan seluruh tabel. Kode status-nya satu, `custom_routes_rejected`, dengan
alasan di `custom_routes_error.reason` di `status.json`:

| `reason` | Penyebab |
| --- | --- |
| `malformed` | Bagian custom tidak sesuai bentuk: bukan array, field wajib hilang, `project_id` bukan UUID, angka minimum negatif. |
| `below_expected_minimum` | Jumlah custom domain kurang dari `expected_minimum_custom_routes`. |
| `too_many_routes` | Lebih dari 10.000 custom domain. |
| `invalid_host` | Host melanggar aturan nama di atas. |
| `duplicate_host` | Host yang sama muncul dua kali. |
| `too_many_upstreams`, `invalid_upstream`, `upstream_not_allowed`, `duplicate_upstream` | Sama dengan aturan upstream `routes`. |
| `caddy_refused` | Caddy menolak site custom yang dirender. Edge mengembalikan file custom sebelumnya dan, bila `routes.caddy` juga berubah, memvalidasi dan me-reload sekali lagi dengan route project baru dan site custom lama, jadi site custom yang ditolak Caddy tidak pernah menahan route project. |

Penolakan itu menempel pada revision yang dilayani: `304` atau revision yang sama
tetap melaporkan `Sakala-Edge-Status: custom_routes_rejected` sampai API menerbitkan
revision baru. `last_error` tetap untuk kegagalan siklus. Snapshot `routes.json`
menyimpan bagian custom yang benar-benar dilayani, bukan yang ditolak. Penolakan tidak
ikut tersimpan di disk: setelah Edge restart, ia baru dilaporkan lagi saat API
menerbitkan revision baru.

### File yang dihasilkan

```caddyfile
# Managed by sakala-edge. Do not edit: the next sync replaces this file.
# Custom domains: 2

# Project 0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b
contoh.net {
	import sakala_custom_domain_tls
	header {
		Cache-Control "no-store"
		X-Robots-Tag "noindex"
	}
	respond "Belum ada aplikasi di alamat ini. Kalau ini domain proyekmu, cek status deployment-nya di Console Sakala." 404
}

# Project 0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a
www.contoh.com {
	import sakala_custom_domain_tls
	reverse_proxy 10.0.47.201:8180
}
```

Setiap custom domain menjadi satu site tingkat atas, diurutkan menurut host, sehingga
Caddy mengelola sertifikatnya sendiri: diterbitkan begitu site muncul dan berhenti
diperpanjang begitu site hilang. Tidak ada on-demand TLS dan tidak ada endpoint `ask`.
Opsi load balancing untuk lebih dari satu node sama dengan route project. Site tidak
mempercayai proxy apa pun, jadi Caddy menimpa `X-Forwarded-For`, `-Proto`, dan
`-Host`, dan `Host` asli diteruskan ke router node, yang mengenal custom domain itu
dari route project (lihat [Caddy Integration](CADDY_INTEGRATION.md#custom-domain)).
File ini selalu ditulis, juga saat tidak ada custom domain, jadi import-nya tidak
pernah menunjuk file yang hilang.

### Caddyfile host

Edge tidak menulis apa pun tentang penerbit sertifikat. Caddyfile host wajib
mendefinisikan snippet `sakala_custom_domain_tls` sebelum mengimport file ini, di
tingkat atas, bukan di dalam site wildcard. Di `sakala-deployment` snippet itu ada di
`caddy/sakala.custom-domains.<env>.Caddyfile`: Let's Encrypt saja, lewat HTTP-01 atau
TLS-ALPN-01, dengan email operator dari `ACME_EMAIL`. Contoh ada di
[`examples/edge/custom-domains.Caddyfile.example`](../examples/edge/custom-domains.Caddyfile.example):

```caddyfile
(sakala_custom_domain_tls) {
	tls {
		issuer acme {
			dir https://acme-v02.api.letsencrypt.org/directory
			email {$ACME_EMAIL}
		}
	}
}

import /var/lib/sakala-edge/custom-domains.caddy
```

Opsi global `acme_dns` tidak boleh ada di Caddy host: Caddy menyalin penyedia DNS
global ke setiap issuer ACME, sehingga sertifikat custom domain akan dicoba lewat
DNS-01 di zona yang bukan milik Sakala dan selalu gagal. Karena snippet memakai
`{$ACME_EMAIL}`, command validate Edge perlu nilai tiruan, sama seperti `CF_TOKEN`,
misalnya `env CF_TOKEN=0000000000000000000000000000000000000000 ACME_EMAIL=validate@example.invalid /usr/local/bin/caddy-cloudflare validate ...`.

Rollback `sakala-edge` ke versi tanpa custom domain tidak mengosongkan file ini, jadi
site lama tetap dilayani dan tidak pernah berubah. Tulis file kosong lalu reload Caddy
sebelum rollback.

## Instalasi dengan systemd

Repository deployment yang akan memasang Edge. Bahan contohnya ada di [`examples/edge/`](../examples/edge/):

- `sakala-edge.service`: unit dengan user `sakala-edge`, `StateDirectory=sakala-edge` (mode `0755` supaya Caddy bisa membaca), dan hardening systemd. Service berhenti dengan SIGTERM; fetch yang sedang berjalan dibatalkan, tetapi penulisan dan rollback yang sedang berjalan diselesaikan dulu.
- `sakala-edge.polkit.rules`: mengizinkan user `sakala-edge` menjalankan `systemctl reload caddy`, tidak ada unit atau aksi lain.
- `sakala-edge.env.example`: environment file, dipasang `root:sakala-edge 0640` karena memuat token.
- `Caddyfile.example`: Caddyfile host.

Unit dan aturan polkit itu belum dicoba di host sungguhan.

## Pengujian

```bash
cargo test -p sakala-edge
cargo test -p sakala-edge --test edge_caddy_smoke_test -- --ignored --nocapture
```

Test kedua opt-in karena butuh Docker dan `caddy:2.11.3-alpine`. Test pertamanya menjalankan Caddy sungguhan dengan Caddyfile statis yang mengimport file hasil Edge (satu site HTTP dan satu site `tls internal`), memvalidasi dan me-reload lewat `docker exec`, lalu memastikan request terbagi ke dua upstream, host tanpa route mendapat 404, dan mematikan satu upstream tidak menggagalkan request berikutnya. Test keduanya mendefinisikan snippet `sakala_custom_domain_tls` dengan `tls internal`, menerapkan tabel dengan dua custom domain, lalu memastikan lewat HTTPS dengan SNI masing-masing bahwa domain dengan upstream sampai ke aplikasi dengan `Host` asli, `X-Forwarded-Proto: https`, dan `X-Forwarded-For` buatan pengunjung ditimpa, domain tanpa upstream menjawab 404, HTTP diarahkan ke HTTPS, dan route project tetap jalan.
