//! Tipos das regras declarativas: [`Rule`], [`Condition`], [`RuleSet`].
//!
//! Uma regra tem um **predicado** ([`Condition`]) que decide se ela se
//! aplica a um dado [`Release`], e um conjunto de **efeitos**
//! (`add_score`, `tag`, `reject`) aplicados quando o predicado casa.
//!
//! Predicado combina os campos opcionais com **AND** — todos os
//! `Some(_)` devem casar; `None` significa "não importa". Predicado
//! totalmente vazio casa sempre (regra default).

use std::collections::HashMap;

use brarr_core::{Language, Release, ReleaseKind, Resolution, VideoCodec};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Cache de regex `title_matches` pré-compiladas, chaveado pelo padrão
/// cru. Construído uma vez por [`crate::Engine`] (ver
/// [`RuleSet::compile_regexes`]) e consultado no caminho quente de
/// avaliação para não recompilar a cada release.
pub(crate) type RegexCache = HashMap<String, Regex>;

/// Conjunto completo de regras avaliadas em ordem para cada release.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    /// Lista de regras na ordem de avaliação. Cada regra que casa
    /// contribui com seus efeitos; ordem importa apenas para tags
    /// (são preservadas) e `reject` (curto-circuita).
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// `RuleSet` correspondente ao scoring hardcoded antigo de
    /// `brarr-cli` (`ScoringWeights::default`). Útil para clientes que
    /// não fornecem `rules.toml` próprio.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            rules: vec![
                Rule {
                    name: Some("PT-BR audio".into()),
                    when: Condition::audio(AudioFilter::PtBr),
                    add_score: 100,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("PT-PT audio".into()),
                    when: Condition::audio(AudioFilter::PtPt),
                    add_score: 25,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("PT ambíguo (sem hint regional)".into()),
                    when: Condition::audio(AudioFilter::Pt),
                    add_score: 50,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Legenda PT-BR".into()),
                    when: Condition::subtitle(SubtitleFilter::PtBr),
                    add_score: 50,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Legenda PT-PT".into()),
                    when: Condition::subtitle(SubtitleFilter::PtPt),
                    add_score: 15,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("HDR".into()),
                    when: Condition::hdr(true),
                    add_score: 10,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Resolução 2160p".into()),
                    when: Condition::resolution(ResolutionFilter::Exact2160),
                    add_score: 20,
                    tag: None,
                    reject: false,
                },
                Rule {
                    name: Some("Resolução 1080p".into()),
                    when: Condition::resolution(ResolutionFilter::Exact1080),
                    add_score: 10,
                    tag: None,
                    reject: false,
                },
            ],
        }
    }
}

impl RuleSet {
    /// Pré-compila todos os `title_matches` do conjunto num
    /// [`RegexCache`]. Padrões inválidos são silenciosamente omitidos
    /// (o leaf correspondente nunca casa); use [`Self::validate`] para
    /// reportá-los ao operador antes de salvar.
    pub(crate) fn compile_regexes(&self) -> RegexCache {
        let mut cache = RegexCache::new();
        for rule in &self.rules {
            collect_patterns(&rule.when, &mut cache);
        }
        cache
    }

