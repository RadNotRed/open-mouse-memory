# Protocol research

Only sanitized evidence belongs here. USB captures can include unrelated keyboard, authentication-token,
audio, and other bus traffic.

- `manifests/` records hardware, firmware, receiver, experiment, and capture hashes.
- `decoded/` contains text-only request/response transcripts.
- `hashes/` contains checksums for raw captures stored outside normal Git history.
- `raw/` is ignored and is a local staging area only.

Name controlled captures as:

```text
pxs2-startup-default.pcapng
pxs2-dpi-800-to-1600.pcapng
pxs2-rate-1000-to-2000.pcapng
pxs2-button-side-rear-back-to-middle.pcapng
```

Every conclusion in protocol documentation should cite one or more manifest/transcript names and carry a
confidence of `hypothesis`, `probable`, or `confirmed`.
