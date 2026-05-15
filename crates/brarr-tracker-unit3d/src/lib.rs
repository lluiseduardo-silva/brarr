//! `brarr-tracker-unit3d` — cliente `HTTP` para a API `UNIT3D`.
//!
//! Cobre os endpoints relevantes para busca (`/api/torrents/filter`,
//! `/api/torrents/{id}`), faz a desserialização das respostas `JSON` em
//! tipos de `brarr-core`, e integra com o parser de `brarr-mediainfo`
//! para enriquecer cada release com info de áudio/legenda. É
//! **library pura** — quem orquestra retries, paralelismo entre
//! trackers ou políticas de cache é o `brarr-cli` / `brarr-orchestrator`.
//!
//! Status: stub. Implementação concreta vem na Fase 4.