    /// Valida os padrões `title_matches` de todas as regras, compilando
    /// cada um. Devolve a lista de erros legíveis (vazia ⇒ tudo ok) para
    /// a UI mostrar no banner ao salvar/pré-visualizar um profile.
    ///
    /// # Errors
    ///
    /// `Err(msgs)` com uma mensagem por padrão de regex inválido.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        for (idx, rule) in self.rules.iter().enumerate() {
            validate_condition(&rule.when, idx, &mut errors);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Coleta recursivamente os padrões `title_matches` de uma condição (e
/// suas subárvores) num cache, compilando os que ainda não estão lá.
fn collect_patterns(cond: &Condition, cache: &mut RegexCache) {
    if let Some(p) = &cond.title_matches
        && !cache.contains_key(p)
        && let Ok(re) = Regex::new(p)
    {
        cache.insert(p.clone(), re);
    }
    if let Some(all) = &cond.all {
        for c in all {
            collect_patterns(c, cache);
        }
    }
    if let Some(any) = &cond.any {
        for c in any {
            collect_patterns(c, cache);
        }
    }
    if let Some(not) = &cond.not {
        collect_patterns(not, cache);
    }
}

/// Acumula erros de regex de uma condição e suas subárvores.
fn validate_condition(cond: &Condition, idx: usize, errors: &mut Vec<String>) {
    if let Some(p) = &cond.title_matches
        && let Err(e) = Regex::new(p)
    {
        errors.push(format!("regra[{idx}]: title_matches inválido ({p:?}): {e}"));
    }
    if let Some(all) = &cond.all {
        for c in all {
            validate_condition(c, idx, errors);
        }
    }
    if let Some(any) = &cond.any {
        for c in any {
            validate_condition(c, idx, errors);
        }
    }
    if let Some(not) = &cond.not {
        validate_condition(not, idx, errors);
    }
}

/// Uma regra declarativa.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Nome opcional para identificar a regra em logs e diagnóstico.
    #[serde(default)]
    pub name: Option<String>,
    /// Predicado: critérios que o release precisa atender para a regra
    /// disparar. Campos `None` (omitidos no TOML) significam "não filtrar
    /// por esse aspecto".
    #[serde(default)]
    pub when: Condition,
    /// Quantos pontos somar ao score quando a regra casa.
    #[serde(default)]
    pub add_score: u32,
    /// Tag opcional anexada ao release quando a regra casa.
    #[serde(default)]
    pub tag: Option<String>,
    /// Se `true`, desqualifica o release (filtro de exclusão).
    #[serde(default)]
    pub reject: bool,
}

/// Predicado de uma regra.
///
/// Um nó casa quando **todos** os campos escalares presentes casam (AND)
/// **e** todos os combinadores presentes casam:
/// - `all`: todas as subcondições casam (AND);
/// - `any`: ao menos uma subcondição casa (OR) — ignorado se vazio;
/// - `not`: a subcondição **não** casa.
///
/// Campos escalares ausentes (`None`) não filtram. Um predicado vazio
/// casa sempre. A árvore (`all`/`any`/`not`) permite lógica booleana
/// arbitrária mantendo o caso simples como um objeto plano — e os
/// `rules_json` antigos (só campos escalares) continuam válidos.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// Filtro de áudio (`pt-br`, `pt-pt`, `pt`, `pt-any`, `en`, `jp`, `zh`).
    #[serde(default)]
    pub audio: Option<AudioFilter>,
    /// Filtro de legenda (`pt-br`, `pt-pt`, `pt-any`, `en`, `jp`, `zh`).
    #[serde(default)]
    pub subtitle: Option<SubtitleFilter>,
    /// Release tem (ou não tem) HDR no vídeo.
    #[serde(default)]
    pub hdr: Option<bool>,
    /// Filtro de resolução.
    #[serde(default)]
    pub resolution: Option<ResolutionFilter>,
    /// Seeders mínimos (release precisa ter ≥ esse valor).
    #[serde(default)]
    pub min_seeders: Option<u32>,
    /// Tamanho máximo em bytes (release não pode exceder).
    #[serde(default)]
    pub max_size_bytes: Option<u64>,
    /// Nome de tracker exato (case-sensitive).
    #[serde(default)]
    pub tracker: Option<String>,
    /// Tamanho mínimo em bytes (release precisa ter ≥ esse valor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size_bytes: Option<u64>,
    /// Filtro de fonte/tipo (`web-dl`, `bluray`, `remux`, `encode`,
    /// `hdtv`, `dvd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<KindFilter>,
    /// Filtro de codec de vídeo (`x264`/`h264`, `x265`/`h265`, `av1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<CodecFilter>,
    /// Grupo de release (match case-insensitive no grupo parseado).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_group: Option<String>,
    /// Release marca `PROPER` (ou não).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proper: Option<bool>,
    /// Release marca `REPACK` (ou não).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repack: Option<bool>,
    /// Substring case-insensitive no título.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    /// Regex (sintaxe da crate `regex`) casada contra o título.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_matches: Option<String>,
    /// Combinador AND: todas as subcondições casam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<Vec<Condition>>,
    /// Combinador OR: ao menos uma subcondição casa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any: Option<Vec<Condition>>,
    /// Combinador NOT: a subcondição não casa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not: Option<Box<Condition>>,
}

