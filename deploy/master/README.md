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

The shared config references `secrets.master_mtc_pass`; it must be inside the
`GIMS_SECRETS_DHALL` record of `gtfs-inmemory-data-server-secrets` (added
2026-09-17), not only a separate key, or the dhall fails to type-check at boot.

Image: nandi Jenkins `ny-internal/nandi/<branch>` with `gimsOnly=true`,
`syncGIMS=true`, `gimsBranch=<this branch>`, deploy flags off. First deploy
2026-09-17: build 8, image `d6f1b6-d7f4f8`.
