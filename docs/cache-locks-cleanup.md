# Locks e limpeza do cache

Os locks desta fase são locais à máquina e usam locks advisory do sistema operacional por meio de `fs4`. O arquivo de lock pode permanecer no disco depois da execução; a posse do lock está no file descriptor e é liberada automaticamente quando o processo termina, inclusive após crash ou `SIGKILL`.

## Locks de operação

Existem namespaces independentes:

```text
cache/
└── locks/
    ├── source/<digest>.lock
    └── target/<digest>.lock
```

- source: nome do profile + database;
- target: Docker context + ID completo do container + database.

Os componentes são codificados em um SHA-256 canônico com separação por tamanho. Hostnames, nomes de database e identificadores de container não aparecem no nome do arquivo. Diretórios recebem `0700` e arquivos `0600` em Unix.

Um lock de source não conflita com um lock de target. Bancos diferentes também podem operar em paralelo. Uma segunda operação com a mesma chave falha imediatamente com `Busy`; não existe espera indefinida dentro dessa infraestrutura.

Essa garantia vale apenas dentro da mesma máquina e cache root. Dois developers continuam podendo exportar o mesmo source ao mesmo tempo.

## Lease dos artefatos

Cada staging nasce com `.artifact.lock` mantido em modo exclusivo até publicação ou descarte. Um cache hit adquire uma lease compartilhada antes de ler metadata e checksum e mantém essa lease dentro de `ValidatedCacheHit`.

O caminho do artefato fica disponível somente por referência ao hit. O artefato publicado não é clonável, reduzindo a chance de o consumidor guardar um path depois de liberar a lease.

O cleanup precisa adquirir o mesmo lock em modo exclusivo. Assim ele não remove:

- dump ainda em criação, mesmo que dure mais que o TTL de partial;
- dump validado que está aguardando ou executando restore;
- artefato usado por outro processo local.

## Cleanup

Defaults:

- artefato completo: TTL de duas horas, contado desde `completed_at`;
- diretório `.part`: uma hora desde a última alteração do diretório.

A remoção nunca começa diretamente com `remove_dir_all`. Sob lock exclusivo, o diretório recebe primeiro um nome único `.deleting-<uuid>` por rename atômico. A partir daí novos readers não conseguem adquiri-lo pelo caminho antigo. O lock é liberado e somente o diretório isolado é removido.

Se o processo cair depois do rename, a próxima limpeza reconhece o nome `.deleting-<uuid>`, readquire o lock exclusivo e conclui a remoção. Symlinks e diretórios fora dos formatos gerenciados não são seguidos.

Entradas com relógio futuro ou metadata inválida são preservadas e contabilizadas no relatório, pois apagá-las oportunisticamente esconderia um problema que aparece em `cache list`/`cache clean`. Os comandos usam o mesmo protocolo de lease desta infraestrutura.

O relatório diferencia artefatos expirados, partials órfãos, deleções interrompidas, locks ativos, relógio futuro e entradas inválidas. `cache purge TENANT` limita a remoção ao profile ativo e também mantém qualquer artefato com lease ativa.
