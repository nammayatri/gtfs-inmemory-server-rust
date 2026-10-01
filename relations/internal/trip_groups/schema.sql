CREATE TABLE public.trip_groups (
    id                             TEXT NOT NULL,
    gtfs_id                        TEXT NOT NULL,
    operator_id                    TEXT,
    code                           TEXT NOT NULL,
    description                    TEXT,
    zone                           TEXT NOT NULL,
    shift                          TEXT NOT NULL,
    first_departure                TIME NOT NULL,
    trip_type                      TEXT NOT NULL,
    depot_id                       TEXT,
    service_type_id                TEXT,
    deleted                        BOOLEAN NOT NULL DEFAULT false,
    created_at                     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                     TIMESTAMPTZ NOT NULL DEFAULT now()
);
