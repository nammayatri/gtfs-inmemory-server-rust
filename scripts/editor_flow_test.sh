#!/usr/bin/env bash
# End-to-end test of the GTFS editor API against a LOCAL Postgres that holds the
# editor schema (db/gtfs_editor/*.sql). Runs tests/editor_flow.rs,
# tests/editor_create_flow.rs, tests/editor_position_review_flow.rs,
# tests/editor_review_merge_flow.rs, tests/editor_feed_config_flow.rs,
# tests/editor_stop_details_flow.rs, tests/editor_feed_lock_flow.rs,
# tests/editor_station_merge_flow.rs, tests/gtfs_stop_alias_flow.rs and
# tests/gtfs_station_code_flow.rs, which use
# their own feeds and accounts, then proves the chennai_bus rows were not touched
# - reseeding them from nandi if they were - and that its station proposals, its
# position reviews and its feed row (data source and version) were not touched
# either.
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

proposals() {
  "$PSQL" "$DB_URL" -Atc "
    SELECT md5(coalesce(string_agg(proposal_id || status || coalesce(change_set_id::text, '')
                                   || coalesce(change_id::text, ''), '|' ORDER BY proposal_id), ''))
      FROM gtfs_station_proposal WHERE gtfs_id = 'chennai_bus'"
}

reviews() {
  "$PSQL" "$DB_URL" -Atc "
    SELECT md5(coalesce(string_agg(review_id || status || coalesce(change_set_id::text, '')
                                   || coalesce(change_id::text, '') || coalesce(review_note, ''),
                                   '|' ORDER BY review_id), ''))
      FROM gtfs_position_review WHERE gtfs_id = 'chennai_bus'"
}

feed() {
  "$PSQL" "$DB_URL" -Atc "SELECT data_source || ' v' || version FROM gtfs_feed WHERE gtfs_id = 'chennai_bus'"
}

"$PSQL" "$DB_URL" -Atc "SELECT 1 FROM gtfs_feed LIMIT 1" >/dev/null
feed_before="$(feed)"
before="$(fingerprint)"
proposals_before="$(proposals)"
reviews_before="$(reviews)"

status=0
EDITOR_TEST_DATABASE_URL="$DB_URL" cargo test --locked --test editor_flow --test editor_create_flow \
  --test editor_position_review_flow --test editor_review_merge_flow --test editor_feed_config_flow \
  --test editor_stop_details_flow --test editor_feed_lock_flow \
  --test editor_station_merge_flow \
  --test gtfs_stop_alias_flow --test gtfs_station_code_flow \
  -- --nocapture || status=$?

proposals_after="$(proposals)"
if [ "$proposals_before" != "$proposals_after" ]; then
  echo "chennai_bus station proposals changed during the test" >&2
  status=1
fi
reviews_after="$(reviews)"
if [ "$reviews_before" != "$reviews_after" ]; then
  echo "chennai_bus position reviews changed during the test" >&2
  status=1
fi
feed_after="$(feed)"
if [ "$feed_before" != "$feed_after" ]; then
  echo "chennai_bus feed row changed during the test: $feed_before -> $feed_after" >&2
  status=1
fi
after="$(fingerprint)"
if [ "$before" != "$after" ]; then
  echo "chennai_bus rows changed during the test; reseeding from $SEEDER" >&2
  "$PYTHON" "$SEEDER" --replace | "$PSQL" "$DB_URL" -q
fi
echo "chennai_bus untouched: $([ "$before" = "$after" ] && echo yes || echo 'no (reseeded)')"
echo "chennai_bus station proposals untouched: $([ "$proposals_before" = "$proposals_after" ] && echo yes || echo no)"
echo "chennai_bus position reviews untouched: $([ "$reviews_before" = "$reviews_after" ] && echo yes || echo no)"
echo "chennai_bus feed row untouched: $([ "$feed_before" = "$feed_after" ] && echo yes || echo no) ($feed_after)"
exit $status
