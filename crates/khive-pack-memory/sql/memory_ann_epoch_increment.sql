INSERT INTO memory_ann_epoch (id, epoch) VALUES (1, 1)
ON CONFLICT(id) DO UPDATE SET epoch = epoch + 1
