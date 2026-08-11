SELECT id, k FROM sbtest.sbtest1 WHERE id = 1
INSERT INTO sbtest.sbtest1 (id, k, c, pad) VALUES (999991, 1000, 'i', 'p')
DELETE FROM sbtest.sbtest1 WHERE id = 999991
SELECT COUNT(*) FROM sbtest.sbtest1
