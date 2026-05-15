# brarr

> Agregador de busca em trackers privados focado em mídia com áudio e legenda em
> português brasileiro. Procura em múltiplos trackers UNIT3D em paralelo,
> normaliza os MediaInfos retornados, pontua cada release pela qualidade da
> faixa PT-BR (áudio dublado, legenda forçada, legenda completa, etc.) e
> recomenda o melhor candidato segundo regras configuráveis.

**Status:** Fase 1 — esqueleto do monorepo. Apenas os stubs dos crates existem;
implementação concreta começa na Fase 2 com o parser de MediaInfo.

A intenção e os princípios do projeto vivem em [`INITIAL_PROMPT.md`](INITIAL_PROMPT.md).
Esse arquivo é a **fonte da verdade** sobre escopo, fases, anti-padrões e estilo —
consulte antes de adicionar dependências, mudar a arquitetura ou subir uma feature
nova.

## Por que existe

Conteúdo bem dublado/legendado em PT-BR é difícil de achar e o estado da arte
hoje é abrir 5 trackers no navegador e cruzar manualmente os MediaInfos.
O brarr automatiza essa varredura num único comando (ou painel web) e deixa
explícita a heurística de "qual release é melhor pra mim".

Além disso, é um projeto pessoal de aprendizado de Rust — async, error handling
tipado, design de APIs, plugins WASM, gRPC. **Qualidade e entendimento valem
mais que velocidade.**

## Arquitetura em uma página

Monorepo Cargo com crates de responsabilidade única:

| Crate | Papel | Fase |
|-------|-------|------|
| `brarr-mediainfo` | Parser de dumps textuais do MediaInfo → tipos | 2 |
| `brarr-core` | Tipos de domínio compartilhados (`Release`, `Language`, ...) | 3 |
| `brarr-tracker-unit3d` | Cliente HTTP para a API UNIT3D | 4 |
| `brarr-cli` | CLI `brarr search --tmdb <id>` | 5 |
| `brarr-orchestrator` | gRPC + UI web (Axum + Askama + HTMX + Tailwind) | 6+ |
| `brarr-decision-service` | Motor de regras para escolher release | 6+ |
| `brarr-plugin-host` | Sandbox WASM para scrapers customizados | 6+ |

Fronteiras rígidas: parser não conhece HTTP, cliente HTTP não conhece regras,
regras não conhecem trackers específicos. Detalhes em
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Pré-requisitos

- **Rust** 1.85 ou superior (precisa de `edition = "2024"`). Recomendado via
  [`rustup`](https://rustup.rs/).
- Plataformas alvo: Linux, macOS, Windows.

## Como rodar (hoje)

Tudo aqui ainda é stub. Os comandos abaixo já funcionam — só não fazem nada útil:

```bash
# Compila todos os crates do workspace
cargo build --workspace

# Roda todos os testes (incluindo doctests)
cargo test --workspace --all-targets

# Lint rigoroso (clippy::pedantic + lints customizados)
cargo clippy --workspace --all-targets -- -D warnings

# Verifica formatação
cargo fmt --all -- --check
```

Para rodar um teste específico:

```bash
cargo test -p brarr-mediainfo nome_do_teste
```

## Como contribuir / workflow

O workflow esperado para qualquer feature está descrito em `INITIAL_PROMPT.md`,
seção *Workflow esperado*. Em resumo:

1. Leia a seção relevante de `INITIAL_PROMPT.md`.
2. Confirme entendimento se houver ambiguidade.
3. Escreva os testes primeiro (TDD), usando fixtures reais em `tests/fixtures/`.
4. Implemente o mínimo para passar.
5. Rode `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
6. Commit com [Conventional Commits](https://www.conventionalcommits.org/)
   (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`).

## Princípios não negociáveis

- Test-first com edge cases documentados.
- Sem `unwrap()` / `expect()` fora de testes.
- Erros tipados: `thiserror` em libs, `anyhow` em binários. Nunca `Box<dyn Error>`.
- Logging via `tracing` (sem `println!` em código de produção).
- Tipos como documentação: newtypes, enums, `Option<T>` no lugar de sentinel values.
- Sem abstrações prematuras — trait só com 2+ implementações reais.

## Licença

`MIT OR Apache-2.0` (a definir formalmente — placeholder no `Cargo.toml`).
