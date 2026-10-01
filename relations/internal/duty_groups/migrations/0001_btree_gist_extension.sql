-- Needed by the exclusion constraints on duty_groups and duties. Requires a role allowed to
-- create extensions (cloudsqlsuperuser on Cloud SQL). Run before 0007 here and duties/0005.
CREATE EXTENSION IF NOT EXISTS btree_gist;
