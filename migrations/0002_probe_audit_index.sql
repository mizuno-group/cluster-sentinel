-- An index for "when did each probe last report about each entity".
--
-- The probe audit asks that question for the whole environment at once, and
-- `status` asks it on every invocation. Without this the query is a full scan
-- of the observations table with a group-by: on a real cluster of sixteen
-- hosts with a fortnight of retention it took **6.9 seconds**, which is not a
-- cost `status` can carry.
--
-- The existing indexes do not help. `idx_observations_target_time` is ordered
-- by (target, finished_at) so a group-by that also keys on probe_id cannot use
-- it, and `idx_observations_probe_time` leads with the wrong column. This one
-- matches the grouping exactly, so the maximum per group is found by walking
-- the index rather than the table.
CREATE INDEX IF NOT EXISTS idx_observations_target_probe_time
    ON observations(target_entity_id, probe_id, finished_at DESC);
