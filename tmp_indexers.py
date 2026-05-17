import json, urllib.request, os, sys

for app, port, key_env in [("radarr", 7878, "RADARR_KEY"), ("sonarr", 8989, "SONARR_KEY")]:
    key = os.environ[key_env]
    req = urllib.request.Request(f"http://localhost:{port}/api/v3/indexer", headers={"X-Api-Key": key})
    with urllib.request.urlopen(req) as r:
        data = json.load(r)
    print(f"=== {app} ===")
    for i in data:
        print(f"  id={i['id']} name={i['name']!r} impl={i['implementation']} "
              f"rss={i.get('enableRss')} search={i.get('enableAutomaticSearch')} "
              f"manual={i.get('enableInteractiveSearch')}")
