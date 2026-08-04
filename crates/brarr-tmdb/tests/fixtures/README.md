# TMDB fixtures

**Provenance: derived from the documented v3 response schema, not
captured from a live account.** Every TMDB endpoint requires
authentication, so these could not be recorded without a read access
token — unlike the UNIT3D and Newznab fixtures, which came off real
trackers.

They are faithful to the shape (field names, nesting, the `type`
codes inside `release_dates`, the `iso_639_1` / `iso_3166_1` pair inside
`translations`) and each one deliberately exercises a variance the
parser has to survive:

| File | What it pins down |
|---|---|
| `movie_603.json` | Top-level `overview` is `""` — the empty-string-not-null case — so the pt-BR text has to come from `translations`. Carries BR and US `release_dates` with digital (type 4) on both. |
| `tv_76479.json` | `external_ids` carrying a `tvdb_id`, `next_episode_to_air`, and seasons out of order including the specials season 0. |
| `season_4.json` | Episode list with one unaired episode whose `air_date` is `""`. |
| `search_movie_duna.json` | Multiple results, one with a null `poster_path`. |
| `find_imdb.json` | `/find` envelope: populated `movie_results`, empty `tv_results`. |

**Re-capture these against a real token when one is available**, then
re-run the suite. Any field TMDB emits that these files miss is a gap the
tests cannot currently see.
