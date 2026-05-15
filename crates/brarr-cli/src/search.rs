//! Orquestração da busca: instancia um [`Unit3dClient`] por
//! [`TrackerConfig`], chama `search_by_tmdb` em paralelo via
//! `futures::future::join_all`, pontua cada release, ordena por score
//! decrescente, e devolve o resultado.
//!
//! Erros em um tracker específico **não abortam** a busca inteira —
//! são coletados em [`SearchOutcome::failures`] para o caller decidir
//! como reportar.

use std::fmt::Write as _;

use brarr_core::{DecisionScore, Language, Release, TmdbId, TrackerSource};
use brarr_tracker_unit3d::{ClientError, Unit3dClient};
use futures::future::join_all;
use tracing::{debug, info, warn};

use crate::config::TrackerConfig;
use crate::scoring::{ScoringWeights, score_release};

/// Um release pontuado, pronto para ordenação/exibição.
#[derive(Debug, Clone)]
pub struct ScoredRelease {
    /// O release original.
    pub release: Release,
    /// Score calculado pelos pesos do [`ScoringWeights`] em uso.
    pub score: DecisionScore,
}

/// Resultado de uma operação de busca, separando sucessos de falhas.
#[derive(Debug)]
pub struct SearchOutcome {
    /// Releases pontuados e ordenados por score (decrescente, então
    /// por seeders desempata).
    pub scored: Vec<ScoredRelease>,
    /// Pares `(tracker_name, erro_renderizado)` para trackers que
    /// falharam durante a busca.
    pub failures: Vec<(String, String)>,
}

/// Executa a busca em paralelo entre os trackers configurados.
///
/// # Errors
///
/// Esta função em si não falha — erros por tracker são coletados em
/// [`SearchOutcome::failures`]. Só falha catastroficamente se a
/// criação de algum `Unit3dClient` falhar (token contém caractere
/// inválido como header HTTP, etc.); nesse caso devolve o primeiro
/// erro encontrado.
pub async fn run_search(
    trackers: &[TrackerConfig],
    tmdb: TmdbId,
    weights: &ScoringWeights,
) -> Result<SearchOutcome, ClientError> {
    // Construção dos clients é síncrona e barata; falhas aqui são
    // bug de configuração (token mal-formado), abortar.
    let clients: Vec<(String, Unit3dClient)> = trackers
        .iter()
        .map(|tc| {
            let source = TrackerSource::new(tc.name.clone(), tc.base_url.clone())
                .map_err(|_| ClientError::InvalidToken)?; // só falha se name vazio
            let client = Unit3dClient::new(source, &tc.token)?;
            Ok::<_, ClientError>((tc.name.clone(), client))
        })
        .collect::<Result<_, _>>()?;

    info!(
        target: "brarr_cli::search",
        tracker_count = clients.len(),
        tmdb = tmdb.get(),
        "starting parallel search"
    );

    let futures = clients.into_iter().map(|(name, client)| async move {
        debug!(target: "brarr_cli::search", %name, "querying tracker");
        let result = client.search_by_tmdb(tmdb).await;
        (name, result)
    });

    let per_tracker: Vec<(String, Result<Vec<Release>, ClientError>)> = join_all(futures).await;

    let mut scored = Vec::new();
    let mut failures = Vec::new();

    for (name, result) in per_tracker {
        match result {
            Ok(releases) => {
                debug!(
                    target: "brarr_cli::search",
                    %name,
                    count = releases.len(),
                    "tracker returned releases"
                );
                for release in releases {
                    let score = score_release(&release, weights);
                    scored.push(ScoredRelease { release, score });
                }
            }
            Err(e) => {
                warn!(
                    target: "brarr_cli::search",
                    %name,
                    error = %e,
                    "tracker failed",
                );
                failures.push((name, format!("{e}")));
            }
        }
    }

    // Ordena por score desc, depois por seeders desc como tiebreaker.
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.release.seeders.cmp(&a.release.seeders))
    });

    info!(
        target: "brarr_cli::search",
        scored = scored.len(),
        failures = failures.len(),
        "search complete"
    );

    Ok(SearchOutcome { scored, failures })
}

