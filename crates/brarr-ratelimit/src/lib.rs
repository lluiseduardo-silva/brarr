//! Respeitar o ritmo que a API do outro lado anuncia.
//!
//! # Por que existe
//!
//! Medido na produção deste operador em 2026-09-09: os dois trackers
//! UNIT3D respondem `x-ratelimit-limit: 30` — trinta requisições por
//! minuto — em **toda** resposta autenticada, e o brarr descartava o
//! header. Um ciclo de varredura gasta as 25 buscas do orçamento num
//! laço sequencial sem pausa nenhuma; com ~290 ms por chamada isso põe
//! **25 requisições dentro de um minuto**, 83% do teto, a cada ciclo,
//! sem folga alguma. 128 minutos das últimas 48 h têm exatamente 25.
//!
//! Bastou uma varredura manual por título — 70 episódios, sem orçamento
//! nenhum, porque o teto do ciclo só governa a varredura agendada — para
//! empilhar 59 requisições num minuto e colher 29 × 429 num tracker, 29
//! no outro e 18 no terceiro. Quando os quatro providers falham juntos o
//! defeito é de quem chama.
//!
//! # O que ele faz
//!
//! Um balde por **host**, não por provider: dois providers podem
//! compartilhar a mesma origem, e é a origem que conta requisição. Cada
//! host guarda o instante em que a próxima requisição pode sair;
//! [`RateLimiter::acquire`] reserva esse instante e dorme até ele.
//!
//! O espaçamento vem do próprio servidor quando ele diz
//! ([`RateLimiter::observe`]), e do padrão do chamador enquanto não
//! disse. Um 429 com `Retry-After` cala o host pelo tempo exato que ele
//! pediu — que é a única coisa aqui que não é estimativa.
//!
//! # O que ele não faz
//!
//! **Não permite rajada.** Um balde com capacidade para acumular
//! créditos é exatamente o que produziu o incidente: o ciclo passava
//! minutos sem pedir nada e depois gastava tudo de uma vez, o que é
//! indistinguível de abuso visto do outro lado. O espaçamento é mínimo e
//! uniforme.
//!
//! Não substitui o retry do chamador. Ele diz *quando* a requisição pode
//! sair; se ela deve sair de novo depois de falhar é decisão do client.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{debug, warn};

/// A janela que os headers `X-RateLimit-*` descrevem.
///
/// Nenhum header diz qual é — `X-RateLimit-Reset` só aparece no 429, e é
/// tarde demais para servir de base. Um minuto é o que o
/// `ThrottleRequests` do Laravel usa por padrão, é o que os dois UNIT3D
/// medidos anunciam, e é a suposição conservadora: uma janela real
/// **maior** que esta faz o espaçamento sobrar, nunca faltar.
pub const WINDOW: Duration = Duration::from_secs(60);

/// Quanto do teto anunciado se deixa de lado.
///
/// Espaçar exatamente `WINDOW / limit` produz a taxa do limite e nada
/// mais: o `ThrottleRequests` conta numa janela **fixa**, então trinta
/// requisições espaçadas de dois segundos ainda podem cair 31 dentro de
/// um mesmo balde por alinhamento de relógio. 10% é a folga que faz o
/// caso normal não encostar na borda.
const HEADROOM: f64 = 1.1;

/// Quanto tempo um host fica calado depois de um 429 que não disse
/// `Retry-After`.
///
/// Uma janela inteira. É o único palpite seguro: sem o header não há
/// como saber quando o balde vira, e insistir cedo é o que transforma um
/// 429 em vários.
const BLIND_PENALTY: Duration = WINDOW;

/// O que uma resposta contou sobre o limite.
///
/// Deliberadamente sem tipo de HTTP nenhum: este crate não conhece
/// `reqwest` nem `http`, pela mesma regra de fronteira que mantém o
/// parser de `MediaInfo` longe do cliente HTTP. Quem chama traduz os
/// headers com [`observe_headers`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Observed {
    /// Status HTTP da resposta.
    pub status: u16,
    /// `X-RateLimit-Limit` — quantas requisições cabem na janela.
    pub limit: Option<u32>,
    /// `X-RateLimit-Remaining` — quantas ainda cabem.
    pub remaining: Option<u32>,
    /// `Retry-After`, quando veio em segundos.
    pub retry_after: Option<Duration>,
}

