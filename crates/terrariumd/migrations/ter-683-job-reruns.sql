-- TER-683: the failed tests that a merge job ran once more, as JSON
-- (`[{step, test, first, rerun, tail}]`). NULL: no rerun.
ALTER TABLE jobs ADD COLUMN reruns TEXT;
