//! `brarr-mediainfo` — parser de dumps textuais do `MediaInfo`.
//!
//! Responsabilidade: transformar a saída textual bruta do `mediainfo`
//! (ou o campo `mediainfo` retornado por trackers `UNIT3D`) em estruturas
//! tipadas (`ParsedMediaInfo`, `AudioTrack`, `SubtitleTrack`, `VideoTrack`,
//! `Language`). Faz normalização de idiomas (ex.: `Portuguese (BR)` e
//! `Portuguese` + `Title: Brazilian` → `pt-BR`).
//!
//! Status: stub. Implementação concreta vem na Fase 2.