impl Condition {
    fn audio(a: AudioFilter) -> Self {
        Self {
            audio: Some(a),
            ..Self::default()
        }
    }
    fn subtitle(s: SubtitleFilter) -> Self {
        Self {
            subtitle: Some(s),
            ..Self::default()
        }
    }
    fn hdr(v: bool) -> Self {
        Self {
            hdr: Some(v),
            ..Self::default()
        }
    }
    fn resolution(r: ResolutionFilter) -> Self {
        Self {
            resolution: Some(r),
            ..Self::default()
        }
    }

    /// Decide se o `release` satisfaz o predicado. Versão pública sem
    /// cache de regex — compila `title_matches` na hora; usada por
    /// testes e callers fora do motor.
    #[must_use]
    pub fn matches(&self, release: &Release) -> bool {
        self.matches_inner(release, None)
    }

    /// Como [`Self::matches`], mas consulta um [`RegexCache`]
    /// pré-compilado para os `title_matches`. Usada pelo [`crate::Engine`]
    /// no caminho quente.
    pub(crate) fn matches_with(&self, release: &Release, regexes: &RegexCache) -> bool {
        self.matches_inner(release, Some(regexes))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "predicado é uma sequência linear de checagens AND + combinadores; quebrar esconde o curto-circuito"
    )]
    fn matches_inner(&self, release: &Release, regexes: Option<&RegexCache>) -> bool {
        if let Some(a) = &self.audio
            && !audio_matches(release, a)
        {
            return false;
        }
        if let Some(s) = &self.subtitle
            && !subtitle_matches(release, s)
        {
            return false;
        }
        if let Some(h) = self.hdr
            && release.enrichment.as_ref().is_some_and(|e| e.has_hdr) != h
        {
            return false;
        }
        if let Some(r) = &self.resolution
            && !resolution_matches(&release.resolution, r)
        {
            return false;
        }
        if let Some(min) = self.min_seeders
            && release.seeders < min
        {
            return false;
        }
        if let Some(max) = self.max_size_bytes
            && release.size_bytes > max
        {
            return false;
        }
        if let Some(min) = self.min_size_bytes
            && release.size_bytes < min
        {
            return false;
        }
        if let Some(name) = &self.tracker
            && release.tracker.name != *name
        {
            return false;
        }
        if let Some(k) = &self.kind
            && !kind_matches(release, k)
        {
            return false;
        }
        if let Some(c) = &self.video_codec
            && !codec_matches(release, c)
        {
            return false;
        }
        if let Some(g) = &self.release_group
            && !release
                .tags
                .release_group
                .as_deref()
                .is_some_and(|rg| rg.eq_ignore_ascii_case(g))
        {
            return false;
        }
        if let Some(p) = self.proper
            && release.tags.proper != p
        {
            return false;
        }
        if let Some(rp) = self.repack
            && release.tags.repack != rp
        {
            return false;
        }
        if let Some(needle) = &self.title_contains
            && !release
                .title
                .to_lowercase()
                .contains(&needle.to_lowercase())
        {
            return false;
        }
        if let Some(pat) = &self.title_matches
            && !title_regex_matches(pat, &release.title, regexes)
        {
            return false;
        }
        if let Some(all) = &self.all
            && !all.iter().all(|c| c.matches_inner(release, regexes))
        {
            return false;
        }
        if let Some(any) = &self.any
            && !any.is_empty()
            && !any.iter().any(|c| c.matches_inner(release, regexes))
        {
            return false;
        }
        if let Some(not) = &self.not
            && not.matches_inner(release, regexes)
        {
            return false;
        }
        true
    }
}

