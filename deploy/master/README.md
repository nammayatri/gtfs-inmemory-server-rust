# GTFS editor GIMS on master

A separate deployment (`app=gtfs-editor-gims`) with its own Service, so live GIMS
traffic is untouched until a switch is decided. The shared
`gtfs-inmemory-data-server` Service selects `app=gtfs-inmemory-data-server` only.

| file | what |
|---|---|
| `10-configmap-overlay.yaml` | dhall overlay: imports the live `gtfs-inmemory-data-server-config` unchanged, overrides the internal DB (master `mtc_internal_master`), `gtfs_db_feeds`, editor settings |
| `20-deployment.json` | replace `IMAGE_TAG` (`<nandi6>-<gims6>`) and `IMAGE_VERSION`; 1 replica |
| `30-service.yaml` | `gtfs-editor-gims:8000` |
| `40-pomerium-route.yaml` | route for `gtfs.sso.c2.integ.internal.svc.movingtech.net` in `pomerium-v2/pomerium-sso-config` |
| `50-managed-cert.yaml` | Google-managed cert for the SSO host; also append `gtfs-sso-cert` to the `networking.gke.io/managed-certificates` annotation on `pomerium-v2/pomerium-internal-ingress` (keep every existing entry) |
| `create_secret.py` | `gtfs-editor-gims-secrets` from the repo `.env` + a generated TOTP key; never overwrites (rotating the key forces every user to re-enrol) |

The deployment command adds `master_mtc_pass` (from the shared secret's env var,
dhall-escaped) to the generated secrets file: the shared config references
`secrets.master_mtc_pass`, which `GIMS_SECRETS_DHALL` does not contain.

Image: nandi Jenkins `ny-internal/nandi/<branch>` with `gimsOnly=true`,
`syncGIMS=true`, `gimsBranch=<this branch>`, deploy flags off. First deploy
2026-09-17: build 8, image `d6f1b6-d7f4f8`.
