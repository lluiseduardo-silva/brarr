import sqlite3
c = sqlite3.connect(r'C:\Projeto\brarr\brarr.db')

print('=== Searches recentes (tmdb_id, imdb_id, request_json) ===')
for r in c.execute("SELECT id, tmdb_id, imdb_id, request_json, submitted_at, result_count FROM searches ORDER BY submitted_at DESC LIMIT 30").fetchall():
    print(r)

print()
print('=== Decisions onde release_name contém Rookie ===')
for r in c.execute("SELECT id, provider_name, release_name, score, seeders, rejected, tags_json FROM decisions WHERE release_name LIKE '%Rookie%' ORDER BY score DESC LIMIT 20").fetchall():
    print(r)

print()
print('=== Push history das decisions do Rookie ===')
for r in c.execute("""
    SELECT ph.id, ph.status, ph.http_status, d.release_name, d.score
    FROM push_history ph JOIN decisions d ON d.id=ph.decision_id
    WHERE d.release_name LIKE '%Rookie%'
    ORDER BY ph.pushed_at DESC LIMIT 20
""").fetchall():
    print(r)
