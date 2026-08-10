SELECT COUNT(*) FROM sbtest.sbtest1
SELECT * FROM sbtest.sbtest1 WHERE id = 1
SELECT * FROM sbtest.sbtest1 WHERE id BETWEEN 1 AND 100
SELECT id, k, c, pad FROM sbtest.sbtest1 WHERE k = 500
SELECT SUM(k), AVG(k), MIN(k), MAX(k) FROM sbtest.sbtest1
