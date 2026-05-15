# Arquitetura do brarr

> Este documento descreve **como** o sistema é organizado e **por quê**.
> A intenção do produto e os princípios obrigatórios vivem em
> [`../INITIAL_PROMPT.md`](../INITIAL_PROMPT.md) — esse aqui é o complemento
> técnico, focado em estrutura de código e fluxo de dados.

## Visão geral

O brarr é dividido em **dois mundos**:

1. **Library world** — crates puros, sem I/O global, com APIs tipadas e
   testáveis isoladamente. São o coração reusável.
2. **Binary world** — `brarr-cli` e `brarr-orchestrator` orquestram os
   crates de library, fazem I/O, configuração, logging, e expõem
   interfaces ao usuário (CLI e Web/gRPC).

Essa divisão existe para que regras, parsing e clientes de tracker sejam
exercitados em testes determinísticos sem subir runtime async, abrir
sockets ou ler arquivos.

## Fluxo de dados (alvo da Fase 5)

```
        ┌────────────────────┐
        │  brarr-cli         │  CLI / TOML config
        └─────────┬──────────┘
                  │ (sync de chamadas async via tokio runtime)
                  ▼
        ┌────────────────────┐
        │ brarr-tracker-     │  HTTP/JSON (reqwest)
        │ unit3d             │
        └─────────┬──────────┘
                  │ resposta JSON crua
                  ▼
        ┌────────────────────┐
        │ brarr-mediainfo    │  parse(text) → ParsedMediaInfo
        └─────────┬──────────┘
                  │ tipos puros
                  ▼
        ┌────────────────────┐
        │ brarr-core         │  Release enriquecido + score
        └────────────────────┘
```

Na Fase 6+ entram `brarr-decision-service` (regras), `brarr-plugin-host`
(scrapers WASM) e `brarr-orchestrator` (gRPC + Web UI) — sem alterar o
contrato dos crates de library.

## Crates: responsabilidade e fronteira

### `brarr-mediainfo` (Fase 2)
Transforma o texto bruto do MediaInfo (mesma saída que o programa
`mediainfo` produz, e que vem dentro do campo `mediainfo` do JSON UNIT3D)
em estruturas tipadas. **Não conhece HTTP, não conhece tracker.**
Normaliza idiomas para um enum `Language` com variantes nomeadas para os
casos comuns (`PtBr`, `PtPt`, `En`, ...) e fallback `Other(String)`.

### `brarr-core` (Fase 3)
Tipos de domínio: `Release`, `TrackerSource`, `AudioRequirement`,
`DecisionScore`, `Language` (reexportado). Conversões entre o que o
parser produz e o que o resto do sistema consome. **Sem deps de runtime
async, sem reqwest, sem axum.**

### `brarr-tracker-unit3d` (Fase 4)
Implementa o contrato `TrackerProvider` (ainda a definir) para a API
UNIT3D. Faz HTTP com `reqwest`, desserializa JSON, alimenta o parser
de mediainfo, devolve `Vec<Release>`. Single-responsibility: **não
prioriza nem decide nada, só busca e converte.**

### `brarr-cli` (Fase 5)
Lê config TOML (`~/.config/brarr/config.toml` ou caminho via flag),
varre todos os trackers configurados em paralelo (`futures::join_all`
ou similar), aplica scoring hardcoded simples, imprime resultado
formatado. Single binary, tokio runtime, `clap` para args.

### `brarr-decision-service` (Fase 6+)
Motor de regras declarativas em cima de `Release`. Substitui o scoring
hardcoded do CLI por algo configurável (regras por usuário, por
biblioteca, por tipo de mídia). Provavelmente exposto via gRPC para
permitir clientes externos.

### `brarr-orchestrator` (Fase 6+)
- Servidor gRPC (via `tonic`) com o contrato consumido pelo CLI e por
  integrações.
- UI web administrativa (Axum + Askama + HTMX, Tailwind via CDN no MVP,
  build pipeline depois se justificar). Templates em
  `crates/brarr-orchestrator/templates/`. Assets estáticos via
  `tower_http::services::ServeDir`.
- Persistência (a definir: SQLite via `sqlx` é o candidato natural).

### `brarr-plugin-host` (Fase 6+)
Carrega plugins WASM que implementam um equivalente do `TrackerProvider`
para fontes não-UNIT3D ou customizações. Runtime a definir
(`wasmtime` provável). Capability-based: o host expõe APIs específicas
(fetch HTTP, logging, KV) — o plugin não enxerga mais que isso.

## Decisões arquiteturais já tomadas

### Por que monorepo Cargo (workspace) ao invés de crates separados
- Refatoração e renomeação cruzam fronteiras com facilidade.
- Versionamento alinhado (`version.workspace = true`).
- `cargo build --workspace` cobre tudo num comando.
- Custo de "split em repos separados" só se paga quando tiver consumidor
  externo real.

### Por que SSR + HTMX e não SPA (Leptos/Yew/Dioxus)
- Evita build pipeline de frontend num projeto de aprendizado de
  *backend* Rust.
- HTMX cobre 90% das interações que uma UI administrativa precisa
  (paginação, modal, atualização parcial) sem JS próprio.
- Migrar depois para SPA é fácil; o contrário não é.

### Por que `thiserror` em libs e `anyhow` em binários
- Lib que devolve `anyhow::Error` força quem chama a perder informação
  de tipo. `thiserror` mantém erros enumeráveis e `match`-áveis.
- Binário não precisa expor erro estruturado pra ninguém — basta
  agregar com `?` e imprimir com `:#}`. `anyhow` faz isso sem dor.

### Por que `clippy::pedantic` global
- Custo: alguns lints ruidosos. Benefício: aprende-se Rust idiomático
  no piloto automático. Em projeto pedagógico, isso vale o ruído.
- Exceções pontuais devem usar `#[allow(clippy::xxx, reason = "...")]`
  com a razão documentada, nunca um `allow` global silencioso.

### Por que `resolver = "3"`
- Default para edition 2024. Melhora resolução de features em
  workspaces (não unifica feature-flags entre target/host).

## Decisões em aberto (revisar quando relevante)

- Banco de dados (`brarr-orchestrator` Fase 6+): SQLite via `sqlx`
  parece suficiente. Postgres só se aparecer requisito multi-instância.
- Runtime WASM (`brarr-plugin-host`): `wasmtime` é o candidato; `wasmer`
  é alternativa.
- Formato dos plugins: WIT/Component Model vs interface custom. WIT
  parece a aposta certa, mas adiar a decisão até ter o primeiro plugin
  real.
- Autenticação da UI web: provavelmente single-user com senha no MVP.
  OAuth / multi-user só se justificar.

## Anti-padrões explicitamente rejeitados

(Vindos de `INITIAL_PROMPT.md`, repetidos aqui para ficarem visíveis no
review de PR.)

- `Box<dyn Error>` em qualquer código de produção.
- `.clone()` defensivo (use refs, lifetimes, `Cow` quando necessário).
- `String` em assinatura quando `&str` resolve.
- `unwrap()` / `expect()` fora de testes.
- Trait com uma única implementação.
- Frameworks "mágicos" que escondem o que tá acontecendo.
- Dependências pesadas (Actix, Diesel) quando crate menor resolve.