impl Observed {
    /// A resposta é um 429.
    #[must_use]
    pub const fn is_throttled(&self) -> bool {
        self.status == 429
    }
}

/// Ler os headers de limite de uma resposta.
///
/// Aceita qualquer par `(nome, valor)`; o nome é comparado sem
/// diferenciar maiúsculas, porque HTTP/2 entrega tudo minúsculo e
/// HTTP/1.1 não promete nada.
///
/// `Retry-After` também admite uma data HTTP, que **não** é interpretada
/// aqui: o valor vira `None` e o chamador cai na penalidade cega. Os dois
/// servidores medidos mandam segundos, e uma data mal convertida
/// atrasaria um host por horas.
pub fn observe_headers<'a, I>(status: u16, headers: I) -> Observed
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut out = Observed {
        status,
        ..Observed::default()
    };
    for (name, value) in headers {
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "x-ratelimit-limit" => out.limit = value.parse().ok(),
            "x-ratelimit-remaining" => out.remaining = value.parse().ok(),
            "retry-after" => out.retry_after = value.parse().ok().map(Duration::from_secs),
            _ => {}
        }
    }
    out
}

/// O que se sabe sobre um host.
#[derive(Debug)]
struct Host {
    /// Quando a próxima requisição pode sair.
    next_allowed: Instant,
    /// Espaçamento em vigor.
    spacing: Duration,
    /// Se o espaçamento veio do servidor ou ainda é o padrão.
    learned: bool,
}

/// Espaçador por host.
///
/// Uma instância por processo, compartilhada por todos os clients: dois
/// clients apontados para a mesma origem que espaçassem em separado
/// dobrariam a taxa que cada um acha que está fazendo.
#[derive(Debug)]
pub struct RateLimiter {
    hosts: Mutex<HashMap<String, Host>>,
    default_spacing: Duration,
}

impl RateLimiter {
    /// Um limitador cujo espaçamento inicial é `default_spacing`.
    ///
    /// Vale só até o primeiro `X-RateLimit-Limit` daquele host. Um
    /// servidor que nunca manda o header — os dois Newznab medidos não
    /// mandam, e um deles ainda assim devolveu 429 — fica neste valor
    /// para sempre, então ele é um limite de verdade e não um
    /// aquecimento.
    #[must_use]
    pub fn new(default_spacing: Duration) -> Self {
        Self {
            hosts: Mutex::new(HashMap::new()),
            default_spacing,
        }
    }

