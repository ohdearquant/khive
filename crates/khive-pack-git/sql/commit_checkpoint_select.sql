SELECT
    (SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind='commits') AS cursor,
    (SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind='commits_checkpoint') AS progress
