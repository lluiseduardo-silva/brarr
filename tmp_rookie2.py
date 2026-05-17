import sqlite3
c = sqlite3.connect(r'C:\Projeto\brarr\brarr.db')

print('=== S08E14 decision detail ===')
for r in c.execute("""
    SELECT d.id, d.search_id, d.provider_id, d.provider_name, d.provider_kind,
           d.release_name, d.score, d.seeders, d.release_id_remote,
           s.tmdb_id, s.imdb_id, s.request_json, s.submitted_at, s.result_count
    FROM decisions d
    LEFT JOIN searches s ON s.id = d.search_id
    WHERE d.release_name LIKE '%S08E14%' AND d.release_name LIKE '%Rookie%'
""").fetchall():
    print(r)

print()
print('=== Quantas decisions essa search produziu? ===')
for r in c.execute("""
    SELECT d.search_id, COUNT(*) c, MAX(d.score) max_score
    FROM decisions d WHERE d.release_name LIKE '%Rookie%S08E14%'
    GROUP BY d.search_id
""").fetchall():
    print(r)

print()
print('=== Tudo dessa search ===')
for r in c.execute("""
    SELECT release_name, score, provider_name
    FROM decisions WHERE search_id = (
        SELECT search_id FROM decisions WHERE release_name LIKE '%Rookie%S08E14%' LIMIT 1
    )
    ORDER BY score DESC
""").fetchall():
    print(r)
