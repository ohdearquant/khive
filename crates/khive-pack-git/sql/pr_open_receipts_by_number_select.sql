SELECT actor, credential FROM git_receipts WHERE namespace=?1 AND repo=?2 AND verb='git.pr_open' AND disposition != 'not_committed' AND json_extract(result,'$.number')=?3 LIMIT 1001
