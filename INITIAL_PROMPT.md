## Princípios obrigatórios

### Engenharia
1. **Test-first sempre que possível.** Antes de implementar lógica, escreva
   os testes. Use os arquivos em `tests/fixtures/` como dados reais.
2. **Edge cases são obrigatórios nos testes.** Strings vazias, valores
   ausentes, tipos inesperados, campos opcionais, encoding estranho, dados
   malformados. Documente o que cada teste cobre.
3. **Sem `unwrap()` em código de produção.** Apenas em testes ou exemplos
   didáticos comentados como tal.
4. **Erros tipados com `thiserror`** nas crates de biblioteca. `anyhow`
   apenas nos binários (orchestrator, cli).
5. **Documentação rustdoc em tudo público.** Use `///`, inclua `# Examples`
   rodáveis quando útil. Privates podem ter `//` se a intenção for óbvia.
6. **Tracing structured logging.** Cada operação importante tem um span.
   Logs em níveis apropriados (debug, info, warn, error). Nada de
   `println!` em código de produção.

### Arquitetura
1. **Separação clara de responsabilidades.** Parser não conhece HTTP.
   Cliente HTTP não conhece regras. Regras não conhecem trackers
   específicos.
2. **Tipos como documentação.** Use newtypes (`struct TmdbId(u32)` em vez
   de `u32`), enums em vez de strings mágicas, `Option<T>` em vez de
   sentinel values.
3. **Traits para abstrair fronteiras.** `trait TrackerProvider` define o
   contrato, implementações concretas (UNIT3D direto, plugin WASM, etc.)
   são detalhes.
4. **Sem abstrações prematuras (YAGNI).** Se só tem uma implementação,
   talvez não precise de trait ainda. Refatorar depois é fácil em Rust.

### Estilo
- `rustfmt.toml` configurado, comportamento padrão da comunidade
- `clippy.toml` com lints rigorosos, idealmente `clippy::pedantic` ativado
  globalmente com exceções pontuais documentadas
- Comentários de docstrings em **inglês** (convenção da comunidade Rust)
- README e documentação de alto nível em **português** (público alvo BR)
- Mensagens de erro voltadas ao usuário em **português**
- Identificadores em inglês (convenção universal)

## Plano de implementação faseado

O esqueleto completo de monorepo deve ser criado no setup inicial, mas a
**implementação concreta** segue uma ordem específica pra eu aprender
incrementalmente sem ser soterrado por complexidade:

### Fase 1 — Setup do workspace (faça agora completamente)
- Criar workspace Cargo com todos os crates listados (mesmo que alguns
  fiquem com apenas `lib.rs` vazio comentando o que vai vir)
- Configurar `Cargo.toml` raiz com `[workspace]`, MSRV, lints globais
- Configurar `rustfmt.toml`, `clippy.toml`, `.gitignore`
- README inicial em português explicando o projeto e como rodar
- Documentar a arquitetura em `docs/ARCHITECTURE.md`

### Fase 2 — Parser de MediaInfo (implementar completamente)
- Crate `brarr-mediainfo` totalmente funcional
- Tipos `ParsedMediaInfo`, `AudioTrack`, `SubtitleTrack`, `VideoTrack`,
  `Language` (enum com variantes PT_BR, PT_PT, EN, etc., mais
  `Other(String)` pra outros idiomas)
- Função `parse(text: &str) -> Result<ParsedMediaInfo, ParseError>`
- Normalização de idiomas (tratar `Portuguese (BR)`, `Portuguese` +
  `Title: Brazilian`, etc.)
- Testes extensivos com edge cases:
    - Os dois MediaInfos reais que vou fornecer (vnlls 1080p e sh4down 2160p)
    - Strings vazias
    - Strings sem seções
    - Seções sem campos
    - Campos sem valor
    - Faixas forced vs não-forced
    - Idiomas ambíguos
    - Múltiplas faixas do mesmo idioma
    - Encoding com `\r\n` vs `\n`
- Documentação rustdoc com exemplo rodável

### Fase 3 — Tipos core compartilhados (implementar completamente)
- Crate `brarr-core` com tipos `Release`, `TrackerSource`, `AudioRequirement`,
  `DecisionScore`, etc.
- Conversões `From<ParsedMediaInfo> for ReleaseEnrichment` ou similar
- Testes de invariantes (não pode ter release sem name, score válido entre
  limites, etc.)

### Fase 4 — Cliente UNIT3D (implementar completamente)
- Crate `brarr-tracker-unit3d` como library pura (sem ser plugin WASM ainda)
- Cliente async com reqwest
- Suporte a `/api/torrents/filter`, `/api/torrents/{id}`
- Conversão de resposta JSON pra tipos `brarr-core`
- Integração com parser de MediaInfo da Fase 2
- Testes unitários com respostas mockadas (use os JSONs reais que forneço
  como fixtures)
- Documentação explicando como conseguir token e configurar