    /// Esperar a vez de falar com `host`.
    ///
    /// Reserva a vaga **antes** de dormir e larga o cadeado no mesmo
    /// movimento: dois chamadores concorrentes pegam instantes
    /// diferentes e saem em fila, em vez de acordarem juntos no mesmo.
    pub async fn acquire(&self, host: &str) {
        let wait = {
            let Ok(mut hosts) = self.hosts.lock() else {
                // Um cadeado envenenado significa que outra tarefa entrou
                // em pânico segurando-o. Falhar aberto é o certo: o pior
                // caso é uma requisição sem espaçamento, contra derrubar
                // toda busca do processo.
                warn!(target: "brarr_ratelimit", host, "cadeado envenenado; seguindo sem espaçar");
                return;
            };
            let entry = self.entry(&mut hosts, host);
            let now = Instant::now();
            let at = entry.next_allowed.max(now);
            entry.next_allowed = at + entry.spacing;
            at.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            debug!(
                target: "brarr_ratelimit",
                host,
                wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                "esperando a vez"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// Contar ao limitador o que a resposta disse.
    ///
    /// Chamado em toda resposta, inclusive nas de erro: um 429 é
    /// justamente a que mais tem a dizer.
    pub fn observe(&self, host: &str, observed: &Observed) {
        let Ok(mut hosts) = self.hosts.lock() else {
            warn!(target: "brarr_ratelimit", host, "cadeado envenenado; ignorando os headers");
            return;
        };
        let entry = self.entry(&mut hosts, host);

        if let Some(limit) = observed.limit.filter(|l| *l > 0) {
            let spacing = spacing_for(limit);
            if !entry.learned || entry.spacing != spacing {
                debug!(
                    target: "brarr_ratelimit",
                    host,
                    limit,
                    spacing_ms = u64::try_from(spacing.as_millis()).unwrap_or(u64::MAX),
                    "espaçamento aprendido do servidor"
                );
            }
            entry.spacing = spacing;
            entry.learned = true;
        }

        let now = Instant::now();
        if observed.is_throttled() {
            let penalty = observed.retry_after.unwrap_or(BLIND_PENALTY);
            entry.next_allowed = entry.next_allowed.max(now + penalty);
            warn!(
                target: "brarr_ratelimit",
                host,
                penalty_s = penalty.as_secs(),
                explicit = observed.retry_after.is_some(),
                "429; o host fica calado"
            );
        } else if observed.remaining == Some(0) {
            // Não é um 429 ainda, e a próxima seria. Sem
            // `X-RateLimit-Reset` — que só vem no 429 — a janela inteira
            // é o único palpite que não erra para menos.
            entry.next_allowed = entry.next_allowed.max(now + WINDOW);
            warn!(target: "brarr_ratelimit", host, "cota zerada; esperando a janela virar");
        }
    }

    /// Espaçamento em vigor para `host`, para teste e diagnóstico.
    #[must_use]
    pub fn spacing_of(&self, host: &str) -> Option<Duration> {
        self.hosts
            .lock()
            .ok()
            .and_then(|hosts| hosts.get(host).map(|h| h.spacing))
    }

    /// A linha de `host`, criada no padrão do limitador se for a
    /// primeira vez.
    fn entry<'a>(&self, hosts: &'a mut HashMap<String, Host>, host: &str) -> &'a mut Host {
        hosts.entry(host.to_owned()).or_insert_with(|| Host {
            next_allowed: Instant::now(),
            spacing: self.default_spacing,
            learned: false,
        })
    }
}

/// O espaçamento que um teto anunciado impõe, com folga.
fn spacing_for(limit: u32) -> Duration {
    Duration::from_secs_f64(WINDOW.as_secs_f64() * HEADROOM / f64::from(limit))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "os testes afirmam caminhos felizes")]

    use super::*;

    #[test]
    fn os_headers_do_unit3d_sao_lidos() {
        // Captura real de capybarabr.com e samaritano.cc em 2026-09-09,
        // resposta 200 autenticada. HTTP/2, portanto minúsculo.
        let o = observe_headers(
            200,
            [
                ("date", "Wed, 09 Sep 2026 06:41:52 GMT"),
                ("x-ratelimit-limit", "30"),
                ("x-ratelimit-remaining", "29"),
                ("cf-cache-status", "DYNAMIC"),
            ],
        );
        assert_eq!(o.limit, Some(30));
        assert_eq!(o.remaining, Some(29));
        assert_eq!(o.retry_after, None);
        assert!(!o.is_throttled());
    }

    #[test]
    fn o_nome_do_header_nao_diferencia_caixa() {
        let o = observe_headers(429, [("Retry-After", "17"), ("X-RateLimit-Limit", "30")]);
        assert_eq!(o.retry_after, Some(Duration::from_secs(17)));
        assert_eq!(o.limit, Some(30));
        assert!(o.is_throttled());
    }

    /// `Retry-After` admite uma data HTTP. Convertê-la mal atrasaria um
    /// host por horas, então um valor que não é um inteiro de segundos
    /// não vira duração nenhuma.
    #[test]
    fn uma_data_em_retry_after_nao_vira_duracao() {
        let o = observe_headers(429, [("retry-after", "Wed, 09 Sep 2026 06:42:00 GMT")]);
        assert_eq!(o.retry_after, None);
    }

    #[test]
    fn trinta_por_minuto_viram_dois_segundos_com_folga() {
        let s = spacing_for(30);
        assert!(
            s > Duration::from_secs(2) && s < Duration::from_millis(2_500),
            "{s:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_primeira_sai_na_hora_e_a_segunda_espera() {
        let limiter = RateLimiter::new(Duration::from_secs(1));
        let started = Instant::now();
        limiter.acquire("a.example").await;
        assert_eq!(started.elapsed(), Duration::ZERO, "a primeira não espera");
        limiter.acquire("a.example").await;
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }

    /// O balde é por host. Um tracker lento não pode atrasar outro — o
    /// fan-out fala com todos ao mesmo tempo de propósito.
    #[tokio::test(start_paused = true)]
    async fn hosts_diferentes_nao_esperam_um_pelo_outro() {
        let limiter = RateLimiter::new(Duration::from_secs(5));
        let started = Instant::now();
        limiter.acquire("a.example").await;
        limiter.acquire("b.example").await;
        limiter.acquire("c.example").await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// O caso que motivou o crate: 25 buscas seguidas contra um host que
    /// anuncia 30/min. Antes elas saíam em ~7 s.
    #[tokio::test(start_paused = true)]
    async fn um_ciclo_inteiro_cabe_na_janela_anunciada() {
        let limiter = RateLimiter::new(Duration::from_millis(500));
        limiter.observe(
            "capybarabr.com",
            &observe_headers(200, [("x-ratelimit-limit", "30")]),
        );

        let started = Instant::now();
        for _ in 0..25 {
            limiter.acquire("capybarabr.com").await;
        }
        let spent = started.elapsed();
        // 24 esperas de ~2,2 s: mais de meia janela e menos de uma.
        assert!(
            spent > Duration::from_secs(50) && spent < WINDOW,
            "um ciclo levou {spent:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn um_429_com_retry_after_cala_o_host_pelo_tempo_pedido() {
        let limiter = RateLimiter::new(Duration::from_millis(100));
        limiter.acquire("x.example").await;
        limiter.observe(
            "x.example",
            &observe_headers(429, [("retry-after", "30"), ("x-ratelimit-limit", "30")]),
        );
        let started = Instant::now();
        limiter.acquire("x.example").await;
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    /// Sem `Retry-After` não há como saber quando o balde vira, e
    /// insistir cedo é o que transforma um 429 em vários.
    #[tokio::test(start_paused = true)]
    async fn um_429_mudo_custa_uma_janela() {
        let limiter = RateLimiter::new(Duration::from_millis(100));
        limiter.observe("y.example", &observe_headers(429, []));
        let started = Instant::now();
        limiter.acquire("y.example").await;
        assert_eq!(started.elapsed(), WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn cota_zerada_espera_a_janela_sem_precisar_do_429() {
        let limiter = RateLimiter::new(Duration::from_millis(100));
        limiter.observe(
            "z.example",
            &observe_headers(
                200,
                [("x-ratelimit-limit", "30"), ("x-ratelimit-remaining", "0")],
            ),
        );
        let started = Instant::now();
        limiter.acquire("z.example").await;
        assert_eq!(started.elapsed(), WINDOW);
    }

    /// Um servidor que não anuncia nada — os dois Newznab medidos — fica
    /// no padrão do chamador, que por isso é um limite e não um
    /// aquecimento.
    #[tokio::test(start_paused = true)]
    async fn sem_header_o_padrao_permanece() {
        let limiter = RateLimiter::new(Duration::from_secs(1));
        limiter.observe(
            "api.nzbgeek.info",
            &observe_headers(200, [("vary", "Accept-Encoding")]),
        );
        assert_eq!(
            limiter.spacing_of("api.nzbgeek.info"),
            Some(Duration::from_secs(1))
        );
    }
}
