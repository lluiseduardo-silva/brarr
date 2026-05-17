import sqlite3
c = sqlite3.connect(r'C:\Projeto\brarr\brarr.db')

print('=== Push history Matrix Reloaded ===')
rows = c.execute(
    "SELECT ph.decision_id, ph.status, ph.http_status, d.score, d.release_name "
    "FROM push_history ph JOIN decisions d ON d.id = ph.decision_id "
    "WHERE d.release_name LIKE '%Reloaded%' ORDER BY ph.pushed_at DESC LIMIT 10"
).fetchall()
for r in rows:
    print(r)

print()
print('=== Top kept decisions Matrix Reloaded (score desc) ===')
rows = c.execute(
    "SELECT score, provider_name, release_name, size_bytes/1073741824 AS gb, tags_json "
    "FROM decisions WHERE release_name LIKE '%Reloaded%' AND rejected=0 "
    "ORDER BY score DESC LIMIT 15"
).fetchall()
for r in rows:
    print(r)
