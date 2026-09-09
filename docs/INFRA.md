# Acesso à infraestrutura

A instância de produção do brarr roda numa LXC do Proxmox de casa. Este
documento existe porque a informação vivia só na cabeça do operador, e
uma investigação que precisa do banco de produção (medir uma rajada,
conferir headers de uma API autenticada) parava antes de começar.

## Chave

`claude_pmx`, na raiz do repositório. **Não é versionada** — o
`.gitignore` carrega `claude_pmx*` — e não deve ser. O que se versiona é
o mapa, não a credencial.

## Endereços

| o quê | endereço | como |
|---|---|---|
| Hipervisor | `root@10.0.1.4` | `ssh -i claude_pmx root@10.0.1.4` |
| LXC do brarr (CT **103**, `arrstack`) | `root@10.0.1.246` | `ssh -i claude_pmx root@10.0.1.246` |

A LXC aceita a mesma chave desde 2026-09-09; antes disso o único caminho
era `pct exec 103 -- ...` pelo hipervisor, que continua valendo como
resgate se o sshd da LXC morrer:

```bash
ssh -i claude_pmx root@10.0.1.4 'pct exec 103 -- bash -lc "docker ps"'
```

Opcionalmente, em `~/.ssh/config`:

```
Host pmx
    HostName 10.0.1.4
    User root
    IdentityFile ~/dev/brarr/claude_pmx
    IdentitiesOnly yes

Host arrstack
    HostName 10.0.1.246
    User root
    IdentityFile ~/dev/brarr/claude_pmx
    IdentitiesOnly yes
```

## O que roda lá

`arrstack` é um Docker host. `brarr` é um container ao lado dos \*arr que
ele substitui (`sonarr`, `sonarr-series`, `sonarr-animes`, `radarr`,
`prowlarr`), dos clients (`qbittorrent` numa LXC própria em 10.0.1.108,
`sabnzbd` aqui) e do `jellyseerr`. O Plex é a CT **101** (10.0.1.248).

## O banco

```
/var/lib/docker/volumes/brarr-data/_data/brarr.db
```

`sqlite3` está instalado na LXC. **Sempre `-readonly`** ao investigar: o
brarr está rodando, e o WAL de uma segunda conexão escrevendo é como se
perde uma passada de varredura (ver `db::begin_write`).

```bash
ssh -i claude_pmx root@10.0.1.246 \
  "sqlite3 -readonly /var/lib/docker/volumes/brarr-data/_data/brarr.db \
   'SELECT name,kind,enabled FROM providers;'"
```

`providers.api_token` e `media_servers.token` são credenciais de
verdade. Ler para usar numa requisição é legítimo; imprimir no terminal
não é — as consultas de diagnóstico deste repositório usam
`length(api_token)` e redigem query strings com `sed`.

Medir a partir da LXC, e não da máquina de desenvolvimento, é o ponto
quando o assunto é rate limit: o limite é por IP de origem, e só de lá a
medição vale.
