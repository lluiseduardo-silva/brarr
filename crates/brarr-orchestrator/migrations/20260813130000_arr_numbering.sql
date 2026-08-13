-- # Quem decidiu a numeração de busca
--
-- `20260808130000` deu ao brarr uma tabela de tradução e uma tela para
-- escolher um episode group do TMDB. Funciona, e ninguém vai usar: são
-- 15 séries neste catálogo cuja numeração diverge, e escolher uma
-- ordenação à mão por título — depois de descobrir que ela existe, e
-- qual das onze é a certa — é um trabalho que ninguém faz. Duas foram
-- aplicadas em uma semana.
--
-- O \*arr já sabe a resposta. Sonarr é numerado pelo TheTVDB, a cena
-- segue o TheTVDB, e o brarr já lê `/api/v3/episode` inteiro em toda
-- passada — season, episode e `absoluteEpisodeNumber`, para cada
-- episódio, com ou sem arquivo. Medido contra os nomes de release que
-- este banco já viu: a coordenada canônica bate em **0** deles e a do
-- Sonarr bate em **100%** (Jujutsu Kaisen 73/73, Solo Leveling 13/13,
-- Rent-a-Girlfriend 10/10, Re:ZERO 4/4). São 669 episódios em 15 séries
-- que ganham tradução sem o operador tocar em nada.
--
-- ## Por que uma coluna de origem, e não um id mágico
--
-- Derivar automaticamente e deixar escolher à mão são duas coisas que
-- precisam conviver, e a pergunta "posso sobrescrever isto?" tem que ter
-- resposta antes da escrita. Testar `search_group_id <> 'arr'` seria
-- decidir por comparação de string um assunto que é de estado.
--
-- Quatro valores explícitos, e NULL como quinto:
--
--   NULL     -- ninguém decidiu; a varredura pode derivar
--   'arr'    -- derivado do \*arr; a varredura mantém atualizado
--   'tmdb'   -- o operador escolheu um episode group; a varredura não toca
--   'manual' -- o operador declarou os blocos à mão; a varredura não toca
--   'off'    -- o operador desligou; a varredura não toca
--
-- `'manual'` existe porque nem todo título tem um \*arr por trás, e o
-- \*arr também erra. Solo Leveling é uma temporada de 25 no TMDB, duas
-- de 12 e 13 no TheTVDB, e as releases seguem o TheTVDB — o TMDB não
-- está errado, quem publica é que corta em outro lugar. Declarar onde o
-- bloco corta é a mesma tabela de tradução, escrita por quem sabe.
--
-- `'off'` existe porque sem ele o botão "voltar ao original" seria um
-- botão que não faz nada: a passada seguinte re-derivaria em trinta
-- minutos. Uma alavanca que move o número mas não o comportamento é uma
-- alavanca quebrada, e este repositório já disse isso uma vez sobre a
-- temporada 0.
--
-- O backfill marca 'tmdb' no que já tem grupo aplicado hoje, porque a
-- única forma de ter um até agora era clicando.

ALTER TABLE library_items ADD COLUMN search_numbering_source TEXT
    CHECK (search_numbering_source IN ('arr', 'tmdb', 'manual', 'off'));

UPDATE library_items SET search_numbering_source = 'tmdb'
    WHERE search_group_id IS NOT NULL;
