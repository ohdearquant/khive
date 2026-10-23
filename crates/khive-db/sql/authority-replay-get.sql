SELECT canonical_request,result FROM authority_replays WHERE epoch_id=?1 AND structural_caller=?2 AND canonical_verb=?3 AND request_id=?4;
