-- Lease claim timestamp: a pod sets this to now() to claim a tick (a single UPDATE, not a held
-- transaction). last_run_at > lock_acquired_at means the claim finished; otherwise it's either
-- still in progress or was abandoned (crashed pod/panic) -- distinguished by how long ago
-- lock_acquired_at was set, checked against repeater_stuck_lock_timeout_secs at claim time.
ALTER TABLE public.repeater_reconciler_state
  ADD COLUMN lock_acquired_at timestamp(6) with time zone;
