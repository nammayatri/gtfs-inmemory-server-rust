#!/usr/bin/env bash
# End-to-end test of the GTFS editor API against a LOCAL Postgres that holds the
# editor schema (db/gtfs_editor/*.sql). Runs tests/editor_flow.rs, which uses its
# own feed and accounts, then proves the chennai_bus rows were not touched -
# reseeding them from nandi if they were.
#
#   scripts/editor_flow_test.sh
#   EDITOR_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55432/mtc_internal_master scripts/editor_flow_test.sh
set -euo pipefail

DB_URL="${EDITOR_TEST_DATABASE_URL:-postgres://postgres@127.0.0.1:55432/mtc_internal_master}"
PSQL="${PSQL:-psql}"
SEEDER="${SEEDER:-/Users/vicky/Documents/nandi/gtfs-v3/scripts/chennai-bus/editor/seed_gtfs_editor.py}"
PYTHON="${PYTHON:-python3}"

case "$DB_URL" in
  *@127.0.0.1*|*@localhost*) ;;
  *) echo "refusing to run against a non-local database" >&2; exit 2 ;;
esac

fingerprint() {
  "$PSQL" "$DB_URL" -Atc "
    SELECT md5(coalesce(string_agg(x, '|' ORDER BY x), '')) FROM (
      SELECT 's' || stop_id || name || lat || lon || coalesce(parent_station, '') || row_version AS x
        FROM gtfs_stop WHERE gtfs_id = 'chennai_bus'
      UNION ALL
      SELECT 'r' || route_id || sequence || coalesce(stop_id, marker_id) || stop_type || stage_no || stage_name
        FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus') t"
}

"$PSQL" "$DB_URL" -Atc "SELECT 1 FROM gtfs_feed LIMIT 1" >/dev/null
before="$(fingerprint)"

EDITOR_TEST_DATABASE_URL="$DB_URL" cargo test --test editor_flow -- --nocapture
status=$?

after="$(fingerprint)"
if [ "$before" != "$after" ]; then
  echo "chennai_bus rows changed during the test; reseeding from $SEEDER" >&2
  "$PYTHON" "$SEEDER" --replace | "$PSQL" "$DB_URL" -q
fi
echo "chennai_bus untouched: $([ "$before" = "$after" ] && echo yes || echo 'no (reseeded)')"
exit $status
