-- TER-607: the branch that a merge job merges onto and pushes. NULL is
-- `next`; `card merge --onto release/X.Y.Z` sets a release branch.
ALTER TABLE jobs ADD COLUMN onto TEXT;
