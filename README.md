# ani-api
HTTP API over [ani-cli-rs](https://github.com/vorlie/ani-cli-rs) (vendored in `vendor/`, GPL-3.0).

- `GET /search?q=naruto&type=sub&provider=anikoto2`
- `GET /episodes?id=<id from search>&type=sub`
- `GET /stream?id=<id>&ep=1&type=sub` -> streams, each with a `proxy_url`
- `GET /proxy?url=&referer=&origin=` header-injecting HLS proxy
- Env: `PORT`, optional `API_KEY` (send as `x-api-key` or `?key=`), optional `PUBLIC_URL`.
