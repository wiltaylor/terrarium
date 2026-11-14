-- TER-1277: the pending runs of one lane, newest activity last. A column's
-- `job` slot (`RERUN_SETTLED`) asks whether the lane of a pending run made
-- progress in the last 10 minutes.
CREATE INDEX IF NOT EXISTS rule_runs_lane_pending ON rule_runs(lane, updated_at)
  WHERE state = 'pending';
