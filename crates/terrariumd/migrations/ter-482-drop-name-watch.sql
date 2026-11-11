-- Remote observation now watches only working-role columns, served by
-- board_columns_role_watch. Project-scoped name lookups use UNIQUE(project, name).
DROP INDEX board_columns_name_watch;
