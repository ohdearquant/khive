SELECT id, repo, disposition, credential FROM git_receipts
WHERE namespace=?1 AND verb='git.push' AND disposition IN ('committed','unknown')
AND lower(json_extract(result,'$.sha'))=?2 AND json_extract(result,'$.ref')=?3
AND lower(json_extract(result,'$.remote')) IN (?4,?5)
ORDER BY rowid DESC LIMIT 1001
