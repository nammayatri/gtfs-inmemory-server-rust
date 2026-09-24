-- Single-row table: when the reconciler tick last actually ran, shared across all replicas
-- (a pod's own timer only knows when that pod checked in). Read/written by
-- run_repeater_reconciler_tick while holding its advisory lock.
CREATE TABLE public.repeater_reconciler_state (
    last_run_at timestamp(6) with time zone
);

INSERT INTO public.repeater_reconciler_state DEFAULT VALUES;