/// Formata o `SearchOutcome` em texto pra exibição na CLI. Pega o
/// `limit` superior dos releases por score.
#[must_use]
pub fn format_outcome(outcome: &SearchOutcome, limit: usize) -> String {
    let mut out = String::new();
    let total = outcome.scored.len();

    if total == 0 && outcome.failures.is_empty() {
        return "Nenhum release encontrado nos trackers configurados.\n".to_string();
    }

    let shown = outcome.scored.len().min(limit);
    let _ = writeln!(out, "Top {shown} de {total} releases encontrados:\n");

    for (rank, sr) in outcome.scored.iter().take(limit).enumerate() {
        let r = &sr.release;
        let _ = writeln!(
            out,
            "{rank_idx:>2}. [{score:>4}] {title}",
            rank_idx = rank + 1,
            score = sr.score.get(),
            title = r.title,
        );

        let pt_audio = r
            .enrichment
            .as_ref()
            .is_some_and(|e| e.has_audio_in(&Language::PtBr));
        let pt_subs = r.enrichment.as_ref().map_or(0, |e| {
            e.subtitle_count_in(&Language::PtBr) + e.subtitle_count_in(&Language::PtPt)
        });
        let hdr = r.enrichment.as_ref().is_some_and(|e| e.has_hdr);

        let mut flags: Vec<String> = Vec::new();
        if pt_audio {
            flags.push("PT-BR audio".to_string());
        }
        if pt_subs > 0 {
            flags.push(format!("{pt_subs} legenda(s) PT"));
        }
        if hdr {
            flags.push("HDR".to_string());
        }
        let flags_str = if flags.is_empty() {
            "—".to_string()
        } else {
            flags.join(" · ")
        };

        let _ = writeln!(
            out,
            "      {tracker} · {size} · {seeders} seeders · {flags_str}",
            tracker = r.tracker.name,
            size = humanize_bytes(r.size_bytes),
            seeders = r.seeders,
        );
        if let Some(url) = &r.urls.details {
            let _ = writeln!(out, "      {url}");
        }
        let _ = writeln!(out);
    }

    if !outcome.failures.is_empty() {
        let _ = writeln!(out, "Trackers que falharam:");
        for (name, err) in &outcome.failures {
            let _ = writeln!(out, "  - {name}: {err}");
        }
    }

    out
}

/// Formata bytes como `1.23 GiB` / `456.78 MiB` etc.
fn humanize_bytes(b: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    // u64 → f64 perde precisão acima de 2^52 ≈ 4 PiB, ordem de magnitude
    // sem cabimento para arquivo de torrent. Sinalizamos explicitamente.
    #[allow(
        clippy::cast_precision_loss,
        reason = "byte counts are several orders of magnitude below the f64 mantissa limit"
    )]
    let mut value = b as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::humanize_bytes;

    #[test]
    fn humanize_bytes_basic_units() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(1023), "1023 B");
        assert_eq!(humanize_bytes(1024), "1.00 KiB");
        assert_eq!(humanize_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(humanize_bytes(1024_u64.pow(3)), "1.00 GiB");
        assert_eq!(humanize_bytes(1024_u64.pow(4)), "1.00 TiB");
    }

    #[test]
    fn humanize_bytes_real_release_sizes() {
        // 8.95 GiB (vnlls 1080p) ≈ 9_608_016_733 bytes
        assert_eq!(humanize_bytes(9_608_016_733), "8.95 GiB");
        // 18.05 GiB-ish (shadow 2160p) ≈ 19_381_275_821 bytes
        let s = humanize_bytes(19_381_275_821);
        assert!(s.starts_with("18.") && s.ends_with(" GiB"), "got {s}");
    }
}