/// Resolve um `title_matches`: usa o cache pré-compilado quando
/// disponível; senão (caminho de teste ou padrão ausente do cache)
/// compila na hora. Regex inválida nunca casa.
fn title_regex_matches(pattern: &str, title: &str, regexes: Option<&RegexCache>) -> bool {
    if let Some(re) = regexes.and_then(|c| c.get(pattern)) {
        return re.is_match(title);
    }
    Regex::new(pattern).is_ok_and(|re| re.is_match(title))
}

fn kind_matches(release: &Release, k: &KindFilter) -> bool {
    match k {
        KindFilter::WebDl => release.kind == ReleaseKind::WebDl,
        KindFilter::BluRay => release.kind == ReleaseKind::BluRay,
        KindFilter::Remux => release.tags.remux,
        KindFilter::Encode => release.kind == ReleaseKind::Encode,
        KindFilter::HdTv => release.kind == ReleaseKind::HdTv,
        KindFilter::Dvd => release.kind == ReleaseKind::Dvd,
    }
}

fn codec_matches(release: &Release, c: &CodecFilter) -> bool {
    matches!(
        (release.tags.video_codec.as_ref(), c),
        (Some(VideoCodec::H264), CodecFilter::X264)
            | (Some(VideoCodec::H265), CodecFilter::X265)
            | (Some(VideoCodec::Av1), CodecFilter::Av1)
    )
}

/// Filtro de áudio: o release precisa ter pelo menos uma faixa que case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioFilter {
    /// Português brasileiro explícito.
    PtBr,
    /// Português europeu.
    PtPt,
    /// Português ambíguo (sem hint regional).
    Pt,
    /// Qualquer variante de português (PT-BR, PT-PT ou Pt).
    PtAny,
    /// Inglês.
    En,
    /// Japonês — útil para regras de anime original ("legendado").
    Jp,
    /// Chinês (qualquer variante — Mandarim, Cantonês). Útil para
    /// doramas e séries chinesas.
    Zh,
}

/// Filtro de legenda: o release precisa ter pelo menos uma faixa de
/// legenda que case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubtitleFilter {
    /// PT-BR.
    PtBr,
    /// PT-PT.
    PtPt,
    /// PT-BR ou PT-PT.
    PtAny,
    /// Inglês.
    En,
    /// Japonês.
    Jp,
    /// Chinês.
    Zh,
}

/// Filtro de resolução.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum ResolutionFilter {
    /// Pelo menos 720p (qualquer ≥ 720p casa).
    #[serde(rename = "min-720")]
    At720,
    /// Pelo menos 1080p (1080p ou 2160p).
    #[serde(rename = "min-1080")]
    At1080,
    /// Pelo menos 2160p.
    #[serde(rename = "min-2160")]
    At2160,
    /// Match exato 1080p (não casa 2160p).
    #[serde(rename = "exact-1080")]
    Exact1080,
    /// Match exato 2160p.
    #[serde(rename = "exact-2160")]
    Exact2160,
}

/// Filtro de fonte/tipo do release (mapeia [`brarr_core::ReleaseKind`] +
/// o flag `remux` parseado do título).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum KindFilter {
    /// `WEB-DL`.
    #[serde(rename = "web-dl")]
    WebDl,
    /// `BluRay` (full disc ou remux).
    #[serde(rename = "bluray")]
    BluRay,
    /// Remux especificamente (lê o flag `remux` parseado do título).
    #[serde(rename = "remux")]
    Remux,
    /// Encode (x264/x265 derivado de outra fonte).
    #[serde(rename = "encode")]
    Encode,
    /// `HDTV`.
    #[serde(rename = "hdtv")]
    HdTv,
    /// `DVD`.
    #[serde(rename = "dvd")]
    Dvd,
}

/// Filtro de codec de vídeo. Aceita as grafias equivalentes (`x264`/
/// `h264`/`avc`, `x265`/`h265`/`hevc`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum CodecFilter {
    /// H.264 / AVC.
    #[serde(rename = "x264", alias = "h264", alias = "avc")]
    X264,
    /// H.265 / HEVC.
    #[serde(rename = "x265", alias = "h265", alias = "hevc")]
    X265,
    /// AV1.
    #[serde(rename = "av1")]
    Av1,
}

