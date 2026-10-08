-- waybill_id's old default, nextval('waybills_waybill_id_seq'), was a leftover from the
-- int->text id migration (same vintage as 0002's fk sequence defaults) that every insert path
-- ignores by always supplying an explicit UUID -- except create_repeat_waybills_bulk, which
-- never set waybill_id and so fell back to it. Sequence advancement survives a rolled-back
-- transaction, so a prior failed/retried call left it pointed at a value that collided with an
-- already-committed row: "duplicate key value violates unique constraint waybills_internal_pkey".
--
-- gen_random_uuid() is built into Postgres core as of 13, no extension needed, and is evaluated
-- per row on a multi-row INSERT, so this is a direct drop-in replacement -- every insert path
-- that omits waybill_id now gets a real UUID instead of the next integer off a stale sequence.
-- waybills_waybill_id_seq itself is left in place, same as 0002's note on its fk sequences.

ALTER TABLE public.waybills_internal
    ALTER COLUMN waybill_id SET DEFAULT gen_random_uuid()::text;
