//! `brarr-core` — tipos de domínio compartilhados entre os demais crates.
//!
//! Aqui vivem as estruturas centrais (`Release`, `TrackerSource`,
//! `AudioRequirement`, `DecisionScore`, etc.) e as conversões entre o
//! dado bruto vindo do parser (`brarr-mediainfo`) e a forma enriquecida
//! consumida pelo `CLI`/orchestrator. Nada de `HTTP`, nada de regras, nada
//! de tracker-específico.
//!
//! Status: stub. Implementação concreta vem na Fase 3.