fn audio_matches(release: &Release, a: &AudioFilter) -> bool {
    let Some(e) = release.enrichment.as_ref() else {
        return false;
    };
    match a {
        AudioFilter::PtBr => e.has_audio_in(&Language::PtBr),
        AudioFilter::PtPt => e.has_audio_in(&Language::PtPt),
        AudioFilter::Pt => e.has_audio_in(&Language::Pt),
        AudioFilter::PtAny => {
            e.has_audio_in(&Language::PtBr)
                || e.has_audio_in(&Language::PtPt)
                || e.has_audio_in(&Language::Pt)
        }
        AudioFilter::En => e.has_audio_in(&Language::En),
        AudioFilter::Jp => e.has_audio_in(&Language::Jp),
        AudioFilter::Zh => e.has_audio_in(&Language::Zh),
    }
}

fn subtitle_matches(release: &Release, s: &SubtitleFilter) -> bool {
    let Some(e) = release.enrichment.as_ref() else {
        return false;
    };
    match s {
        SubtitleFilter::PtBr => e.has_subtitle_in(&Language::PtBr),
        SubtitleFilter::PtPt => e.has_subtitle_in(&Language::PtPt),
        SubtitleFilter::PtAny => {
            e.has_subtitle_in(&Language::PtBr) || e.has_subtitle_in(&Language::PtPt)
        }
        SubtitleFilter::En => e.has_subtitle_in(&Language::En),
        SubtitleFilter::Jp => e.has_subtitle_in(&Language::Jp),
        SubtitleFilter::Zh => e.has_subtitle_in(&Language::Zh),
    }
}

fn resolution_matches(r: &Resolution, filter: &ResolutionFilter) -> bool {
    let rank = resolution_rank(r);
    match filter {
        ResolutionFilter::At720 => rank >= 1,
        ResolutionFilter::At1080 => rank >= 2,
        ResolutionFilter::At2160 => rank >= 3,
        ResolutionFilter::Exact1080 => matches!(r, Resolution::P1080),
        ResolutionFilter::Exact2160 => matches!(r, Resolution::P2160),
    }
}

