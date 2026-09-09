-- # Um season pack vira um grab por episódio
--
-- A busca interativa oferece "episódio em branco = pack" e grava
-- `scope = 'season'` desde `20260805120000`. O importador nunca aprendeu
-- a outra metade: `plan_and_place` deriva o marcador do episódio **só**
-- de `grab.episode_id` e nunca lê `grab.season_number`, então um pack
-- chega ali como `marker = None` — e daí em diante o importador se
-- comporta como se fosse um filme. `pick_video` devolve "o maior vídeo
-- que não é sample" (um arquivo dos 26) e `destination` toma o ramo de
-- filme, `{root}/Título/Título.mkv`, sem pasta de temporada e sem
-- marcador no nome.
--
-- Medido no disco deste operador: `Cowboy Bebop S01`, 26 arquivos
-- baixados, **um** colocado, `Animes/Cowboy Bebop/Cowboy Bebop.mkv` —
-- que é o episódio 23, por ser o maior. E como `covers_target` faz
-- `scope = 'season'` cobrir toda a temporada, a tela declarava os 26
-- episódios presentes, todos apontando para aquele único arquivo. Não é
-- caso isolado: são exatamente três linhas no banco, e as três são todo
-- pack que já entrou (Cowboy Bebop, Tremembé, Fim).
--
-- ## Por que um quarto escopo
--
-- `scope = 'season'` está **certo** entre a reserva e o import: enquanto
-- o pack baixa, ele precisa cobrir a temporada inteira, ou a varredura
-- sairia pegando os 26 episódios individualmente ao lado dele. E passa a
-- estar **errado** no instante em que os arquivos são colocados, porque
-- aí o brarr sabe exatamente quais episódios recebeu. O fan-out *é* essa
-- transição, então é onde o escopo muda.
--
-- `fanned` quer dizer: esta aquisição aconteceu, a cobertura é dos
-- filhos, e esta linha não cobre nada. Isso resolve o pack parcial por
-- construção — um pack de 5 arquivos numa temporada de 26 deixa 21
-- episódios descobertos, que é a verdade — em vez de por uma regra que
-- alguém precisa lembrar de aplicar.
--
-- O pai continua `imported`, e com isso continua ocupando sua chave em
-- `idx_grabs_unique_item`, então o mesmo pack não é pego duas vezes.
--
-- ## As duas colunas
--
-- `parent_grab_id` é `ON DELETE SET NULL` e não `CASCADE`: histórico de
-- aquisição nunca é apagado, e um cascade seria justamente uma rota para
-- apagá-lo. Um filho que perde o pai continua sendo o registro verdadeiro
-- de um arquivo em disco.
--
-- `pack_report` é a metade durável de "contar e reportar, nunca descartar
-- em silêncio": quais arquivos do pack não parearam com episódio nenhum,
-- e por quê. Sem ela isso viveria só num `warn!` e o operador veria 23 de
-- 26 episódios sem explicação nenhuma. Não cabe em `error`, que significa
-- "este grab falhou" em todo lugar e renderiza vermelho, nem em
-- `import_wait_reason`, que é "esperar não é falhar" (`20260806140000`).
--
-- ## Sem backfill
--
-- As três linhas quebradas ficam `season` de propósito. Elas ainda
-- apontam para um arquivo que existe, e reescrever o escopo aqui
-- descobriria os episódios sem remover o arquivo errado da biblioteca —
-- trocaria uma mentira por uma bagunça. O reparo é pela UI, com
-- "esquecer", que cuida do arquivo e da linha juntos.
--
-- O CHECK de `scope` não é alterável no SQLite, então isto é o rebuild de
-- 12 passos, na forma de `20260805120000`: tabela nova, cópia, drop,
-- rename, e **todos** os índices recriados.

PRAGMA foreign_keys = OFF;

