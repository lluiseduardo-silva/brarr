# TMDB fixtures

**Provenance: captured from the live v3 API**, unmodified, on 2026-08-04
with `language=pt-BR`. They are raw responses — minified, full of fields
brarr never reads — on purpose: trimming them would quietly remove the
variance the parser exists to survive.

| File | Endpoint | What it pins down |
|---|---|---|
| `movie_603.json` | `/movie/603` + `external_ids,release_dates,translations` | The Matrix. **No digital date in BR or US** — only theatrical (type 3) and Blu-ray (type 5). A digital date *does* exist for AE, which is exactly the case `release_date_of` refuses to borrow from. 50 translation blocks. |
| `movie_693134.json` | same, for Duna: Parte Dois | A recent film that *does* have a digital window: BR type 4 on 2024-05-21, and **two** type-4 entries under US (2024-04-16 and 2024-05-21), so "first match wins" is a real decision and not a hypothetical. |
| `tv_76479.json` | `/tv/76479` + `external_ids,translations` | The Boys, now **Ended**: `next_episode_to_air` is null and `episode_run_time` is an *empty array* rather than a missing field. Season 0 is the specials bucket with 76 entries — larger than any real season, which is why sorting by number matters. |
| `season_4.json` | `/tv/76479/season/4` | Eight fully-aired episodes with pt-BR titles. |
| `search_movie_duna.json` | `/search/movie?query=duna` | A real page: 20 hits, **3 with no poster and 14 with no pt-BR synopsis**. The tail of a search is where a strict parser breaks. |
| `find_imdb.json` | `/find/tt0133093?external_source=imdb_id` | The five-array envelope, with only `movie_results` populated. |

## What these fixtures cannot cover

Two behaviours are exercised by unit tests instead, because no captured
response happened to contain them:

- **The language fallback** (pt-BR → pt-PT → en-US). Every title captured
  here has a pt-BR overview on the top level, so the fallback never
  fires. `model.rs` covers it directly.
- **An unaired episode** (`air_date: ""`). Season 4 finished airing.
  `dto.rs` covers the empty-string date parse.

## Re-capturing

```bash
curl -s "https://api.themoviedb.org/3/movie/603?language=pt-BR\
&append_to_response=external_ids,release_dates,translations&api_key=$KEY" \
  -o movie_603.json
```

Expect assertions to move when you do: TMDB data is live, and this
directory has already caught one stale assumption — the previous,
schema-derived fixtures claimed a digital release date for The Matrix
that does not exist.
