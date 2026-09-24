-- Sign-in lockout counts a user's recent failed codes and last successful
-- sign-in from the audit log (src/editor/auth.rs lockout_seconds, failed_codes)
-- on every code attempt and every /auth/me. Without this the scan grows with
-- the whole log.
CREATE INDEX IF NOT EXISTS gtfs_audit_log_actor_action_idx
    ON gtfs_audit_log (actor, action, at DESC) WHERE actor IS NOT NULL;
