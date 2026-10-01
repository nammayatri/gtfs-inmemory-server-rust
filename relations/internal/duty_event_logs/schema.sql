CREATE TABLE public.duty_event_logs (
    id              TEXT NOT NULL,
    gtfs_id         TEXT NOT NULL,
    operator_id     TEXT,
    event_type      TEXT NOT NULL,
    duty_group_id   TEXT,
    duty_id         TEXT,
    duty_repeat_id  TEXT,
    operation_date  DATE,
    actor_person_id TEXT,
    trigger         TEXT,
    old_value       JSONB,
    new_value       JSONB,
    reason          TEXT,
    error_code      TEXT,
    error_message   TEXT,
    resolved_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
