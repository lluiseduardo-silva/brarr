//! `brarr-decision-service` — motor de regras para escolher qual release pegar.
//!
//! Recebe um conjunto de releases candidatos (já enriquecidos com info
//! de mídia) e aplica as regras configuradas pelo usuário (idiomas
//! obrigatórios, codecs preferidos, resolução mínima, tracker priorizado,
//! tamanho máximo, etc.) para produzir um ranking + uma decisão final.
//! Não fala `HTTP`, não conhece `UNIT3D` especificamente.
//!
//! Status: stub. Não implementar até a Fase 6+.
