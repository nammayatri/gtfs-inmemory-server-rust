CREATE TABLE public.duty_repeats (
    id                     TEXT NOT NULL,
    trip_group_id          TEXT NOT NULL,
    gtfs_id                TEXT NOT NULL,
    operator_id            TEXT,
    repeat_status          TEXT NOT NULL DEFAULT 'active',
    recurrence_days        SMALLINT[] NOT NULL DEFAULT '{}',
    effective_from         DATE NOT NULL,
    effective_till         DATE,
    generated_till         DATE,
    vehicle_number         TEXT,
    driver_token_number    TEXT,
    driver_name            TEXT,
    conductor_token_number TEXT,
    conductor_name         TEXT,
    deleted                BOOLEAN NOT NULL DEFAULT false,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
