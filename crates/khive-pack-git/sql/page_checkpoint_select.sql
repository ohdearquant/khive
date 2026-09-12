SELECT
    (SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?2) AS floor,
    (SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?3) AS progress
