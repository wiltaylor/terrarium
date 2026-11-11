ALTER TABLE projects ADD COLUMN home_board_columns TEXT;
CREATE INDEX cards_home ON cards(archived, id);
CREATE INDEX cards_home_project ON cards(project, archived, id);
CREATE INDEX monitors_home_project ON monitors(project, id);
CREATE INDEX pipelines_home_project ON pipelines(project, id);
CREATE INDEX pipeline_runs_home_latest ON pipeline_runs(pipeline, sort_at DESC, ord ASC, pk DESC);
