//! `brarr-orchestrator` — servidor `gRPC` + UI web administrativa.
//!
//! Junta tudo: expõe a API `gRPC` consumida pelo `CLI` e por integrações
//! externas, hospeda a UI web (Axum + Askama + `HTMX`, Tailwind via `CDN`
//! por enquanto, assets via `tower-http::services::ServeDir`), coordena
//! os trackers via `brarr-tracker-unit3d`/`brarr-plugin-host` e delega
//! decisões para `brarr-decision-service`.
//!
//! Páginas previstas (implementar sob demanda):
//! - `/` Dashboard (decisões pendentes, grabs recentes)
//! - `/trackers` Gerenciamento de trackers
//! - `/releases` Histórico de decisões
//! - `/settings` Configurações globais
//!
//! Status: stub. Não implementar até a Fase 6+.

fn main() {
    // Phase 6+ stub — entry point intencionalmente vazio.
}
