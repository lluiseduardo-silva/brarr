# TheTVDB fixtures

**Provenance: captured from the live v4 API**, unmodified, on 2026-08-13
with a project key. They are raw responses — full of fields brarr never
reads — on purpose: trimming them would quietly remove the variance the
parser exists to survive.

`brarr-tmdb`'s fixtures carry the opposite note, because no token existed
when that crate was written. These are the real thing, and they exist so
the numbers this crate is *for* are asserted by the default suite rather
than only by `tests/live_api.rs`, which is `#[ignore]`d and needs a key
and a network. A claim that only a skipped test defends is a claim
nobody checks.

| File | Endpoint | What it pins down |
|---|---|---|
| `dbs_official_page0.json` | `/series/295068/episodes/official?page=0` | **The reason this crate exists.** Dragon Ball Super under the broadcast split: 14/13/19/30/55, the shape on the operator's disk and in every release, against TMDB's single season of 131. Also carries 2 specials in season 0, so "specials are excluded" is exercised rather than assumed. `S02E01` carries absolute **15**. |
| `dbs_absolute_page0.json` | `/series/295068/episodes/absolute` | The same 131 episodes as **one** season — the axis an anime release named `Série - 224` carries instead of a marker. Season 0 is absent entirely here, which is why a special can shift the absolute numbering of the official axis without appearing on this one. |
| `solo_leveling_official_page0.json` | `/series/389597/episodes/official` | 12/13 where TMDB has one season of 25. TMDB is not wrong — the people publishing cut somewhere else. The operator declared these blocks by hand before anything could derive them. |

## What the captures confirmed about the envelope

- `links` is `snake_case` (`total_items`, `page_size`) while the records
  are `camelCase` (`seasonNumber`, `absoluteNumber`). A blanket
  `rename_all` would kill the cursor and stop pagination at page one.
  Both spellings appear in every file here.
- `page_size` is **500**, so all three series fit in one page and
  `links.next` is `null`. Pagination itself is therefore still covered by
  the hand-built mocks in `client_wiremock.rs` — a real multi-page
  capture would need a series past 500 episodes.
- `data.series.name` comes back in the **original language**
  (`ドラゴンボール超[スーパー]`), not translated. Harmless for brarr,
  which takes every title from TMDB, and pinned so nobody wires this
  field into a screen expecting Portuguese.

## Re-capturing

```bash
KEY=$(cat tvdbapikey.txt)
TOKEN=$(curl -s -X POST https://api4.thetvdb.com/v4/login \
  -H 'Content-Type: application/json' -d "{\"apikey\":\"$KEY\"}" \
  | python -c 'import sys,json;print(json.load(sys.stdin)["data"]["token"])')
curl -s "https://api4.thetvdb.com/v4/series/295068/episodes/official?page=0" \
  -H "Authorization: Bearer $TOKEN" -o dbs_official_page0.json
```

The login response carries a bearer token and is **never** saved here.
