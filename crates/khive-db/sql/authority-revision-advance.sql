UPDATE authority_revisions SET revision=revision+1 WHERE epoch_id=?1 AND revision=?2 AND revision<9223372036854775807;