fn resolution_rank(r: &Resolution) -> u8 {
    match r {
        Resolution::P720 => 1,
        Resolution::P1080 => 2,
        Resolution::P2160 => 3,
        Resolution::Sd | Resolution::Other(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::too_many_arguments,
        clippy::similar_names
    )]

    use super::{
        AudioFilter, Condition, ResolutionFilter, SubtitleFilter, audio_matches,
        resolution_matches, subtitle_matches,
    };
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource,
    };
    use url::Url;

    fn tracker(name: &str) -> TrackerSource {
        TrackerSource::new(name, Url::parse("https://e.com/").unwrap()).unwrap()
    }

    fn release(
        audio: Vec<Language>,
        subs: Vec<Language>,
        resolution: Resolution,
        seeders: u32,
        size: u64,
        hdr: bool,
        tracker_name: &str,
    ) -> Release {
        let mut r = Release::new(
            "1",
            tracker(tracker_name),
            "x",
            ReleaseKind::WebDl,
            resolution,
            size,
        )
        .unwrap();
        r.seeders = seeders;
        r.enrichment = Some(ReleaseEnrichment {
            audio_languages: audio,
            subtitle_languages: subs,
            has_hdr: hdr,
            ..ReleaseEnrichment::default()
        });
        r
    }

    #[test]
    fn empty_condition_matches_any_release() {
        let r = release(vec![], vec![], Resolution::P1080, 0, 0, false, "t");
        assert!(Condition::default().matches(&r));
    }

    #[test]
    fn audio_pt_br_matches_only_when_present() {
        let with_pt_br = release(
            vec![Language::PtBr],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let without = release(
            vec![Language::En],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(audio_matches(&with_pt_br, &AudioFilter::PtBr));
        assert!(!audio_matches(&without, &AudioFilter::PtBr));
    }

    #[test]
    fn audio_pt_any_matches_any_pt_variant() {
        let pt_br = release(
            vec![Language::PtBr],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let pt_pt = release(
            vec![Language::PtPt],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let pt_ambiguous = release(
            vec![Language::Pt],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let only_en = release(
            vec![Language::En],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(audio_matches(&pt_br, &AudioFilter::PtAny));
        assert!(audio_matches(&pt_pt, &AudioFilter::PtAny));
        assert!(audio_matches(&pt_ambiguous, &AudioFilter::PtAny));
        assert!(!audio_matches(&only_en, &AudioFilter::PtAny));
    }

    #[test]
    fn audio_jp_filter_matches_only_japanese_track() {
        let with_jp = release(
            vec![Language::Jp, Language::PtBr],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let without = release(
            vec![Language::En, Language::PtBr],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(audio_matches(&with_jp, &AudioFilter::Jp));
        assert!(!audio_matches(&without, &AudioFilter::Jp));
    }

    #[test]
    fn audio_zh_filter_matches_chinese_track() {
        let with_zh = release(
            vec![Language::Zh],
            vec![],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(audio_matches(&with_zh, &AudioFilter::Zh));
        assert!(!audio_matches(&with_zh, &AudioFilter::Jp));
    }

    #[test]
    fn subtitle_en_jp_zh_filters_match_respective_tracks() {
        let multi = release(
            vec![],
            vec![Language::En, Language::Jp, Language::Zh],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(subtitle_matches(&multi, &SubtitleFilter::En));
        assert!(subtitle_matches(&multi, &SubtitleFilter::Jp));
        assert!(subtitle_matches(&multi, &SubtitleFilter::Zh));
        assert!(!subtitle_matches(&multi, &SubtitleFilter::PtAny));
    }

    #[test]
    fn subtitle_pt_any_matches_pt_br_or_pt_pt() {
        let pt_br = release(
            vec![],
            vec![Language::PtBr],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let pt_pt = release(
            vec![],
            vec![Language::PtPt],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        let en_only = release(
            vec![],
            vec![Language::En],
            Resolution::P1080,
            0,
            0,
            false,
            "t",
        );
        assert!(subtitle_matches(&pt_br, &SubtitleFilter::PtAny));
        assert!(subtitle_matches(&pt_pt, &SubtitleFilter::PtAny));
        assert!(!subtitle_matches(&en_only, &SubtitleFilter::PtAny));
    }

    #[test]
    fn resolution_at_1080_matches_1080_and_2160() {
        assert!(resolution_matches(
            &Resolution::P1080,
            &ResolutionFilter::At1080
        ));
        assert!(resolution_matches(
            &Resolution::P2160,
            &ResolutionFilter::At1080
        ));
        assert!(!resolution_matches(
            &Resolution::P720,
            &ResolutionFilter::At1080
        ));
        assert!(!resolution_matches(
            &Resolution::Sd,
            &ResolutionFilter::At1080
        ));
    }

    #[test]
    fn resolution_exact_1080_excludes_2160() {
        assert!(resolution_matches(
            &Resolution::P1080,
            &ResolutionFilter::Exact1080
        ));
        assert!(!resolution_matches(
            &Resolution::P2160,
            &ResolutionFilter::Exact1080
        ));
    }

    #[test]
    fn condition_combines_fields_with_and() {
        let r = release(
            vec![Language::PtBr],
            vec![],
            Resolution::P2160,
            10,
            1_000_000_000,
            true,
            "capybara",
        );
        let cond = Condition {
            audio: Some(AudioFilter::PtBr),
            hdr: Some(true),
            resolution: Some(ResolutionFilter::At2160),
            min_seeders: Some(5),
            tracker: Some("capybara".into()),
            ..Condition::default()
        };
        assert!(cond.matches(&r));

        let cond_seeders_too_high = Condition {
            min_seeders: Some(100),
            ..cond.clone()
        };
        assert!(!cond_seeders_too_high.matches(&r));

        let cond_wrong_tracker = Condition {
            tracker: Some("locadora".into()),
            ..cond
        };
        assert!(!cond_wrong_tracker.matches(&r));
    }

    #[test]
    fn condition_max_size_caps_release() {
        let r = release(
            vec![],
            vec![],
            Resolution::P1080,
            0,
            5_000_000_000,
            false,
            "t",
        );
        let small_only = Condition {
            max_size_bytes: Some(2_000_000_000),
            ..Condition::default()
        };
        assert!(!small_only.matches(&r));
        let permissive = Condition {
            max_size_bytes: Some(10_000_000_000),
            ..Condition::default()
        };
        assert!(permissive.matches(&r));
    }

    #[test]
    fn ruleset_serde_json_roundtrips_baseline_exactly() {
        // The orchestrator persists `RuleSet`s as JSON in
        // `quality_profiles.rules_json`. Round-tripping the baseline
        // through serde_json must preserve every rule + condition
        // exactly so the engine produces identical scores before and
        // after a save.
        let baseline = super::RuleSet::baseline();
        let json = serde_json::to_string(&baseline).unwrap();
        let parsed: super::RuleSet = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.rules.len(), baseline.rules.len());
        for (a, b) in parsed.rules.iter().zip(baseline.rules.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.add_score, b.add_score);
            assert_eq!(a.tag, b.tag);
            assert_eq!(a.reject, b.reject);
            assert_eq!(a.when.audio, b.when.audio);
            assert_eq!(a.when.subtitle, b.when.subtitle);
            assert_eq!(a.when.hdr, b.when.hdr);
            assert_eq!(a.when.resolution, b.when.resolution);
            assert_eq!(a.when.min_seeders, b.when.min_seeders);
            assert_eq!(a.when.max_size_bytes, b.when.max_size_bytes);
            assert_eq!(a.when.tracker, b.when.tracker);
        }
    }

    #[test]
    fn condition_hdr_required_matches_only_hdr() {
        let hdr = release(vec![], vec![], Resolution::P2160, 0, 0, true, "t");
        let sdr = release(vec![], vec![], Resolution::P1080, 0, 0, false, "t");
        let only_hdr = Condition {
            hdr: Some(true),
            ..Condition::default()
        };
        let only_sdr = Condition {
            hdr: Some(false),
            ..Condition::default()
        };
        assert!(only_hdr.matches(&hdr));
        assert!(!only_hdr.matches(&sdr));
        assert!(only_sdr.matches(&sdr));
        assert!(!only_sdr.matches(&hdr));
    }
}

#[cfg(test)]
mod expanded_filter_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{AudioFilter, CodecFilter, Condition, KindFilter, Rule, RuleSet};
    use brarr_core::{
        Language, Release, ReleaseEnrichment, ReleaseKind, Resolution, TrackerSource, VideoCodec,
    };
    use url::Url;

    fn rel(title: &str) -> Release {
        let tracker = TrackerSource::new("t", Url::parse("https://e.com/").unwrap()).unwrap();
        Release::new(
            "1",
            tracker,
            title,
            ReleaseKind::WebDl,
            Resolution::P1080,
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn any_combinator_is_or() {
        let cond = Condition {
            any: Some(vec![
                Condition {
                    kind: Some(KindFilter::BluRay),
                    ..Default::default()
                },
                Condition {
                    video_codec: Some(CodecFilter::X265),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let mut r = rel("Movie x265");
        r.tags.video_codec = Some(VideoCodec::H265);
        assert!(cond.matches(&r)); // casa pelo codec, apesar de ser WEB-DL
        let mut r2 = rel("Movie x264");
        r2.tags.video_codec = Some(VideoCodec::H264);
        assert!(!cond.matches(&r2)); // nem BluRay nem x265
    }

    #[test]
    fn not_combinator_negates() {
        let cond = Condition {
            not: Some(Box::new(Condition {
                video_codec: Some(CodecFilter::X264),
                ..Default::default()
            })),
            ..Default::default()
        };
        let mut r = rel("m");
        r.tags.video_codec = Some(VideoCodec::H265);
        assert!(cond.matches(&r));
        r.tags.video_codec = Some(VideoCodec::H264);
        assert!(!cond.matches(&r));
    }

    #[test]
    fn scalar_leaf_plus_nested_all_and_not() {
        let cond = Condition {
            kind: Some(KindFilter::WebDl),
            all: Some(vec![
                Condition {
                    video_codec: Some(CodecFilter::X265),
                    ..Default::default()
                },
                Condition {
                    not: Some(Box::new(Condition {
                        proper: Some(true),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let mut r = rel("m");
        r.tags.video_codec = Some(VideoCodec::H265);
        r.tags.proper = false;
        assert!(cond.matches(&r));
        r.tags.proper = true;
        assert!(!cond.matches(&r));
    }

    #[test]
    fn kind_remux_reads_parsed_flag() {
        let cond = Condition {
            kind: Some(KindFilter::Remux),
            ..Default::default()
        };
        let mut r = rel("Movie BluRay REMUX");
        r.kind = ReleaseKind::BluRay;
        r.tags.remux = true;
        assert!(cond.matches(&r));
        r.tags.remux = false;
        assert!(!cond.matches(&r));
    }

    #[test]
    fn codec_alias_h265_deserializes_to_x265() {
        let cond: Condition = serde_json::from_str(r#"{"video_codec":"h265"}"#).unwrap();
        assert_eq!(cond.video_codec, Some(CodecFilter::X265));
    }

    #[test]
    fn min_and_max_size_bound_the_range() {
        let cond = Condition {
            min_size_bytes: Some(500),
            max_size_bytes: Some(2_000),
            ..Default::default()
        };
        let mut r = rel("m");
        r.size_bytes = 1_000;
        assert!(cond.matches(&r));
        r.size_bytes = 100;
        assert!(!cond.matches(&r));
        r.size_bytes = 5_000;
        assert!(!cond.matches(&r));
    }

    #[test]
    fn release_group_match_is_case_insensitive() {
        let cond = Condition {
            release_group: Some("nex".into()),
            ..Default::default()
        };
        let mut r = rel("m");
        r.tags.release_group = Some("NeX".into());
        assert!(cond.matches(&r));
        r.tags.release_group = Some("RARBG".into());
        assert!(!cond.matches(&r));
    }

    #[test]
    fn title_contains_is_case_insensitive() {
        let cond = Condition {
            title_contains: Some("dual".into()),
            ..Default::default()
        };
        assert!(cond.matches(&rel("Movie 1080p DUAL")));
        assert!(!cond.matches(&rel("Movie 1080p")));
    }

    #[test]
    fn title_matches_runs_the_regex() {
        let cond = Condition {
            title_matches: Some(r"(?i)\bS\d{2}E\d{2}\b".into()),
            ..Default::default()
        };
        assert!(cond.matches(&rel("Show S01E02 1080p")));
        assert!(!cond.matches(&rel("Movie 1080p")));
    }

    #[test]
    fn invalid_regex_never_matches_and_validate_reports_it() {
        let cond = Condition {
            title_matches: Some("(".into()),
            ..Default::default()
        };
        assert!(!cond.matches(&rel("anything (")));

        let rs = RuleSet {
            rules: vec![Rule {
                name: None,
                when: cond,
                add_score: 1,
                tag: None,
                reject: false,
            }],
        };
        let errs = rs.validate().unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("title_matches"));
    }

    #[test]
    fn legacy_flat_rules_json_still_loads_and_scores() {
        // `rules_json` salvo ANTES dos campos novos — só `audio`.
        let json = r#"{"rule":[{"name":"pt","when":{"audio":"pt-br"},"add_score":100,"tag":null,"reject":false}]}"#;
        let rs: RuleSet = serde_json::from_str(json).unwrap();
        assert_eq!(rs.rules.len(), 1);
        let mut r = rel("m");
        r.enrichment = Some(ReleaseEnrichment {
            audio_languages: vec![Language::PtBr],
            ..ReleaseEnrichment::default()
        });
        let out = crate::Engine::new(rs).evaluate(&r);
        assert_eq!(out.score.get(), 100);
    }

    #[test]
    fn unset_new_fields_serialize_without_clutter() {
        let c = Condition {
            audio: Some(AudioFilter::PtBr),
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("video_codec"));
        assert!(!json.contains("\"all\""));
        assert!(json.contains("pt-br"));
    }
}
