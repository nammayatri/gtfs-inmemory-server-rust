ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS timezone text NOT NULL DEFAULT 'Asia/Kolkata';
