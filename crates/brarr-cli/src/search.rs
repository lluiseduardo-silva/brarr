//! Orquestração da busca: instancia um [`Unit3dClient`] por
//! [`TrackerConfig`], chama `search_by_tmdb` em paralelo via
//! `futures::future::join_all`, pontua cada release, ordena por score
//! decrescente, e devolve o resultado.
//!
//! Erros em um tracker específico **não abortam** a busca inteira —
//! são coletados em [`SearchOutcome::failures`] para o caller decidir
//! como reportar.

use std::fmt::Write as _;

use brarr_core::{
    DecisionScore, ExternalIds, Language, Release, ReleaseKind, Resolution, TmdbId, TrackerSource,
};
use brarr_decision_service::Engine;
use brarr_tracker_unit3d::{ClientError, Unit3dClient};
use futures::future::join_all;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::TrackerConfig;

/// Um release pontuado, pronto para ordenação/exibição.
#[derive(Debug, Clone)]
pub struct ScoredRelease {
    /// O release original.
    pub release: Release,
    /// Resultado da avaliação pelo motor de regras: score, tags,
    /// flag de rejeição e nomes das regras que casaram.
    pub outcome: brarr_decision_service::DecisionOutcome,
}

impl ScoredRelease {
    /// Atalho conveniente para `outcome.score`.
    #[must_use]
    pub fn score(&self) -> DecisionScore {
        self.outcome.score
    }
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
    engine: &Engine,
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
                    let outcome = engine.evaluate(&release);
                    if outcome.rejected {
                        debug!(
                            target: "brarr_cli::search",
                            id = %release.tracker_release_id,
                            title = %release.title,
                            "release rejected by rules engine"
                        );
                        continue;
                    }
                    scored.push(ScoredRelease { release, outcome });
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
        b.outcome
            .score
            .cmp(&a.outcome.score)
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
            score = sr.outcome.score.get(),
            title = r.title,
        );

        let mut flags = release_flags(r);
        for tag in &sr.outcome.tags {
            flags.push(tag.clone());
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

/// Serializa o `SearchOutcome` como JSON em uma única linha, pronto
/// para pipe em `jq` ou ingest em outra ferramenta.
///
/// Schema (estável em minor versions, mudanças quebram em major):
/// ```json
/// {
///   "total": <int>,
///   "shown": <int>,
///   "releases": [ <ReleaseJson> ],
///   "failures": [ { "tracker": <str>, "error": <str> } ]
/// }
/// ```
///
/// Cada `<ReleaseJson>` carrega rank, score, identificação do tracker,
/// resolução/kind, contadores, URLs, e flags PT/HDR derivadas do
/// `enrichment` quando disponível.
///
/// Falha apenas se a serialização do `serde_json` der erro — improvável
/// pois todos os tipos derivam `Serialize`.
///
/// # Errors
///
/// Devolve [`serde_json::Error`] se a serialização falhar.
pub fn format_outcome_json(
    outcome: &SearchOutcome,
    limit: usize,
) -> Result<String, serde_json::Error> {
    let total = outcome.scored.len();
    let shown = total.min(limit);
    let releases: Vec<ReleaseJson> = outcome
        .scored
        .iter()
        .take(limit)
        .enumerate()
        .map(|(idx, sr)| ReleaseJson::from_scored(idx + 1, sr))
        .collect();
    let failures: Vec<FailureJson> = outcome
        .failures
        .iter()
        .map(|(name, err)| FailureJson {
            tracker: name.clone(),
            error: err.clone(),
        })
        .collect();

    let payload = SearchOutcomeJson {
        total,
        shown,
        releases,
        failures,
    };
    serde_json::to_string(&payload)
}

/// Schema JSON de `SearchOutcome`. Veja [`format_outcome_json`].
#[derive(Debug, Serialize)]
struct SearchOutcomeJson {
    total: usize,
    shown: usize,
    releases: Vec<ReleaseJson>,
    failures: Vec<FailureJson>,
}

#[derive(Debug, Serialize)]
struct FailureJson {
    tracker: String,
    error: String,
}

#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "flat JSON shape is intentional — easier to consume in jq pipes"
)]
struct ReleaseJson {
    rank: usize,
    score: u32,
    tags: Vec<String>,
    matched_rules: Vec<String>,
    title: String,
    tracker: String,
    tracker_release_id: String,
    year: Option<u16>,
    kind: String,
    resolution: String,
    size_bytes: u64,
    size_human: String,
    seeders: u32,
    leechers: u32,
    snatches: u32,
    details_url: Option<String>,
    download_url: Option<String>,
    external_ids: ExternalIdsJson,
    audio_pt_br: bool,
    audio_pt_pt: bool,
    audio_pt_ambiguous: bool,
    subtitle_count_pt_br: usize,
    subtitle_count_pt_pt: usize,
    hdr: bool,
    container_format: Option<String>,
    duration_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ExternalIdsJson {
    tmdb: Option<u32>,
    imdb: Option<u32>,
    tvdb: Option<u32>,
    mal: Option<u32>,
}

impl ReleaseJson {
    fn from_scored(rank: usize, sr: &ScoredRelease) -> Self {
        let r = &sr.release;
        let (audio_pt_br, audio_pt_pt, audio_pt_ambiguous, subs_br, subs_pt, hdr, container, dur) =
            match r.enrichment.as_ref() {
                Some(e) => (
                    e.has_audio_in(&Language::PtBr),
                    e.has_audio_in(&Language::PtPt),
                    e.has_audio_in(&Language::Pt),
                    e.subtitle_count_in(&Language::PtBr),
                    e.subtitle_count_in(&Language::PtPt),
                    e.has_hdr,
                    e.container_format.clone(),
                    e.duration.map(|d| d.as_secs()),
                ),
                None => (false, false, false, 0, 0, false, None, None),
            };

        Self {
            rank,
            score: sr.outcome.score.get(),
            tags: sr.outcome.tags.clone(),
            matched_rules: sr.outcome.matched_rules.clone(),
            title: r.title.clone(),
            tracker: r.tracker.name.clone(),
            tracker_release_id: r.tracker_release_id.clone(),
            year: r.year,
            kind: release_kind_label(&r.kind),
            resolution: resolution_label(&r.resolution),
            size_bytes: r.size_bytes,
            size_human: humanize_bytes(r.size_bytes),
            seeders: r.seeders,
            leechers: r.leechers,
            snatches: r.snatches,
            details_url: r.urls.details.as_ref().map(url::Url::to_string),
            download_url: r.urls.download.as_ref().map(url::Url::to_string),
            external_ids: ExternalIdsJson::from_core(&r.external_ids),
            audio_pt_br,
            audio_pt_pt,
            audio_pt_ambiguous,
            subtitle_count_pt_br: subs_br,
            subtitle_count_pt_pt: subs_pt,
            hdr,
            container_format: container,
            duration_secs: dur,
        }
    }
}

impl ExternalIdsJson {
    fn from_core(ids: &ExternalIds) -> Self {
        Self {
            tmdb: ids.tmdb.map(TmdbId::get),
            imdb: ids.imdb.map(brarr_core::ImdbId::get),
            tvdb: ids.tvdb.map(brarr_core::TvdbId::get),
            mal: ids.mal.map(brarr_core::MalId::get),
        }
    }
}

fn release_kind_label(k: &ReleaseKind) -> String {
    match k {
        ReleaseKind::WebDl => "WEB-DL".to_string(),
        ReleaseKind::BluRay => "BluRay".to_string(),
        ReleaseKind::Encode => "Encode".to_string(),
        ReleaseKind::HdTv => "HDTV".to_string(),
        ReleaseKind::Dvd => "DVD".to_string(),
        ReleaseKind::Other(s) => s.clone(),
    }
}

fn resolution_label(r: &Resolution) -> String {
    match r {
        Resolution::Sd => "SD".to_string(),
        Resolution::P720 => "720p".to_string(),
        Resolution::P1080 => "1080p".to_string(),
        Resolution::P2160 => "2160p".to_string(),
        Resolution::Other(s) => s.clone(),
    }
}

/// Constrói lista de flags PT/HDR/legendas para um [`Release`], pronta
/// para exibição. Distingue PT-BR explícito, PT-PT, e o "PT ambíguo"
/// (idioma rotulado só como "Portuguese" sem hint regional). Releases
/// com `Language::Pt` no áudio pontuam +50 no scorer, então precisam
/// aparecer no display também — sem essa branch, releases com score
/// inflado saíam como `—` e confundiam o usuário.
fn release_flags(r: &Release) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let Some(e) = r.enrichment.as_ref() else {
        return flags;
    };

