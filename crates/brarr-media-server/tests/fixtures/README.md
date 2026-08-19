# Fixtures — de onde vieram

## `plex_sections.json`

`GET /library/sections` com `Accept: application/json`, capturado **ao
vivo** em 2026-08-18 de um Plex Media Server **1.43.3.10861-07dfddaeb**.
Não foi editado — nem para encurtar, nem para anonimizar caminhos.

Não contém segredo: o `X-Plex-Token` viaja no header e não aparece em
lugar nenhum da resposta.

| O que fixa | Por quê |
|---|---|
| **Duas seções do mesmo `type`** (`Animes` e `Series`, ambas `show`) | É a razão de o casamento ser por caminho. Escolher a seção pelo tipo de mídia acerta metade dos episódios desta biblioteca. |
| `key` é **string** (`"3"`), `Location.id` é **inteiro** (`3`) | No mesmo objeto. Um `key` tipado como número compila, roda e quebra numa instalação onde a seção não é numérica. |
| `Location` é uma **lista** | Uma seção pode ser montada sobre mais de um diretório. |
| Campos que o brarr ignora (`agent`, `scanner`, `uuid`, `scannedAt`, …) | Provam que a desserialização tolera o payload inteiro em vez de exigir um subconjunto. |

### O que ela *não* cobre

- **Seção com múltiplas `Location`** — este servidor tem uma por seção.
  Coberto por teste unitário em `src/plex/mod.rs`.
- **Caminho Windows ou UNC** — este servidor é Linux. Idem.
- **O JSON antigo com `_children`**, que versões de PMS anteriores à 1.3
  emitiam no lugar de `MediaContainer`. Deliberadamente fora: a faixa é
  de 2016 e o suporte a ela seria código sem nenhum servidor para
  exercê-lo.

### Como recapturar

```bash
T=$(grep -o 'PlexOnlineToken="[^"]*"' \
  "/var/lib/plexmediaserver/Library/Application Support/Plex Media Server/Preferences.xml" \
  | cut -d'"' -f2)
curl -s -H "X-Plex-Token: $T" -H "Accept: application/json" \
  http://127.0.0.1:32400/library/sections | python3 -m json.tool
```

## Emby / Jellyfin

Sem fixture. Os payloads que o brarr lê dos dois (`System/Info`,
`Library/VirtualFolders`, e o corpo que ele *envia* em
`Library/Media/Updated`) são pequenos e estão inline em
`tests/client_wiremock.rs`. Assim que houver uma instância de verdade
para capturar — os contêineres descartáveis do roteiro de validação —
vale trazer as duas respostas para cá.
