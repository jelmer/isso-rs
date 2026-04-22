# Isso – a commenting server similar to Disqus

Isso – *Ich schrei sonst* – is a lightweight commenting server. This
repository is a Rust reimplementation of the original
[Python project](https://github.com/isso-comments/isso); the server binary
is `isso`, the SQLite database format is unchanged, and the JSON HTTP
API and JavaScript frontend remain compatible with the Python server.

- Crate: [`isso` on crates.io](https://crates.io/crates/isso)
- Repository: [github.com/jelmer/isso-rs](https://github.com/jelmer/isso-rs)

## Features

- **Comments written in Markdown**, with XSS-safe HTML rendering (ammonia-
  based sanitiser).
- **SQLite backend** with the same schema the Python server uses — this
  branch can open a DB written by the Python server, and vice-versa.
- **Disqus / WordPress / generic-JSON importers** (`isso import`).
- **Admin UI** served from the same binary.
- **Configurable JS client** — the `static/js/` tree is unchanged from the
  Python repo and is served by `isso` itself or by any HTTP server you
  prefer.
- **Multi-site** via multiple `-c` flags; each site mounts at its
  `[general] name` slug.

## Getting started

### Via `cargo install` (released version, no checkout required)

```console
$ cargo install isso
```

Drops the binary in `~/.cargo/bin/isso`. The admin UI still needs the
`templates/` and (optionally) `static/` trees — clone the repo or copy
those directories alongside wherever you deploy the binary.

### From git (latest development version)

```console
$ cargo install --git https://github.com/jelmer/isso-rs
```

### From a checkout

```console
$ cargo build --release
```

Produces `./target/release/isso`. Requires a recent stable Rust toolchain
(1.70+). The build links against system SQLite via `sqlx`.

### Configuration

`isso` reads the same `isso.cfg` format as the Python server — a
documented reference file lives at `isso.cfg` in this repository.

```console
$ isso -c /path/to/isso.cfg
```

For multi-site deployments, pass `-c` multiple times; each config's
`[general] name` becomes the sub-path the site mounts under.

### Importing from another comment system

```console
$ isso -c isso.cfg import --type=auto export.xml
```

Supports Disqus XML, WordPress WXR, and the Isso-native generic JSON.

### Docker

The `Dockerfile` builds a two-stage image (Node for the JS bundles, Rust
for the binary) and runs `isso` directly as the entrypoint:

```console
$ docker build -t isso .
$ docker run -p 8080:8080 -v $PWD/config:/config -v $PWD/db:/db isso
```

## Differences from the upstream Python server

| Area | Status |
|---|---|
| SQLite schema + migrations | Identical — schema-equivalence is asserted by tests |
| JSON HTTP API | Identical for the endpoints the JS client calls |
| itsdangerous cookie format | Byte-compatible with Python for uncompressed payloads; interoperable for compressed payloads (valid DEFLATE, different bytes) |
| PBKDF2 hash output | Byte-identical to Python's `hashlib.pbkdf2_hmac` |
| Bloomfilter voter format | Byte-identical to Python's 256-byte, 11-probe SHA-256 scheme |
| Markdown renderer | `pulldown-cmark` + `ammonia` sanitiser instead of `mistune` + `bleach` — the allowlist is the same, so the output is semantically equivalent for the default config |
| gevent / uWSGI deployment | Replaced by axum + tokio. The `unix:///path` listener format still works |
| WSGI entry points (`isso.run`, `isso.dispatch`) | Gone — the binary binds its own socket |
| Admin templates | Rendered by `minijinja` from the same HTML templates; minijinja HTML-escapes `/` which Jinja2 doesn't, so URL attribute values differ in incidental characters |

See `docs/porting-reference.md` for the full wire-compatibility
specification the port was built against.

## Tests

```console
$ cargo test
```

125 tests at the time of the last commit: 93 unit, 22 HTTP integration,
4 migrate-fixture end-to-end, 6 schema-equivalence.

## Contributing

Issues and pull requests welcome at
[github.com/jelmer/isso-rs](https://github.com/jelmer/isso-rs). Frontend JS
lives under `static/js/`; the Rust crate sources live under
`src/`.

## License

MIT, see [LICENSE](LICENSE).