    if e.has_audio_in(&Language::PtBr) {
        flags.push("PT-BR audio".to_string());
    } else if e.has_audio_in(&Language::PtPt) {
        flags.push("PT-PT audio".to_string());
    } else if e.has_audio_in(&Language::Pt) {
        flags.push("PT audio (ambíguo)".to_string());
    }

    let pt_subs = e.subtitle_count_in(&Language::PtBr) + e.subtitle_count_in(&Language::PtPt);
    if pt_subs > 0 {
        flags.push(format!("{pt_subs} legenda(s) PT"));
    }
    if e.has_hdr {
        flags.push("HDR".to_string());
    }
    flags
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

    use super::{humanize_bytes, release_flags};
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource,
    };
    use url::Url;

    fn tracker() -> TrackerSource {
        TrackerSource::new("t", Url::parse("https://e.com/").unwrap()).unwrap()
    }

    fn release_with(audio: Vec<Language>, subs: Vec<Language>, hdr: bool) -> Release {
        let mut r = Release::new(
            "1",
            tracker(),
            "x",
            ReleaseKind::WebDl,
            Resolution::P1080,
            0,
        )
        .unwrap();
        r.enrichment = Some(ReleaseEnrichment {
            container_format: None,
            duration: None,
            audio_languages: audio,
            subtitle_languages: subs,
            has_forced_subs: false,
            has_hdr: hdr,
        });
        r
    }

    #[test]
    fn release_flags_shows_pt_br_audio() {
        let r = release_with(vec![Language::PtBr], vec![], false);
        assert_eq!(release_flags(&r), vec!["PT-BR audio".to_string()]);
    }

    #[test]
    fn release_flags_shows_pt_pt_audio_when_no_pt_br() {
        let r = release_with(vec![Language::PtPt], vec![], false);
        assert_eq!(release_flags(&r), vec!["PT-PT audio".to_string()]);
    }

    #[test]
    fn release_flags_shows_ambiguous_pt_when_neither_regional_present() {
        let r = release_with(vec![Language::Pt], vec![], false);
        assert_eq!(release_flags(&r), vec!["PT audio (ambíguo)".to_string()]);
    }

    #[test]
    fn release_flags_prefers_pt_br_over_other_variants() {
        // When PT-BR present alongside PT or PT-PT, only PT-BR shows.
        let r = release_with(
            vec![Language::PtBr, Language::PtPt, Language::Pt],
            vec![],
            false,
        );
        assert_eq!(release_flags(&r), vec!["PT-BR audio".to_string()]);
    }

    #[test]
    fn release_flags_combines_audio_subs_and_hdr() {
        let r = release_with(
            vec![Language::PtBr],
            vec![Language::PtBr, Language::PtPt],
            true,
        );
        let f = release_flags(&r);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0], "PT-BR audio");
        assert_eq!(f[1], "2 legenda(s) PT");
        assert_eq!(f[2], "HDR");
    }

    #[test]
    fn release_flags_empty_when_no_enrichment() {
        let r = Release::new(
            "1",
            tracker(),
            "x",
            ReleaseKind::WebDl,
            Resolution::P1080,
            0,
        )
        .unwrap();
        assert!(release_flags(&r).is_empty());
    }

    #[test]
    fn release_flags_empty_when_enrichment_lacks_pt_and_hdr() {
        let r = release_with(vec![Language::En], vec![Language::En], false);
        assert!(release_flags(&r).is_empty());
    }

    use super::{ScoredRelease, SearchOutcome, format_outcome_json};
    use brarr_core::DecisionScore;
    use brarr_decision_service::DecisionOutcome;

    fn scored(release: Release, score: u32) -> ScoredRelease {
        ScoredRelease {
            release,
            outcome: DecisionOutcome {
                score: DecisionScore::saturating(score),
                tags: Vec::new(),
                rejected: false,
                matched_rules: Vec::new(),
            },
        }
    }

    #[test]
    fn format_outcome_json_emits_valid_single_line() {
        let mut r = release_with(vec![Language::PtBr], vec![Language::PtBr], false);
        r.seeders = 42;
        let outcome = SearchOutcome {
            scored: vec![scored(r, 110)],
            failures: vec![("locadora".to_string(), "timeout".to_string())],
        };
        let json = format_outcome_json(&outcome, 10).expect("serialize");
        assert!(!json.contains('\n'), "json must be single-line");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["total"], 1);
        assert_eq!(v["shown"], 1);
        let release = &v["releases"][0];
        assert_eq!(release["rank"], 1);
        assert_eq!(release["score"], 110);
        assert_eq!(release["audio_pt_br"], true);
        assert_eq!(release["audio_pt_pt"], false);
        assert_eq!(release["audio_pt_ambiguous"], false);
        assert_eq!(release["subtitle_count_pt_br"], 1);
        assert_eq!(release["seeders"], 42);
        assert_eq!(v["failures"][0]["tracker"], "locadora");
        assert_eq!(v["failures"][0]["error"], "timeout");
    }

    #[test]
    fn format_outcome_json_respects_limit() {
        let r1 = release_with(vec![Language::PtBr], vec![], false);
        let r2 = release_with(vec![Language::En], vec![], false);
        let outcome = SearchOutcome {
            scored: vec![scored(r1, 100), scored(r2, 10)],
            failures: vec![],
        };
        let json = format_outcome_json(&outcome, 1).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["total"], 2);
        assert_eq!(v["shown"], 1);
        assert_eq!(v["releases"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn format_outcome_json_flags_ambiguous_pt_audio() {
        let r = release_with(vec![Language::Pt], vec![], false);
        let outcome = SearchOutcome {
            scored: vec![scored(r, 60)],
            failures: vec![],
        };
        let json = format_outcome_json(&outcome, 10).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let release = &v["releases"][0];
        assert_eq!(release["audio_pt_br"], false);
        assert_eq!(release["audio_pt_pt"], false);
        assert_eq!(release["audio_pt_ambiguous"], true);
    }

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