CREATE TABLE grabs_new (
    id                TEXT    PRIMARY KEY NOT NULL,
    item_id           TEXT    NOT NULL,
    episode_id        TEXT,
    season_number     INTEGER,

    decision_id       TEXT,
    provider_id       TEXT,
    provider_name     TEXT    NOT NULL,

    release_id_remote TEXT    NOT NULL,
    release_name      TEXT    NOT NULL,
    download_url      TEXT,
    -- `local` = a file that was already on disk when brarr met it.
    protocol          TEXT    NOT NULL CHECK (protocol IN ('torrent', 'usenet', 'local')),

    client_id         TEXT,
    client_item_id    TEXT,

    status            TEXT    NOT NULL CHECK (status IN
                          ('reserved', 'sent', 'downloading', 'completed',
                           'imported', 'failed', 'rejected')),
    error             TEXT,
    imported_path     TEXT,
    file_missing_at   INTEGER,

    grabbed_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,

    import_wait_reason  TEXT,
    import_attempted_at INTEGER,

    -- `fanned` = the pack was placed; its children carry the coverage.
    scope             TEXT    NOT NULL DEFAULT 'item'
                          CHECK (scope IN ('item', 'season', 'episode', 'fanned')),

    -- The pack this row came out of, for a child written by the fan-out.
    parent_grab_id    TEXT,
    -- Files the pack held that paired with no episode, and why.
    pack_report       TEXT,

    FOREIGN KEY (item_id)        REFERENCES library_items(id)    ON DELETE CASCADE,
    FOREIGN KEY (episode_id)     REFERENCES library_episodes(id) ON DELETE SET NULL,
    FOREIGN KEY (decision_id)    REFERENCES decisions(id)        ON DELETE SET NULL,
    FOREIGN KEY (provider_id)    REFERENCES providers(id)        ON DELETE SET NULL,
    FOREIGN KEY (client_id)      REFERENCES download_clients(id) ON DELETE SET NULL,
    FOREIGN KEY (parent_grab_id) REFERENCES grabs_new(id)        ON DELETE SET NULL
) STRICT;

INSERT INTO grabs_new (
    id, item_id, episode_id, season_number, decision_id, provider_id,
    provider_name, release_id_remote, release_name, download_url, protocol,
    client_id, client_item_id, status, error, imported_path, file_missing_at,
    grabbed_at, updated_at, import_wait_reason, import_attempted_at, scope
)
SELECT
    id, item_id, episode_id, season_number, decision_id, provider_id,
    provider_name, release_id_remote, release_name, download_url, protocol,
    client_id, client_item_id, status, error, imported_path, file_missing_at,
    grabbed_at, updated_at, import_wait_reason, import_attempted_at, scope
FROM grabs;

DROP TABLE grabs;

ALTER TABLE grabs_new RENAME TO grabs;

-- Every index, recreated exactly as it stood. The two unique families are
-- what the whole barrier rests on, so they are copied rather than
-- rewritten: `20260805120000` for the tracker halves, `20260813120000`
-- for the local ones.
CREATE UNIQUE INDEX idx_grabs_unique_episode
    ON grabs(provider_id, release_id_remote, item_id, episode_id)
    WHERE episode_id IS NOT NULL AND file_missing_at IS NULL;

CREATE UNIQUE INDEX idx_grabs_unique_item
    ON grabs(provider_id, release_id_remote, item_id)
    WHERE episode_id IS NULL AND file_missing_at IS NULL;

CREATE UNIQUE INDEX idx_grabs_unique_local_episode
    ON grabs(item_id, release_id_remote, episode_id)
    WHERE protocol = 'local' AND file_missing_at IS NULL AND episode_id IS NOT NULL;

CREATE UNIQUE INDEX idx_grabs_unique_local_whole
    ON grabs(item_id, release_id_remote)
    WHERE protocol = 'local' AND file_missing_at IS NULL AND episode_id IS NULL;

CREATE INDEX idx_grabs_status     ON grabs(status);
CREATE INDEX idx_grabs_item       ON grabs(item_id);
CREATE INDEX idx_grabs_grabbed_at ON grabs(grabbed_at DESC);
CREATE INDEX idx_grabs_client     ON grabs(client_id);
CREATE INDEX idx_grabs_season     ON grabs(item_id, season_number);
CREATE INDEX idx_grabs_scope      ON grabs(scope);

CREATE INDEX idx_grabs_imported_present
    ON grabs(status)
    WHERE status = 'imported' AND file_missing_at IS NULL;

CREATE INDEX idx_grabs_awaiting_import
    ON grabs(import_attempted_at, updated_at)
    WHERE status = 'completed';

CREATE INDEX idx_grabs_parent ON grabs(parent_grab_id);

PRAGMA foreign_keys = ON;
