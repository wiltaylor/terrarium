-- Version groups and card labels replaced these objects. Dropping the
-- tables also drops their indexes. This discards legacy metadata only.
DROP TABLE test_run_versions;
DROP TABLE version_reviews;
DROP TABLE version_artifacts;
DROP TABLE version_cards;
DROP TABLE versions;
