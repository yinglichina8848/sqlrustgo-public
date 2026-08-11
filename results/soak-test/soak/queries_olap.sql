SELECT * FROM sbtest.sbtest1 WHERE k BETWEEN 100 AND 500 ORDER BY k
SELECT AVG(k) AS avg_k, MIN(id), MAX(id), COUNT(*) FROM sbtest.sbtest1