### Fase 5 — CLI inicial (implementar completamente)
- Crate `brarr-cli` com subcomando `search`
- Aceita configuração via arquivo TOML (lista de trackers com URLs e tokens)
- Comando: `brarr search --tmdb 603` busca em todos os trackers configurados
- Output formatado mostrando releases encontrados, com scores baseados em
  regras hardcoded simples ("tem áudio PT-BR" pesa +100, "tem só legenda
  PT-BR" pesa +50, etc.)
- Logging via tracing pra debug

### Fase 6+ — gRPC, decision service, plugin host, orchestrator
**Não implementar ainda.** Deixar os crates correspondentes com stub
mínimo (`lib.rs` com comentário `// TODO: Phase 6`). Eu vou pedir
explicitamente quando quiser avançar.

## Workflow esperado

1. Quando eu pedir uma feature ou ajuste:
    - Você primeiro confirma entendimento se houver ambiguidade
    - Escreve os testes primeiro (TDD), eu reviso, você implementa
    - Roda `cargo test`, `cargo clippy --all-targets -- -D warnings`,
      `cargo fmt --check` antes de considerar pronto
    - Mostra o diff e explica o que foi feito

2. Quando aparecer conceito Rust que pode ser novo pra mim:
    - **Comente inline** explicando (ou em docstring se for público)
    - Conceitos típicos: ownership/borrowing, lifetimes explícitos,
      trait bounds complexos, async-trait, Pin/Box, generics com `impl Trait`,
      macros derive customizadas
    - Links pra documentação oficial (`https://doc.rust-lang.org/...`) quando
      introduzir um conceito pela primeira vez

3. Decisões arquiteturais devem ser documentadas:
    - Por que escolheu certa estrutura
    - Que alternativas considerou
    - Em quais condições mudaria de ideia
    - Use comentários no código ou seção em `docs/ARCHITECTURE.md`

4. Mensagens de commit em inglês, conventional commits
   (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`).

## Anti-padrões a evitar

- `Box<dyn Error>` como tipo de erro padrão (use thiserror/anyhow apropriadamente)
- `.clone()` defensivo (use referências, lifetimes, Cow quando necessário)
- `String` quando `&str` resolve (mas explique o trade-off quando relevante)
- `unwrap()` ou `expect()` fora de testes
- Abstrações com uma única implementação ("trait com uma struct")
- Arquivos monolíticos gigantes — quebrar por responsabilidade
- Dependências pesadas quando crate pequeno resolve (não trazer Actix se
  Axum mínimo basta, não trazer Diesel se SQLx ou sqlite simples basta)
- Frameworks "mágicos" que escondem o que tá acontecendo

## Dados de teste reais

Vou fornecer dois JSONs reais de respostas da API UNIT3D (locadora.cc e
capybarabr.com) pro mesmo filme (Matrix 1999). Use eles como fixtures nos
testes. Os MediaInfos brutos dentro desses JSONs devem alimentar o parser.

Casos importantes que esses dados cobrem:
- Idiomas em formato `Portuguese (BR)` (locadora) vs `Portuguese` +
  `Title: Brazilian Portuguese` (capybara) — parser deve normalizar ambos
  pra `pt-BR`
- Faixas forced + completas separadas
- Múltiplas legendas em vários idiomas (capybara tem 2, locadora tem 28+)
- Codecs diferentes (AVC vs HEVC, AC-3 vs E-AC-3 vs Atmos)
- Resoluções diferentes (1080p vs 2160p HDR)

## O que eu espero da primeira mensagem sua

1. Confirmação do entendimento do projeto e da abordagem
2. Setup completo do workspace (Fase 1) — todos os arquivos criados
3. Pergunta clara sobre o que implementar primeiro da Fase 2 em diante
4. Se algo do prompt foi ambíguo ou se você sugere ajustes na arquitetura,
   levantar antes de codar

## Filosofia geral

Esse é um projeto pessoal de aprendizado e produto. Prazer de programar
importa mais que velocidade. Qualidade importa mais que features. Eu
prefiro **entender profundamente** o que estamos fazendo a **ter pronto
rápido sem entender**.

Quando estiver em dúvida sobre trade-offs, pergunte. Quando achar que
estou tomando decisão errada, fale. Você é meu pair programmer, não meu
executor.

## Interface web

A ferramenta terá interface web administrativa desde o início (mesmo que
mínima nas fases iniciais). Stack escolhida:

- **Framework web:** Axum (mesmo time do tokio, integra com tonic)
- **Templates:** Askama (type-safe, compile-time)
- **Interatividade:** HTMX para AJAX/atualizações parciais sem JavaScript próprio
- **CSS:** Tailwind via CDN inicialmente; build pipeline depois se necessário
- **Assets estáticos:** servidos via tower-http ServeDir

A interface web ficará no crate `brarr-orchestrator` em módulo `web/`.
Templates HTML em `crates/brarr-orchestrator/templates/`.

Não usar Leptos, Dioxus, Yew ou qualquer framework full-WASM no MVP.
Filosofia é SSR + HTMX, evitando build pipeline de frontend.

Páginas iniciais a planejar (implementar conforme demanda):
- `/` Dashboard com decisões pendentes e grabs recentes
- `/trackers` Gerenciar trackers configurados
- `/releases` Histórico de decisões
- `/settings` Configurações globais (regras default, idiomas, etc.)
---

Vamos começar.