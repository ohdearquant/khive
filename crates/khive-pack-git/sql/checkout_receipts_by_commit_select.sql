SELECT json_extract(result,'$.tree') AS tree FROM git_receipts
WHERE namespace=?1 AND verb='git.checkout' AND disposition='committed' AND repo=?2
AND lower(json_extract(result,'$.commit'))=?3
ORDER BY rowid DESC LIMIT 4
