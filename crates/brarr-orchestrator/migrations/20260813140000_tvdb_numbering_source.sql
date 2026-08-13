-- # `'tvdb'` faltava no CHECK
--
-- `20260813130000` criou a coluna com quatro valores permitidos, e a
-- fonte `tvdb` foi acrescentada **depois**, no Rust, sem voltar aqui.
-- Resultado em produção: `Source::Tvdb.label()` devolve `"tvdb"`, o
-- enum casa, o clippy passa, os 1068 testes passam — e toda escrita da
-- varredura da TheTVDB morre no banco com
--
--     CHECK constraint failed:
--     search_numbering_source IN ('arr', 'tmdb', 'manual', 'off')
--
-- É a mesma forma de defeito que `tests/css_coverage.rs` existe para
-- pegar do lado do CSS: um valor válido no Rust e inerte no destino,
-- com a suíte verde. A suíte estava verde porque nenhum teste chegava a
-- **persistir** um `Source` — os dois que existiam eram puros, sobre
-- `may_replace` e sobre `label`/`parse`. `every_source_can_be_persisted`
-- fecha isso percorrendo o enum de verdade contra um banco migrado.
--
-- ## Por que não é um rebuild de tabela
--
-- SQLite não tem `ALTER TABLE ... DROP CONSTRAINT`, e o procedimento de
-- 12 passos mexeria numa tabela com cinco filhas por FK. `DROP COLUMN`
-- existe desde 3.35 e a coluna só é referenciada pelo próprio CHECK, o
-- que torna o caminho curto viável: cria a nova com o CHECK certo, copia,
-- derruba a antiga, renomeia. O `RENAME COLUMN` reescreve a referência
-- dentro do CHECK junto — verificado numa cópia da base de produção,
-- com os valores preservados, `integrity_check` e `foreign_key_check` ok.

ALTER TABLE library_items ADD COLUMN numbering_source TEXT
    CHECK (numbering_source IN ('arr', 'tvdb', 'tmdb', 'manual', 'off'));

UPDATE library_items SET numbering_source = search_numbering_source;

ALTER TABLE library_items DROP COLUMN search_numbering_source;

ALTER TABLE library_items RENAME COLUMN numbering_source TO search_numbering_source;
