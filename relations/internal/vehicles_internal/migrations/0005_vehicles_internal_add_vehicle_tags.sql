-- Ordered fare-classification tags, highest priority first; rider-app fare classes match on them in this order.
-- Each element is an active tag of public.vehicle_tag_internal for the same gtfs_id, checked by the vehicle upsert API.
ALTER TABLE public.vehicles_internal
    ADD COLUMN IF NOT EXISTS vehicle_tags text[];
