# Artefatos de dump

Cada dump completo é uma unidade imutável identificada por UUID:

```text
cache/
└── profiles/
    └── local-source/
        └── salt_sagatec/
            └── 8f2d.../
                ├── dump.sql.zst
                └── metadata.json
```

Durante a criação, o último diretório possui o sufixo `.part`. A listagem do cache ignora nomes parciais e também ignora diretórios publicados que não tenham simultaneamente `dump.sql.zst` e `metadata.json`. Um `.artifact.lock` interno protege tanto o staging quanto readers de artefatos completos.

## Publicação

A ordem implementada é:

1. criar a hierarquia privada;
2. criar o diretório UUID com sufixo `.part`;
3. criar `dump.sql.zst` com `create_new`;
4. comprimir o stream;
5. executar `fsync` no dump;
6. comparar a metadata com o resultado tipado da compressão e conferir o tamanho em disco;
7. gravar e sincronizar `metadata.json.part`;
8. renomear para `metadata.json`;
9. sincronizar o diretório parcial;
10. renomear atomicamente o diretório para o UUID final;
11. sincronizar o diretório pai nos sistemas Unix.

Falhas normais removem o staging por `Drop`. Uma interrupção abrupta do processo pode deixar o diretório `*.part`, que permanece invisível até o cleanup oportunista. Depois do rename final, os dois arquivos já estão completos. As regras de lock e isolamento antes da remoção estão em [`cache-locks-cleanup.md`](cache-locks-cleanup.md).

No Unix, diretórios recebem modo `0700` e arquivos `0600`. macOS e Linux seguem o mesmo fluxo; a sincronização explícita de diretório é condicional a Unix para manter a implementação portável.

## Metadata v1

O formato `mysql-sql-zstd-v1` registra:

- dump ID;
- tenant lookup e tenant ID canônico;
- database e profile;
- fingerprint do source;
- versões do source e client;
- charset e collation;
- versão da DumpPolicy;
- criação e conclusão em segundos Unix;
- bytes SQL e comprimidos;
- SHA-256 do SQL e do arquivo Zstd.

Identificadores, versões e hashes são value objects validados. A metadata não possui host, username, credential key, senha, nomes de objetos SQL ou valores de `DEFINER`.

O store exige o `CompressionMetrics` que produziu o arquivo e confere bytes e os dois SHA-256 contra a metadata, sem reler um artefato possivelmente enorme. Ele também confere o tamanho efetivo em disco depois do `fsync`. Na leitura, o cache volta a validar identidade, TTL, fingerprint, tamanho e checksum conforme [`cache-validity.md`](cache-validity.md).
